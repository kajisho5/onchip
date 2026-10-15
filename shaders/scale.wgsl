// scale: resample premultiplied linear RGBA16F with a windowed kernel.
// Kernels (`kernel_id` matches onchip_core::job::ScaleFilter::id()):
//   0 bilinear  (triangle, support 1)
//   1 bicubic   (Catmull-Rom, a = -0.5, support 2)
//   2 lanczos3  (support 3)
// When downscaling, the kernel is stretched by the scale ratio so it also acts as an
// anti-aliasing filter (same behaviour as swscale / most resamplers).

struct Params {
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
    kernel_id: u32,
    _pad0: u32,
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

    let fx = f32(p.src_w) / f32(p.dst_w);
    let fy = f32(p.src_h) / f32(p.dst_h);
    let stretch_x = max(fx, 1.0);
    let stretch_y = max(fy, 1.0);
    let sup = support(p.kernel_id);
    let sup_x = sup * stretch_x;
    let sup_y = sup * stretch_y;

    // Source-space center of this destination pixel (pixel centers at +0.5).
    let sx = (f32(gid.x) + 0.5) * fx - 0.5;
    let sy = (f32(gid.y) + 0.5) * fy - 0.5;

    let x0 = i32(ceil(sx - sup_x));
    let x1 = i32(floor(sx + sup_x));
    let y0 = i32(ceil(sy - sup_y));
    let y1 = i32(floor(sy + sup_y));

    let max_x = i32(p.src_w) - 1;
    let max_y = i32(p.src_h) - 1;

    var acc = vec4<f32>(0.0);
    var wsum = 0.0;
    for (var iy = y0; iy <= y1; iy = iy + 1) {
        let wy = kernel((f32(iy) - sy) / stretch_y, p.kernel_id);
        if wy == 0.0 { continue; }
        let cy = clamp(iy, 0, max_y);
        for (var ix = x0; ix <= x1; ix = ix + 1) {
            let wx = kernel((f32(ix) - sx) / stretch_x, p.kernel_id);
            if wx == 0.0 { continue; }
            let cx = clamp(ix, 0, max_x);
            let w = wx * wy;
            acc += textureLoad(src_tex, vec2<i32>(cx, cy), 0) * w;
            wsum += w;
        }
    }
    var result = vec4<f32>(0.0);
    if wsum != 0.0 { result = acc / wsum; }
    textureStore(out_tex, vec2<i32>(i32(gid.x), i32(gid.y)), result);
}
