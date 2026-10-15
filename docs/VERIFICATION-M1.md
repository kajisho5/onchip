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
