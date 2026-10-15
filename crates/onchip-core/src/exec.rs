//! Executes a validated [`Graph`] on the GPU, one frame per command buffer.
//!
//! All intermediate textures are allocated once, when the executor is built, because
//! every size in the graph is static. Per frame we only build bind groups (the source
//! planes change) and record one command buffer.

use std::collections::HashMap;
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};

use crate::color::{CodedRange, ColorSpec};
use crate::frame::{GpuFrame, NativeHandle, PixelFormat, PlaneSet};
use crate::gpu::{self, GpuContext, GpuError, Pipelines};
use crate::graph::Graph;
use crate::job::Op;

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error(transparent)]
    Gpu(#[from] GpuError),
    #[error("image node {id:?}: {source}")]
    Image { id: String, source: ImageError },
    #[error("frame is {got:?} but the executor was built for {expected:?}")]
    FormatMismatch {
        got: PixelFormat,
        expected: PixelFormat,
    },
    #[error("frame is {got_w}x{got_h} but the executor was built for {w}x{h}")]
    SizeMismatch {
        got_w: u32,
        got_h: u32,
        w: u32,
        h: u32,
    },
    #[error("pixel format {0:?} is not supported by this adapter (16-bit norm textures)")]
    UnsupportedFormat(PixelFormat),
}

#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("cannot decode {path}: {source}")]
    Decode {
        path: String,
        source: image::ImageError,
    },
}

/// A decoded overlay image in the working format (premultiplied linear RGBA16F).
pub struct WorkingImage {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` half floats, row-major.
    pub rgba16f: Vec<u16>,
}

impl WorkingImage {
    /// Load a PNG (treated as sRGB-encoded, straight alpha) into the working format.
    pub fn load_png(path: &str) -> Result<WorkingImage, ImageError> {
        let bytes = std::fs::read(path).map_err(|e| ImageError::Io {
            path: path.to_string(),
            source: e,
        })?;
        let img = image::load_from_memory(&bytes)
            .map_err(|e| ImageError::Decode {
                path: path.to_string(),
                source: e,
            })?
            .to_rgba8();
        let (width, height) = img.dimensions();
        let mut rgba16f = Vec::with_capacity((width * height * 4) as usize);
        for px in img.pixels() {
            let a = px[3] as f32 / 255.0;
            for c in 0..3 {
                let enc = px[c] as f32 / 255.0;
                let lin = srgb_to_linear(enc) * a; // premultiply
                rgba16f.push(gpu::f32_to_f16_bits(lin));
            }
            rgba16f.push(gpu::f32_to_f16_bits(a));
        }
        Ok(WorkingImage {
            width,
            height,
            rgba16f,
        })
    }

    pub fn from_rgba_f32(width: u32, height: u32, rgba: &[f32]) -> WorkingImage {
        WorkingImage {
            width,
            height,
            rgba16f: rgba.iter().map(|v| gpu::f32_to_f16_bits(*v)).collect(),
        }
    }
}

pub fn srgb_to_linear(x: f32) -> f32 {
    if x <= 0.04045 {
        x / 12.92
    } else {
        ((x + 0.055) / 1.055).powf(2.4)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Yuv2RgbParams {
    m0: [f32; 4],
    m1: [f32; 4],
    m2: [f32; 4],
    y_offset: f32,
    y_scale: f32,
    c_offset: f32,
    c_scale: f32,
    width: u32,
    height: u32,
    transfer: u32,
    _pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ScaleParams {
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
    kernel_id: u32,
    _pad: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct OverlayParams {
    x: i32,
    y: i32,
    fg_w: u32,
    fg_h: u32,
    out_w: u32,
    out_h: u32,
    opacity: f32,
    _pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Rgb2YuvParams {
    m0: [f32; 4],
    m1: [f32; 4],
    m2: [f32; 4],
    y_mul: f32,
    y_add: f32,
    c_mul: f32,
    c_add: f32,
    max_code: f32,
    shift: u32,
    width: u32,
    height: u32,
    y_stride_words: u32,
    uv_stride_words: u32,
    bits16: u32,
    transfer: u32,
}

fn row4(r: [f32; 3]) -> [f32; 4] {
    [r[0], r[1], r[2], 0.0]
}

/// Per-node GPU resources.
enum NodeRes {
    /// Source planes come from the frame; nothing to allocate.
    Source,
    Working {
        tex: wgpu::Texture,
        width: u32,
        height: u32,
        params: wgpu::Buffer,
    },
    Image {
        tex: wgpu::Texture,
    },
    Rgb2Yuv {
        y_buf: wgpu::Buffer,
        uv_buf: wgpu::Buffer,
        y_stride: u32,
        uv_stride: u32,
        params: wgpu::Buffer,
        width: u32,
        height: u32,
    },
    Sink,
}

/// Output plane layout of the sink, for anything that needs to read it.
#[derive(Debug, Clone, Copy)]
pub struct PlaneLayout {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
}

pub struct Executor {
    ctx: Arc<GpuContext>,
    pipelines: Pipelines,
    graph: Graph,
    res: HashMap<String, NodeRes>,
    src_w: u32,
    src_h: u32,
    src_format: PixelFormat,
    out_w: u32,
    out_h: u32,
    frames_done: u64,
}

impl Executor {
    /// Build an executor for frames of `src_w x src_h` in `src_format` with the source
    /// color the graph was validated against.
    pub fn new(
        ctx: Arc<GpuContext>,
        graph: Graph,
        src_w: u32,
        src_h: u32,
        src_format: PixelFormat,
    ) -> Result<Executor, ExecError> {
        if src_format == PixelFormat::P010 && !ctx.supports_p010() {
            return Err(ExecError::UnsupportedFormat(src_format));
        }
        let device = &ctx.device;
        let pipelines = Pipelines::new(device);
        let source_color = graph.colors[&graph.source];
        let out_color = graph.output_color;

        let mut res: HashMap<String, NodeRes> = HashMap::new();
        let mut sizes: HashMap<String, (u32, u32)> = HashMap::new();
        let mut out_w = src_w;
        let mut out_h = src_h;

        for id in &graph.order {
            let node = graph.node(id);
            let r = match &node.op {
                Op::Source => {
                    sizes.insert(id.clone(), (src_w, src_h));
                    NodeRes::Source
                }
                Op::Yuv2Rgb => {
                    let (w, h) = sizes[&node.inputs[0]];
                    sizes.insert(id.clone(), (w, h));
                    let cr = CodedRange::new(
                        source_color.range,
                        src_format.coded_bits(),
                        src_format.word_bits(),
                    );
                    let m = source_color.matrix.ycc_to_rgb();
                    let p = Yuv2RgbParams {
                        m0: row4(m[0]),
                        m1: row4(m[1]),
                        m2: row4(m[2]),
                        y_offset: cr.y_offset,
                        y_scale: cr.y_scale,
                        c_offset: cr.c_offset,
                        c_scale: cr.c_scale,
                        width: w,
                        height: h,
                        transfer: source_color.transfer.id(),
                        _pad: 0,
                    };
                    NodeRes::Working {
                        tex: gpu::create_working_texture(device, id, w, h),
                        width: w,
                        height: h,
                        params: uniform(device, id, bytemuck::bytes_of(&p)),
                    }
                }
                Op::Scale {
                    width,
                    height,
                    filter,
                } => {
                    let (sw, sh) = sizes[&node.inputs[0]];
                    sizes.insert(id.clone(), (*width, *height));
                    let p = ScaleParams {
                        src_w: sw,
                        src_h: sh,
                        dst_w: *width,
                        dst_h: *height,
                        kernel_id: filter.id(),
                        _pad: [0; 3],
                    };
                    NodeRes::Working {
                        tex: gpu::create_working_texture(device, id, *width, *height),
                        width: *width,
                        height: *height,
                        params: uniform(device, id, bytemuck::bytes_of(&p)),
                    }
                }
                Op::Image { path } => {
                    let img = WorkingImage::load_png(path).map_err(|e| ExecError::Image {
                        id: id.clone(),
                        source: e,
                    })?;
                    sizes.insert(id.clone(), (img.width, img.height));
                    NodeRes::Image {
                        tex: upload_working_image(&ctx, id, &img),
                    }
                }
                Op::Overlay { x, y, opacity } => {
                    let (bw, bh) = sizes[&node.inputs[0]];
                    let (fw, fh) = sizes[&node.inputs[1]];
                    sizes.insert(id.clone(), (bw, bh));
                    let p = OverlayParams {
                        x: *x,
                        y: *y,
                        fg_w: fw,
                        fg_h: fh,
                        out_w: bw,
                        out_h: bh,
                        opacity: *opacity as f32,
                        _pad: 0,
                    };
                    NodeRes::Working {
                        tex: gpu::create_working_texture(device, id, bw, bh),
                        width: bw,
                        height: bh,
                        params: uniform(device, id, bytemuck::bytes_of(&p)),
                    }
                }
                Op::Rgb2Yuv => {
                    let (w, h) = sizes[&node.inputs[0]];
                    sizes.insert(id.clone(), (w, h));
                    out_w = w;
                    out_h = h;
                    let out_format = src_format; // encoder gets what the decoder gave, M0
                    let bytes = out_format.word_bits() / 8;
                    let y_stride = gpu::align_row(w.div_ceil(4) * 4 * bytes);
                    let uv_stride = gpu::align_row(w.div_ceil(4) * 4 * bytes);
                    let rows = h.div_ceil(2) * 2;
                    let y_size = (y_stride * rows) as u64;
                    let uv_size = (uv_stride * h.div_ceil(2)) as u64;
                    let cr = CodedRange::new(
                        out_color.range,
                        out_format.coded_bits(),
                        out_format.word_bits(),
                    );
                    let m = out_color.matrix.rgb_to_ycc();
                    let p = Rgb2YuvParams {
                        m0: row4(m[0]),
                        m1: row4(m[1]),
                        m2: row4(m[2]),
                        y_mul: cr.enc_y_mul,
                        y_add: cr.enc_y_add,
                        c_mul: cr.enc_c_mul,
                        c_add: cr.enc_c_add,
                        max_code: cr.max_code,
                        shift: cr.shift,
                        width: w,
                        height: h,
                        y_stride_words: y_stride / 4,
                        uv_stride_words: uv_stride / 4,
                        bits16: (bytes == 2) as u32,
                        transfer: out_color.transfer.id(),
                    };
                    NodeRes::Rgb2Yuv {
                        y_buf: device.create_buffer(&wgpu::BufferDescriptor {
                            label: Some("rgb2yuv.y"),
                            size: y_size,
                            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                            mapped_at_creation: false,
                        }),
                        uv_buf: device.create_buffer(&wgpu::BufferDescriptor {
                            label: Some("rgb2yuv.uv"),
                            size: uv_size,
                            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                            mapped_at_creation: false,
                        }),
                        y_stride,
                        uv_stride,
                        params: uniform(device, id, bytemuck::bytes_of(&p)),
                        width: w,
                        height: h,
                    }
                }
                Op::Sink => NodeRes::Sink,
            };
            res.insert(id.clone(), r);
        }

        Ok(Executor {
            ctx,
            pipelines,
            graph,
            res,
            src_w,
            src_h,
            src_format,
            out_w,
            out_h,
            frames_done: 0,
        })
    }

    pub fn output_layout(&self) -> PlaneLayout {
        PlaneLayout {
            width: self.out_w,
            height: self.out_h,
            format: self.src_format,
        }
    }

    pub fn output_color(&self) -> ColorSpec {
        self.graph.output_color
    }

    pub fn frames_done(&self) -> u64 {
        self.frames_done
    }

    /// Allocate an output frame of the right size and format, with planes stored as
    /// separate textures. Used by `run_frame`; backends that own the encoder surface
    /// supply their own frame to `run_frame_into` instead.
    pub fn alloc_output_frame(&self) -> GpuFrame {
        let device = &self.ctx.device;
        let fmts = self.src_format.plane_formats();
        let usage = wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::TEXTURE_BINDING;
        let out_y = gpu::create_texture(device, "out.y", self.out_w, self.out_h, fmts[0], usage);
        let out_uv = gpu::create_texture(
            device,
            "out.uv",
            self.out_w.div_ceil(2),
            self.out_h.div_ceil(2),
            fmts[1],
            usage,
        );
        GpuFrame {
            planes: PlaneSet::Separate(vec![out_y, out_uv]),
            format: self.src_format,
            color: self.graph.output_color,
            width: self.out_w,
            height: self.out_h,
            pts: 0,
            native: NativeHandle::None,
        }
    }

    /// Run the graph on one frame. Returns the output frame as bi-planar textures that
    /// an encoder (or the PNG stand-in sink) can consume. Nothing in here reads back
    /// to the CPU.
    pub fn run_frame(&mut self, input: &GpuFrame) -> Result<GpuFrame, ExecError> {
        let mut out = self.alloc_output_frame();
        self.run_frame_into(input, &mut out)?;
        Ok(out)
    }

    /// Run the graph on one frame, writing the result into `output` — typically a
    /// frame whose texture is an imported encoder surface (D3D12 shared NV12, ...).
    /// `output` must have the executor's output size and format; its `pts` and `color`
    /// are set from the input / graph.
    pub fn run_frame_into(
        &mut self,
        input: &GpuFrame,
        output: &mut GpuFrame,
    ) -> Result<(), ExecError> {
        if input.format != self.src_format {
            return Err(ExecError::FormatMismatch {
                got: input.format,
                expected: self.src_format,
            });
        }
        if input.width != self.src_w || input.height != self.src_h {
            return Err(ExecError::SizeMismatch {
                got_w: input.width,
                got_h: input.height,
                w: self.src_w,
                h: self.src_h,
            });
        }
        if output.format != self.src_format {
            return Err(ExecError::FormatMismatch {
                got: output.format,
                expected: self.src_format,
            });
        }
        if output.width != self.out_w || output.height != self.out_h {
            return Err(ExecError::SizeMismatch {
                got_w: output.width,
                got_h: output.height,
                w: self.out_w,
                h: self.out_h,
            });
        }
        let device = &self.ctx.device;
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("onchip frame"),
        });

        let view_of = |id: &str| -> wgpu::TextureView {
            match &self.res[id] {
                NodeRes::Working { tex, .. } | NodeRes::Image { tex } => {
                    tex.create_view(&Default::default())
                }
                _ => unreachable!("no texture view for {id}"),
            }
        };

        for id in &self.graph.order {
            let node = self.graph.node(id);
            match (&node.op, &self.res[id]) {
                (Op::Source, _) | (Op::Image { .. }, _) | (Op::Sink, _) => {}
                (
                    Op::Yuv2Rgb,
                    NodeRes::Working {
                        tex,
                        width,
                        height,
                        params,
                    },
                ) => {
                    let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("yuv2rgb"),
                        layout: &self.pipelines.yuv2rgb_layout,
                        entries: &[
                            bind_tex(0, &input.plane_view(0)),
                            bind_tex(1, &input.plane_view(1)),
                            wgpu::BindGroupEntry {
                                binding: 2,
                                resource: wgpu::BindingResource::Sampler(
                                    &self.pipelines.chroma_sampler,
                                ),
                            },
                            bind_tex(3, &tex.create_view(&Default::default())),
                            bind_buf(4, params),
                        ],
                    });
                    dispatch(
                        &mut encoder,
                        &self.pipelines.yuv2rgb,
                        &bg,
                        *width,
                        *height,
                        16,
                    );
                }
                (
                    Op::Scale { .. },
                    NodeRes::Working {
                        tex,
                        width,
                        height,
                        params,
                    },
                ) => {
                    let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("scale"),
                        layout: &self.pipelines.scale_layout,
                        entries: &[
                            bind_tex(0, &view_of(&node.inputs[0])),
                            bind_tex(1, &tex.create_view(&Default::default())),
                            bind_buf(2, params),
                        ],
                    });
                    dispatch(
                        &mut encoder,
                        &self.pipelines.scale,
                        &bg,
                        *width,
                        *height,
                        16,
                    );
                }
                (
                    Op::Overlay { .. },
                    NodeRes::Working {
                        tex,
                        width,
                        height,
                        params,
                    },
                ) => {
                    let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("overlay"),
                        layout: &self.pipelines.overlay_layout,
                        entries: &[
                            bind_tex(0, &view_of(&node.inputs[0])),
                            bind_tex(1, &view_of(&node.inputs[1])),
                            bind_tex(2, &tex.create_view(&Default::default())),
                            bind_buf(3, params),
                        ],
                    });
                    dispatch(
                        &mut encoder,
                        &self.pipelines.overlay,
                        &bg,
                        *width,
                        *height,
                        16,
                    );
                }
                (
                    Op::Rgb2Yuv,
                    NodeRes::Rgb2Yuv {
                        y_buf,
                        uv_buf,
                        y_stride,
                        uv_stride,
                        params,
                        width,
                        height,
                    },
                ) => {
                    let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("rgb2yuv"),
                        layout: &self.pipelines.rgb2yuv_layout,
                        entries: &[
                            bind_tex(0, &view_of(&node.inputs[0])),
                            bind_buf(1, y_buf),
                            bind_buf(2, uv_buf),
                            bind_buf(3, params),
                        ],
                    });
                    // one invocation per 4x2 block, workgroup 8x8
                    let blocks_x = width.div_ceil(4);
                    let blocks_y = height.div_ceil(2);
                    dispatch(
                        &mut encoder,
                        &self.pipelines.rgb2yuv,
                        &bg,
                        blocks_x,
                        blocks_y,
                        8,
                    );

                    // GPU -> GPU copy of the packed planes into the output planes.
                    for (plane, (buf, stride)) in [(y_buf, y_stride), (uv_buf, uv_stride)]
                        .into_iter()
                        .enumerate()
                    {
                        let (tex, aspect) = output.plane_copy_target(plane);
                        let (pw, ph) = output.plane_size(plane);
                        encoder.copy_buffer_to_texture(
                            wgpu::TexelCopyBufferInfo {
                                buffer: buf,
                                layout: wgpu::TexelCopyBufferLayout {
                                    offset: 0,
                                    bytes_per_row: Some(*stride),
                                    rows_per_image: None,
                                },
                            },
                            wgpu::TexelCopyTextureInfo {
                                texture: tex,
                                mip_level: 0,
                                origin: wgpu::Origin3d::ZERO,
                                aspect,
                            },
                            wgpu::Extent3d {
                                width: pw,
                                height: ph,
                                depth_or_array_layers: 1,
                            },
                        );
                    }
                    debug_assert_eq!((*width, *height), (self.out_w, self.out_h));
                }
                (op, _) => unreachable!("resource/op mismatch for {id} ({})", op.name()),
            }
        }

        self.ctx.queue.submit([encoder.finish()]);
        self.frames_done += 1;
        output.pts = input.pts;
        output.color = self.graph.output_color;
        Ok(())
    }

    pub fn context(&self) -> &Arc<GpuContext> {
        &self.ctx
    }
}

fn uniform(device: &wgpu::Device, label: &str, data: &[u8]) -> wgpu::Buffer {
    let buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: data.len() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: true,
    });
    buf.slice(..)
        .get_mapped_range_mut()
        .expect("fresh mapped_at_creation buffer")
        .copy_from_slice(data);
    buf.unmap();
    buf
}

fn bind_tex<'a>(binding: u32, view: &'a wgpu::TextureView) -> wgpu::BindGroupEntry<'a> {
    wgpu::BindGroupEntry {
        binding,
        resource: wgpu::BindingResource::TextureView(view),
    }
}

fn bind_buf<'a>(binding: u32, buf: &'a wgpu::Buffer) -> wgpu::BindGroupEntry<'a> {
    wgpu::BindGroupEntry {
        binding,
        resource: buf.as_entire_binding(),
    }
}

fn dispatch(
    encoder: &mut wgpu::CommandEncoder,
    pipeline: &wgpu::ComputePipeline,
    bg: &wgpu::BindGroup,
    w: u32,
    h: u32,
    wg: u32,
) {
    let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
        label: None,
        timestamp_writes: None,
    });
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, bg, &[]);
    pass.dispatch_workgroups(w.div_ceil(wg), h.div_ceil(wg), 1);
}

/// Upload a working-format image as a sampled texture.
pub fn upload_working_image(ctx: &GpuContext, label: &str, img: &WorkingImage) -> wgpu::Texture {
    let tex = gpu::create_texture(
        &ctx.device,
        label,
        img.width,
        img.height,
        wgpu::TextureFormat::Rgba16Float,
        wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
    );
    ctx.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        bytemuck::cast_slice(&img.rgba16f),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(img.width * 8),
            rows_per_image: Some(img.height),
        },
        wgpu::Extent3d {
            width: img.width,
            height: img.height,
            depth_or_array_layers: 1,
        },
    );
    tex
}
