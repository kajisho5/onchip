//! wgpu device setup and the four compute pipelines.

use std::sync::Arc;

use crate::backend::AdapterInfo;

#[derive(Debug, thiserror::Error)]
pub enum GpuError {
    #[error("no suitable GPU adapter found")]
    NoAdapter,
    #[error("failed to create device: {0}")]
    Device(String),
    #[error("GPU validation error: {0}")]
    Validation(String),
    #[error("buffer map failed: {0}")]
    Map(String),
}

pub const SHADER_COMMON: &str = include_str!("../../../shaders/common.wgsl");
pub const SHADER_YUV2RGB: &str = include_str!("../../../shaders/yuv2rgb.wgsl");
pub const SHADER_SCALE: &str = include_str!("../../../shaders/scale.wgsl");
pub const SHADER_OVERLAY: &str = include_str!("../../../shaders/overlay.wgsl");
pub const SHADER_RGB2YUV: &str = include_str!("../../../shaders/rgb2yuv.wgsl");

/// Device, queue and adapter description shared by everything that touches the GPU.
pub struct GpuContext {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub info: AdapterInfo,
}

impl std::fmt::Debug for GpuContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuContext")
            .field("info", &self.info)
            .finish()
    }
}

/// Which backends to ask wgpu for. Metal on macOS, D3D12 on Windows, Vulkan elsewhere
/// (and as a fallback), so the chosen adapter is the one the platform backend can share
/// textures with.
pub fn preferred_backends() -> wgpu::Backends {
    if cfg!(target_os = "macos") {
        wgpu::Backends::METAL
    } else if cfg!(target_os = "windows") {
        wgpu::Backends::DX12
    } else {
        wgpu::Backends::VULKAN
    }
}

impl GpuContext {
    /// Create a context on the platform's native backend. `backends` overrides the
    /// default (useful for tests on CI machines without a real GPU).
    pub fn new(backends: Option<wgpu::Backends>) -> Result<Arc<GpuContext>, GpuError> {
        pollster::block_on(Self::new_async(backends))
    }

    pub async fn new_async(backends: Option<wgpu::Backends>) -> Result<Arc<GpuContext>, GpuError> {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
        desc.backends = backends.unwrap_or_else(preferred_backends);
        let instance = wgpu::Instance::new(desc);
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                ..Default::default()
            })
            .await
            .map_err(|_| GpuError::NoAdapter)?;

        let adapter_features = adapter.features();
        let mut required_features = wgpu::Features::empty();
        let has_16bit_norm = adapter_features.contains(wgpu::Features::TEXTURE_FORMAT_16BIT_NORM);
        if has_16bit_norm {
            required_features |= wgpu::Features::TEXTURE_FORMAT_16BIT_NORM;
        }

        let limits = adapter.limits();
        let device_desc = wgpu::DeviceDescriptor {
            label: Some("onchip"),
            required_features,
            required_limits: limits,
            memory_hints: wgpu::MemoryHints::Performance,
            ..Default::default()
        };
        let (device, queue) = adapter
            .request_device(&device_desc)
            .await
            .map_err(|e| GpuError::Device(e.to_string()))?;

        let ai = adapter.get_info();
        let unified_memory = matches!(
            ai.device_type,
            wgpu::DeviceType::IntegratedGpu | wgpu::DeviceType::Cpu
        ) || (cfg!(target_os = "macos") && cfg!(target_arch = "aarch64"));
        let info = AdapterInfo {
            name: ai.name.clone(),
            backend: format!("{:?}", ai.backend),
            device_type: format!("{:?}", ai.device_type),
            driver: ai.driver.clone(),
            driver_info: ai.driver_info.clone(),
            vendor: ai.vendor,
            device: ai.device,
            unified_memory,
            texture_format_16bit_norm: has_16bit_norm,
        };

        Ok(Arc::new(GpuContext {
            instance,
            adapter,
            device,
            queue,
            info,
        }))
    }

    pub fn supports_p010(&self) -> bool {
        self.info.texture_format_16bit_norm
    }
}

/// The four compute pipelines with their bind group layouts.
pub struct Pipelines {
    pub yuv2rgb: wgpu::ComputePipeline,
    pub yuv2rgb_layout: wgpu::BindGroupLayout,
    pub scale: wgpu::ComputePipeline,
    pub scale_layout: wgpu::BindGroupLayout,
    pub overlay: wgpu::ComputePipeline,
    pub overlay_layout: wgpu::BindGroupLayout,
    pub rgb2yuv: wgpu::ComputePipeline,
    pub rgb2yuv_layout: wgpu::BindGroupLayout,
    pub chroma_sampler: wgpu::Sampler,
}

fn module(device: &wgpu::Device, label: &str, src: &str) -> wgpu::ShaderModule {
    let full = format!("{SHADER_COMMON}\n{src}");
    device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(full.into()),
    })
}

fn tex_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn storage_tex_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::StorageTexture {
            access: wgpu::StorageTextureAccess::WriteOnly,
            format: wgpu::TextureFormat::Rgba16Float,
            view_dimension: wgpu::TextureViewDimension::D2,
        },
        count: None,
    }
}

fn uniform_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn storage_buf_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: false },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn sampler_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    }
}

fn pipeline(
    device: &wgpu::Device,
    label: &str,
    module: &wgpu::ShaderModule,
    layout: &wgpu::BindGroupLayout,
) -> wgpu::ComputePipeline {
    let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(layout)],
        immediate_size: 0,
    });
    device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(label),
        layout: Some(&pl),
        module,
        entry_point: Some("main"),
        compilation_options: Default::default(),
        cache: None,
    })
}

impl Pipelines {
    pub fn new(device: &wgpu::Device) -> Pipelines {
        let yuv2rgb_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("yuv2rgb"),
            entries: &[
                tex_entry(0),
                tex_entry(1),
                sampler_entry(2),
                storage_tex_entry(3),
                uniform_entry(4),
            ],
        });
        let scale_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("scale"),
            entries: &[tex_entry(0), storage_tex_entry(1), uniform_entry(2)],
        });
        let overlay_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("overlay"),
            entries: &[
                tex_entry(0),
                tex_entry(1),
                storage_tex_entry(2),
                uniform_entry(3),
            ],
        });
        let rgb2yuv_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rgb2yuv"),
            entries: &[
                tex_entry(0),
                storage_buf_entry(1),
                storage_buf_entry(2),
                uniform_entry(3),
            ],
        });

        let m_yuv2rgb = module(device, "yuv2rgb", SHADER_YUV2RGB);
        let m_scale = module(device, "scale", SHADER_SCALE);
        let m_overlay = module(device, "overlay", SHADER_OVERLAY);
        let m_rgb2yuv = module(device, "rgb2yuv", SHADER_RGB2YUV);

        let chroma_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("chroma"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        Pipelines {
            yuv2rgb: pipeline(device, "yuv2rgb", &m_yuv2rgb, &yuv2rgb_layout),
            yuv2rgb_layout,
            scale: pipeline(device, "scale", &m_scale, &scale_layout),
            scale_layout,
            overlay: pipeline(device, "overlay", &m_overlay, &overlay_layout),
            overlay_layout,
            rgb2yuv: pipeline(device, "rgb2yuv", &m_rgb2yuv, &rgb2yuv_layout),
            rgb2yuv_layout,
            chroma_sampler,
        }
    }
}

/// Create a 2D texture with the usages the filter graph needs.
pub fn create_texture(
    device: &wgpu::Device,
    label: &str,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
    usage: wgpu::TextureUsages,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage,
        view_formats: &[],
    })
}

/// Working-format texture: sampled by the next filter, written by this one.
pub fn create_working_texture(device: &wgpu::Device, label: &str, w: u32, h: u32) -> wgpu::Texture {
    create_texture(
        device,
        label,
        w,
        h,
        wgpu::TextureFormat::Rgba16Float,
        wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::STORAGE_BINDING
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST,
    )
}

/// Round `bytes` up to wgpu's required `bytes_per_row` alignment (256).
pub fn align_row(bytes: u32) -> u32 {
    let a = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    bytes.div_ceil(a) * a
}

/// Block until all submitted work is done.
pub fn wait_idle(device: &wgpu::Device) -> Result<(), GpuError> {
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|e| GpuError::Device(e.to_string()))?;
    Ok(())
}

/// Convert an `f32` to IEEE half-precision bits (round-to-nearest-even).
pub fn f32_to_f16_bits(v: f32) -> u16 {
    let x = v.to_bits();
    let sign = ((x >> 16) & 0x8000) as u16;
    let mut exp = ((x >> 23) & 0xff) as i32;
    let mut mant = x & 0x7f_ffff;
    if exp == 0xff {
        // inf / nan
        return sign | 0x7c00 | if mant != 0 { 0x200 } else { 0 };
    }
    exp = exp - 127 + 15;
    if exp >= 0x1f {
        return sign | 0x7c00; // overflow -> inf
    }
    if exp <= 0 {
        if exp < -10 {
            return sign; // underflow -> zero
        }
        mant |= 0x80_0000;
        let shift = (14 - exp) as u32;
        let half = 1u32 << (shift - 1);
        let rem = mant & ((1 << shift) - 1);
        let mut m = mant >> shift;
        if rem > half || (rem == half && (m & 1) == 1) {
            m += 1;
        }
        return sign | m as u16;
    }
    let half = 0x1000u32;
    let rem = mant & 0x1fff;
    let mut m = mant >> 13;
    let mut e = exp as u32;
    if rem > half || (rem == half && (m & 1) == 1) {
        m += 1;
        if m == 0x400 {
            m = 0;
            e += 1;
            if e >= 0x1f {
                return sign | 0x7c00;
            }
        }
    }
    sign | ((e << 10) as u16) | m as u16
}

/// Convert IEEE half-precision bits to `f32`.
pub fn f16_bits_to_f32(h: u16) -> f32 {
    let sign = ((h & 0x8000) as u32) << 16;
    let exp = ((h >> 10) & 0x1f) as u32;
    let mant = (h & 0x3ff) as u32;
    let bits = if exp == 0 {
        if mant == 0 {
            sign
        } else {
            // subnormal
            let mut e = 127 - 15 + 1;
            let mut m = mant;
            while m & 0x400 == 0 {
                m <<= 1;
                e -= 1;
            }
            m &= 0x3ff;
            sign | ((e as u32) << 23) | (m << 13)
        }
    } else if exp == 0x1f {
        sign | 0x7f80_0000 | (mant << 13)
    } else {
        sign | ((exp + 127 - 15) << 23) | (mant << 13)
    };
    f32::from_bits(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f16_round_trip_common_values() {
        for v in [
            0.0f32, 1.0, 0.5, 0.25, 0.1, 0.9, 2.0, 1000.0, -1.0, 65504.0, 6.1e-5, 1e-6,
        ] {
            let back = f16_bits_to_f32(f32_to_f16_bits(v));
            let tol = (v.abs() * 1e-3).max(1e-7);
            assert!((back - v).abs() <= tol, "{v} -> {back}");
        }
        assert_eq!(f32_to_f16_bits(1.0), 0x3c00);
        assert_eq!(f32_to_f16_bits(-2.0), 0xc000);
        assert_eq!(f32_to_f16_bits(f32::INFINITY), 0x7c00);
    }

    #[test]
    fn row_alignment() {
        assert_eq!(align_row(1), 256);
        assert_eq!(align_row(256), 256);
        assert_eq!(align_row(1920), 2048);
        assert_eq!(align_row(3840), 3840);
    }

    #[test]
    fn shaders_parse_with_naga() {
        for (name, src) in [
            ("yuv2rgb", SHADER_YUV2RGB),
            ("scale", SHADER_SCALE),
            ("overlay", SHADER_OVERLAY),
            ("rgb2yuv", SHADER_RGB2YUV),
        ] {
            let full = format!("{SHADER_COMMON}\n{src}");
            let module = wgpu::naga::front::wgsl::parse_str(&full)
                .unwrap_or_else(|e| panic!("{name}: {}", e.emit_to_string(&full)));
            let mut validator = wgpu::naga::valid::Validator::new(
                wgpu::naga::valid::ValidationFlags::all(),
                wgpu::naga::valid::Capabilities::all(),
            );
            validator
                .validate(&module)
                .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        }
    }
}
