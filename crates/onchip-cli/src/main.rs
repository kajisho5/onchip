//! `onchip` command line.
//!
//! Exit codes: 0 success, 2 bad input (job file, arguments, missing files),
//! 3 backend / GPU failure.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, anyhow};
use clap::{Args, Parser, Subcommand, ValueEnum};
use onchip_core::backend::{BackendInfo, Codec};
use onchip_core::color::ColorSpec;
use onchip_core::exec::Executor;
use onchip_core::graph::Graph;
use onchip_core::job::{Job, ScaleFilter};
use onchip_core::pngio::{PngSink, PngSource};
use onchip_core::{GpuContext, PixelFormat};

const EXIT_INPUT: u8 = 2;
const EXIT_BACKEND: u8 = 3;

#[derive(Parser)]
#[command(
    name = "onchip",
    version,
    about = "Video frames stay on-chip. Decode, filter, encode — no round trip."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show the GPU adapter, backend and hardware codecs this machine offers
    Probe(ProbeArgs),
    /// Run a job file or a scale/overlay shorthand
    Run(RunArgs),
    /// Time a job (onchip only until M1-3; `--against ffmpeg` arrives with the Windows path)
    Bench(BenchArgs),
    /// Compare two PNG sequences (PSNR). SSIM / ΔE2000 arrive in M3
    Verify(VerifyArgs),
}

#[derive(Args)]
struct ProbeArgs {
    /// Machine-readable output
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct RunArgs {
    /// `job.json`, or an input path (PNG file / directory) for the shorthand form
    input: PathBuf,
    /// Output path (shorthand form only)
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Output codec (shorthand form only). Only `png` works until M1.
    #[arg(long, default_value = "png")]
    codec: CodecArg,
    /// Scale to WIDTHxHEIGHT, e.g. 1920x1080
    #[arg(long, value_name = "WxH")]
    scale: Option<String>,
    /// Resampling filter for --scale
    #[arg(long, default_value = "lanczos3")]
    filter: FilterArg,
    /// Overlay PNG at X,Y with optional opacity: `logo.png@32,32` or `logo.png@32,32@0.9`
    #[arg(long, value_name = "PATH@X,Y[@OPACITY]")]
    overlay: Option<String>,
    /// Source color (`bt709`, `bt709_full`)
    #[arg(long, default_value = "bt709")]
    color: String,
    /// Print the job JSON that would run, and stop
    #[arg(long)]
    dump_job: bool,
    /// Quiet: no per-frame progress
    #[arg(short, long)]
    quiet: bool,
}

#[derive(Args)]
struct BenchArgs {
    /// `job.json` to time
    job: PathBuf,
    /// Repeat the whole job this many times and report the best run
    #[arg(long, default_value_t = 1)]
    runs: u32,
    /// Name recorded as `fixture` in the result (defaults to the input file stem)
    #[arg(long)]
    fixture: Option<String>,
    /// Name recorded as `graph` in the result
    #[arg(long, default_value = "custom")]
    graph: String,
    /// Compare against another tool. Only `ffmpeg` is planned; not implemented until M1-3
    #[arg(long)]
    against: Option<String>,
    /// Directory for the result JSON
    #[arg(long, default_value = "bench/results")]
    out_dir: PathBuf,
}

#[derive(Args)]
struct VerifyArgs {
    /// PNG sequence (directory) or single PNG to check
    target: PathBuf,
    /// Reference PNG sequence or PNG
    #[arg(long)]
    r#ref: PathBuf,
    /// Fail (exit 1) if mean PSNR is below this
    #[arg(long)]
    min_psnr: Option<f64>,
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum CodecArg {
    Png,
    H264,
    Hevc,
}

impl From<CodecArg> for Codec {
    fn from(c: CodecArg) -> Codec {
        match c {
            CodecArg::Png => Codec::Png,
            CodecArg::H264 => Codec::H264,
            CodecArg::Hevc => Codec::Hevc,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum FilterArg {
    Bilinear,
    Bicubic,
    Lanczos3,
}

impl From<FilterArg> for ScaleFilter {
    fn from(f: FilterArg) -> ScaleFilter {
        match f {
            FilterArg::Bilinear => ScaleFilter::Bilinear,
            FilterArg::Bicubic => ScaleFilter::Bicubic,
            FilterArg::Lanczos3 => ScaleFilter::Lanczos3,
        }
    }
}

/// Error carrying the exit code it should produce.
#[derive(Debug)]
struct Exit(u8, anyhow::Error);

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.1)
    }
}

impl std::error::Error for Exit {}

fn input_err<E: Into<anyhow::Error>>(e: E) -> Exit {
    Exit(EXIT_INPUT, e.into())
}

fn backend_err<E: Into<anyhow::Error>>(e: E) -> Exit {
    Exit(EXIT_BACKEND, e.into())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.cmd {
        Cmd::Probe(a) => probe(a),
        Cmd::Run(a) => run(a),
        Cmd::Bench(a) => bench(a),
        Cmd::Verify(a) => verify(a),
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(Exit(code, err)) => {
            eprintln!("onchip: {err:#}");
            ExitCode::from(code)
        }
    }
}

fn platform_info(ctx: &GpuContext) -> BackendInfo {
    if cfg!(target_os = "windows") {
        onchip_backend_win::backend_info(ctx)
    } else if cfg!(target_os = "macos") {
        onchip_backend_mac::backend_info(ctx)
    } else {
        BackendInfo {
            name: "none (Linux is M5+; GPU filters only)".into(),
            adapter: ctx.info.clone(),
            hw_decoders: vec![],
            hw_encoders: vec![],
        }
    }
}

fn gpu() -> Result<std::sync::Arc<GpuContext>, Exit> {
    GpuContext::new(None).map_err(backend_err)
}

fn probe(a: ProbeArgs) -> Result<u8, Exit> {
    let ctx = gpu()?;
    let info = platform_info(&ctx);
    if a.json {
        let v = serde_json::json!({
            "onchip": onchip_core::VERSION,
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "backend": info.name,
            "adapter": info.adapter,
            "hw_decoders": info.hw_decoders,
            "hw_encoders": info.hw_encoders,
        });
        println!("{}", serde_json::to_string_pretty(&v).expect("json"));
        return Ok(0);
    }
    println!(
        "onchip {}  ({} / {})",
        onchip_core::VERSION,
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!("backend        {}", info.name);
    let ad = &info.adapter;
    println!("adapter        {}", ad.name);
    println!("api            {}  ({})", ad.backend, ad.device_type);
    println!("driver         {} {}", ad.driver, ad.driver_info);
    println!(
        "unified memory {}",
        if ad.unified_memory { "yes" } else { "no" }
    );
    println!(
        "16-bit norm    {}  (P010 {})",
        ad.texture_format_16bit_norm,
        if ad.texture_format_16bit_norm {
            "ok"
        } else {
            "unavailable"
        }
    );
    print_codecs("hw decoders", &info.hw_decoders);
    print_codecs("hw encoders", &info.hw_encoders);
    Ok(0)
}

fn print_codecs(title: &str, list: &[onchip_core::backend::CodecInfo]) {
    if list.is_empty() {
        println!("{title:<14} (none found)");
        return;
    }
    for (i, c) in list.iter().enumerate() {
        let formats: Vec<String> = c
            .formats
            .iter()
            .map(|f| format!("{f:?}").to_lowercase())
            .collect();
        let fmt = if formats.is_empty() {
            String::new()
        } else {
            format!("  [{}]", formats.join(", "))
        };
        let head = if i == 0 { title } else { "" };
        println!("{head:<14} {:?}  {}{}", c.codec, c.name, fmt);
    }
}

fn parse_scale(s: &str) -> Result<(u32, u32), Exit> {
    let (w, h) = s
        .split_once('x')
        .ok_or_else(|| input_err(anyhow!("--scale expects WxH, got {s:?}")))?;
    let w: u32 = w
        .parse()
        .map_err(|_| input_err(anyhow!("--scale width {w:?} is not a number")))?;
    let h: u32 = h
        .parse()
        .map_err(|_| input_err(anyhow!("--scale height {h:?} is not a number")))?;
    if w == 0 || h == 0 {
        return Err(input_err(anyhow!("--scale dimensions must be > 0")));
    }
    Ok((w, h))
}

fn parse_overlay(s: &str) -> Result<(String, i32, i32, f64), Exit> {
    let mut parts = s.split('@');
    let path = parts.next().unwrap_or_default().to_string();
    let pos = parts
        .next()
        .ok_or_else(|| input_err(anyhow!("--overlay expects PATH@X,Y, got {s:?}")))?;
    let (x, y) = pos
        .split_once(',')
        .ok_or_else(|| input_err(anyhow!("--overlay position {pos:?} must be X,Y")))?;
    let x: i32 = x
        .trim()
        .parse()
        .map_err(|_| input_err(anyhow!("--overlay x {x:?} is not a number")))?;
    let y: i32 = y
        .trim()
        .parse()
        .map_err(|_| input_err(anyhow!("--overlay y {y:?} is not a number")))?;
    let opacity: f64 = match parts.next() {
        Some(o) => o
            .parse()
            .map_err(|_| input_err(anyhow!("--overlay opacity {o:?} is not a number")))?,
        None => 1.0,
    };
    if !(0.0..=1.0).contains(&opacity) {
        return Err(input_err(anyhow!("--overlay opacity must be within 0..1")));
    }
    if path.is_empty() {
        return Err(input_err(anyhow!("--overlay needs a PNG path")));
    }
    Ok((path, x, y, opacity))
}

fn load_job(a: &RunArgs) -> Result<Job, Exit> {
    let is_job_file = a
        .input
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("json"));
    if is_job_file {
        let text = std::fs::read_to_string(&a.input)
            .with_context(|| format!("reading {}", a.input.display()))
            .map_err(input_err)?;
        return Job::from_json(&text).map_err(input_err);
    }
    let output = a
        .output
        .as_ref()
        .ok_or_else(|| input_err(anyhow!("-o/--output is required in shorthand form")))?;
    let scale = match &a.scale {
        Some(s) => {
            let (w, h) = parse_scale(s)?;
            Some((w, h, a.filter.into()))
        }
        None => None,
    };
    let overlay = match &a.overlay {
        Some(o) => Some(parse_overlay(o)?),
        None => None,
    };
    Ok(Job::shorthand(
        &a.input.to_string_lossy(),
        &output.to_string_lossy(),
        a.codec.into(),
        scale,
        overlay,
    ))
}

fn run(a: RunArgs) -> Result<u8, Exit> {
    let job = load_job(&a)?;
    if a.dump_job {
        println!("{}", job.to_json_pretty());
        return Ok(0);
    }
    let color = ColorSpec::from_short_name(&a.color)
        .ok_or_else(|| input_err(anyhow!("unknown --color {:?}", a.color)))?;
    let graph = Graph::build(&job, color).map_err(input_err)?;

    if job.output.codec != Codec::Png {
        return Err(backend_err(anyhow!(
            "codec {:?} needs the platform backend, which lands in M1 (Windows) / M2 (macOS). \
             Use codec `png` with a PNG sequence until then.",
            job.output.codec
        )));
    }

    let ctx = gpu()?;
    let mut source =
        PngSource::open(ctx.clone(), Path::new(&job.input), color).map_err(input_err)?;
    let mut sink = PngSink::open(ctx.clone(), Path::new(&job.output.path)).map_err(input_err)?;
    let mut exec = Executor::new(
        ctx.clone(),
        graph,
        source.width,
        source.height,
        PixelFormat::Nv12,
    )
    .map_err(backend_err)?;
    let out = exec.output_layout();
    if !a.quiet {
        eprintln!(
            "onchip: {} frame(s) {}x{} -> {}x{} on {} ({})",
            source.len(),
            source.width,
            source.height,
            out.width,
            out.height,
            ctx.info.name,
            ctx.info.backend
        );
    }
    let t0 = std::time::Instant::now();
    while let Some(frame) = source.next_frame().map_err(input_err)? {
        let out_frame = exec.run_frame(&frame).map_err(backend_err)?;
        let path = sink.push(&out_frame).map_err(backend_err)?;
        if !a.quiet {
            eprintln!("  wrote {}", path.display());
        }
    }
    let secs = t0.elapsed().as_secs_f64();
    if !a.quiet {
        eprintln!(
            "onchip: {} frame(s) in {:.3}s ({:.1} fps incl. PNG I/O)",
            sink.count(),
            secs,
            sink.count() as f64 / secs.max(1e-9)
        );
    }
    Ok(0)
}

fn bench(a: BenchArgs) -> Result<u8, Exit> {
    if let Some(tool) = &a.against {
        return Err(backend_err(anyhow!(
            "--against {tool} is not implemented yet (M1-3, ROADMAP RM-010). \
             Run without --against to time onchip alone."
        )));
    }
    if a.runs == 0 {
        return Err(input_err(anyhow!("--runs must be >= 1")));
    }
    let text = std::fs::read_to_string(&a.job)
        .with_context(|| format!("reading {}", a.job.display()))
        .map_err(input_err)?;
    let job = Job::from_json(&text).map_err(input_err)?;
    if job.output.codec != Codec::Png {
        return Err(backend_err(anyhow!(
            "bench only supports the PNG path until M1"
        )));
    }
    let color = ColorSpec::BT709_LIMITED;
    let ctx = gpu()?;
    let mut best: Option<onchip_bench::Stopwatch> = None;
    for run in 0..a.runs {
        let graph = Graph::build(&job, color).map_err(input_err)?;
        let mut source =
            PngSource::open(ctx.clone(), Path::new(&job.input), color).map_err(input_err)?;
        let mut exec = Executor::new(
            ctx.clone(),
            graph,
            source.width,
            source.height,
            PixelFormat::Nv12,
        )
        .map_err(backend_err)?;
        // Time only the GPU graph: upload the frames first, then run them all.
        let mut frames = Vec::with_capacity(source.len());
        while let Some(f) = source.next_frame().map_err(input_err)? {
            frames.push(f);
        }
        let mut sw = onchip_bench::Stopwatch::new();
        let mut last = None;
        for f in &frames {
            last = Some(exec.run_frame(f).map_err(backend_err)?);
            sw.tick();
        }
        onchip_core::gpu::wait_idle(&ctx.device).map_err(backend_err)?;
        drop(last);
        let secs = sw.seconds();
        eprintln!(
            "run {}/{}: {} frames, {:.3}s, {:.1} fps",
            run + 1,
            a.runs,
            sw.frames(),
            secs,
            sw.fps()
        );
        if best.as_ref().is_none_or(|b| sw.seconds() < b.seconds()) {
            best = Some(sw);
        }
    }
    let sw = best.expect("at least one run");
    let fixture = a.fixture.clone().unwrap_or_else(|| {
        Path::new(&job.input)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or("input".into())
    });
    let result = onchip_bench::BenchResult {
        schema: onchip_bench::BenchResult::SCHEMA,
        date_utc: onchip_bench::now_utc(),
        tool: "onchip".into(),
        tool_version: onchip_core::VERSION.into(),
        fixture,
        graph: a.graph.clone(),
        frames: sw.frames(),
        wall_seconds: sw.seconds(),
        fps: sw.fps(),
        ms_per_frame: sw.ms_per_frame(),
        cpu_percent: None,
        adapter: ctx.info.clone(),
        notes: vec![
            "M0: PNG stand-in codec ends; GPU graph time only (frames pre-uploaded)".into(),
        ],
    };
    let path = result.write(&a.out_dir).map_err(backend_err)?;
    println!("{}", serde_json::to_string_pretty(&result).expect("json"));
    eprintln!("wrote {}", path.display());
    Ok(0)
}

fn verify(a: VerifyArgs) -> Result<u8, Exit> {
    let report = onchip_bench::verify_png_sequences(&a.target, &a.r#ref).map_err(input_err)?;
    if a.json {
        println!("{}", serde_json::to_string_pretty(&report).expect("json"));
    } else {
        println!("frames     {}", report.frames);
        println!("psnr mean  {:.2} dB", report.psnr_mean);
        println!("psnr min   {:.2} dB", report.psnr_min);
    }
    if let Some(min) = a.min_psnr
        && report.psnr_mean < min
    {
        eprintln!(
            "onchip: mean PSNR {:.2} dB is below the required {min:.2} dB",
            report.psnr_mean
        );
        return Ok(1);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_parsing() {
        assert_eq!(parse_scale("1920x1080").unwrap(), (1920, 1080));
        assert!(parse_scale("1920").is_err());
        assert!(parse_scale("0x10").is_err());
    }

    #[test]
    fn overlay_parsing() {
        let (p, x, y, o) = parse_overlay("logo_sample.png@32,48").unwrap();
        assert_eq!((p.as_str(), x, y, o), ("logo_sample.png", 32, 48, 1.0));
        let (_, _, _, o) = parse_overlay("logo_sample.png@-4,0@0.5").unwrap();
        assert_eq!(o, 0.5);
        assert!(parse_overlay("logo_sample.png").is_err());
        assert!(parse_overlay("logo_sample.png@1").is_err());
        assert!(parse_overlay("logo_sample.png@1,2@7").is_err());
    }

    #[test]
    fn cli_parses_run_shorthand() {
        let cli = Cli::try_parse_from([
            "onchip",
            "run",
            "in/",
            "-o",
            "out/",
            "--scale",
            "1280x720",
            "--overlay",
            "logo_sample.png@32,32",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::Run(a) => {
                let job = load_job(&RunArgs {
                    dump_job: true,
                    ..a
                })
                .unwrap();
                assert_eq!(job.graph.len(), 7);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn exit_code_mapping() {
        let e = input_err(anyhow!("x"));
        assert_eq!(e.0, EXIT_INPUT);
        let e = backend_err(anyhow!("x"));
        assert_eq!(e.0, EXIT_BACKEND);
    }
}
