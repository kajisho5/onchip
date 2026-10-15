//! GPU-resident frame representation.

use std::any::Any;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::color::ColorSpec;

/// Pixel layout of a [`GpuFrame`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PixelFormat {
    /// 8-bit bi-planar 4:2:0: plane 0 = Y (`R8Unorm`), plane 1 = CbCr (`Rg8Unorm`).
    Nv12,
    /// 10-bit-in-16 bi-planar 4:2:0: plane 0 = Y (`R16Unorm`), plane 1 = CbCr (`Rg16Unorm`).
    P010,
    /// Internal working format: one plane of premultiplied linear `Rgba16Float`.
    Rgba16Float,
}

impl PixelFormat {
    pub fn is_biplanar_yuv(self) -> bool {
        matches!(self, PixelFormat::Nv12 | PixelFormat::P010)
    }

    /// Coded bit depth of a Y'CbCr format.
    pub fn coded_bits(self) -> u32 {
        match self {
            PixelFormat::Nv12 => 8,
            PixelFormat::P010 => 10,
            PixelFormat::Rgba16Float => 16,
        }
    }

    /// Storage word width per sample of a Y'CbCr format.
    pub fn word_bits(self) -> u32 {
        match self {
            PixelFormat::Nv12 => 8,
            PixelFormat::P010 => 16,
            PixelFormat::Rgba16Float => 16,
        }
    }

    /// wgpu formats of each plane when the planes are separate textures.
    pub fn plane_formats(self) -> Vec<wgpu::TextureFormat> {
        match self {
            PixelFormat::Nv12 => vec![wgpu::TextureFormat::R8Unorm, wgpu::TextureFormat::Rg8Unorm],
            PixelFormat::P010 => {
                vec![
                    wgpu::TextureFormat::R16Unorm,
                    wgpu::TextureFormat::Rg16Unorm,
                ]
            }
            PixelFormat::Rgba16Float => vec![wgpu::TextureFormat::Rgba16Float],
        }
    }

    /// wgpu multi-planar format when both planes live in one texture (D3D12 / Vulkan
    /// imports). `None` for formats that have no interleaved representation.
    pub fn interleaved_format(self) -> Option<wgpu::TextureFormat> {
        match self {
            PixelFormat::Nv12 => Some(wgpu::TextureFormat::NV12),
            PixelFormat::P010 => Some(wgpu::TextureFormat::P010),
            PixelFormat::Rgba16Float => None,
        }
    }
}

/// Opaque handle to the native resource backing a frame (a `CVPixelBuffer`,
/// an `ID3D11Texture2D`, ...). Kept alive for as long as the frame is, so the
/// wrapped `wgpu::Texture`s stay valid. The PNG stand-in uses [`NativeHandle::None`].
#[derive(Clone)]
pub enum NativeHandle {
    None,
    Opaque(Arc<dyn Any + Send + Sync>),
}

impl std::fmt::Debug for NativeHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NativeHandle::None => write!(f, "NativeHandle::None"),
            NativeHandle::Opaque(_) => write!(f, "NativeHandle::Opaque(..)"),
        }
    }
}

/// How the planes of a frame are stored.
#[derive(Debug)]
pub enum PlaneSet {
    /// One `wgpu::Texture` per plane (`[Y, CbCr]` for NV12 / P010).
    Separate(Vec<wgpu::Texture>),
    /// One multi-planar texture (`TextureFormat::NV12` / `P010`); planes are addressed
    /// with `TextureAspect::Plane0` / `Plane1`. This is what a D3D12 or Vulkan import
    /// of a decoder / encoder surface looks like.
    Interleaved(wgpu::Texture),
}

/// A frame whose pixels live in GPU-accessible memory.
#[derive(Debug)]
pub struct GpuFrame {
    pub planes: PlaneSet,
    pub format: PixelFormat,
    pub color: ColorSpec,
    pub width: u32,
    pub height: u32,
    /// Presentation timestamp in `timebase` units.
    pub pts: i64,
    /// Keeps the native resource alive while the frame exists.
    pub native: NativeHandle,
}

impl GpuFrame {
    /// A sampled view of plane `idx` with that plane's single-plane format
    /// (`R8Unorm` for Y, `Rg8Unorm` for CbCr, ...), whichever way the planes are stored.
    pub fn plane_view(&self, idx: usize) -> wgpu::TextureView {
        match &self.planes {
            PlaneSet::Separate(planes) => {
                planes[idx].create_view(&wgpu::TextureViewDescriptor::default())
            }
            PlaneSet::Interleaved(tex) => {
                let aspect = plane_aspect(idx);
                tex.create_view(&wgpu::TextureViewDescriptor {
                    label: None,
                    format: Some(self.format.plane_formats()[idx]),
                    dimension: Some(wgpu::TextureViewDimension::D2),
                    usage: None,
                    aspect,
                    base_mip_level: 0,
                    mip_level_count: None,
                    base_array_layer: 0,
                    array_layer_count: None,
                })
            }
        }
    }

    /// Where to copy plane `idx` into (texture + aspect), for `copy_buffer_to_texture`.
    pub fn plane_copy_target(&self, idx: usize) -> (&wgpu::Texture, wgpu::TextureAspect) {
        match &self.planes {
            PlaneSet::Separate(planes) => (&planes[idx], wgpu::TextureAspect::All),
            PlaneSet::Interleaved(tex) => (tex, plane_aspect(idx)),
        }
    }

    /// Size in texels of plane `idx` (chroma planes of 4:2:0 are half size).
    pub fn plane_size(&self, idx: usize) -> (u32, u32) {
        if idx == 0 || !self.format.is_biplanar_yuv() {
            (self.width, self.height)
        } else {
            (self.width.div_ceil(2), self.height.div_ceil(2))
        }
    }

    pub fn plane_count(&self) -> usize {
        self.format.plane_formats().len()
    }
}

pub fn plane_aspect(idx: usize) -> wgpu::TextureAspect {
    match idx {
        0 => wgpu::TextureAspect::Plane0,
        1 => wgpu::TextureAspect::Plane1,
        _ => wgpu::TextureAspect::Plane2,
    }
}
