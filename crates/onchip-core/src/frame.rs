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

    /// wgpu formats of each plane.
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

/// A frame whose pixels live in GPU-accessible memory.
#[derive(Debug)]
pub struct GpuFrame {
    /// One `wgpu::Texture` per plane. NV12 / P010 have `[Y, CbCr]`.
    pub planes: Vec<wgpu::Texture>,
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
    pub fn plane_view(&self, idx: usize) -> wgpu::TextureView {
        self.planes[idx].create_view(&wgpu::TextureViewDescriptor::default())
    }
}
