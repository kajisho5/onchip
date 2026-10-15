# Windows interop notes (Media Foundation ⇄ D3D12 ⇄ wgpu)

Facts only; anything inferred is labelled **(inference)**. The code this describes is
`crates/onchip-backend-win/src/{d3d,mft,decoder,encoder,pipeline}.rs`.

## Status

| Item | State |
|---|---|
| Code | written; builds and passes `clippy -D warnings` on Windows (MSVC) |
| Runtime on a real Windows GPU | **runs end to end** (2026-10-15, AMD Radeon integrated GPU, see Outcome) |
| PIX capture | not yet taken (RM-008) |

## Design as implemented

```
file ──demux (CPU)──▶ H.264 packets ──▶ MFT decoder (D3D11, hardware or D3D11VA)
                                             │ IMFSample / IMFDXGIBuffer → ID3D11Texture2D
                        plan B: CopySubresourceRegion (visible box) ─▶ shared NV12 ID3D11Texture2D
                        plan A: MFT decodes straight into it           (SHARED | SHARED_NTHANDLE)
                                             │ IDXGIResource1::CreateSharedHandle
                                             ▼
                                   ID3D12Resource  (ID3D12Device::OpenSharedHandle on wgpu's device)
                                             │ wgpu_hal::dx12::Device::texture_from_raw
                                             ▼
                                   wgpu::Texture (format NV12; Plane0 / Plane1 views)
                                             │ yuv2rgb → scale → overlay → rgb2yuv  (WGSL)
                                             │ copy_buffer_to_texture into Plane0 / Plane1 of
                                             ▼ a second shared NV12 surface (encoder input)
                                   MFT encoder (D3D11, MFCreateDXGISurfaceBuffer) ──▶ H.264 ──mux (CPU)──▶ file
```

- Device pairing: the D3D11 device is created on the adapter whose LUID matches wgpu's
  D3D12 device (`IDXGIFactory4::EnumAdapterByLuid`), so NT shared handles open on both.
- Synchronisation (CPU-side, no readback): after D3D11 work, `ID3D11DeviceContext4::Signal`
  on a shared `ID3D11Fence` and `ID3D12Fence::SetEventOnCompletion` + wait; before D3D11
  reads a wgpu-written surface, `wgpu::Device::poll(wait)`. GPU-side waits: ROADMAP RM-002.
- Decoder selection: vendor hardware MFTs (`MFT_ENUM_FLAG_HARDWARE`) first; if none (NVIDIA
  ships no decoder MFT), the Microsoft H.264 decoder with the DXGI device manager, which
  decodes on the GPU through D3D11VA. Both yield D3D11 textures; neither touches RAM.
- Encoder selection: vendor hardware MFTs only. No software fallback on purpose.
- Plan A vs B is decided from `MFT_OUTPUT_STREAM_PROVIDES_SAMPLES`: **(inference)** every
  D3D11-aware decoder MFT sets it, so plan B (one GPU-side copy per frame) is the expected
  path and plan A exists for completeness.
- Encoder input surfaces are pooled (8); a surface is reused only when the `IMFSample`
  wrapping it has no reference left besides ours (COM refcount check).
- Encoder is asked for CBR, GOP = 2 s, **B-frames = 0** (so dts == pts; RM-012 lifts this).

## Environment (2026-10-15)
- GPU: AMD Radeon(TM) Graphics (integrated, unified memory), driver 32.0.21037.3009, D3D12 via wgpu
- OS: Windows 11 (build not recorded yet)
- Toolchain: rustc 1.99.0, MSVC 14.44.35207, FFmpeg 9.0.2 (external, for fixtures/baseline only)
- `onchip probe --json`: `texture_format_nv12: true`, `texture_format_p010: true`,
  `hw_decoders: []` (no vendor decoder MFT), `hw_encoders: [AMDh264Encoder (nv12)]`

## Crate versions and references verified
| Crate | Version | API used | Reference |
|---|---|---|---|
| `windows` | 0.62.2 | `MFTEnumEx`, `IMFTransform`, `IMFDXGIDeviceManager`, `MFCreateDXGISurfaceBuffer`, `IMFDXGIBuffer`, `ID3D11Device5::CreateFence`, `IDXGIResource1::CreateSharedHandle`, `ID3D12Device::OpenSharedHandle`, `ICodecAPI` | signatures read from the crate source in `~/.cargo/registry` |
| `wgpu` / `wgpu-hal` | 30.0.1 | `Device::as_hal::<Dx12>`, `hal::dx12::Device::texture_from_raw`, `Device::create_texture_from_hal`, `TextureFormat::NV12` + `TextureAspect::Plane0/1`, `copy_buffer_to_texture` into a plane | read from the crate source; `wgpu-core` 30 validates planar-split copies (`command/transfer.rs`) |
| `mp4` | 0.14.0 | `Mp4Reader` / `Mp4Writer` (H.264 only; `hvcC` not parsed → RM-011) | crate source |

## Outcome (2026-10-15)
- Decoder MFT: `Microsoft H264 Video Decoder MFT` with the DXGI device manager (D3D11VA), sync.
  The vendor-hardware enumeration (`MFT_ENUM_FLAG_HARDWARE`) finds nothing on this AMD
  machine; the fallback to the Microsoft decoder is what runs. It allocates its own D3D11
  textures (`MFT_OUTPUT_STREAM_PROVIDES_SAMPLES`), so the path is **plan B**.
- Encoder MFT: `AMDh264Encoder`, async (event driven: 601 NeedInput / 602 HaveOutput / 603 DrainComplete).
  It does not provide output samples; we hand it memory buffers of `cbSize`.
- Plan taken: **plan B — one GPU-side `CopySubresourceRegion` (visible box) from the decoder's
  texture into the shared NV12 surface**, then zero-copy into wgpu and zero-copy into the encoder.
- `onchip run` 1080p H.264 → scale 1280x720 + overlay → H.264: 150 frames in 2.457 s
  (**61.1 fps**, with `ONCHIP_TRACE=1` printing every stage to a file — not a benchmark number).
  Output verified with ffprobe: h264, 1280x720, 150 frames; plays with thumbnails in Explorer.
- Bugs found and fixed on hardware: (1) the DXVA fallback had been lost before the first
  commit; (2) `surface_busy` counted a `cast()`'s own reference, so the 8-surface encoder pool
  looked permanently busy and the run stalled at exactly 8 frames; (3) draining the Microsoft
  decoder at end of stream released its whole reorder buffer at once and exhausted the decoder
  surface pool — the output queue is now capped at 6 and a refused packet is parked.
- Notes: ICodecAPI settings (CBR, mean bitrate, GOP, B-frames=0) — refusals not yet recorded
  (`encoder_notes` in the interop_poc JSON).

## Proof of zero readback (pending)
- What is already known from the code path: between decoder output and encoder input the
  only operations are `CopySubresourceRegion` (D3D11, GPU→GPU), wgpu compute passes,
  `copy_buffer_to_texture` (GPU→GPU) and `MFCreateDXGISurfaceBuffer` (wraps the texture).
  The CPU-side waits are fence/event waits, not maps. **This is an inference until PIX shows it.**
- Tool (PIX / GPUView):
- Capture: `docs/img/pix-m1.png`
- Procedure: capture `interop_poc.exe in.mp4 out.mp4` for ~60 frames; in the PIX event
  list filter for `Map`, `ReadFromSubresource`, `CopyTextureRegion`/`CopyResource` with a
  readback-heap destination; none must appear between the decoder's output and the
  encoder's input.
