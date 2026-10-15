# ROADMAP

Milestones from `docs/DESIGN.md` §5. Each planned item below M1 is tracked as a GitHub Issue
labelled `milestone:Mx`. Run `scripts/create-roadmap-issues.sh` once after cloning (needs `gh auth`).

## M0 — Skeleton ✅
- [x] Cargo workspace, 5 crates, CI (macos-14 / windows-latest), release-please
- [x] Job definition parser (JSON, `deny_unknown_fields`)
- [x] Filter graph DAG with cycle / dangling-input / ColorSpec validation
- [x] WGSL compute shaders: `yuv2rgb`, `scale` (bilinear / bicubic / lanczos3), `overlay`, `rgb2yuv`
- [x] `onchip probe` (wgpu adapter + hardware MFT enumeration on Windows)
- [x] `onchip run` in PNG-sequence mode
- [x] GPU tests behind `#[ignore]`, CPU tests in CI

## M1 — Windows on-chip path (code written, hardware run pending)
- [x] RM-001 interop: MF decoder → D3D11 texture → D3D12 (plan B GPU copy on AMD iGPU) → wgpu — **verified on hardware 2026-10-15**
- [x] RM-002a shared fence sync D3D11 ⇄ D3D12 (CPU-side wait)
- [ ] RM-002 GPU-side waits instead of CPU waits
- [x] RM-003 wrap D3D12 texture as `wgpu::Texture` via `wgpu-hal` (NV12, plane views)
- [x] RM-004 export: wgpu → shared NV12 → `IMFSample` → HW encoder MFT
- [x] RM-005 mp4 demuxer (`mp4` crate, no Source Reader) — verified vs ffprobe
- [x] RM-006 mp4 muxer — verified with ffmpeg decode
- [x] RM-007 `WinBackend` implementing `VideoBackend`
- [ ] RM-008 PIX capture proving zero readback (`docs/img/pix-m1.png`)
- [x] RM-009 `docs/INTEROP-WIN.md` outcome section from a real run
- [x] RM-010 bench skeleton: `bench/fixtures/gen.py`, `onchip bench --against ffmpeg`, `onchip verify` on mp4 (PSNR; SSIM is M3)
- [ ] RM-011 HEVC: parse `hvcC` ourselves (the `mp4` crate drops it), HEVC decoder/encoder MFTs
- [ ] RM-012 allow B-frames: use `MFSampleExtension_DecodeTimestamp` for dts, negative ctts in the muxer
- [ ] RM-013 P010 (10-bit) surfaces and MFT types

## M2 — macOS on-chip path
- [ ] RM-020 VideoToolbox decode → `CVMetalTextureCache` → `MTLTexture` → wgpu
- [ ] RM-021 `CVPixelBufferPool` export → `VTCompressionSession`
- [ ] RM-022 `MacBackend` implementing `VideoBackend`
- [ ] RM-023 Instruments (Metal System Trace) capture proving zero readback
- [ ] RM-024 `docs/INTEROP-MAC.md`

## M3 — Color management
- [ ] RM-030 BT.2020 primaries
- [ ] RM-031 PQ / HLG transfer functions
- [ ] RM-032 full / limited range on both ends
- [ ] RM-033 chroma siting (center / top-left)
- [ ] RM-034 ΔE2000 in `onchip verify`, mean < 0.5 vs FFmpeg reference, enforced in CI
- [ ] RM-035 `scale_space` / `blend_space` option (linear | encoded) so comparisons against FFmpeg's encoded-domain filters are apples-to-apples (see docs/VERIFICATION-M0.md)

## M4 — Public benchmarks
- [ ] RM-040 fixtures: 1080p / 4K × 8bit / 10bit × 30 s, synthetic only
- [ ] RM-041 FFmpeg CPU filter path baseline
- [ ] RM-042 FFmpeg HW filter path baseline (scale_vt / scale_cuda + overlay_cuda)
- [ ] RM-043 results README with charts, wins and losses alike
- [ ] RM-044 CPU usage and RAM↔GPU transfer metrics in results JSON

## M5+ — Later
- [ ] RM-050 Linux: Vulkan Video + VA-API
- [ ] RM-051 AV1 (HW only)
- [ ] RM-052 ProRes on macOS
- [ ] RM-053 audio passthrough
- [ ] RM-054 callable from ffmpeg-skill / blender-skill style agent skills
