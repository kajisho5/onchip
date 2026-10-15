//! Media Foundation hardware H.264 encoder fed from shared NV12 surfaces that wgpu wrote.

use std::collections::VecDeque;
use std::sync::Arc;

use onchip_core::backend::{BackendError, Codec, Encoder, EncoderConfig, Packet};
use onchip_core::frame::GpuFrame;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize, ICodecAPI, IMFMediaBuffer,
    IMFSample, MF_MT_AVG_BITRATE, MF_MT_MPEG2_PROFILE, MFCreateDXGISurfaceBuffer,
    MFCreateMemoryBuffer, MFCreateSample, MFSampleExtension_CleanPoint,
    MFSampleExtension_DecodeTimestamp, MFVideoFormat_H264, MFVideoFormat_NV12,
    eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_High,
};
use windows::Win32::System::Variant::VARIANT;
use windows::core::Interface;

use crate::d3d::{D3dShared, SharedSurface, surface_of, werr};
use crate::mft::{Pulled, Transform, enumerate_hw, video_media_type, video_type_info};

pub struct WinEncoder {
    d3d: Arc<D3dShared>,
    tf: Transform,
    cfg: EncoderConfig,
    frame_duration_100ns: i64,
    /// Input samples still referenced by the MFT, keyed by surface pointer.
    in_flight: Vec<(Arc<SharedSurface>, IMFSample, IMFMediaBuffer)>,
    out_queue: VecDeque<Packet>,
    eos_sent: bool,
    pub packets_out: u64,
    codec_api_notes: Vec<String>,
}

/// Current COM reference count of an object (AddRef + Release), measured on the
/// pointer we already hold — no `cast`, which would add a reference of its own and
/// make every object look busy.
fn com_refcount<T: Interface>(obj: &T) -> u32 {
    // SAFETY: AddRef/Release are always safe to pair on a live COM object; the vtable
    // starts with the IUnknown methods for every COM interface.
    unsafe {
        let raw = obj.as_raw();
        let vt = *(raw as *const *const windows::core::IUnknown_Vtbl);
        ((*vt).AddRef)(raw);
        ((*vt).Release)(raw)
    }
}

impl WinEncoder {
    pub fn open(d3d: Arc<D3dShared>, cfg: &EncoderConfig) -> Result<WinEncoder, BackendError> {
        if cfg.codec != Codec::H264 {
            return Err(BackendError::UnsupportedCodec(cfg.codec));
        }
        let input_info = video_type_info(MFVideoFormat_NV12);
        let output_info = video_type_info(MFVideoFormat_H264);
        let mut found = enumerate_hw(
            crate::mft::encoder_category(),
            Some(&input_info),
            Some(&output_info),
        )?;
        if found.is_empty() {
            return Err(BackendError::Platform(
                "no hardware H.264 encoder MFT with NV12 input".into(),
            ));
        }
        let (name, act) = found.remove(0);
        let mut tf = Transform::activate(name, &act, Some(&d3d.dxgi_manager))?;

        // Encoders want the output type first.
        let out_type = video_media_type(MFVideoFormat_H264, cfg.width, cfg.height, cfg.framerate)?;
        // SAFETY: live media type.
        unsafe {
            out_type
                .SetUINT32(
                    &MF_MT_AVG_BITRATE,
                    cfg.bitrate_bps.min(u32::MAX as u64) as u32,
                )
                .map_err(|e| werr("MF_MT_AVG_BITRATE", e))?;
            out_type
                .SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)
                .map_err(|e| werr("MF_MT_MPEG2_PROFILE", e))?;
        }
        tf.set_output_type(&out_type)?;
        let in_type = video_media_type(MFVideoFormat_NV12, cfg.width, cfg.height, cfg.framerate)?;
        tf.set_input_type(&in_type)?;
        tf.refresh_stream_info()?;

        // Codec settings via ICodecAPI. Best effort: vendors differ in what they accept,
        // and none of these change correctness, so failures are recorded, not fatal.
        let mut codec_api_notes = Vec::new();
        if let Ok(api) = tf.mft.cast::<ICodecAPI>() {
            let settings: [(&str, &windows::core::GUID, VARIANT); 4] = [
                (
                    "rate control CBR",
                    &CODECAPI_AVEncCommonRateControlMode,
                    VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
                ),
                (
                    "mean bitrate",
                    &CODECAPI_AVEncCommonMeanBitRate,
                    VARIANT::from(cfg.bitrate_bps.min(u32::MAX as u64) as u32),
                ),
                (
                    "GOP size",
                    &CODECAPI_AVEncMPVGOPSize,
                    VARIANT::from(cfg.gop_frames.max(1)),
                ),
                // No B-frames: keeps decode order == presentation order, so the muxer
                // can use dts == pts (ROADMAP RM-012 lifts this).
                (
                    "B-frames = 0",
                    &CODECAPI_AVEncMPVDefaultBPictureCount,
                    VARIANT::from(0u32),
                ),
            ];
            for (what, key, value) in &settings {
                // SAFETY: live ICodecAPI; key and value are valid for the call.
                if let Err(e) = unsafe { api.SetValue(*key, value) } {
                    codec_api_notes.push(format!("{what}: {e}"));
                }
            }
        } else {
            codec_api_notes.push("ICodecAPI not supported by this MFT".into());
        }
        tf.start()?;

        let frame_duration_100ns = if cfg.framerate.0 > 0 {
            (10_000_000i64 * cfg.framerate.1 as i64) / cfg.framerate.0 as i64
        } else {
            333_333
        };
        Ok(WinEncoder {
            d3d,
            tf,
            cfg: cfg.clone(),
            frame_duration_100ns,
            in_flight: Vec::new(),
            out_queue: VecDeque::new(),
            eos_sent: false,
            packets_out: 0,
            codec_api_notes,
        })
    }

    /// Settings the encoder refused (informational).
    pub fn codec_api_notes(&self) -> &[String] {
        &self.codec_api_notes
    }

    pub fn mft_name(&self) -> &str {
        &self.tf.name
    }

    pub fn is_async(&self) -> bool {
        self.tf.is_async
    }

    fn ticks_to_100ns(&self, t: i64) -> i64 {
        let tb = self.cfg.timebase;
        (t as i128 * 10_000_000 * tb.0 as i128 / tb.1 as i128) as i64
    }

    fn hns_to_ticks(&self, t: i64) -> i64 {
        let tb = self.cfg.timebase;
        (t as i128 * tb.1 as i128 / (10_000_000 * tb.0 as i128)) as i64
    }

    /// Whether the encoder still holds a reference to the sample wrapping `surface`.
    /// Call before writing into a pooled encoder surface again.
    pub fn surface_busy(&mut self, surface: &Arc<SharedSurface>) -> bool {
        // sample: our reference + the MFT's = 2 or more while busy; exactly 1 = done.
        // buffer: the sample's + ours = 2 when free; the MFT may hold the buffer (and
        // with it the texture) a little longer than the sample, so check it too.
        self.in_flight
            .retain(|(_, sample, buffer)| com_refcount(sample) > 1 || com_refcount(buffer) > 2);
        self.in_flight
            .iter()
            .any(|(s, _, _)| Arc::ptr_eq(s, surface))
    }

    pub fn in_flight_len(&self) -> usize {
        self.in_flight.len()
    }

    fn output_sample(&self) -> Result<Option<IMFSample>, BackendError> {
        if self.tf.provides_samples {
            return Ok(None);
        }
        let size = if self.tf.output_buffer_size > 0 {
            self.tf.output_buffer_size
        } else {
            self.cfg.width * self.cfg.height * 4
        };
        // SAFETY: fresh sample + buffer.
        unsafe {
            let buf = MFCreateMemoryBuffer(size).map_err(|e| werr("MFCreateMemoryBuffer", e))?;
            let sample = MFCreateSample().map_err(|e| werr("MFCreateSample", e))?;
            sample.AddBuffer(&buf).map_err(|e| werr("AddBuffer", e))?;
            Ok(Some(sample))
        }
    }

    fn sample_to_packet(&self, sample: IMFSample) -> Result<Packet, BackendError> {
        crate::pipeline::trace(|| "encoder: sample_to_packet".into());
        // SAFETY: live sample; the contiguous buffer is locked, copied, unlocked.
        unsafe {
            let buf = sample
                .ConvertToContiguousBuffer()
                .map_err(|e| werr("ConvertToContiguousBuffer", e))?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut len: u32 = 0;
            buf.Lock(&mut ptr, None, Some(&mut len))
                .map_err(|e| werr("IMFMediaBuffer::Lock", e))?;
            let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            buf.Unlock()
                .map_err(|e| werr("IMFMediaBuffer::Unlock", e))?;
            let keyframe = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let pts_hns = sample.GetSampleTime().unwrap_or(0);
            let dts_hns = sample
                .GetUINT64(&MFSampleExtension_DecodeTimestamp)
                .map(|v| v as i64)
                .unwrap_or(pts_hns);
            Ok(Packet {
                data,
                pts: self.hns_to_ticks(pts_hns),
                dts: self.hns_to_ticks(dts_hns),
                keyframe,
            })
        }
    }

    fn drain_outputs(&mut self, blocking: bool) -> Result<bool, BackendError> {
        let mut produced = false;
        loop {
            let provided = self.output_sample()?;
            let pulled = if blocking && !produced {
                self.tf.pull_blocking(provided)?
            } else {
                self.tf.pull(provided)?
            };
            match pulled {
                Pulled::Sample(s) => {
                    let p = self.sample_to_packet(s)?;
                    self.out_queue.push_back(p);
                    self.packets_out += 1;
                    produced = true;
                }
                Pulled::StreamChange => {
                    // output type is fixed for an encoder; re-read stream info and go on
                    crate::pipeline::trace(|| "encoder: stream change".into());
                    self.tf.refresh_stream_info()?;
                }
                Pulled::NeedInput | Pulled::DrainComplete => return Ok(produced),
            }
        }
    }
}

impl Encoder for WinEncoder {
    fn push(&mut self, frame: GpuFrame) -> Result<(), BackendError> {
        let surface = surface_of(&frame).ok_or(BackendError::Interop(
            "encoder input must be a frame from WinBackend::alloc_encoder_frame".into(),
        ))?;
        // wgpu must be done writing the surface before D3D11 reads it.
        crate::pipeline::trace(|| "encoder: waiting for wgpu".into());
        self.d3d.wgpu_wait_idle()?;
        crate::pipeline::trace(|| "encoder: wgpu idle, building sample".into());
        // SAFETY: the surface's D3D11 texture is live and shared.
        let (sample, buf) = unsafe {
            let buf = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &surface.tex11, 0, false)
                .map_err(|e| werr("MFCreateDXGISurfaceBuffer", e))?;
            let sample = MFCreateSample().map_err(|e| werr("MFCreateSample", e))?;
            sample.AddBuffer(&buf).map_err(|e| werr("AddBuffer", e))?;
            sample
                .SetSampleTime(self.ticks_to_100ns(frame.pts))
                .map_err(|e| werr("SetSampleTime", e))?;
            sample
                .SetSampleDuration(self.frame_duration_100ns)
                .map_err(|e| werr("SetSampleDuration", e))?;
            (sample, buf)
        };
        loop {
            crate::pipeline::trace(|| "encoder: ProcessInput".into());
            if self.tf.push(&sample)? {
                break;
            }
            crate::pipeline::trace(|| "encoder: not accepting, pulling output".into());
            if !self.drain_outputs(true)? && !self.tf.is_async {
                return Err(BackendError::Platform(format!(
                    "{}: refuses input and produces no output",
                    self.tf.name
                )));
            }
        }
        self.in_flight.push((surface, sample, buf));
        Ok(())
    }

    fn pull(&mut self) -> Result<Option<Packet>, BackendError> {
        if let Some(p) = self.out_queue.pop_front() {
            return Ok(Some(p));
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
