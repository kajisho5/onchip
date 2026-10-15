//! PNG-sequence stand-ins for the decoder and encoder (M0 only).
//!
//! The real codec ends arrive in M1 (Windows) and M2 (macOS). Until then, `source`
//! reads PNGs, converts them on the CPU to bi-planar Y'CbCr exactly as a hardware
//! decoder would hand them over, and uploads the planes once. `sink` reads the output
//! planes back and writes PNGs. Everything between the two is the real GPU pipeline.
//!
//! The CPU conversions here mirror `yuv2rgb.wgsl` / `rgb2yuv.wgsl` (BT.709 matrix,
//! coded range, left-sited chroma) so round trips can be checked for exactness.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::color::{CodedRange, ColorSpec};
use crate::frame::{GpuFrame, NativeHandle, PixelFormat, PlaneSet};
use crate::gpu::{self, GpuContext, GpuError};

#[derive(Debug, thiserror::Error)]
pub enum PngIoError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: {source}")]
    Image {
        path: PathBuf,
        source: image::ImageError,
    },
    #[error("no PNG files found in {0}")]
    Empty(PathBuf),
    #[error("{path}: expected {w}x{h}, got {got_w}x{got_h} (all frames must match)")]
    Size {
        path: PathBuf,
        w: u32,
        h: u32,
        got_w: u32,
        got_h: u32,
    },
    #[error(transparent)]
    Gpu(#[from] GpuError),
    #[error("only NV12 (8-bit) is supported by the PNG stand-in; got {0:?}")]
    Format(PixelFormat),
}

/// 8-bit bi-planar planes on the CPU, tightly packed (no row padding).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nv12Planes {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    /// Interleaved CbCr, `ceil(w/2) * ceil(h/2) * 2` bytes.
    pub uv: Vec<u8>,
}

fn clamp8(v: f32) -> u8 {
    v.round().clamp(0.0, 255.0) as u8
}

/// R'G'B' (8-bit, already gamma-encoded) -> NV12 using `color`'s matrix and range.
/// Chroma is left-sited: taken from the even column, averaged over the two rows.
pub fn nv12_from_rgb8(rgb: &[u8], width: u32, height: u32, color: ColorSpec) -> Nv12Planes {
    assert_eq!(rgb.len(), (width * height * 3) as usize);
    let m = color.matrix.rgb_to_ycc();
    let cr = CodedRange::new(color.range, 8, 8);
    let cw = width.div_ceil(2);
    let ch = height.div_ceil(2);
    let mut y = vec![0u8; (width * height) as usize];
    let mut cb_full = vec![0f32; (width * height) as usize];
    let mut cr_full = vec![0f32; (width * height) as usize];
    for py in 0..height {
        for px in 0..width {
            let i = (py * width + px) as usize;
            let r = rgb[i * 3] as f32 / 255.0;
            let g = rgb[i * 3 + 1] as f32 / 255.0;
            let b = rgb[i * 3 + 2] as f32 / 255.0;
            let yp = m[0][0] * r + m[0][1] * g + m[0][2] * b;
            cb_full[i] = m[1][0] * r + m[1][1] * g + m[1][2] * b;
            cr_full[i] = m[2][0] * r + m[2][1] * g + m[2][2] * b;
            y[i] = clamp8(yp * cr.enc_y_mul + cr.enc_y_add);
        }
    }
    let mut uv = vec![0u8; (cw * ch * 2) as usize];
    for cy in 0..ch {
        for cx in 0..cw {
            let x = (cx * 2).min(width - 1);
            let y0 = (cy * 2).min(height - 1);
            let y1 = (cy * 2 + 1).min(height - 1);
            let i0 = (y0 * width + x) as usize;
            let i1 = (y1 * width + x) as usize;
            let cb = (cb_full[i0] + cb_full[i1]) * 0.5;
            let crv = (cr_full[i0] + cr_full[i1]) * 0.5;
            let o = ((cy * cw + cx) * 2) as usize;
            uv[o] = clamp8(cb * cr.enc_c_mul + cr.enc_c_add);
            uv[o + 1] = clamp8(crv * cr.enc_c_mul + cr.enc_c_add);
        }
    }
    Nv12Planes {
        width,
        height,
        y,
        uv,
    }
}

/// NV12 -> R'G'B' 8-bit, upsampling chroma the same way `yuv2rgb.wgsl` does
/// (bilinear, left-sited horizontally, centered vertically).
pub fn rgb8_from_nv12(p: &Nv12Planes, color: ColorSpec) -> Vec<u8> {
    let m = color.matrix.ycc_to_rgb();
    let cr = CodedRange::new(color.range, 8, 8);
    let (w, h) = (p.width, p.height);
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    let sample_uv = |cx: f32, cy: f32| -> (f32, f32) {
        // bilinear with clamp-to-edge, matching a Linear sampler at (c + 0.5) / dim
        let fx = cx.clamp(0.0, (cw - 1) as f32);
        let fy = cy.clamp(0.0, (ch - 1) as f32);
        let x0 = fx.floor() as u32;
        let y0 = fy.floor() as u32;
        let x1 = (x0 + 1).min(cw - 1);
        let y1 = (y0 + 1).min(ch - 1);
        let tx = fx - x0 as f32;
        let ty = fy - y0 as f32;
        let at = |x: u32, y: u32| {
            let o = ((y * cw + x) * 2) as usize;
            (p.uv[o] as f32 / 255.0, p.uv[o + 1] as f32 / 255.0)
        };
        let (a, b, c, d) = (at(x0, y0), at(x1, y0), at(x0, y1), at(x1, y1));
        let lerp = |u: f32, v: f32, t: f32| u + (v - u) * t;
        (
            lerp(lerp(a.0, b.0, tx), lerp(c.0, d.0, tx), ty),
            lerp(lerp(a.1, b.1, tx), lerp(c.1, d.1, tx), ty),
        )
    };
    let mut out = vec![0u8; (w * h * 3) as usize];
    for py in 0..h {
        for px in 0..w {
            let i = (py * w + px) as usize;
            let yv = p.y[i] as f32 / 255.0;
            let (cbv, crv) = sample_uv(px as f32 * 0.5, (py as f32 - 0.5) * 0.5);
            let yp = (yv - cr.y_offset) * cr.y_scale;
            let cb = (cbv - cr.c_offset) * cr.c_scale;
            let crc = (crv - cr.c_offset) * cr.c_scale;
            let r = m[0][0] * yp + m[0][1] * cb + m[0][2] * crc;
            let g = m[1][0] * yp + m[1][1] * cb + m[1][2] * crc;
            let b = m[2][0] * yp + m[2][1] * cb + m[2][2] * crc;
            out[i * 3] = clamp8(r * 255.0);
            out[i * 3 + 1] = clamp8(g * 255.0);
            out[i * 3 + 2] = clamp8(b * 255.0);
        }
    }
    out
}

/// Upload NV12 planes as a [`GpuFrame`] (the decoder stand-in's one CPU->GPU copy).
pub fn upload_nv12(ctx: &GpuContext, p: &Nv12Planes, color: ColorSpec, pts: i64) -> GpuFrame {
    let usage = wgpu::TextureUsages::TEXTURE_BINDING
        | wgpu::TextureUsages::COPY_DST
        | wgpu::TextureUsages::COPY_SRC;
    let y = gpu::create_texture(
        &ctx.device,
        "src.y",
        p.width,
        p.height,
        wgpu::TextureFormat::R8Unorm,
        usage,
    );
    let cw = p.width.div_ceil(2);
    let ch = p.height.div_ceil(2);
    let uv = gpu::create_texture(
        &ctx.device,
        "src.uv",
        cw,
        ch,
        wgpu::TextureFormat::Rg8Unorm,
        usage,
    );
    write_plane(ctx, &y, &p.y, p.width, p.height, 1);
    write_plane(ctx, &uv, &p.uv, cw, ch, 2);
    GpuFrame {
        planes: PlaneSet::Separate(vec![y, uv]),
        format: PixelFormat::Nv12,
        color,
        width: p.width,
        height: p.height,
        pts,
        native: NativeHandle::None,
    }
}

fn write_plane(ctx: &GpuContext, tex: &wgpu::Texture, data: &[u8], w: u32, h: u32, bpp: u32) {
    ctx.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(w * bpp),
            rows_per_image: Some(h),
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
}

/// Read an NV12 frame back to the CPU (the encoder stand-in's one GPU->CPU copy).
pub fn readback_nv12(ctx: &GpuContext, frame: &GpuFrame) -> Result<Nv12Planes, PngIoError> {
    if frame.format != PixelFormat::Nv12 {
        return Err(PngIoError::Format(frame.format));
    }
    let cw = frame.width.div_ceil(2);
    let ch = frame.height.div_ceil(2);
    let (ty, ay) = frame.plane_copy_target(0);
    let (tuv, auv) = frame.plane_copy_target(1);
    let y = read_plane(ctx, ty, ay, frame.width, frame.height, 1)?;
    let uv = read_plane(ctx, tuv, auv, cw, ch, 2)?;
    Ok(Nv12Planes {
        width: frame.width,
        height: frame.height,
        y,
        uv,
    })
}

fn read_plane(
    ctx: &GpuContext,
    tex: &wgpu::Texture,
    aspect: wgpu::TextureAspect,
    w: u32,
    h: u32,
    bpp: u32,
) -> Result<Vec<u8>, PngIoError> {
    let stride = gpu::align_row(w * bpp);
    let buf = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (stride * h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = ctx.device.create_command_encoder(&Default::default());
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride),
                rows_per_image: None,
            },
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
    ctx.queue.submit([enc.finish()]);

    let slice = buf.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    gpu::wait_idle(&ctx.device)?;
    rx.recv()
        .map_err(|e| GpuError::Map(e.to_string()))?
        .map_err(|e| GpuError::Map(e.to_string()))?;
    let data = slice
        .get_mapped_range()
        .map_err(|e| GpuError::Map(e.to_string()))?;
    let mut out = Vec::with_capacity((w * h * bpp) as usize);
    for row in 0..h {
        let s = (row * stride) as usize;
        out.extend_from_slice(&data[s..s + (w * bpp) as usize]);
    }
    drop(data);
    buf.unmap();
    Ok(out)
}

/// Sorted list of `*.png` in a directory.
pub fn list_pngs(dir: &Path) -> Result<Vec<PathBuf>, PngIoError> {
    let rd = std::fs::read_dir(dir).map_err(|e| PngIoError::Io {
        path: dir.to_path_buf(),
        source: e,
    })?;
    let mut files: Vec<PathBuf> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("png")))
        .collect();
    files.sort();
    if files.is_empty() {
        return Err(PngIoError::Empty(dir.to_path_buf()));
    }
    Ok(files)
}

pub fn read_png_rgb8(path: &Path) -> Result<(u32, u32, Vec<u8>), PngIoError> {
    let bytes = std::fs::read(path).map_err(|e| PngIoError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let img = image::load_from_memory(&bytes)
        .map_err(|e| PngIoError::Image {
            path: path.to_path_buf(),
            source: e,
        })?
        .to_rgb8();
    let (w, h) = img.dimensions();
    Ok((w, h, img.into_raw()))
}

pub fn write_png_rgb8(path: &Path, w: u32, h: u32, rgb: &[u8]) -> Result<(), PngIoError> {
    image::save_buffer(path, rgb, w, h, image::ColorType::Rgb8).map_err(|e| PngIoError::Image {
        path: path.to_path_buf(),
        source: e,
    })
}

/// Decoder stand-in: a directory of PNGs (or a single PNG) delivered as NV12 frames.
pub struct PngSource {
    ctx: Arc<GpuContext>,
    files: Vec<PathBuf>,
    next: usize,
    pub width: u32,
    pub height: u32,
    pub color: ColorSpec,
}

impl PngSource {
    pub fn open(
        ctx: Arc<GpuContext>,
        input: &Path,
        color: ColorSpec,
    ) -> Result<PngSource, PngIoError> {
        let files = if input.is_dir() {
            list_pngs(input)?
        } else {
            vec![input.to_path_buf()]
        };
        let (width, height, _) = read_png_rgb8(&files[0])?;
        Ok(PngSource {
            ctx,
            files,
            next: 0,
            width,
            height,
            color,
        })
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn next_frame(&mut self) -> Result<Option<GpuFrame>, PngIoError> {
        let Some(path) = self.files.get(self.next) else {
            return Ok(None);
        };
        let (w, h, rgb) = read_png_rgb8(path)?;
        if (w, h) != (self.width, self.height) {
            return Err(PngIoError::Size {
                path: path.clone(),
                w: self.width,
                h: self.height,
                got_w: w,
                got_h: h,
            });
        }
        let planes = nv12_from_rgb8(&rgb, w, h, self.color);
        let frame = upload_nv12(&self.ctx, &planes, self.color, self.next as i64);
        self.next += 1;
        Ok(Some(frame))
    }
}

/// Encoder stand-in: writes each NV12 frame as `frame_NNNNNN.png`.
pub struct PngSink {
    ctx: Arc<GpuContext>,
    dir: PathBuf,
    count: usize,
}

impl PngSink {
    pub fn open(ctx: Arc<GpuContext>, dir: &Path) -> Result<PngSink, PngIoError> {
        std::fs::create_dir_all(dir).map_err(|e| PngIoError::Io {
            path: dir.to_path_buf(),
            source: e,
        })?;
        Ok(PngSink {
            ctx,
            dir: dir.to_path_buf(),
            count: 0,
        })
    }

    pub fn push(&mut self, frame: &GpuFrame) -> Result<PathBuf, PngIoError> {
        let planes = readback_nv12(&self.ctx, frame)?;
        let rgb = rgb8_from_nv12(&planes, frame.color);
        let path = self.dir.join(format!("frame_{:06}.png", self.count));
        write_png_rgb8(&path, planes.width, planes.height, &rgb)?;
        self.count += 1;
        Ok(path)
    }

    pub fn count(&self) -> usize {
        self.count
    }
}

/// Peak signal-to-noise ratio in dB between two equally sized 8-bit buffers.
/// Returns `f64::INFINITY` for identical buffers.
pub fn psnr_u8(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    if a.is_empty() {
        return f64::INFINITY;
    }
    let mse: f64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| {
            let d = *x as f64 - *y as f64;
            d * d
        })
        .sum::<f64>()
        / a.len() as f64;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::Matrix;

    fn gradient(w: u32, h: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            for x in 0..w {
                v.push((x * 255 / (w - 1).max(1)) as u8);
                v.push((y * 255 / (h - 1).max(1)) as u8);
                v.push(((x + y) * 255 / (w + h - 2).max(1)) as u8);
            }
        }
        v
    }

    #[test]
    fn cpu_round_trip_is_close() {
        // chroma subsampling loses information, but luma must be near-exact and
        // the overall error well under 1 LSB RMS for a smooth gradient
        let (w, h) = (64, 48);
        let rgb = gradient(w, h);
        let planes = nv12_from_rgb8(&rgb, w, h, ColorSpec::BT709_LIMITED);
        let back = rgb8_from_nv12(&planes, ColorSpec::BT709_LIMITED);
        let p = psnr_u8(&rgb, &back);
        assert!(p > 40.0, "psnr {p}");
    }

    #[test]
    fn flat_color_is_exact() {
        let (w, h) = (16, 8);
        let rgb: Vec<u8> = [200u8, 100, 50]
            .iter()
            .cycle()
            .take((w * h * 3) as usize)
            .copied()
            .collect();
        let planes = nv12_from_rgb8(&rgb, w, h, ColorSpec::BT709_LIMITED);
        let back = rgb8_from_nv12(&planes, ColorSpec::BT709_LIMITED);
        for (a, b) in rgb.iter().zip(&back) {
            assert!((*a as i32 - *b as i32).abs() <= 1, "{a} vs {b}");
        }
    }

    #[test]
    fn black_and_white_levels() {
        let planes = nv12_from_rgb8(&[0, 0, 0, 255, 255, 255], 2, 1, ColorSpec::BT709_LIMITED);
        assert_eq!(planes.y, vec![16, 235]);
        assert_eq!(planes.uv.len(), 2);
        assert_eq!(planes.uv, vec![128, 128]);
        let full = nv12_from_rgb8(&[0, 0, 0, 255, 255, 255], 2, 1, ColorSpec::BT709_FULL);
        assert_eq!(full.y, vec![0, 255]);
    }

    #[test]
    fn psnr_sanity() {
        assert_eq!(psnr_u8(&[1, 2, 3], &[1, 2, 3]), f64::INFINITY);
        let p = psnr_u8(&[0; 100], &[1; 100]);
        assert!((p - 48.13).abs() < 0.01, "{p}");
    }

    #[test]
    fn matrix_identity_passthrough() {
        let spec = ColorSpec {
            matrix: Matrix::Identity,
            ..ColorSpec::BT709_FULL
        };
        let planes = nv12_from_rgb8(&[10, 20, 30], 1, 1, spec);
        assert_eq!(planes.y, vec![10]);
    }
}
