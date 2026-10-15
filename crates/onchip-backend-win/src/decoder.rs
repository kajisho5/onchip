//! Media Foundation hardware H.264 decoder whose output frames are shared NV12
//! surfaces visible to wgpu (see `d3d.rs`).

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use onchip_core::ColorSpec;
use onchip_core::backend::{BackendError, Codec, Decoder, Packet, StreamInfo};
use onchip_core::container::{AvcConfig, avcc_to_annexb};
use onchip_core::frame::GpuFrame;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVDecVideoAcceleration_H264, ICodecAPI, IMFDXGIBuffer, IMFSample, MF_MT_FRAME_SIZE,
    MFCreateDXGISurfaceBuffer, MFCreateMemoryBuffer, MFCreateSample, MFSampleExtension_CleanPoint,
    MFVideoFormat_H264, MFVideoFormat_NV12,
};
use windows::Win32::System::Variant::VARIANT;
use windows::core::Interface;

use crate::d3d::{D3dShared, InteropPath, SharedSurface, SurfaceRole, werr};
use crate::mft::{
    Pulled, Transform, enumerate_any, enumerate_hw, unpack2, video_media_type, video_type_info,
};

const POOL_MAX: usize = 16;

struct Slot {
    surface: Arc<SharedSurface>,
    in_use: Arc<AtomicBool>,
}

/// Released when the frame that borrowed a pool slot is dropped.
struct SlotGuard(Arc<AtomicBool>);

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub struct WinDecoder {
    d3d: Arc<D3dShared>,
    tf: Transform,
    avc: AvcConfig,
    width: u32,
    height: u32,
    color: ColorSpec,
    timebase: (u32, u32),
    frame_duration_100ns: i64,
    pool: Vec<Slot>,
    out_queue: VecDeque<GpuFrame>,
    pub interop: InteropPath,
    eos_sent: bool,
    pub frames_out: u64,
}

impl WinDecoder {
    pub fn open(
        d3d: Arc<D3dShared>,
        stream: &StreamInfo,
        framerate: (u32, u32),
    ) -> Result<WinDecoder, BackendError> {
        if stream.codec != Codec::H264 {
            return Err(BackendError::UnsupportedCodec(stream.codec));
        }
        let avc = AvcConfig::parse(&stream.extradata)
            .map_err(|e| BackendError::Platform(format!("avcC: {e}")))?;

        let input_info = video_type_info(MFVideoFormat_H264);
        let output_info = video_type_info(MFVideoFormat_NV12);
        // Vendor hardware MFTs first (Intel / some AMD / NVIDIA-less boxes ship them).
        // When there is none, the Microsoft H.264 decoder does the hardware decode via
        // D3D11VA once it is given the DXGI device manager — still on-chip.
        let mut found = enumerate_hw(
            crate::mft::decoder_category(),
            Some(&input_info),
            Some(&output_info),
        )?;
        let mut via_dxva = false;
        if found.is_empty() {
            found = enumerate_any(
                crate::mft::decoder_category(),
                Some(&input_info),
                Some(&output_info),
            )?;
            via_dxva = true;
        }
        if found.is_empty() {
            return Err(BackendError::Platform(
                "no H.264 decoder MFT with NV12 output (neither vendor hardware nor Microsoft)"
                    .into(),
            ));
        }
        let (mut name, act) = found.remove(0);
        if via_dxva {
            name.push_str(" (D3D11VA via DXGI device manager)");
        }
        let mut tf = Transform::activate(name, &act, Some(&d3d.dxgi_manager))?;
        if via_dxva {
            // Ask the Microsoft decoder for DXVA explicitly (it defaults to it when a D3D
            // manager is set, but some builds want the switch). Best effort.
            if let Ok(api) = tf.mft.cast::<ICodecAPI>() {
                let on = VARIANT::from(1u32);
                // SAFETY: live ICodecAPI; GUID and VARIANT are valid for the call.
                let _ = unsafe { api.SetValue(&CODECAPI_AVDecVideoAcceleration_H264, &on) };
            }
        }

        let in_type = video_media_type(MFVideoFormat_H264, stream.width, stream.height, framerate)?;
        tf.set_input_type(&in_type)?;
        let out_type = tf.choose_output_type(MFVideoFormat_NV12)?;
        tf.refresh_stream_info()?;
        // The surfaces we hand to wgpu use the stream's declared size. The MFT's own
        // textures may be the coded size (e.g. 1088 rows for 1080p); plan B copies only
        // the visible box, plan A decodes straight into our surface.
        // SAFETY: the type we just set carries a frame size.
        let (type_w, type_h) = unsafe { out_type.GetUINT64(&MF_MT_FRAME_SIZE) }
            .map(unpack2)
            .unwrap_or((stream.width, stream.height));
        let width = if stream.width > 0 {
            stream.width
        } else {
            type_w
        };
        let height = if stream.height > 0 {
            stream.height
        } else {
            type_h
        };
        let width = width & !1;
        let height = height & !1;
        tf.start()?;

        let interop = if tf.provides_samples {
            InteropPath::GpuCopyIntoShared
        } else {
            InteropPath::SharedHandleZeroCopy
        };
        let frame_duration_100ns = if framerate.0 > 0 {
            (10_000_000i64 * framerate.1 as i64) / framerate.0 as i64
        } else {
            333_333
        };
        Ok(WinDecoder {
            d3d,
            tf,
            avc,
            width,
            height,
            color: stream.color,
            timebase: stream.timebase,
            frame_duration_100ns,
            pool: Vec::new(),
            out_queue: VecDeque::new(),
            interop,
            eos_sent: false,
            frames_out: 0,
        })
    }

    pub fn mft_name(&self) -> &str {
        &self.tf.name
    }

    pub fn is_async(&self) -> bool {
        self.tf.is_async
    }

    pub fn provides_samples(&self) -> bool {
        self.tf.provides_samples
    }

    fn ticks_to_100ns(&self, t: i64) -> i64 {
        (t as i128 * 10_000_000 * self.timebase.0 as i128 / self.timebase.1 as i128) as i64
    }

    fn hns_to_ticks(&self, t: i64) -> i64 {
        (t as i128 * self.timebase.1 as i128 / (10_000_000 * self.timebase.0 as i128)) as i64
    }

    fn acquire_slot(&mut self) -> Result<(Arc<SharedSurface>, Arc<AtomicBool>), BackendError> {
        for s in &self.pool {
            if s.in_use
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok((s.surface.clone(), s.in_use.clone()));
            }
        }
        if self.pool.len() >= POOL_MAX {
            return Err(BackendError::Interop(format!(
                "decoder surface pool exhausted ({POOL_MAX} frames in flight) — drop frames before pulling more"
            )));
        }
        let surface =
            self.d3d
                .create_shared_surface(self.width, self.height, SurfaceRole::DecoderOutput)?;
        let in_use = Arc::new(AtomicBool::new(true));
        self.pool.push(Slot {
            surface: surface.clone(),
            in_use: in_use.clone(),
        });
        Ok((surface, in_use))
    }

    fn make_input_sample(&self, packet: &Packet) -> Result<IMFSample, BackendError> {
        let mut data = if packet.keyframe {
            self.avc.parameter_sets_annexb()
        } else {
            Vec::new()
        };
        data.extend_from_slice(&avcc_to_annexb(&packet.data, self.avc.nal_length_size));
        // SAFETY: buffer created with enough room; locked, written, unlocked in order.
        unsafe {
            let buf = MFCreateMemoryBuffer(data.len() as u32)
                .map_err(|e| werr("MFCreateMemoryBuffer", e))?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            buf.Lock(&mut ptr, None, None)
                .map_err(|e| werr("IMFMediaBuffer::Lock", e))?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            buf.Unlock()
                .map_err(|e| werr("IMFMediaBuffer::Unlock", e))?;
            buf.SetCurrentLength(data.len() as u32)
                .map_err(|e| werr("SetCurrentLength", e))?;
            let sample = MFCreateSample().map_err(|e| werr("MFCreateSample", e))?;
            sample.AddBuffer(&buf).map_err(|e| werr("AddBuffer", e))?;
            sample
                .SetSampleTime(self.ticks_to_100ns(packet.pts))
                .map_err(|e| werr("SetSampleTime", e))?;
            sample
                .SetSampleDuration(self.frame_duration_100ns)
                .map_err(|e| werr("SetSampleDuration", e))?;
            if packet.keyframe {
                sample
                    .SetUINT32(&MFSampleExtension_CleanPoint, 1)
                    .map_err(|e| werr("CleanPoint", e))?;
            }
            Ok(sample)
        }
    }

    /// Plan A: a sample wrapping one of our shared surfaces for the MFT to decode into.
    fn make_output_sample(&self, surface: &SharedSurface) -> Result<IMFSample, BackendError> {
        // SAFETY: the surface is a live D3D11 texture; MF holds its own reference.
        unsafe {
            let buf = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &surface.tex11, 0, false)
                .map_err(|e| werr("MFCreateDXGISurfaceBuffer", e))?;
            let sample = MFCreateSample().map_err(|e| werr("MFCreateSample", e))?;
            sample.AddBuffer(&buf).map_err(|e| werr("AddBuffer", e))?;
            Ok(sample)
        }
    }

    /// Turn an MFT output sample into a frame on a shared surface (plan A: it already
    /// is one; plan B: GPU copy from the MFT's texture).
    fn finish_output(
        &mut self,
        sample: IMFSample,
        slot: Option<(Arc<SharedSurface>, Arc<AtomicBool>)>,
    ) -> Result<GpuFrame, BackendError> {
        let (surface, in_use) = match slot {
            Some(s) => s,
            None => self.acquire_slot()?,
        };
        if self.interop == InteropPath::GpuCopyIntoShared {
            // SAFETY: the sample's first buffer is a DXGI buffer (the MFT has the D3D
            // manager); the texture it refers to stays alive for the copy.
            unsafe {
                let buf = sample
                    .GetBufferByIndex(0)
                    .map_err(|e| werr("GetBufferByIndex", e))?;
                let dxgi: IMFDXGIBuffer = buf.cast().map_err(|e| werr("IMFDXGIBuffer", e))?;
                let mut raw: *mut std::ffi::c_void = std::ptr::null_mut();
                dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw)
                    .map_err(|e| werr("IMFDXGIBuffer::GetResource", e))?;
                let src = ID3D11Texture2D::from_raw(raw);
                let sub = dxgi.GetSubresourceIndex().unwrap_or(0);
                let src_res = src
                    .cast::<windows::Win32::Graphics::Direct3D11::ID3D11Resource>()
                    .map_err(|e| werr("ID3D11Resource", e))?;
                // Visible area only; the MFT's texture may be the padded coded size.
                let region = windows::Win32::Graphics::Direct3D11::D3D11_BOX {
                    left: 0,
                    top: 0,
                    front: 0,
                    right: surface.width,
                    bottom: surface.height,
                    back: 1,
                };
                let ctx = self.d3d.context11.lock().expect("context lock");
                ctx.CopySubresourceRegion(
                    &surface.as_resource(),
                    0,
                    0,
                    0,
                    0,
                    &src_res,
                    sub,
                    Some(&region),
                );
            }
        }
        // Make sure the decode (plan A) or the copy (plan B) has finished before wgpu
        // touches the surface.
        crate::pipeline::trace(|| "decoder: signal+wait D3D11 fence".into());
        self.d3d.d3d11_signal_and_wait()?;
        crate::pipeline::trace(|| "decoder: fence passed".into());
        // SAFETY: sample is live.
        let pts_hns = unsafe { sample.GetSampleTime() }.unwrap_or(0);
        drop(sample);
        let pts = self.hns_to_ticks(pts_hns);
        let guard: Arc<dyn std::any::Any + Send + Sync> = Arc::new(SlotGuard(in_use));
        self.frames_out += 1;
        Ok(surface.frame(self.color, pts, Some(guard)))
    }

    /// Try to pull everything the MFT has ready into `out_queue`. Returns whether
    /// anything was produced.
    fn drain_outputs(&mut self, blocking: bool) -> Result<bool, BackendError> {
        let mut produced = false;
        loop {
            // plan A needs a surface before asking for output
            let slot = if self.interop == InteropPath::SharedHandleZeroCopy {
                Some(self.acquire_slot()?)
            } else {
                None
            };
            let provided = match &slot {
                Some((surface, _)) => Some(self.make_output_sample(surface)?),
                None => None,
            };
            let pulled = if blocking && !produced {
                self.tf.pull_blocking(provided)?
            } else {
                self.tf.pull(provided)?
            };
            match pulled {
                Pulled::Sample(s) => {
                    let frame = self.finish_output(s, slot)?;
                    self.out_queue.push_back(frame);
                    produced = true;
                }
                Pulled::StreamChange => {
                    if let Some((_, in_use)) = slot {
                        in_use.store(false, Ordering::Release);
                    }
                    // Re-negotiate the output type (same stream size: the surfaces keep
                    // the declared dimensions, plan B copies the visible box).
                    self.tf.choose_output_type(MFVideoFormat_NV12)?;
                    self.tf.refresh_stream_info()?;
                }
                Pulled::NeedInput | Pulled::DrainComplete => {
                    if let Some((_, in_use)) = slot {
                        in_use.store(false, Ordering::Release);
                    }
                    return Ok(produced);
                }
            }
        }
    }
}

impl Decoder for WinDecoder {
    fn push(&mut self, packet: Packet) -> Result<(), BackendError> {
        let sample = self.make_input_sample(&packet)?;
        loop {
            if self.tf.push(&sample)? {
                return Ok(());
            }
            // MFT is full: pull what it has and try again
            if !self.drain_outputs(true)? && !self.tf.is_async {
                return Err(BackendError::Platform(format!(
                    "{}: refuses input and produces no output",
                    self.tf.name
                )));
            }
        }
    }

    fn pull(&mut self) -> Result<Option<GpuFrame>, BackendError> {
        if let Some(f) = self.out_queue.pop_front() {
            return Ok(Some(f));
        }
        self.drain_outputs(self.eos_sent)?;
        Ok(self.out_queue.pop_front())
    }

    fn flush(&mut self) -> Result<(), BackendError> {
        if !self.eos_sent {
            self.tf.drain()?;
            self.eos_sent = true;
        }
        Ok(())
    }
}
