//! Windows backend: Media Foundation hardware MFTs with D3D12 zero-copy import/export.
//!
//! M0 shipped the probe (hardware MFT enumeration). M1 adds the decode → D3D12 →
//! encode path: `d3d` (device pair, shared surfaces, fence), `mft` (transform driver),
//! `decoder`, `encoder`, and [`WinBackend`]. The `interop_poc` example exercises the
//! whole path on one file and prints which interop plan was taken.

use onchip_core::GpuContext;
use onchip_core::backend::BackendInfo;

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

#[cfg(not(target_os = "windows"))]
pub fn enumerate_hw_codecs(
    _ctx: &GpuContext,
) -> (
    Vec<onchip_core::backend::CodecInfo>,
    Vec<onchip_core::backend::CodecInfo>,
) {
    (Vec::new(), Vec::new())
}

/// Open the backend. Only works on Windows with wgpu on D3D12.
#[cfg(not(target_os = "windows"))]
pub fn open(_ctx: std::sync::Arc<GpuContext>) -> Result<(), onchip_core::backend::BackendError> {
    Err(onchip_core::backend::BackendError::Unavailable(
        "the Windows backend only runs on Windows",
    ))
}

#[cfg(target_os = "windows")]
pub use mf::enumerate_hw_codecs;

#[cfg(target_os = "windows")]
pub mod d3d;
#[cfg(target_os = "windows")]
pub mod decoder;
#[cfg(target_os = "windows")]
pub mod encoder;
#[cfg(target_os = "windows")]
pub mod mft;
#[cfg(target_os = "windows")]
pub mod pipeline;

#[cfg(target_os = "windows")]
pub use backend::{WinBackend, open};

#[cfg(target_os = "windows")]
mod backend {
    use std::sync::Arc;

    use onchip_core::backend::{
        BackendError, BackendInfo, EncoderConfig, StreamInfo, VideoBackend,
    };
    use onchip_core::frame::GpuFrame;
    use onchip_core::{ColorSpec, GpuContext};
    use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_NOSOCKET, MFStartup};

    use crate::d3d::{D3dShared, SharedSurface, SurfaceRole};
    use crate::decoder::WinDecoder;
    use crate::encoder::WinEncoder;

    pub struct WinBackend {
        pub ctx: Arc<GpuContext>,
        pub d3d: Arc<D3dShared>,
    }

    /// Start Media Foundation and build the D3D11 side next to wgpu's D3D12 device.
    pub fn open(ctx: Arc<GpuContext>) -> Result<WinBackend, BackendError> {
        // SAFETY: MFStartup may be called more than once per process; it is reference counted.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }
            .map_err(|e| crate::d3d::werr("MFStartup", e))?;
        let d3d = D3dShared::new(ctx.clone())?;
        Ok(WinBackend { ctx, d3d })
    }

    impl WinBackend {
        /// A fresh shared surface the executor can render into and the encoder can read.
        pub fn alloc_encoder_surface(
            &self,
            width: u32,
            height: u32,
        ) -> Result<Arc<SharedSurface>, BackendError> {
            self.d3d
                .create_shared_surface(width, height, SurfaceRole::EncoderInput)
        }

        pub fn surface_frame(&self, surface: &Arc<SharedSurface>, color: ColorSpec) -> GpuFrame {
            surface.frame(color, 0, None)
        }

        pub fn open_decoder_with_rate(
            &self,
            stream: &StreamInfo,
            framerate: (u32, u32),
        ) -> Result<WinDecoder, BackendError> {
            WinDecoder::open(self.d3d.clone(), stream, framerate)
        }
    }

    impl VideoBackend for WinBackend {
        type Decoder = WinDecoder;
        type Encoder = WinEncoder;

        fn probe(&self) -> BackendInfo {
            crate::backend_info(&self.ctx)
        }

        fn open_decoder(&self, stream: &StreamInfo) -> Result<WinDecoder, BackendError> {
            WinDecoder::open(self.d3d.clone(), stream, (30, 1))
        }

        fn open_encoder(&self, cfg: &EncoderConfig) -> Result<WinEncoder, BackendError> {
            WinEncoder::open(self.d3d.clone(), cfg)
        }
    }
}

#[cfg(target_os = "windows")]
mod mf {
    use onchip_core::GpuContext;
    use onchip_core::backend::{Codec, CodecInfo};
    use onchip_core::frame::PixelFormat;
    use windows::Win32::Media::MediaFoundation::{
        MF_VERSION, MFMediaType_Video, MFSTARTUP_NOSOCKET, MFShutdown, MFStartup,
        MFT_CATEGORY_VIDEO_DECODER, MFT_CATEGORY_VIDEO_ENCODER, MFT_REGISTER_TYPE_INFO,
        MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_NV12, MFVideoFormat_P010,
    };
    use windows::core::GUID;

    use crate::mft::enumerate_hw;

    /// Enumerate hardware video decoder / encoder MFTs for H.264 and HEVC.
    ///
    /// Facts about what this does and does not prove: an MFT being registered as
    /// hardware means the vendor's driver exposes it; it does not guarantee it accepts
    /// D3D11 textures or P010. That is checked when the MFT is actually opened.
    pub fn enumerate_hw_codecs(_ctx: &GpuContext) -> (Vec<CodecInfo>, Vec<CodecInfo>) {
        // SAFETY: MFStartup/MFShutdown are balanced; MF is reference counted per process.
        let started = unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }.is_ok();
        let mut decoders = Vec::new();
        let mut encoders = Vec::new();
        for (codec, guid) in [
            (Codec::H264, MFVideoFormat_H264),
            (Codec::Hevc, MFVideoFormat_HEVC),
        ] {
            let compressed = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: guid,
            };
            if let Ok(list) = enumerate_hw(&MFT_CATEGORY_VIDEO_DECODER, Some(&compressed), None) {
                for (name, _) in list {
                    decoders.push(CodecInfo {
                        codec,
                        name,
                        formats: probe_formats(&MFT_CATEGORY_VIDEO_DECODER, &compressed, true),
                    });
                }
            }
            if let Ok(list) = enumerate_hw(&MFT_CATEGORY_VIDEO_ENCODER, None, Some(&compressed)) {
                for (name, _) in list {
                    encoders.push(CodecInfo {
                        codec,
                        name,
                        formats: probe_formats(&MFT_CATEGORY_VIDEO_ENCODER, &compressed, false),
                    });
                }
            }
        }
        if started {
            // SAFETY: balanced with the MFStartup above.
            let _ = unsafe { MFShutdown() };
        }
        (decoders, encoders)
    }

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
                enumerate_hw(category, Some(compressed), Some(&raw))
                    .map(|l| !l.is_empty())
                    .unwrap_or(false)
            } else {
                enumerate_hw(category, Some(&raw), Some(compressed))
                    .map(|l| !l.is_empty())
                    .unwrap_or(false)
            };
            if found {
                out.push(fmt);
            }
        }
        out
    }
}
