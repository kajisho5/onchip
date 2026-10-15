//! M1-1 interop proof of concept (Windows only): run one H.264 file through
//! MF decode → shared NV12 surface → wgpu (identity graph) → shared NV12 surface →
//! MF encode → mp4, and print which interop plan the decoder took.
//!
//! Usage: cargo run --release -p onchip-backend-win --example interop_poc -- in.mp4 out.mp4
//!
//! Capture this run with PIX to prove there is no readback (docs/INTEROP-WIN.md).

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("interop_poc only runs on Windows");
    std::process::exit(3);
}

#[cfg(target_os = "windows")]
fn main() {
    use onchip_core::GpuContext;
    use onchip_core::backend::Codec;
    use onchip_core::job::Job;

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: interop_poc <in.mp4> <out.mp4> [WxH]");
        std::process::exit(2);
    }
    let scale = args.get(3).and_then(|s| {
        let (w, h) = s.split_once('x')?;
        Some((
            w.parse().ok()?,
            h.parse().ok()?,
            onchip_core::job::ScaleFilter::Lanczos3,
        ))
    });
    let job = Job::shorthand(&args[1], &args[2], Codec::H264, scale, None);
    let ctx = match GpuContext::new(None) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("GPU: {e}");
            std::process::exit(3);
        }
    };
    eprintln!("adapter: {} ({})", ctx.info.name, ctx.info.backend);
    match onchip_backend_win::pipeline::transcode(ctx, &job, |n| {
        if n % 30 == 0 {
            eprintln!("  {n} frames");
        }
    }) {
        Ok(report) => {
            println!("{}", serde_json::to_string_pretty(&report).expect("json"));
        }
        Err(e) => {
            eprintln!("interop_poc failed: {e}");
            std::process::exit(3);
        }
    }
}
