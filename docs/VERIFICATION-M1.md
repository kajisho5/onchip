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

## What is NOT verified yet
- Nothing on this page involving Media Foundation, D3D11, D3D12 or a real GPU has run.
  Every `unsafe` COM call was written from the crate's own signatures, but runtime
  behaviour (type negotiation, async event flow, shared-handle opening, plane copies)
  is **unverified** until the steps below are done on Windows.

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
