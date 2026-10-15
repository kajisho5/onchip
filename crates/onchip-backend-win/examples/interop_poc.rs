//! M1-1 interop proof of concept (Windows only). See docs/DESIGN.md §4.2 and the M1
//! section of ROADMAP.md. Steps, in order:
//!
//!  1. Create a D3D11 device, register it with an `IMFDXGIDeviceManager`.
//!  2. Enumerate a hardware H.264 decoder MFT, hand it the device manager
//!     (`MFT_MESSAGE_SET_D3D_MANAGER`), decode one frame, get its `ID3D11Texture2D` (NV12).
//!  3. Plan A: if the MFT accepts caller-provided output samples, allocate the output
//!     texture with `D3D11_RESOURCE_MISC_SHARED_NTHANDLE`, `CreateSharedHandle`, and open
//!     it on the D3D12 device with `OpenSharedHandle` — zero copy.
//!  4. Plan B: otherwise `CopySubresourceRegion` into a shared texture inside D3D11 (a
//!     GPU-side copy, still on-chip), then open that on D3D12.
//!  5. Sync with a shared fence (`ID3D11Device5::CreateFence(D3D11_FENCE_FLAG_SHARED)` /
//!     `ID3D12Device::OpenSharedHandle`).
//!  6. Wrap the D3D12 texture as a `wgpu::Texture` via `wgpu-hal`'s dx12 backend.
//!  7. Run yuv2rgb → rgb2yuv (identity) into a D3D12-owned shared NV12 texture.
//!  8. Open it on D3D11, wrap it in an `IMFSample`, push it through a hardware encoder
//!     MFT, write the Annex B stream to a file.
//!  9. Capture with PIX and confirm no `Map` / `ReadFromSubresource` / readback-heap copy.
//! 10. Record plan A/B outcome and API versions in docs/INTEROP-WIN.md.
//!
//! Until M1 lands this binary only states that.

fn main() {
    eprintln!("interop_poc: not implemented yet (M1, ROADMAP RM-001). See the module docs.");
    std::process::exit(3);
}
