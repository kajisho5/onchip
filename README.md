> Video frames stay on-chip. Decode, filter, encode — no round trip.

Research prototype — not an FFmpeg replacement.

**onchip** is a GPU-resident video filter graph. Decoded frames go straight into a
[wgpu](https://wgpu.rs) compute pipeline (one WGSL codebase for Metal and D3D12),
get filtered, and go straight back into the OS hardware encoder. Uncompressed pixels
never touch system RAM. Codecs are delegated to the OS (VideoToolbox on macOS,
Media Foundation on Windows) — onchip never implements a codec.

The research question: if the RAM↔GPU round trip is removed entirely and the filters
share one shader codebase across platforms, can we beat FFmpeg's CPU and HW filter paths
on both throughput **and** color accuracy? See [`docs/DESIGN.md`](docs/DESIGN.md).

## Status

| Milestone | Scope | Status |
|---|---|---|
| M0 | Workspace, CI, job parser, filter graph, WGSL filters (scale / overlay / color), `onchip probe` | ✅ done |
| M1 | **Windows**: Media Foundation decode → D3D12 → Media Foundation encode, on-chip proof via PIX | 🟡 code complete, not yet run on hardware ([docs/VERIFICATION-M1.md](docs/VERIFICATION-M1.md)) |
| M2 | **macOS**: VideoToolbox decode → Metal → VideoToolbox encode, on-chip proof via Instruments | ⬜ not started |
| M3 | Color management: BT.2020 PQ, full/limited range, `onchip verify` ΔE2000 | ⬜ not started |
| M4 | Public benchmarks vs FFmpeg (wins and losses alike) | ⬜ not started |
| M5+ | Linux (Vulkan Video), AV1, ProRes, audio | ⬜ roadmap |

## What "on-chip" means

From decoder output to encoder input, **uncompressed pixel data is never written back
to system RAM**. GPU→GPU copies are allowed (they stay on-chip). Compressed bitstreams
are small and may pass through the CPU for demux/mux.

On Apple Silicon there is no discrete GPU memory at all — memory is unified. onchip
therefore talks about *GPU-accessible memory*, never "VRAM".

## Usage

```sh
onchip probe                       # GPU adapter, backend, hardware codecs
onchip probe --json                # same, machine-readable
onchip run job.json                # run a declarative filter graph
onchip run in/ -o out/ --scale 1920x1080 --overlay logo_sample.png@32,32          # PNG sequence (any OS)
onchip run in.mp4 -o out.mp4 --codec h264 --scale 1920x1080 --overlay logo_sample.png@32,32   # Windows
onchip bench job.json --against ffmpeg --ffmpeg-encoder h264_mf                   # same input & filters, FFmpeg CPU path
onchip verify out.mp4 --ref ffmpeg_out.mp4                                        # PSNR (FFmpeg decodes both)
```

With codec `png`, `source`/`sink` read and write PNG sequences on any OS. With codec
`h264` on Windows the full on-chip path runs: hardware decode → GPU graph → hardware
encode. macOS (VideoToolbox) is M2.

A job file:

```json
{
  "input": "frames_in/",
  "output": { "path": "frames_out/", "codec": "png" },
  "graph": [
    { "id": "src",  "op": "source" },
    { "id": "rgb",  "op": "yuv2rgb", "in": ["src"] },
    { "id": "sc",   "op": "scale",   "in": ["rgb"], "width": 1920, "height": 1080, "filter": "lanczos3" },
    { "id": "logo", "op": "image",   "path": "logo_sample.png" },
    { "id": "ov",   "op": "overlay", "in": ["sc", "logo"], "x": 32, "y": 32, "opacity": 0.9 },
    { "id": "yuv",  "op": "rgb2yuv", "in": ["ov"] },
    { "id": "out",  "op": "sink",    "in": ["yuv"] }
  ]
}
```

## Build

```sh
cargo build --workspace
cargo test --workspace              # CPU-only tests
cargo test --workspace -- --ignored # GPU tests (needs a real adapter)
```

## Verified environment

<!-- onchip:env:begin -->
_Run `scripts/update-readme-env.ps1` (Windows) after `onchip probe --json` to fill this in._
<!-- onchip:env:end -->

## Reproducing the benchmarks

Filled in at M4. Fixtures are synthetic (`bench/fixtures/gen.py`) — no real footage is
ever committed or required.

## Verification

What was actually run and measured for each milestone lives in `docs/VERIFICATION-*.md`
(M0: [`docs/VERIFICATION-M0.md`](docs/VERIFICATION-M0.md)). Numbers there are facts from
one machine on one day; inferences are labelled as such.

## Layout

```
crates/onchip-core         shared types, job parser, filter graph, WGSL shaders
crates/onchip-backend-win  Media Foundation + D3D12 interop   (cfg(windows))
crates/onchip-backend-mac  VideoToolbox + Metal interop        (cfg(macos))
crates/onchip-cli          the `onchip` binary
crates/onchip-bench        benchmark harness and result schema
shaders/                   WGSL compute shaders (embedded via include_str!)
docs/                      DESIGN.md, INTEROP-WIN.md, INTEROP-MAC.md
```

## License

MIT OR Apache-2.0, at your option.
