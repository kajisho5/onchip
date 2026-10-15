# Windows interop notes (Media Foundation ⇄ D3D12 ⇄ wgpu)

Facts only; anything inferred is labelled **(inference)**. The code this describes is
`crates/onchip-backend-win/src/{d3d,mft,decoder,encoder,pipeline}.rs`.

## Status

| Item | State |
|---|---|
| Code | written and type-checked (`cargo check/clippy --target x86_64-pc-windows-gnu`, windows 0.62.2, wgpu 30.0.1) |
| Runtime on a real Windows GPU | **not yet run** — fill in the Environment / Outcome sections from the first run |
| PIX capture | not yet taken |

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

## Environment (fill in)
- GPU / driver:
- OS build:
- `onchip probe --json`:

## Crate versions and references verified
| Crate | Version | API used | Reference |
|---|---|---|---|
| `windows` | 0.62.2 | `MFTEnumEx`, `IMFTransform`, `IMFDXGIDeviceManager`, `MFCreateDXGISurfaceBuffer`, `IMFDXGIBuffer`, `ID3D11Device5::CreateFence`, `IDXGIResource1::CreateSharedHandle`, `ID3D12Device::OpenSharedHandle`, `ICodecAPI` | signatures read from the crate source in `~/.cargo/registry` |
| `wgpu` / `wgpu-hal` | 30.0.1 | `Device::as_hal::<Dx12>`, `hal::dx12::Device::texture_from_raw`, `Device::create_texture_from_hal`, `TextureFormat::NV12` + `TextureAspect::Plane0/1`, `copy_buffer_to_texture` into a plane | read from the crate source; `wgpu-core` 30 validates planar-split copies (`command/transfer.rs`) |
| `mp4` | 0.14.0 | `Mp4Reader` / `Mp4Writer` (H.264 only; `hvcC` not parsed → RM-011) | crate source |

## Outcome (fill in from `interop_poc`)
- Decoder MFT / async:
- Encoder MFT / async:
- Plan taken:
- Frames in / out, fps:
- Notes (ICodecAPI refusals etc.):

## Proof of zero readback (fill in)
- Tool (PIX / GPUView):
- Capture: `docs/img/pix-m1.png`
- Procedure: capture `interop_poc.exe in.mp4 out.mp4` for ~60 frames; in the PIX event
  list filter for `Map`, `ReadFromSubresource`, `CopyTextureRegion`/`CopyResource` with a
  readback-heap destination; none must appear between the decoder's output and the
  encoder's input.
