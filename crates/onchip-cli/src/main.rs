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
    /// Time a job, optionally against FFmpeg on the same input and filters
    Bench(BenchArgs),
    /// Compare two outputs (PNG sequences or video files) by PSNR. SSIM / ΔE2000 arrive in M3
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
    /// Output codec (shorthand form only). `png` everywhere; `h264` on Windows (M1)
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
    /// Name recorded as `graph` in the result (`auto` derives it from the job)
    #[arg(long, default_value = "auto")]
    graph: String,
    /// Compare against another tool (`ffmpeg`): same input, same filters, FFmpeg's CPU
    /// filter path, encoded with --ffmpeg-encoder
    #[arg(long)]
    against: Option<String>,
    /// FFmpeg binary
    #[arg(long, default_value = "ffmpeg")]
    ffmpeg: String,
    /// FFmpeg video encoder for the baseline (`h264_mf`, `h264_nvenc`, `h264_videotoolbox`, `libx264`)
    #[arg(long, default_value = "h264_mf")]
    ffmpeg_encoder: String,
    /// FFmpeg `-hwaccel` for the baseline's decode (`d3d11va`, `cuda`, `videotoolbox`); filters stay on the CPU
    #[arg(long)]
    ffmpeg_hwaccel: Option<String>,
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
    /// FFmpeg binary used to decode video inputs to frames
    #[arg(long, default_value = "ffmpeg")]
    ffmpeg: String,
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
    println!(
        "nv12 textures  {}  (needed to share decoder/encoder surfaces with wgpu)",
        if ad.texture_format_nv12 { "yes" } else { "no" }
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
        return run_video(&job, graph, a.quiet);
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

/// mp4 in → platform backend → mp4 out. Windows only until M2.
#[cfg(target_os = "windows")]
fn run_video(job: &Job, _graph: Graph, quiet: bool) -> Result<u8, Exit> {
    let ctx = gpu()?;
    if !quiet {
        eprintln!(
            "onchip: {} -> {} on {} ({})",
            job.input, job.output.path, ctx.info.name, ctx.info.backend
        );
    }
    let report = onchip_backend_win::pipeline::transcode(ctx, job, |n| {
        if !quiet && n % 60 == 0 {
            eprintln!("  {n} frames");
        }
    })
    .map_err(backend_err)?;
    if !quiet {
        eprintln!(
            "onchip: {} frames in {:.3}s ({:.1} fps)  decoder={} ({})  encoder={} ({})  {}",
            report.frames_encoded,
            report.seconds,
            report.fps,
            report.decoder_mft,
            if report.decoder_async {
                "async"
            } else {
                "sync"
            },
            report.encoder_mft,
            if report.encoder_async {
                "async"
            } else {
                "sync"
            },
            report.interop
        );
        for n in &report.encoder_notes {
            eprintln!("  encoder note: {n}");
        }
    }
    Ok(0)
}

#[cfg(not(target_os = "windows"))]
fn run_video(job: &Job, _graph: Graph, _quiet: bool) -> Result<u8, Exit> {
    Err(backend_err(anyhow!(
        "codec {:?} needs the platform backend: Windows (M1) is implemented, macOS lands in M2. \
         On this OS use codec `png` with a PNG sequence.",
        job.output.codec
    )))
}

fn bench(a: BenchArgs) -> Result<u8, Exit> {
    if a.runs == 0 {
        return Err(input_err(anyhow!("--runs must be >= 1")));
    }
    if let Some(tool) = &a.against
        && tool != "ffmpeg"
    {
        return Err(input_err(anyhow!(
            "--against only knows `ffmpeg`, got {tool:?}"
        )));
    }
    let text = std::fs::read_to_string(&a.job)
        .with_context(|| format!("reading {}", a.job.display()))
        .map_err(input_err)?;
    let job = Job::from_json(&text).map_err(input_err)?;
    let spec = onchip_bench::ffmpeg::FilterSpec::from_job(&job);
    let graph_name = if a.graph == "auto" {
        spec.graph_name()
    } else {
        a.graph.clone()
    };
    let fixture = a.fixture.clone().unwrap_or_else(|| {
        Path::new(&job.input)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or("input".into())
    });
    let ctx = gpu()?;
    let mut results = Vec::new();

    // ---- onchip ----
    let (frames, seconds, notes, settings) = match job.output.codec {
        Codec::Png => {
            let (f, s, n) = bench_png(&ctx, &job, a.runs)?;
            (f, s, n, None)
        }
        _ => bench_video(&ctx, &job, a.runs)?,
    };
    results.push(onchip_bench::BenchResult {
        schema: onchip_bench::BenchResult::SCHEMA,
        date_utc: onchip_bench::now_utc(),
        tool: "onchip".into(),
        tool_version: onchip_core::VERSION.into(),
        fixture: fixture.clone(),
        graph: graph_name.clone(),
        frames,
        wall_seconds: seconds,
        fps: if seconds > 0.0 {
            frames as f64 / seconds
        } else {
            0.0
        },
        ms_per_frame: if frames > 0 {
            seconds * 1000.0 / frames as f64
        } else {
            0.0
        },
        cpu_percent: None,
        adapter: ctx.info.clone(),
        notes,
    });

    // ---- ffmpeg baseline ----
    if a.against.is_some() {
        if job.output.codec == Codec::Png {
            return Err(input_err(anyhow!(
                "--against ffmpeg needs a video job (codec h264)"
            )));
        }
        let out = Path::new(&job.output.path).with_extension("ffmpeg.mp4");
        let mut best: Option<onchip_bench::ffmpeg::FfmpegRun> = None;
        for run in 0..a.runs {
            let r = onchip_bench::ffmpeg::run_ffmpeg_cpu_filters(
                &a.ffmpeg,
                Path::new(&job.input),
                &out,
                &spec,
                &a.ffmpeg_encoder,
                settings,
                a.ffmpeg_hwaccel.as_deref(),
            )
            .map_err(backend_err)?;
            eprintln!(
                "ffmpeg run {}/{}: {} frames, {:.3}s, {:.1} fps",
                run + 1,
                a.runs,
                r.frames,
                r.seconds,
                r.frames as f64 / r.seconds.max(1e-9)
            );
            if best.as_ref().is_none_or(|b| r.seconds < b.seconds) {
                best = Some(r);
            }
        }
        let r = best.expect("at least one run");
        results.push(onchip_bench::BenchResult {
            schema: onchip_bench::BenchResult::SCHEMA,
            date_utc: onchip_bench::now_utc(),
            tool: if a.ffmpeg_hwaccel.is_some() {
                "ffmpeg-hwdec-cpufilter".into()
            } else {
                "ffmpeg-cpu".into()
            },
            tool_version: r.ffmpeg_version.clone(),
            fixture,
            graph: graph_name,
            frames: r.frames,
            wall_seconds: r.seconds,
            fps: r.frames as f64 / r.seconds.max(1e-9),
            ms_per_frame: r.seconds * 1000.0 / r.frames.max(1) as f64,
            cpu_percent: None,
            adapter: ctx.info.clone(),
            notes: vec![
                format!("encoder={}", a.ffmpeg_encoder),
                match settings {
                    Some(s) => format!(
                        "same encoder settings as onchip: {} bit/s CBR, GOP {}, no B-frames, High",
                        s.bitrate_bps, s.gop_frames
                    ),
                    None => "encoder settings: FFmpeg defaults".into(),
                },
                r.command.clone(),
            ],
        });
    }

    for r in &results {
        let path = r.write(&a.out_dir).map_err(backend_err)?;
        println!("{}", serde_json::to_string_pretty(r).expect("json"));
        eprintln!("wrote {}", path.display());
    }
    if results.len() == 2 {
        let (o, f) = (&results[0], &results[1]);
        eprintln!(
            "onchip {:.1} fps vs {} {:.1} fps  ({:.2}x)",
            o.fps,
            f.tool,
            f.fps,
            o.fps / f.fps.max(1e-9)
        );
    }
    Ok(0)
}

/// Time the PNG path (GPU graph only; frames pre-uploaded). Returns (frames, best seconds, notes).
fn bench_png(
    ctx: &std::sync::Arc<GpuContext>,
    job: &Job,
    runs: u32,
) -> Result<(u64, f64, Vec<String>), Exit> {
    let color = ColorSpec::BT709_LIMITED;
    let mut best: Option<onchip_bench::Stopwatch> = None;
    for run in 0..runs {
        let graph = Graph::build(job, color).map_err(input_err)?;
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
        eprintln!(
            "onchip run {}/{}: {} frames, {:.3}s, {:.1} fps",
            run + 1,
            runs,
            sw.frames(),
            sw.seconds(),
            sw.fps()
        );
        if best.as_ref().is_none_or(|b| sw.seconds() < b.seconds()) {
            best = Some(sw);
        }
    }
    let sw = best.expect("at least one run");
    Ok((
        sw.frames(),
        sw.seconds(),
        vec!["PNG stand-in codec ends; GPU graph time only (frames pre-uploaded)".into()],
    ))
}

type VideoBench = (
    u64,
    f64,
    Vec<String>,
    Option<onchip_bench::ffmpeg::EncodeSettings>,
);

#[cfg(target_os = "windows")]
fn bench_video(ctx: &std::sync::Arc<GpuContext>, job: &Job, runs: u32) -> Result<VideoBench, Exit> {
    let mut best: Option<onchip_backend_win::pipeline::TranscodeReport> = None;
    for run in 0..runs {
        let r = onchip_backend_win::pipeline::transcode(ctx.clone(), job, |_| {})
            .map_err(backend_err)?;
        eprintln!(
            "onchip run {}/{}: {} frames, {:.3}s, {:.1} fps",
            run + 1,
            runs,
            r.frames_encoded,
            r.seconds,
            r.fps
        );
        if best.as_ref().is_none_or(|b| r.seconds < b.seconds) {
            best = Some(r);
        }
    }
    let r = best.expect("at least one run");
    Ok((
        r.frames_encoded,
        r.seconds,
        vec![
            "full path: demux + HW decode + GPU graph + HW encode + mux".into(),
            format!(
                "encoder settings: {} bit/s CBR, GOP {}, no B-frames, High ({}x{})",
                r.bitrate_bps, r.gop_frames, r.out_width, r.out_height
            ),
            format!(
                "decoder={} ({})",
                r.decoder_mft,
                if r.decoder_async { "async" } else { "sync" }
            ),
            format!(
                "encoder={} ({})",
                r.encoder_mft,
                if r.encoder_async { "async" } else { "sync" }
            ),
            r.interop.clone(),
        ],
        Some(onchip_bench::ffmpeg::EncodeSettings {
            bitrate_bps: r.bitrate_bps,
            gop_frames: r.gop_frames,
        }),
    ))
}

#[cfg(not(target_os = "windows"))]
fn bench_video(
    _ctx: &std::sync::Arc<GpuContext>,
    _job: &Job,
    _runs: u32,
) -> Result<VideoBench, Exit> {
    Err(backend_err(anyhow!(
        "video benchmarks need the Windows backend (M1); macOS is M2"
    )))
}

fn is_video_file(p: &Path) -> bool {
    p.is_file()
        && p.extension().is_some_and(|e| {
            ["mp4", "mov", "m4v", "mkv", "h264", "264"]
                .contains(&e.to_string_lossy().to_lowercase().as_str())
        })
}

fn verify(a: VerifyArgs) -> Result<u8, Exit> {
    // Video files are decoded with FFmpeg (external) to PNG sequences first, so both
    // sides go through the same decoder and chroma upsampling.
    let tmp = std::env::temp_dir().join(format!("onchip-verify-{}", std::process::id()));
    let target = if is_video_file(&a.target) {
        let d = tmp.join("target");
        onchip_bench::ffmpeg::decode_to_png_dir(&a.ffmpeg, &a.target, &d).map_err(backend_err)?;
        d
    } else {
        a.target.clone()
    };
    let reference = if is_video_file(&a.r#ref) {
        let d = tmp.join("ref");
        onchip_bench::ffmpeg::decode_to_png_dir(&a.ffmpeg, &a.r#ref, &d).map_err(backend_err)?;
        d
    } else {
        a.r#ref.clone()
    };
    let report = onchip_bench::verify_png_sequences(&target, &reference).map_err(input_err);
    let _ = std::fs::remove_dir_all(&tmp);
    let report = report?;
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
