//! D3D11 ⇄ D3D12 ⇄ wgpu plumbing: the device pair, shared NV12 surfaces, and the
//! shared fence used to order work between the two APIs.
//!
//! Facts about the design (see docs/DESIGN.md §4.2):
//! - Media Foundation hardware MFTs speak D3D11. wgpu speaks D3D12. The two devices are
//!   created on the same adapter (matched by LUID) so shared NT handles work.
//! - A surface is one `ID3D11Texture2D` in NV12 with `SHARED | SHARED_NTHANDLE`, opened
//!   on the D3D12 device as an `ID3D12Resource` and wrapped as a `wgpu::Texture` of
//!   format `NV12`. The same GPU memory is visible to both APIs; nothing is copied.
//! - Ordering: D3D11 work is signalled on a shared fence and the CPU waits on it before
//!   the wgpu work that reads the surface is submitted; wgpu work is waited on with
//!   `device.poll(wait)` before D3D11 reads the surface. These are CPU-side waits, not
//!   readbacks — no pixel data moves. GPU-side waits are ROADMAP RM-002.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use onchip_core::backend::BackendError;
use onchip_core::frame::{GpuFrame, NativeHandle, PixelFormat, PlaneSet};
use onchip_core::{ColorSpec, GpuContext};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_FENCE_FLAG_SHARED, D3D11_RESOURCE_MISC_SHARED,
    D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11CreateDevice, ID3D11Device, ID3D11Device5, ID3D11DeviceContext,
    ID3D11DeviceContext4, ID3D11Fence, ID3D11Multithread, ID3D11Resource, ID3D11Texture2D,
};
use windows::Win32::Graphics::Direct3D12::{ID3D12Device, ID3D12Fence, ID3D12Resource};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIAdapter,
    IDXGIFactory4, IDXGIResource1,
};
use windows::Win32::Media::MediaFoundation::{IMFDXGIDeviceManager, MFCreateDXGIDeviceManager};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::Interface;

pub fn werr(what: &str, e: windows::core::Error) -> BackendError {
    BackendError::Platform(format!("{what}: {e}"))
}

/// Which interop path a surface pool is using. Reported in `onchip probe` and the
/// verification log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteropPath {
    /// Plan A: the MFT writes straight into our shared texture — zero copy.
    SharedHandleZeroCopy,
    /// Plan B: the MFT owns its output texture; we copy GPU→GPU into a shared one.
    GpuCopyIntoShared,
}

/// The D3D11 side plus the handles into wgpu's D3D12 device.
pub struct D3dShared {
    pub ctx: Arc<GpuContext>,
    pub device11: ID3D11Device,
    pub device11_5: ID3D11Device5,
    /// The immediate context is not thread safe; all use goes through this lock.
    pub context11: Mutex<ID3D11DeviceContext4>,
    pub dxgi_manager: IMFDXGIDeviceManager,
    pub reset_token: u32,
    pub device12: ID3D12Device,
    fence11: ID3D11Fence,
    fence12: ID3D12Fence,
    fence_value: AtomicU64,
    fence_event: HANDLE,
    pub adapter_luid: i64,
}

unsafe impl Send for D3dShared {}
unsafe impl Sync for D3dShared {}

impl Drop for D3dShared {
    fn drop(&mut self) {
        // SAFETY: the event was created by CreateEventW in `new` and is not used after this.
        unsafe {
            let _ = CloseHandle(self.fence_event);
        }
    }
}

impl D3dShared {
    /// Build the D3D11 device on the same adapter as wgpu's D3D12 device.
    pub fn new(ctx: Arc<GpuContext>) -> Result<Arc<D3dShared>, BackendError> {
        if !ctx.supports_nv12_textures() {
            return Err(BackendError::Unavailable(
                "this adapter / wgpu backend does not offer NV12 textures (Features::TEXTURE_FORMAT_NV12)",
            ));
        }
        // wgpu's D3D12 device and adapter LUID
        // SAFETY: as_hal returns a guard over wgpu's own objects; we only clone COM pointers.
        let (device12, luid) =
            unsafe {
                let dev = ctx.device.as_hal::<wgpu::hal::api::Dx12>().ok_or(
                    BackendError::Unavailable("wgpu is not running on the D3D12 backend"),
                )?;
                let device12: ID3D12Device = dev.raw_device().clone();
                let luid = device12.GetAdapterLuid();
                (
                    device12,
                    ((luid.HighPart as i64) << 32) | luid.LowPart as i64,
                )
            };

        // Find the same adapter through DXGI
        // SAFETY: plain DXGI factory calls with valid out-pointers.
        let adapter: IDXGIAdapter = unsafe {
            let factory: IDXGIFactory4 =
                CreateDXGIFactory1().map_err(|e| werr("CreateDXGIFactory1", e))?;
            let luid_struct = windows::Win32::Foundation::LUID {
                LowPart: luid as u32,
                HighPart: (luid >> 32) as i32,
            };
            factory
                .EnumAdapterByLuid::<IDXGIAdapter>(luid_struct)
                .map_err(|e| werr("EnumAdapterByLuid", e))?
        };

        // D3D11 device with video support
        let mut device11: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        // SAFETY: standard device creation; out-pointers are valid.
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                windows::Win32::Foundation::HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1]),
                D3D11_SDK_VERSION,
                Some(&mut device11),
                None,
                Some(&mut context),
            )
            .map_err(|e| werr("D3D11CreateDevice", e))?;
        }
        let device11 = device11.ok_or(BackendError::Platform(
            "D3D11CreateDevice gave no device".into(),
        ))?;
        let context = context.ok_or(BackendError::Platform(
            "D3D11CreateDevice gave no context".into(),
        ))?;
        let device11_5 = device11
            .cast::<ID3D11Device5>()
            .map_err(|e| werr("ID3D11Device5", e))?;
        let context4 = context
            .cast::<ID3D11DeviceContext4>()
            .map_err(|e| werr("ID3D11DeviceContext4", e))?;
        let mt = device11
            .cast::<ID3D11Multithread>()
            .map_err(|e| werr("ID3D11Multithread", e))?;
        // Media Foundation uses the device from its own threads.
        // SAFETY: valid interface.
        unsafe {
            let _ = mt.SetMultithreadProtected(true);
        }

        // DXGI device manager for the MFTs
        let mut reset_token = 0u32;
        let mut mgr: Option<IMFDXGIDeviceManager> = None;
        // SAFETY: valid out-pointers; ResetDevice takes the device as IUnknown.
        unsafe {
            MFCreateDXGIDeviceManager(&mut reset_token, &mut mgr)
                .map_err(|e| werr("MFCreateDXGIDeviceManager", e))?;
        }
        let dxgi_manager = mgr.ok_or(BackendError::Platform("no DXGI device manager".into()))?;
        // SAFETY: device11 outlives the manager (both owned here).
        unsafe {
            dxgi_manager
                .ResetDevice(&device11, reset_token)
                .map_err(|e| werr("IMFDXGIDeviceManager::ResetDevice", e))?;
        }

        // Shared fence: created on D3D11, opened on D3D12
        let mut fence11: Option<ID3D11Fence> = None;
        // SAFETY: valid out-pointer.
        unsafe {
            device11_5
                .CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence11)
                .map_err(|e| werr("ID3D11Device5::CreateFence", e))?;
        }
        let fence11 = fence11.ok_or(BackendError::Platform("no D3D11 fence".into()))?;
        // SAFETY: the handle is closed right after the D3D12 side opened it.
        let fence12: ID3D12Fence = unsafe {
            let h = fence11
                .CreateSharedHandle(None, 0x1000_0000 /* GENERIC_ALL */, None)
                .map_err(|e| werr("ID3D11Fence::CreateSharedHandle", e))?;
            let mut f: Option<ID3D12Fence> = None;
            let r = device12.OpenSharedHandle(h, &mut f);
            let _ = CloseHandle(h);
            r.map_err(|e| werr("ID3D12Device::OpenSharedHandle(fence)", e))?;
            f.ok_or(BackendError::Platform("no D3D12 fence".into()))?
        };
        // SAFETY: anonymous auto-reset event.
        let fence_event =
            unsafe { CreateEventW(None, false, false, None).map_err(|e| werr("CreateEventW", e))? };

        Ok(Arc::new(D3dShared {
            ctx,
            device11,
            device11_5,
            context11: Mutex::new(context4),
            dxgi_manager,
            reset_token,
            device12,
            fence11,
            fence12,
            fence_value: AtomicU64::new(0),
            fence_event,
            adapter_luid: luid,
        }))
    }

    /// Signal the shared fence from the D3D11 queue and block the CPU until the GPU has
    /// passed it. Call after D3D11 work that produces data wgpu will read.
    pub fn d3d11_signal_and_wait(&self) -> Result<(), BackendError> {
        let v = self.fence_value.fetch_add(1, Ordering::SeqCst) + 1;
        {
            let ctx = self.context11.lock().expect("context lock");
            // SAFETY: fence and context are live; Signal enqueues on the immediate context.
            unsafe {
                ctx.Signal(&self.fence11, v)
                    .map_err(|e| werr("ID3D11DeviceContext4::Signal", e))?;
                ctx.Flush();
            }
        }
        // SAFETY: event handle is owned by us; the D3D12 view of the fence sees D3D11 signals.
        unsafe {
            if self.fence12.GetCompletedValue() < v {
                self.fence12
                    .SetEventOnCompletion(v, self.fence_event)
                    .map_err(|e| werr("ID3D12Fence::SetEventOnCompletion", e))?;
                let r = WaitForSingleObject(self.fence_event, 10_000);
                if r != WAIT_OBJECT_0 {
                    return Err(BackendError::Platform(format!(
                        "D3D11→D3D12 fence wait for value {v} returned {r:?} (completed {})",
                        self.fence12.GetCompletedValue()
                    )));
                }
            }
        }
        Ok(())
    }

    /// Block until all wgpu work submitted so far has finished. Call before D3D11 reads
    /// a surface wgpu wrote.
    pub fn wgpu_wait_idle(&self) -> Result<(), BackendError> {
        onchip_core::gpu::wait_idle(&self.ctx.device)
            .map_err(|e| BackendError::Interop(e.to_string()))
    }

    /// Create one shared NV12 surface: D3D11 texture + D3D12 resource + wgpu texture.
    pub fn create_shared_surface(
        self: &Arc<Self>,
        width: u32,
        height: u32,
        role: SurfaceRole,
    ) -> Result<Arc<SharedSurface>, BackendError> {
        // Decoder-output surfaces are plan-B copy targets (and plan-A decode targets when
        // the MFT lets us supply them); encoder-input surfaces are read by the encoder.
        // Keep bind flags minimal: NV12 + SHARED_NTHANDLE is picky on some drivers.
        let bind = match role {
            SurfaceRole::DecoderOutput | SurfaceRole::EncoderInput => {
                D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0
            }
        };
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind as u32,
            CPUAccessFlags: 0,
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0)
                as u32,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        // SAFETY: desc is fully initialised; out-pointer valid.
        unsafe {
            self.device11
                .CreateTexture2D(&desc, None, Some(&mut tex))
                .map_err(|e| werr("CreateTexture2D(NV12 shared)", e))?;
        }
        let tex11 = tex.ok_or(BackendError::Platform(
            "CreateTexture2D gave no texture".into(),
        ))?;

        // SAFETY: shared handle created from a SHARED_NTHANDLE resource; opened on D3D12.
        let (handle, res12) = unsafe {
            let dxgi: IDXGIResource1 = tex11.cast().map_err(|e| werr("IDXGIResource1", e))?;
            let h = dxgi
                .CreateSharedHandle(
                    None,
                    DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                    None,
                )
                .map_err(|e| werr("IDXGIResource1::CreateSharedHandle", e))?;
            let mut r: Option<ID3D12Resource> = None;
            self.device12
                .OpenSharedHandle(h, &mut r)
                .map_err(|e| werr("ID3D12Device::OpenSharedHandle(texture)", e))?;
            (
                h,
                r.ok_or(BackendError::Platform(
                    "OpenSharedHandle gave no resource".into(),
                ))?,
            )
        };

        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        // SAFETY: the resource was created by D3D12 on wgpu's device, with the stated
        // format/size, and is kept alive by `SharedSurface` for as long as the wgpu
        // texture exists.
        let wgpu_tex = unsafe {
            let hal_tex = wgpu::hal::dx12::Device::texture_from_raw(
                res12.clone(),
                wgpu::TextureFormat::NV12,
                wgpu::TextureDimension::D2,
                size,
                1,
                1,
            );
            self.ctx
                .device
                .create_texture_from_hal::<wgpu::hal::api::Dx12>(
                    hal_tex,
                    &wgpu::TextureDescriptor {
                        label: Some("shared NV12 surface"),
                        size,
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: wgpu::TextureFormat::NV12,
                        usage: wgpu::TextureUsages::COPY_DST
                            | wgpu::TextureUsages::COPY_SRC
                            | wgpu::TextureUsages::TEXTURE_BINDING,
                        view_formats: &[],
                    },
                    wgpu::TextureUses::UNINITIALIZED,
                )
        };

        Ok(Arc::new(SharedSurface {
            d3d: self.clone(),
            tex11,
            handle,
            res12,
            wgpu_tex,
            width,
            height,
            role,
        }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceRole {
    DecoderOutput,
    EncoderInput,
}

/// One NV12 surface visible to D3D11, D3D12 and wgpu at the same time.
pub struct SharedSurface {
    pub d3d: Arc<D3dShared>,
    pub tex11: ID3D11Texture2D,
    handle: HANDLE,
    pub res12: ID3D12Resource,
    pub wgpu_tex: wgpu::Texture,
    pub width: u32,
    pub height: u32,
    pub role: SurfaceRole,
}

unsafe impl Send for SharedSurface {}
unsafe impl Sync for SharedSurface {}

impl Drop for SharedSurface {
    fn drop(&mut self) {
        // SAFETY: the NT handle is ours; the COM objects release themselves.
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

impl SharedSurface {
    pub fn as_resource(&self) -> ID3D11Resource {
        self.tex11
            .cast()
            .expect("ID3D11Texture2D is an ID3D11Resource")
    }

    /// Wrap this surface as a [`GpuFrame`]. The frame keeps the surface alive through
    /// its native handle; `guard` (if any) is released when the frame is dropped.
    pub fn frame(
        self: &Arc<Self>,
        color: ColorSpec,
        pts: i64,
        guard: Option<Arc<dyn std::any::Any + Send + Sync>>,
    ) -> GpuFrame {
        let keep: Arc<dyn std::any::Any + Send + Sync> = match guard {
            Some(g) => Arc::new((self.clone(), g)),
            None => Arc::new(self.clone()),
        };
        GpuFrame {
            planes: PlaneSet::Interleaved(self.wgpu_tex.clone()),
            format: PixelFormat::Nv12,
            color,
            width: self.width,
            height: self.height,
            pts,
            native: NativeHandle::Opaque(keep),
        }
    }
}

/// Recover the `SharedSurface` behind a frame produced by this backend.
pub fn surface_of(frame: &GpuFrame) -> Option<Arc<SharedSurface>> {
    match &frame.native {
        NativeHandle::Opaque(any) => {
            if let Some(s) = any.downcast_ref::<Arc<SharedSurface>>() {
                return Some(s.clone());
            }
            // (surface, guard) pairs
            if let Some((s, _)) =
                any.downcast_ref::<(Arc<SharedSurface>, Arc<dyn std::any::Any + Send + Sync>)>()
            {
                return Some(s.clone());
            }
            None
        }
        NativeHandle::None => None,
    }
}
