//! GPU tests. They need a real adapter, so they are `#[ignore]` and run locally with
//! `cargo test -p onchip-core -- --ignored`. CI runs only the CPU tests.
//!
//! Reference-image tests look for `tests/ref/*.png` (generated with FFmpeg, see
//! `tests/ref/README.md`) and skip with a message when the files are absent.

use std::path::PathBuf;
use std::sync::Arc;

use crate::backend::Codec;
use crate::color::ColorSpec;
use crate::exec::{Executor, WorkingImage};
use crate::gpu::GpuContext;
use crate::graph::Graph;
use crate::job::{Job, Node, Op, ScaleFilter};
use crate::pngio::{self, Nv12Planes};

fn ctx() -> Arc<GpuContext> {
    // Any backend: tests may run on a CI box with a software Vulkan driver.
    GpuContext::new(Some(wgpu::Backends::all())).expect("GPU adapter")
}

fn gradient_rgb(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            v.push((x * 255 / (w - 1).max(1)) as u8);
            v.push((y * 255 / (h - 1).max(1)) as u8);
            v.push((((x * 7 + y * 3) % 256) as u8).wrapping_mul(1));
        }
    }
    v
}

fn run_graph(ctx: &Arc<GpuContext>, job: &Job, planes: &Nv12Planes) -> Nv12Planes {
    let graph = Graph::build(job, ColorSpec::BT709_LIMITED).expect("graph");
    let mut exec = Executor::new(
        ctx.clone(),
        graph,
        planes.width,
        planes.height,
        crate::PixelFormat::Nv12,
    )
    .expect("executor");
    let frame = pngio::upload_nv12(ctx, planes, ColorSpec::BT709_LIMITED, 0);
    let out = exec.run_frame(&frame).expect("run");
    pngio::readback_nv12(ctx, &out).expect("readback")
}

fn passthrough_job() -> Job {
    Job::shorthand("in", "out", Codec::Png, None, None)
}

#[test]
#[ignore = "needs a GPU adapter"]
fn gpu_yuv_round_trip_is_near_exact() {
    let ctx = ctx();
    let (w, h) = (640, 360);
    let rgb = gradient_rgb(w, h);
    let planes = pngio::nv12_from_rgb8(&rgb, w, h, ColorSpec::BT709_LIMITED);
    let out = run_graph(&ctx, &passthrough_job(), &planes);
    let py = pngio::psnr_u8(&planes.y, &out.y);
    let puv = pngio::psnr_u8(&planes.uv, &out.uv);
    eprintln!("round trip PSNR  Y={py:.2} dB  CbCr={puv:.2} dB");
    assert!(py >= 50.0, "luma PSNR {py}");
    assert!(puv >= 40.0, "chroma PSNR {puv}");
}

#[test]
#[ignore = "needs a GPU adapter"]
fn gpu_flat_frame_is_bit_exact() {
    let ctx = ctx();
    let (w, h) = (128, 64);
    let rgb: Vec<u8> = [180u8, 90, 40]
        .iter()
        .cycle()
        .take((w * h * 3) as usize)
        .copied()
        .collect();
    let planes = pngio::nv12_from_rgb8(&rgb, w, h, ColorSpec::BT709_LIMITED);
    let out = run_graph(&ctx, &passthrough_job(), &planes);
    let dy = planes
        .y
        .iter()
        .zip(&out.y)
        .map(|(a, b)| (*a as i32 - *b as i32).abs())
        .max()
        .unwrap();
    let duv = planes
        .uv
        .iter()
        .zip(&out.uv)
        .map(|(a, b)| (*a as i32 - *b as i32).abs())
        .max()
        .unwrap();
    assert!(dy <= 1, "max luma delta {dy}");
    assert!(duv <= 1, "max chroma delta {duv}");
}

#[test]
#[ignore = "needs a GPU adapter"]
fn gpu_odd_dimensions_do_not_crash() {
    let ctx = ctx();
    let (w, h) = (257, 131);
    let rgb = gradient_rgb(w, h);
    let planes = pngio::nv12_from_rgb8(&rgb, w, h, ColorSpec::BT709_LIMITED);
    let out = run_graph(&ctx, &passthrough_job(), &planes);
    assert_eq!(out.width, w);
    assert_eq!(out.height, h);
    assert_eq!(out.y.len(), (w * h) as usize);
    let py = pngio::psnr_u8(&planes.y, &out.y);
    assert!(py >= 50.0, "luma PSNR {py}");
}

#[test]
#[ignore = "needs a GPU adapter"]
fn gpu_identity_scale_is_exact() {
    let ctx = ctx();
    let (w, h) = (320, 180);
    let rgb = gradient_rgb(w, h);
    let planes = pngio::nv12_from_rgb8(&rgb, w, h, ColorSpec::BT709_LIMITED);
    let base = run_graph(&ctx, &passthrough_job(), &planes);
    for f in [
        ScaleFilter::Bilinear,
        ScaleFilter::Bicubic,
        ScaleFilter::Lanczos3,
    ] {
        let job = Job::shorthand("in", "out", Codec::Png, Some((w, h, f)), None);
        let out = run_graph(&ctx, &job, &planes);
        let p = pngio::psnr_u8(&base.y, &out.y);
        eprintln!("{f:?} identity scale PSNR {p:.2}");
        assert!(p >= 50.0, "{f:?}: PSNR {p}");
    }
}

#[test]
#[ignore = "needs a GPU adapter"]
fn gpu_downscale_of_flat_is_flat() {
    let ctx = ctx();
    let (w, h) = (256, 128);
    let rgb: Vec<u8> = [120u8, 200, 60]
        .iter()
        .cycle()
        .take((w * h * 3) as usize)
        .copied()
        .collect();
    let planes = pngio::nv12_from_rgb8(&rgb, w, h, ColorSpec::BT709_LIMITED);
    let job = Job::shorthand(
        "in",
        "out",
        Codec::Png,
        Some((64, 32, ScaleFilter::Lanczos3)),
        None,
    );
    let out = run_graph(&ctx, &job, &planes);
    assert_eq!((out.width, out.height), (64, 32));
    let y0 = planes.y[0];
    for v in &out.y {
        assert!((*v as i32 - y0 as i32).abs() <= 1, "{v} vs {y0}");
    }
}

#[test]
#[ignore = "needs a GPU adapter"]
fn gpu_overlay_composites_opaque_box() {
    let ctx = ctx();
    let (w, h) = (128, 64);
    let dir = std::env::temp_dir().join(format!("onchip-overlay-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // 32x16 opaque green box, sRGB-encoded PNG
    let logo = dir.join("logo_sample.png");
    let px: Vec<u8> = [0u8, 255, 0, 255]
        .iter()
        .cycle()
        .take(32 * 16 * 4)
        .copied()
        .collect();
    image::save_buffer(&logo, &px, 32, 16, image::ColorType::Rgba8).unwrap();

    let rgb: Vec<u8> = [0u8, 0, 0]
        .iter()
        .cycle()
        .take((w * h * 3) as usize)
        .copied()
        .collect();
    let planes = pngio::nv12_from_rgb8(&rgb, w, h, ColorSpec::BT709_LIMITED);
    let job = Job::shorthand(
        "in",
        "out",
        Codec::Png,
        None,
        Some((logo.to_string_lossy().into_owned(), 8, 8, 1.0)),
    );
    let out = run_graph(&ctx, &job, &planes);
    let back = pngio::rgb8_from_nv12(&out, ColorSpec::BT709_LIMITED);
    let at = |x: u32, y: u32| {
        let i = ((y * w + x) * 3) as usize;
        [back[i], back[i + 1], back[i + 2]]
    };
    // inside the box: green (sRGB 255 in, BT.709 out: not bit-identical, but green)
    let g = at(16, 12);
    assert!(g[1] > 200 && g[0] < 40 && g[2] < 40, "inside {g:?}");
    // outside the box: black
    let b = at(100, 50);
    assert!(b.iter().all(|c| *c < 8), "outside {b:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "needs a GPU adapter"]
fn gpu_working_image_round_trip() {
    // WorkingImage conversion is CPU only, but it feeds the GPU overlay path;
    // keep it next to the GPU tests so the whole path is exercised together.
    let img = WorkingImage::from_rgba_f32(2, 1, &[0.5, 0.25, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0]);
    assert_eq!(img.rgba16f.len(), 8);
    assert_eq!(crate::gpu::f16_bits_to_f32(img.rgba16f[0]), 0.5);
}

fn ref_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/ref")
}

#[test]
#[ignore = "needs a GPU adapter and FFmpeg-generated references (tests/ref/README.md)"]
fn gpu_scale_matches_ffmpeg_reference() {
    let src = ref_dir().join("src_1080p_yuv420p.png");
    let reference = ref_dir().join("scale_720p_lanczos.png");
    if !src.exists() || !reference.exists() {
        eprintln!("skipping: generate tests/ref with the commands in tests/ref/README.md");
        return;
    }
    let ctx = ctx();
    let (w, h, rgb) = pngio::read_png_rgb8(&src).unwrap();
    let planes = pngio::nv12_from_rgb8(&rgb, w, h, ColorSpec::BT709_LIMITED);
    let job = Job::shorthand(
        "in",
        "out",
        Codec::Png,
        Some((1280, 720, ScaleFilter::Lanczos3)),
        None,
    );
    let out = run_graph(&ctx, &job, &planes);
    let back = pngio::rgb8_from_nv12(&out, ColorSpec::BT709_LIMITED);
    let (rw, rh, ref_rgb) = pngio::read_png_rgb8(&reference).unwrap();
    assert_eq!((rw, rh), (1280, 720));
    let p = pngio::psnr_u8(&back, &ref_rgb);
    eprintln!("scale vs ffmpeg lanczos PSNR {p:.2} dB");
    // swscale's lanczos and ours differ in windowing details; 40 dB is "visually
    // identical", 50 dB is the M3 target once chroma siting is fully matched.
    assert!(p >= 40.0, "PSNR {p}");
}

#[test]
fn cpu_graph_build_for_tests_is_valid() {
    // make sure the fixtures above are valid even on machines without a GPU
    let job = Job::shorthand(
        "in",
        "out",
        Codec::Png,
        Some((64, 32, ScaleFilter::Lanczos3)),
        None,
    );
    Graph::build(&job, ColorSpec::BT709_LIMITED).unwrap();
    let mut job = passthrough_job();
    job.graph.push(Node {
        id: "x".into(),
        inputs: vec![],
        op: Op::Image {
            path: "none.png".into(),
        },
    });
    assert!(Graph::build(&job, ColorSpec::BT709_LIMITED).is_err());
}
