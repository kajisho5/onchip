# M0 verification log

Facts only. Numbers below were measured on 2026-10-15 in a Linux container with no GPU,
using Mesa's `llvmpipe` software Vulkan driver (`VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json`),
debug build. Throughput numbers from this setup are meaningless and are not reported.

## Toolchain
- rustc 1.97.0, cargo 1.97.0
- wgpu 30.0.1, windows 0.62.2, clap 4.6.7, image 0.25.10, serde_json 1 (`preserve_order`)
- FFmpeg 6.1.1 (Ubuntu) — used only to generate fixtures and references

## Commands that passed
```
cargo build --workspace
cargo test --workspace                       # 37 CPU tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo check  --workspace --target x86_64-pc-windows-gnu   # Media Foundation probe code type-checks
cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu -- -D warnings
cargo test -p onchip-core -- --ignored       # 8 GPU tests on llvmpipe
```

## GPU test results (llvmpipe)
| Test | Result |
|---|---|
| `gpu_yuv_round_trip_is_near_exact` (640x360 gradient, NV12 → yuv2rgb → rgb2yuv → NV12) | Y 68.15 dB, CbCr 42.25 dB |
| `gpu_flat_frame_is_bit_exact` | max delta ≤ 1 code |
| `gpu_odd_dimensions_do_not_crash` (257x131) | pass |
| `gpu_identity_scale_is_exact` (bilinear / bicubic / lanczos3 at 1:1) | PSNR ∞ (bit-exact vs passthrough) |
| `gpu_downscale_of_flat_is_flat` (256x128 → 64x32 lanczos3) | pass |
| `gpu_overlay_composites_opaque_box` | pass |

The CbCr figure is lower than luma by design: `rgb2yuv` sites chroma on the even column and
averages the two rows, `yuv2rgb` resamples it back bilinearly, so vertical chroma detail is
low-passed once per round trip. Luma is untouched by that path.

## End-to-end on a synthetic sequence (12 frames, `testsrc2` 640x360)
`onchip run seq/ -o out/ --scale 320x180 --overlay logo_sample.png@16,16@0.9` produced 12
PNGs; scale, overlay position and colors visually match the FFmpeg equivalent.

PSNR against FFmpeg output on the *same* content, for calibration:

| Comparison | PSNR (mean, 12 frames) |
|---|---|
| FFmpeg `format=yuv420p` round trip vs. source RGB | 24.70 dB |
| onchip passthrough (NV12 in, NV12 out) vs. source RGB | 23.85 dB |
| onchip passthrough vs. FFmpeg BT.709 round trip | 26.65 dB |
| onchip scale lanczos3 vs. FFmpeg `scale=flags=lanczos` | 24.97 dB |
| onchip scale+overlay vs. FFmpeg `scale,overlay` | 25.31 dB |

Interpretation (labelled):
- **Fact**: `testsrc2` is saturated color bars with hard edges; 4:2:0 subsampling alone costs
  FFmpeg ~25 dB on it. The absolute PSNRs above are dominated by the content, not by onchip.
- **Fact**: the difference image between onchip and FFmpeg is zero inside flat color regions,
  non-zero only on edges and inside the semi-transparent overlay.
- **Inference**: the remaining gap comes from (a) chroma siting and upsampling filter
  (onchip: left-sited bilinear; swscale: its own chroma filter), ROADMAP RM-033; and (b) onchip
  compositing and scaling in linear light while FFmpeg does both in the encoded domain. A
  `blend_space` / `scale_space` option to match FFmpeg's encoded-domain behaviour is tracked as
  RM-035 so M3's ΔE comparison can be apples-to-apples.

## Not verified here
- Any real GPU, Metal or D3D12 — the two platform backends are M1 / M2.
- P010 (needs `TEXTURE_FORMAT_16BIT_NORM`; llvmpipe reports it, but no 10-bit fixture exists in M0).
- GitHub Issues for ROADMAP items were not created from this environment (no `gh` auth);
  run `scripts/create-roadmap-issues.sh` once locally.
