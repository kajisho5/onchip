//! FFmpeg as an *external* baseline: spawned as a process, timed by wall clock, never
//! linked. Also used to turn mp4 files into PNG sequences for `onchip verify`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use crate::BenchError;

/// The filter chain both tools run, derived from a job's graph.
#[derive(Debug, Clone, Default)]
pub struct FilterSpec {
    pub scale: Option<(u32, u32, String)>,
    /// `(path, x, y, opacity)`
    pub overlay: Option<(String, i32, i32, f64)>,
}

impl FilterSpec {
    pub fn from_job(job: &onchip_core::job::Job) -> FilterSpec {
        use onchip_core::job::{Op, ScaleFilter};
        let mut spec = FilterSpec::default();
        let mut image_path = None;
        for n in &job.graph {
            match &n.op {
                Op::Scale {
                    width,
                    height,
                    filter,
                } => {
                    let f = match filter {
                        ScaleFilter::Bilinear => "bilinear",
                        ScaleFilter::Bicubic => "bicubic",
                        ScaleFilter::Lanczos3 => "lanczos",
                    };
                    spec.scale = Some((*width, *height, f.to_string()));
                }
                Op::Image { path } => image_path = Some(path.clone()),
                Op::Overlay { x, y, opacity } => {
                    if let Some(p) = &image_path {
                        spec.overlay = Some((p.clone(), *x, *y, *opacity));
                    }
                }
                _ => {}
            }
        }
        spec
    }

    /// Short name for result files: `scale-overlay`, `scale`, `overlay`, `passthrough`.
    pub fn graph_name(&self) -> String {
        match (&self.scale, &self.overlay) {
            (Some(_), Some(_)) => "scale-overlay".into(),
            (Some(_), None) => "scale".into(),
            (None, Some(_)) => "overlay".into(),
            (None, None) => "passthrough".into(),
        }
    }

    /// FFmpeg `-filter_complex` for the CPU filter path. The overlay opacity is applied
    /// with `colorchannelmixer=aa=` on the overlay input, as FFmpeg has no opacity knob
    /// on `overlay` itself.
    pub fn ffmpeg_cpu_filter(&self) -> String {
        let mut chain = String::from("[0:v]");
        let mut cur = String::from("v0");
        if let Some((w, h, f)) = &self.scale {
            chain.push_str(&format!("scale={w}:{h}:flags={f}[v0];"));
        } else {
            chain.push_str("null[v0];");
        }
        if let Some((_, x, y, opacity)) = &self.overlay {
            chain.push_str(&format!(
                "[1:v]format=rgba,colorchannelmixer=aa={opacity}[ov];[{cur}][ov]overlay={x}:{y}[v1]"
            ));
            cur = "v1".into();
        } else {
            chain.push_str("[v0]null[v1]");
            cur = "v1".into();
        }
        let _ = cur;
        chain
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FfmpegRun {
    pub command: String,
    pub seconds: f64,
    pub frames: u64,
    pub output: PathBuf,
    pub ffmpeg_version: String,
}

pub fn ffmpeg_version(ffmpeg: &str) -> String {
    Command::new(ffmpeg)
        .arg("-version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.lines().next().map(|l| l.to_string()))
        .unwrap_or_else(|| "unknown".into())
}

/// Count video frames in a file with ffprobe.
pub fn ffprobe_frame_count(ffprobe: &str, path: &Path) -> Result<u64, BenchError> {
    let out = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-count_frames",
            "-show_entries",
            "stream=nb_read_frames",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .map_err(|e| BenchError::Io {
            path: path.into(),
            source: e,
        })?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.trim()
        .parse::<u64>()
        .map_err(|_| BenchError::Tool(format!("ffprobe gave {s:?} for {}", path.display())))
}

/// Encoder settings onchip used, replayed onto the FFmpeg encoder so both sides are
/// rate-controlled the same way: `-b:v`, `-g`, `-bf 0`, H.264 High profile (100) and,
/// for the Media Foundation encoders, `-rate_control cbr` (FFmpeg's mfenc maps these to
/// MF_MT_AVG_BITRATE / AVEncMPVGOPSize / AVEncMPVDefaultBPictureCount /
/// MF_MT_MPEG2_PROFILE / AVEncCommonRateControlMode — libavcodec/mfenc.c).
#[derive(Debug, Clone, Copy)]
pub struct EncodeSettings {
    pub bitrate_bps: u64,
    pub gop_frames: u32,
}

/// Run the FFmpeg CPU-filter baseline: decode (software or `hwaccel`), filter on the
/// CPU, encode with `encoder` (e.g. `h264_mf`, `h264_nvenc`, `libx264`).
pub fn run_ffmpeg_cpu_filters(
    ffmpeg: &str,
    input: &Path,
    output: &Path,
    spec: &FilterSpec,
    encoder: &str,
    settings: Option<EncodeSettings>,
    hwaccel: Option<&str>,
) -> Result<FfmpegRun, BenchError> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-y", "-benchmark"]);
    if let Some(hw) = hwaccel {
        cmd.args(["-hwaccel", hw]);
    }
    cmd.arg("-i").arg(input);
    if let Some((p, _, _, _)) = &spec.overlay {
        cmd.arg("-i").arg(p);
    }
    cmd.args([
        "-filter_complex",
        &spec.ffmpeg_cpu_filter(),
        "-map",
        "[v1]",
        "-an",
    ]);
    cmd.args(["-c:v", encoder]);
    if let Some(s) = settings {
        cmd.args(["-b:v", &s.bitrate_bps.to_string()]);
        cmd.args(["-g", &s.gop_frames.to_string()]);
        cmd.args(["-bf", "0"]);
        if encoder.ends_with("_mf") {
            // mfenc has no named profile constants; 100 = AV_PROFILE_H264_HIGH.
            cmd.args(["-profile:v", "100", "-rate_control", "cbr"]);
        } else {
            cmd.args(["-profile:v", "high"]);
        }
    }
    cmd.args(["-pix_fmt", "yuv420p"]).arg(output);
    let command = format!("{cmd:?}");
    let t0 = Instant::now();
    let status = cmd.status().map_err(|e| BenchError::Io {
        path: ffmpeg.into(),
        source: e,
    })?;
    let seconds = t0.elapsed().as_secs_f64();
    if !status.success() {
        return Err(BenchError::Tool(format!(
            "ffmpeg exited with {status}: {command}"
        )));
    }
    let frames = ffprobe_frame_count("ffprobe", output)?;
    Ok(FfmpegRun {
        command,
        seconds,
        frames,
        output: output.to_path_buf(),
        ffmpeg_version: ffmpeg_version(ffmpeg),
    })
}

/// Decode a video file to `frame_%06d.png` in `dir` (for verify). Chroma upsampling and
/// YUV→RGB are FFmpeg's, applied identically to both sides of a comparison.
pub fn decode_to_png_dir(ffmpeg: &str, input: &Path, dir: &Path) -> Result<u64, BenchError> {
    std::fs::create_dir_all(dir).map_err(|e| BenchError::Io {
        path: dir.into(),
        source: e,
    })?;
    let pattern = dir.join("frame_%06d.png");
    let status = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(input)
        .args(["-an", "-pix_fmt", "rgb24", "-start_number", "0"])
        .arg(&pattern)
        .status()
        .map_err(|e| BenchError::Io {
            path: ffmpeg.into(),
            source: e,
        })?;
    if !status.success() {
        return Err(BenchError::Tool(format!(
            "ffmpeg could not decode {}",
            input.display()
        )));
    }
    Ok(onchip_core::pngio::list_pngs(dir)?.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use onchip_core::backend::Codec;
    use onchip_core::job::{Job, ScaleFilter};

    #[test]
    fn filter_spec_from_job() {
        let job = Job::shorthand(
            "in.mp4",
            "out.mp4",
            Codec::H264,
            Some((1280, 720, ScaleFilter::Lanczos3)),
            Some(("logo_sample.png".into(), 32, 32, 0.9)),
        );
        let spec = FilterSpec::from_job(&job);
        assert_eq!(spec.graph_name(), "scale-overlay");
        let f = spec.ffmpeg_cpu_filter();
        assert!(f.contains("scale=1280:720:flags=lanczos"), "{f}");
        assert!(f.contains("overlay=32:32"), "{f}");
        assert!(f.contains("aa=0.9"), "{f}");
        let plain = FilterSpec::from_job(&Job::shorthand("a", "b", Codec::H264, None, None));
        assert_eq!(plain.graph_name(), "passthrough");
        assert!(plain.ffmpeg_cpu_filter().ends_with("[v1]"));
    }

    #[test]
    fn ffmpeg_baseline_runs_when_ffmpeg_is_present() {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join(format!("onchip-bench-ffmpeg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("fixture_bars_tiny.mp4");
        let logo = dir.join("logo_sample.png");
        let st = Command::new("ffmpeg")
            .args([
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x180:rate=30",
                "-frames:v",
                "10",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&input)
            .status()
            .unwrap();
        assert!(st.success());
        let st = Command::new("ffmpeg")
            .args([
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=0x80FF40@0.9:size=64x32,format=rgba",
                "-frames:v",
                "1",
            ])
            .arg(&logo)
            .status()
            .unwrap();
        assert!(st.success());
        let job = Job::shorthand(
            &input.to_string_lossy(),
            "unused",
            Codec::H264,
            Some((160, 90, ScaleFilter::Lanczos3)),
            Some((logo.to_string_lossy().into_owned(), 8, 8, 0.9)),
        );
        let spec = FilterSpec::from_job(&job);
        let out = dir.join("baseline.mp4");
        let run = run_ffmpeg_cpu_filters(
            "ffmpeg",
            &input,
            &out,
            &spec,
            "libx264",
            Some(EncodeSettings {
                bitrate_bps: 500_000,
                gop_frames: 5,
            }),
            None,
        )
        .unwrap();
        assert_eq!(run.frames, 10, "{}", run.command);
        let pngs = dir.join("pngs");
        assert_eq!(decode_to_png_dir("ffmpeg", &out, &pngs).unwrap(), 10);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
