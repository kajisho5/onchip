# M1 verification log

Facts only. Inferences are labelled.

## What is verified (2026-10-15, Linux container, no Windows)
- The whole M1 code path builds and passes `clippy -D warnings` for `x86_64-pc-windows-gnu`
  (`cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu -- -D warnings`).
- The container pieces are verified on Linux against FFmpeg: `Mp4Demuxer` produces the
  same frame count and keyframe count as `ffprobe`, `Mp4Muxer` output decodes cleanly
  with `ffmpeg -f null -`, and the demux → mux round trip is lossless
  (`cargo test -p onchip-core container`).
- `onchip verify a.mp4 --ref b.mp4` works on Linux (FFmpeg decodes both to frames).
- `onchip bench --against ffmpeg` filter chain is exercised on Linux with `libx264`
  (`cargo test -p onchip-bench`).

## Hardware run (2026-10-15, AMD Radeon integrated GPU, Windows 11)
Done from this session by driving the Windows machine (winget installs, batch scripts):
- `cargo build --release --workspace` — OK (MSVC)
- `cargo test --workspace` — OK; `cargo test -p onchip-core -- --ignored` — **8/8 GPU tests pass on D3D12**
  (YUV round trip, bit-exact flat frame, odd sizes, identity scale, downscale, overlay, FFmpeg reference)
- `onchip probe --json` — NV12/P010 texture features present; AMDh264Encoder found; no vendor decoder MFT
- `interop_poc` and `onchip run` (1080p → 720p + overlay, H.264) — **complete end to end**:
  150 frames in 2.457 s = 61.1 fps with full stage tracing on; ffprobe confirms h264 1280x720 150 frames
- Interop plan: **B** (one GPU-side copy out of the Microsoft D3D11VA decoder's texture); encoder side zero-copy
- Three hardware-only bugs fixed along the way (see docs/INTEROP-WIN.md, Outcome)

## First benchmark run (2026-10-15, same machine) — superseded, kept for the record
`onchip bench --against ffmpeg --ffmpeg-encoder h264_mf --runs 3`, best of 3, committed in
`bench/results/20261015143*`:

| fixture → graph | onchip | ffmpeg (CPU decode + swscale + h264_mf) | ratio |
|---|---|---|---|
| 1080p → 1280x720 + overlay | 69.0 fps | 207.8 fps | 0.33x |
| 2160p → 1920x1080 + overlay | 22.5 fps | 93.8 fps | 0.24x |

`onchip verify` (onchip output vs FFmpeg output): 23.2 dB (1080p), 23.5 dB (2160p).

Two things were wrong with this run and were diagnosed with FFmpeg's `psnr` filter on the
Windows machine (per-plane, ±1-frame shifts, each output against an unencoded swscale
reference, overlay region excluded) plus the scale reference test on lavapipe:
1. **Unequal encoder settings.** The job had no `bitrate`, so onchip used its default
   (2.0 Mbit/s CBR, High, GOP 60, no B-frames) while FFmpeg ran `h264_mf` with FFmpeg's
   defaults — Constrained Baseline at 1.19 Mbit/s, non-CBR. Against the unencoded reference
   the FFmpeg output scored 30.5 dB (47 dB on its first frame), onchip's 25.0 dB with no frame
   above 25.8 dB — i.e. CBR at 2 Mbit/s starving every frame of this noisy fixture, not a
   pipeline fault (no plane swap: Y was the worst plane; no frame shift; same averages).
   Fix: the bench now replays onchip's bitrate/GOP/B-frames/profile/CBR onto ffmpeg.
2. **The 2-D scale kernel was the throughput bottleneck.** `interop_poc` (no filters) ran
   the same 1080p fixture at 139.6 fps; with lanczos3 downscale it fell to 69 fps, and at 4K
   (stretch 2 → 12 taps per axis → 144 fetches per pixel) to 22.5 fps. Fix: separable
   two-pass scale (same kernel, ~2·taps fetches).

Also found: `gpu_scale_matches_ffmpeg_reference` had never actually run (it skips without
`tests/ref/`), and its RGB-PSNR criterion against an RGB-scaled reference is unreachable for
any 4:2:0 pipeline; it now asserts luma PSNR ≥ 40 dB against a 4:2:0-chain swscale
reference (40.6 dB on lavapipe, before and after the separable change — the two shaders
agree).

## What is NOT verified yet
- PIX capture proving zero readback (RM-008). The code path contains no `Map`/readback, but that
  is an inference until captured.
- Playback in a player other than the Explorer thumbnail (VLC check pending).
- Numbers above are with tracing enabled; benchmark numbers come from `onchip bench` (below).

## Steps to run on Windows (copy-paste)
```powershell
# prerequisites: rustup (stable), Visual Studio Build Tools (MSVC), ffmpeg + ffprobe on PATH
git clone https://github.com/kajisho5/onchip; cd onchip
cargo build --release --workspace
cargo test --workspace
cargo test -p onchip-core -- --ignored            # GPU tests on the real adapter
.\target\release\onchip.exe probe --json           # expect hw decoders/encoders listed
python bench\fixtures\gen.py --seconds 10          # synthetic H.264 fixtures
cargo run --release -p onchip-backend-win --example interop_poc -- `
  bench\fixtures\fixture_bars_1080p_nv12.mp4 out_poc.mp4
.\target\release\onchip.exe run bench\fixtures\fixture_bars_1080p_nv12.mp4 -o out.mp4 `
  --codec h264 --scale 1280x720 --overlay bench\fixtures\logo_sample.png@32,32@0.9
.\target\release\onchip.exe verify out.mp4 --ref out_ffmpeg.mp4
.\target\release\onchip.exe bench job.json --against ffmpeg --ffmpeg-encoder h264_mf
.\scripts\update-readme-env.ps1
```
Then: PIX capture of `interop_poc` → `docs/img/pix-m1.png`, fill in `docs/INTEROP-WIN.md`.

## Results (fill in)
| Check | Result |
|---|---|
| `onchip probe` lists HW MFTs | |
| `interop_poc` completes, plan A/B | |
| `onchip run` 1080p → 720p + overlay plays in VLC | |
| PIX: no readback | |
| bench vs ffmpeg (fps) | |
