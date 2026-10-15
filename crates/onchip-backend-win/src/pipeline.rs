//! The whole Windows path on one file: demux → HW decode → wgpu filter graph →
//! HW encode → mux. Used by `onchip run` and by the `interop_poc` example.
//!
//! Where data moves (and where it does not):
//! - compressed packets: file → CPU → MFT (and back) — allowed, they are small
//! - pixels: decoder surface → wgpu → encoder surface — never leave the GPU

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use onchip_core::backend::{BackendError, Codec, Decoder, Encoder, EncoderConfig, Packet};
use onchip_core::container::{AvcConfig, Mp4Demuxer, Mp4Muxer, annexb_to_avcc};
use onchip_core::exec::Executor;
use onchip_core::graph::Graph;
use onchip_core::job::Job;
use onchip_core::{GpuContext, PixelFormat};

use crate::d3d::{InteropPath, SharedSurface};
use crate::{WinBackend, open};

const ENCODER_POOL: usize = 8;

/// Stage tracing to stderr, on when `ONCHIP_TRACE` is set. Cheap enough to leave in.
pub fn trace(msg: impl FnOnce() -> String) {
    if std::env::var_os("ONCHIP_TRACE").is_some() {
        eprintln!("[onchip] {}", msg());
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error(transparent)]
    Backend(#[from] BackendError),
    #[error("container: {0}")]
    Container(#[from] onchip_core::container::ContainerError),
    #[error("graph: {0}")]
    Graph(#[from] onchip_core::graph::GraphError),
    #[error("executor: {0}")]
    Exec(#[from] onchip_core::exec::ExecError),
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TranscodeReport {
    pub input: String,
    pub output: String,
    pub frames_decoded: u64,
    pub frames_encoded: u64,
    pub packets_written: u64,
    pub seconds: f64,
    pub fps: f64,
    pub decoder_mft: String,
    pub decoder_async: bool,
    pub encoder_mft: String,
    pub encoder_async: bool,
    pub interop: String,
    pub encoder_notes: Vec<String>,
    pub adapter: String,
}

/// Run `job` (H.264 mp4 in, H.264 mp4 out) through the Windows backend.
pub fn transcode(
    ctx: Arc<GpuContext>,
    job: &Job,
    mut progress: impl FnMut(u64),
) -> Result<TranscodeReport, PipelineError> {
    if job.output.codec != Codec::H264 {
        return Err(PipelineError::Other(format!(
            "output codec {:?} is not supported by the Windows path yet (H.264 only in M1; HEVC is RM-011)",
            job.output.codec
        )));
    }
    trace(|| "opening backend (MFStartup, D3D11 device, DXGI manager, shared fence)".into());
    let backend: WinBackend = open(ctx.clone())?;
    trace(|| format!("backend open; adapter luid {:#x}", backend.d3d.adapter_luid));
    let mut demux = Mp4Demuxer::open(Path::new(&job.input))?;
    trace(|| {
        format!(
            "demux: {}x{} {:?} {} samples, fps {:?}",
            demux.info.width,
            demux.info.height,
            demux.info.codec,
            demux.sample_count(),
            demux.framerate
        )
    });
    let framerate = demux.framerate;
    let timebase = demux.info.timebase;

    let mut decoder = backend.open_decoder_with_rate(&demux.info, framerate)?;
    trace(|| {
        format!(
            "decoder: {} async={} provides_samples={} interop={:?}",
            decoder.mft_name(),
            decoder.is_async(),
            decoder.provides_samples(),
            decoder.interop
        )
    });
    let graph = Graph::build(job, demux.info.color)?;
    let mut exec = Executor::new(
        ctx.clone(),
        graph,
        demux.info.width,
        demux.info.height,
        PixelFormat::Nv12,
    )?;
    let out = exec.output_layout();
    let out_color = exec.output_color();
    if out.width % 2 != 0 || out.height % 2 != 0 {
        return Err(PipelineError::Other(format!(
            "output size {}x{} must be even for NV12 encoder surfaces",
            out.width, out.height
        )));
    }

    let fps = framerate.0 as f64 / framerate.1.max(1) as f64;
    let bitrate = match job
        .output
        .bitrate_bps()
        .map_err(|e| PipelineError::Other(e.to_string()))?
    {
        Some(b) => b,
        None => ((out.width as f64 * out.height as f64 * fps * 0.07) as u64).max(2_000_000),
    };
    let gop = job
        .output
        .gop
        .unwrap_or_else(|| (fps * 2.0).round().max(1.0) as u32);
    let enc_cfg = EncoderConfig {
        codec: Codec::H264,
        width: out.width,
        height: out.height,
        format: PixelFormat::Nv12,
        color: out_color,
        bitrate_bps: bitrate,
        gop_frames: gop,
        framerate,
        timebase,
    };
    let mut encoder = crate::encoder::WinEncoder::open(backend.d3d.clone(), &enc_cfg)?;
    trace(|| {
        format!(
            "encoder: {} async={} notes={:?}",
            encoder.mft_name(),
            encoder.is_async(),
            encoder.codec_api_notes()
        )
    });

    let mut pool: Vec<Arc<SharedSurface>> = Vec::with_capacity(ENCODER_POOL);
    for _ in 0..ENCODER_POOL {
        pool.push(backend.alloc_encoder_surface(out.width, out.height)?);
    }
    trace(|| {
        format!(
            "{} encoder surfaces {}x{} allocated",
            ENCODER_POOL, out.width, out.height
        )
    });
    let mut next_slot = 0usize;

    let frame_duration =
        (timebase.1 as u64 * framerate.1 as u64 / framerate.0.max(1) as u64) as u32;
    let mut muxer: Option<Mp4Muxer> = None;
    let mut pending_packets: Vec<Packet> = Vec::new();
    let mut frames_encoded = 0u64;
    let mut packets_written = 0u64;
    let started = Instant::now();

    // Writes encoder output through the muxer, creating it on the first keyframe that
    // carries SPS/PPS.
    let sink = |packet: Packet,
                muxer: &mut Option<Mp4Muxer>,
                pending: &mut Vec<Packet>,
                written: &mut u64|
     -> Result<(), PipelineError> {
        let (data, sps, pps) = annexb_to_avcc(&packet.data);
        if muxer.is_none() {
            if sps.is_empty() || pps.is_empty() {
                pending.push(Packet { data, ..packet });
                return Ok(());
            }
            let avc = AvcConfig {
                nal_length_size: 4,
                sps,
                pps,
            };
            let m = Mp4Muxer::create(
                Path::new(&job.output.path),
                out.width,
                out.height,
                timebase.1,
                frame_duration.max(1),
                &avc,
            )?;
            *muxer = Some(m);
            // packets that arrived before the parameter sets (should not happen with a
            // sane encoder, but do not lose them)
            let m = muxer.as_mut().expect("just set");
            for p in pending.drain(..) {
                m.write_packet(&p)?;
                *written += 1;
            }
        }
        let m = muxer.as_mut().expect("muxer exists");
        m.write_packet(&Packet { data, ..packet })?;
        *written += 1;
        trace(|| format!("packet written ({} so far)", *written));
        Ok(())
    };

    // Process one decoded frame: filter into a free encoder surface, hand to encoder.
    let mut process = |frame: onchip_core::frame::GpuFrame,
                       encoder: &mut crate::encoder::WinEncoder,
                       muxer: &mut Option<Mp4Muxer>,
                       pending: &mut Vec<Packet>,
                       written: &mut u64,
                       encoded: &mut u64|
     -> Result<(), PipelineError> {
        // find a surface the encoder is done with
        let mut tries = 0;
        let surface = loop {
            let s = pool[next_slot % ENCODER_POOL].clone();
            if !encoder.surface_busy(&s) {
                next_slot += 1;
                break s;
            }
            next_slot += 1;
            tries += 1;
            if tries >= ENCODER_POOL {
                // everything in flight: pull encoder output until something frees up
                match encoder.pull()? {
                    Some(p) => sink(p, muxer, pending, written)?,
                    None => std::thread::sleep(std::time::Duration::from_millis(1)),
                }
                tries = 0;
            }
        };
        let mut target = backend.surface_frame(&surface, out_color);
        trace(|| format!("frame pts={}: running graph", frame.pts));
        exec.run_frame_into(&frame, &mut target)?;
        trace(|| {
            format!(
                "frame pts={}: graph submitted, pushing to encoder",
                frame.pts
            )
        });
        encoder.push(target)?;
        *encoded += 1;
        trace(|| {
            format!(
                "frame pts={}: encoder accepted ({} so far)",
                frame.pts, *encoded
            )
        });
        while let Some(p) = encoder.pull()? {
            sink(p, muxer, pending, written)?;
        }
        Ok(())
    };

    let mut frames_decoded = 0u64;
    while let Some(packet) = demux.next_packet()? {
        trace(|| {
            format!(
                "packet dts={} pts={} key={} {} bytes",
                packet.dts,
                packet.pts,
                packet.keyframe,
                packet.data.len()
            )
        });
        decoder.push(packet)?;
        trace(|| "packet accepted by decoder".into());
        while let Some(frame) = decoder.pull()? {
            frames_decoded += 1;
            process(
                frame,
                &mut encoder,
                &mut muxer,
                &mut pending_packets,
                &mut packets_written,
                &mut frames_encoded,
            )?;
            progress(frames_decoded);
        }
    }
    trace(|| "draining decoder".into());
    decoder.flush()?;
    while let Some(frame) = decoder.pull()? {
        frames_decoded += 1;
        process(
            frame,
            &mut encoder,
            &mut muxer,
            &mut pending_packets,
            &mut packets_written,
            &mut frames_encoded,
        )?;
        progress(frames_decoded);
    }
    trace(|| "draining encoder".into());
    encoder.flush()?;
    while let Some(p) = encoder.pull()? {
        sink(p, &mut muxer, &mut pending_packets, &mut packets_written)?;
    }
    let seconds = started.elapsed().as_secs_f64();
    match muxer {
        Some(m) => {
            m.finish()?;
        }
        None => {
            return Err(PipelineError::Other(
                "encoder produced no keyframe with SPS/PPS; nothing was written".into(),
            ));
        }
    }

    Ok(TranscodeReport {
        input: job.input.clone(),
        output: job.output.path.clone(),
        frames_decoded,
        frames_encoded,
        packets_written,
        seconds,
        fps: if seconds > 0.0 {
            frames_encoded as f64 / seconds
        } else {
            0.0
        },
        decoder_mft: decoder.mft_name().to_string(),
        decoder_async: decoder.is_async(),
        encoder_mft: encoder.mft_name().to_string(),
        encoder_async: encoder.is_async(),
        interop: match decoder.interop {
            InteropPath::SharedHandleZeroCopy => "plan A: shared handle, zero copy".into(),
            InteropPath::GpuCopyIntoShared => "plan B: GPU copy into shared surface".into(),
        },
        encoder_notes: encoder.codec_api_notes().to_vec(),
        adapter: ctx.info.name.clone(),
    })
}
