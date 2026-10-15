//! Windows backend: Media Foundation hardware MFTs with D3D12 zero-copy import/export.
//!
//! M0 ships the probe (hardware MFT enumeration). The decode → D3D12 → encode path is
//! M1 (`ROADMAP.md` RM-001..RM-010); its interop PoC lives in `examples/interop_poc.rs`.

use onchip_core::GpuContext;
use onchip_core::backend::{BackendError, BackendInfo};

/// Name reported by `onchip probe`.
pub const NAME: &str = "mediafoundation+d3d12";

pub fn backend_info(ctx: &GpuContext) -> BackendInfo {
    let (hw_decoders, hw_encoders) = enumerate_hw_codecs(ctx);
    BackendInfo {
        name: NAME.into(),
        adapter: ctx.info.clone(),
        hw_decoders,
        hw_encoders,
    }
}

/// Placeholder until M1.
pub fn open() -> Result<(), BackendError> {
    Err(BackendError::Unavailable(
        "Windows decode/encode path lands in M1",
    ))
}

#[cfg(not(target_os = "windows"))]
pub fn enumerate_hw_codecs(
    _ctx: &GpuContext,
) -> (
    Vec<onchip_core::backend::CodecInfo>,
    Vec<onchip_core::backend::CodecInfo>,
) {
    (Vec::new(), Vec::new())
}

#[cfg(target_os = "windows")]
pub use mf::enumerate_hw_codecs;

#[cfg(target_os = "windows")]
mod mf {
    use onchip_core::GpuContext;
    use onchip_core::backend::{Codec, CodecInfo};
    use onchip_core::frame::PixelFormat;
    use windows::Win32::Media::MediaFoundation::{
        IMFActivate, MF_VERSION, MFMediaType_Video, MFSTARTUP_NOSOCKET, MFShutdown, MFStartup,
        MFT_CATEGORY_VIDEO_DECODER, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG,
        MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_FRIENDLY_NAME_Attribute,
        MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_HEVC,
        MFVideoFormat_NV12, MFVideoFormat_P010,
    };
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::core::{GUID, PWSTR};

    /// Enumerate hardware video decoder / encoder MFTs for H.264 and HEVC.
    ///
    /// Facts about what this does and does not prove: an MFT being registered as
    /// hardware means the vendor's driver exposes it; it does not guarantee it accepts
    /// D3D11 textures or P010. That is checked when the MFT is actually opened (M1).
    pub fn enumerate_hw_codecs(_ctx: &GpuContext) -> (Vec<CodecInfo>, Vec<CodecInfo>) {
        // SAFETY: MFStartup/MFShutdown are balanced; MF is re-entrant per thread.
        let started = unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }.is_ok();
        let mut decoders = Vec::new();
        let mut encoders = Vec::new();
        for (codec, guid) in [
            (Codec::H264, MFVideoFormat_H264),
            (Codec::Hevc, MFVideoFormat_HEVC),
        ] {
            // decoders: input = compressed, output = anything
            let input = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: guid,
            };
            for name in enumerate(&MFT_CATEGORY_VIDEO_DECODER, Some(&input), None) {
                decoders.push(CodecInfo {
                    codec,
                    name,
                    formats: probe_formats(&MFT_CATEGORY_VIDEO_DECODER, &input, true),
                });
            }
            // encoders: input = anything, output = compressed
            let output = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: guid,
            };
            for name in enumerate(&MFT_CATEGORY_VIDEO_ENCODER, None, Some(&output)) {
                encoders.push(CodecInfo {
                    codec,
                    name,
                    formats: probe_formats(&MFT_CATEGORY_VIDEO_ENCODER, &output, false),
                });
            }
        }
        if started {
            // SAFETY: balanced with the MFStartup above.
            let _ = unsafe { MFShutdown() };
        }
        (decoders, encoders)
    }

    /// Which of NV12 / P010 the hardware MFT registers for this codec. `is_decoder`
    /// selects whether the uncompressed side is the output (decoder) or input (encoder).
    fn probe_formats(
        category: &GUID,
        compressed: &MFT_REGISTER_TYPE_INFO,
        is_decoder: bool,
    ) -> Vec<PixelFormat> {
        let mut out = Vec::new();
        for (fmt, guid) in [
            (PixelFormat::Nv12, MFVideoFormat_NV12),
            (PixelFormat::P010, MFVideoFormat_P010),
        ] {
            let raw = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: guid,
            };
            let found = if is_decoder {
                !enumerate(category, Some(compressed), Some(&raw)).is_empty()
            } else {
                !enumerate(category, Some(&raw), Some(compressed)).is_empty()
            };
            if found {
                out.push(fmt);
            }
        }
        out
    }

    fn enumerate(
        category: &GUID,
        input: Option<&MFT_REGISTER_TYPE_INFO>,
        output: Option<&MFT_REGISTER_TYPE_INFO>,
    ) -> Vec<String> {
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count: u32 = 0;
        let flags: MFT_ENUM_FLAG = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER;
        // SAFETY: out-pointers are valid for the call; the returned array is freed below.
        let hr = unsafe {
            MFTEnumEx(
                *category,
                flags,
                input.map(|p| p as *const _),
                output.map(|p| p as *const _),
                &mut activates,
                &mut count,
            )
        };
        if hr.is_err() || activates.is_null() {
            return Vec::new();
        }
        let mut names = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            // SAFETY: `activates` holds `count` entries allocated by MFTEnumEx.
            let slot = unsafe { &mut *activates.add(i) };
            if let Some(act) = slot.take() {
                names.push(friendly_name(&act).unwrap_or_else(|| format!("<unnamed MFT #{i}>")));
                drop(act);
            }
        }
        // SAFETY: the array was allocated with CoTaskMemAlloc by MFTEnumEx.
        unsafe { CoTaskMemFree(Some(activates as *const _)) };
        names
    }

    fn friendly_name(act: &IMFActivate) -> Option<String> {
        let mut pw = PWSTR::null();
        let mut len: u32 = 0;
        // SAFETY: GetAllocatedString allocates with CoTaskMemAlloc; freed below.
        let ok = unsafe { act.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut pw, &mut len) }
            .is_ok();
        if !ok || pw.is_null() {
            return None;
        }
        // SAFETY: `pw` is a valid NUL-terminated wide string of `len` characters.
        let s = unsafe { pw.to_string() }.ok();
        // SAFETY: allocated by GetAllocatedString.
        unsafe { CoTaskMemFree(Some(pw.as_ptr() as *const _)) };
        s
    }
}
