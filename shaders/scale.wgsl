// scale: resample premultiplied linear RGBA16F with a windowed kernel.
// Kernels (`kernel_id` matches onchip_core::job::ScaleFilter::id()):
//   0 bilinear  (triangle, support 1)
//   1 bicubic   (Catmull-Rom, a = -0.5, support 2)
//   2 lanczos3  (support 3)
// When downscaling, the kernel is stretched by the scale ratio so it also acts as an
// anti-aliasing filter (same behaviour as swscale / most resamplers).
//
// The 2-D kernel k(x)·k(y) is separable, so the resample runs as two 1-D passes:
//   axis 0: src (src_w × src_h) → tmp (dst_w × src_h)   horizontal
//   axis 1: tmp (dst_w × src_h) → dst (dst_w × dst_h)   vertical
// Each pass normalises by its own weight sum, so the result equals the 2-D
// normalisation (Σwx·Σwy = Σ wx·wy) up to float rounding, at ~2·taps instead of taps².

struct Params {
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
    kernel_id: u32,
    axis: u32,       // 0 = horizontal pass, 1 = vertical pass
    _pad1: u32,
    _pad2: u32,
};

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var out_tex: texture_storage_2d<rgba16float, write>;
@group(0) @binding(2) var<uniform> p: Params;

const PI: f32 = 3.14159265358979;

fn sinc(x: f32) -> f32 {
    if abs(x) < 1e-5 { return 1.0; }
    let px = PI * x;
    return sin(px) / px;
}

fn kernel(x_in: f32, kernel_id: u32) -> f32 {
    let x = abs(x_in);
    if kernel_id == 0u {
        return max(0.0, 1.0 - x);
    }
    if kernel_id == 1u {
        // Catmull-Rom (Keys cubic, a = -0.5)
        let a = -0.5;
        if x < 1.0 { return (a + 2.0) * x * x * x - (a + 3.0) * x * x + 1.0; }
        if x < 2.0 { return a * x * x * x - 5.0 * a * x * x + 8.0 * a * x - 4.0 * a; }
        return 0.0;
    }
    // lanczos3
    if x >= 3.0 { return 0.0; }
    return sinc(x) * sinc(x / 3.0);
}

fn support(kernel_id: u32) -> f32 {
    if kernel_id == 0u { return 1.0; }
    if kernel_id == 1u { return 2.0; }
    return 3.0;
}

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    if gid.x >= p.dst_w || gid.y >= p.dst_h { return; }

    // Length of the axis being resampled, before and after.
    var src_len: u32 = p.src_w;
    var dst_len: u32 = p.dst_w;
    var out_i: u32 = gid.x;
    if p.axis == 1u {
        src_len = p.src_h;
        dst_len = p.dst_h;
        out_i = gid.y;
    }

    let f = f32(src_len) / f32(dst_len);
    let stretch = max(f, 1.0);
    let sup = support(p.kernel_id) * stretch;

    // Source-space center of this destination sample (pixel centers at +0.5).
    let s = (f32(out_i) + 0.5) * f - 0.5;
    let i0 = i32(ceil(s - sup));
    let i1 = i32(floor(s + sup));
    let max_i = i32(src_len) - 1;

    var acc = vec4<f32>(0.0);
    var wsum = 0.0;
    for (var i = i0; i <= i1; i = i + 1) {
        let w = kernel((f32(i) - s) / stretch, p.kernel_id);
        if w == 0.0 { continue; }
        let c = clamp(i, 0, max_i);
        var coord = vec2<i32>(c, i32(gid.y));
        if p.axis == 1u { coord = vec2<i32>(i32(gid.x), c); }
        acc += textureLoad(src_tex, coord, 0) * w;
        wsum += w;
    }
    var result = vec4<f32>(0.0);
    if wsum != 0.0 { result = acc / wsum; }
    textureStore(out_tex, vec2<i32>(i32(gid.x), i32(gid.y)), result);
}
