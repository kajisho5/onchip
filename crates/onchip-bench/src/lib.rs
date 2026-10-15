//! Benchmark harness: result schema, timing helpers and PNG-sequence comparison.
//!
//! The FFmpeg baseline runner and the HW-encoder fixtures arrive with M1-3 / M4.
//! Results are plain JSON in `bench/results/` so they can be diffed and charted.

use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};

pub mod ffmpeg;

#[derive(Debug, thiserror::Error)]
pub enum BenchError {
    #[error("external tool: {0}")]
    Tool(String),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Png(#[from] onchip_core::pngio::PngIoError),
    #[error("sequence length mismatch: {a} vs {b} frames")]
    Length { a: usize, b: usize },
    #[error("frame {index}: size mismatch {a:?} vs {b:?}")]
    Size {
        index: usize,
        a: (u32, u32),
        b: (u32, u32),
    },
}

/// One benchmark run of one tool on one fixture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchResult {
    pub schema: u32,
    pub date_utc: String,
    /// `"onchip"`, `"ffmpeg-cpu"`, `"ffmpeg-hw"`.
    pub tool: String,
    pub tool_version: String,
    pub fixture: String,
    pub graph: String,
    pub frames: u64,
    pub wall_seconds: f64,
    pub fps: f64,
    pub ms_per_frame: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_percent: Option<f64>,
    pub adapter: onchip_core::backend::AdapterInfo,
    #[serde(default)]
    pub notes: Vec<String>,
}

impl BenchResult {
    pub const SCHEMA: u32 = 1;

    pub fn write(&self, dir: &Path) -> Result<PathBuf, BenchError> {
        std::fs::create_dir_all(dir).map_err(|e| BenchError::Io {
            path: dir.into(),
            source: e,
        })?;
        let name = format!(
            "{}_{}_{}_{}.json",
            self.date_utc.replace([':', '-', 'T', 'Z', '.'], ""),
            self.tool,
            self.fixture,
            self.graph
        );
        let path = dir.join(name);
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, text).map_err(|e| BenchError::Io {
            path: path.clone(),
            source: e,
        })?;
        Ok(path)
    }
}

/// Wall-clock timer that produces `(frames, seconds, fps, ms/frame)`.
pub struct Stopwatch {
    start: Instant,
    frames: u64,
}

impl Default for Stopwatch {
    fn default() -> Self {
        Self::new()
    }
}

impl Stopwatch {
    pub fn new() -> Stopwatch {
        Stopwatch {
            start: Instant::now(),
            frames: 0,
        }
    }

    pub fn tick(&mut self) {
        self.frames += 1;
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    pub fn seconds(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    pub fn fps(&self) -> f64 {
        let s = self.seconds();
        if s > 0.0 { self.frames as f64 / s } else { 0.0 }
    }

    pub fn ms_per_frame(&self) -> f64 {
        if self.frames > 0 {
            self.seconds() * 1000.0 / self.frames as f64
        } else {
            0.0
        }
    }
}

/// RFC 3339 UTC timestamp without pulling in a date crate.
pub fn now_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = civil_from_unix(secs as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Howard Hinnant's days-to-civil algorithm.
fn civil_from_unix(t: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = t.div_euclid(86_400);
    let rem = t.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    )
}

/// Per-frame and mean PSNR between two PNG sequences (CPU, RGB8).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyReport {
    pub frames: usize,
    pub psnr_per_frame: Vec<f64>,
    pub psnr_mean: f64,
    pub psnr_min: f64,
}

pub fn verify_png_sequences(a: &Path, b: &Path) -> Result<VerifyReport, BenchError> {
    use onchip_core::pngio::{list_pngs, psnr_u8, read_png_rgb8};
    let fa = if a.is_dir() {
        list_pngs(a)?
    } else {
        vec![a.to_path_buf()]
    };
    let fb = if b.is_dir() {
        list_pngs(b)?
    } else {
        vec![b.to_path_buf()]
    };
    if fa.len() != fb.len() {
        return Err(BenchError::Length {
            a: fa.len(),
            b: fb.len(),
        });
    }
    let mut per = Vec::with_capacity(fa.len());
    for (index, (pa, pb)) in fa.iter().zip(&fb).enumerate() {
        let (wa, ha, ra) = read_png_rgb8(pa)?;
        let (wb, hb, rb) = read_png_rgb8(pb)?;
        if (wa, ha) != (wb, hb) {
            return Err(BenchError::Size {
                index,
                a: (wa, ha),
                b: (wb, hb),
            });
        }
        per.push(psnr_u8(&ra, &rb));
    }
    let finite: Vec<f64> = per.iter().copied().filter(|p| p.is_finite()).collect();
    let psnr_mean = if finite.is_empty() {
        f64::INFINITY
    } else {
        finite.iter().sum::<f64>() / finite.len() as f64
    };
    let psnr_min = per.iter().copied().fold(f64::INFINITY, f64::min);
    Ok(VerifyReport {
        frames: per.len(),
        psnr_per_frame: per,
        psnr_mean,
        psnr_min,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil_from_unix(951_782_400), (2000, 2, 29, 0, 0, 0));
        assert_eq!(civil_from_unix(1_792_022_400), (2026, 10, 15, 0, 0, 0));
    }

    #[test]
    fn stopwatch_counts() {
        let mut s = Stopwatch::new();
        s.tick();
        s.tick();
        assert_eq!(s.frames(), 2);
        assert!(s.fps() > 0.0);
    }

    #[test]
    fn result_round_trip() {
        let r = BenchResult {
            schema: BenchResult::SCHEMA,
            date_utc: now_utc(),
            tool: "onchip".into(),
            tool_version: "0.1.0".into(),
            fixture: "fixture_bars_1080p_nv12".into(),
            graph: "scale-overlay".into(),
            frames: 900,
            wall_seconds: 3.0,
            fps: 300.0,
            ms_per_frame: 3.333,
            cpu_percent: None,
            adapter: onchip_core::backend::AdapterInfo {
                name: "Fictional GPU 9000".into(),
                backend: "Dx12".into(),
                device_type: "DiscreteGpu".into(),
                driver: "test".into(),
                driver_info: "0.0".into(),
                vendor: 0,
                device: 0,
                unified_memory: false,
                texture_format_16bit_norm: true,
                texture_format_nv12: true,
                texture_format_p010: false,
            },
            notes: vec![],
        };
        let text = serde_json::to_string(&r).unwrap();
        let back: BenchResult = serde_json::from_str(&text).unwrap();
        assert_eq!(back.frames, 900);
    }
}
