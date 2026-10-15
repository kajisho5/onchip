# macOS interop notes (VideoToolbox ⇄ Metal ⇄ wgpu)

Filled in during M2. Facts only; anything inferred is labelled **(inference)**.

## Environment
- Chip / macOS version:
- `onchip probe --json` output:

## Crate versions and references verified
| Crate | Version | API used | Reference URL |
|---|---|---|---|
| `objc2-video-toolbox` | | | |
| `objc2-core-video` | | | |
| `wgpu` / `wgpu-hal` | | | |

## Import (CVPixelBuffer → MTLTexture → wgpu)
- Outcome:

## Export (wgpu → IOSurface-backed CVPixelBuffer → VTCompressionSession)
- Outcome:

## Proof of zero readback
- Tool: Instruments (Metal System Trace)
- Capture: `docs/img/instruments-m2.png`
- Procedure:
