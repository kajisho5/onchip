# Windows interop notes (Media Foundation ⇄ D3D12 ⇄ wgpu)

Filled in during M1. Facts only; anything inferred is labelled **(inference)**.

## Environment
- GPU / driver:
- OS build:
- `onchip probe --json` output:

## Crate versions and references verified
| Crate | Version | API used | Reference URL |
|---|---|---|---|
| `windows` | | | |
| `wgpu` / `wgpu-hal` | | | |

## Decoder output
- MFT used:
- Does the MFT accept caller-provided output samples (`MFT_OUTPUT_STREAM_PROVIDES_SAMPLES` absent)?

## Plan A — shared NT handle (zero copy)
- Outcome:
- Notes:

## Plan B — GPU-side copy into shared texture
- Outcome:
- Notes:

## Synchronisation
- Fence mechanism:

## Proof of zero readback
- Tool (PIX / GPUView):
- Capture: `docs/img/pix-m1.png`
- Procedure:
