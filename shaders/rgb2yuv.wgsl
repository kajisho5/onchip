// rgb2yuv: premultiplied linear RGBA16F -> bi-planar Y'CbCr (NV12 / P010) written as
// packed bytes into two storage buffers (Y plane, interleaved CbCr plane). Each
// invocation produces a 4x2 luma block and the 2 chroma pairs below it, so no atomics
// are needed to pack sub-word samples. The buffers are then copied on the GPU into the
// encoder's plane textures (a GPU->GPU copy, still on-chip).
//
// Chroma siting: left (chroma from the even luma column of each pair), vertically
// averaged over the two rows. Matches yuv2rgb.wgsl. Other sitings are M3.

struct Params {
    // Row-major R'G'B' -> Y'CbCr matrix, one row per vec4 (w unused).
    m0: vec4<f32>,
    m1: vec4<f32>,
    m2: vec4<f32>,
    // code = round(v * mul + add), clamped to [0, max_code], then << shift
    y_mul: f32,
    y_add: f32,
    c_mul: f32,
    c_add: f32,
    max_code: f32,
    shift: u32,
    width: u32,
    height: u32,
    y_stride_words: u32,   // u32 words per Y row
    uv_stride_words: u32,  // u32 words per CbCr row
    bits16: u32,           // 0 = 8-bit samples (NV12), 1 = 16-bit samples (P010)
    transfer: u32,
};

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var<storage, read_write> y_buf: array<u32>;
@group(0) @binding(2) var<storage, read_write> uv_buf: array<u32>;
@group(0) @binding(3) var<uniform> p: Params;

fn load_ycc(x: i32, y: i32) -> vec3<f32> {
    let cx = clamp(x, 0, i32(p.width) - 1);
    let cy = clamp(y, 0, i32(p.height) - 1);
    let px = textureLoad(src_tex, vec2<i32>(cx, cy), 0);
    // Un-premultiply against a black background: for opaque video alpha is 1,
    // for overlaid regions the composite is already premultiplied over an opaque bg.
    let rgb_lin = px.rgb;
    let rgb_enc = oetf3(rgb_lin, p.transfer);
    return vec3<f32>(dot(p.m0.xyz, rgb_enc), dot(p.m1.xyz, rgb_enc), dot(p.m2.xyz, rgb_enc));
}

fn code_y(v: f32) -> u32 {
    let c = clamp(round(v * p.y_mul + p.y_add), 0.0, p.max_code);
    return u32(c) << p.shift;
}

fn code_c(v: f32) -> u32 {
    let c = clamp(round(v * p.c_mul + p.c_add), 0.0, p.max_code);
    return u32(c) << p.shift;
}

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let bx = i32(gid.x) * 4;
    let by = i32(gid.y) * 2;
    if bx >= i32(p.width) || by >= i32(p.height) { return; }

    var ycc: array<vec3<f32>, 8>;
    for (var r = 0; r < 2; r = r + 1) {
        for (var c = 0; c < 4; c = c + 1) {
            ycc[r * 4 + c] = load_ycc(bx + c, by + r);
        }
    }

    // ---- luma ----
    for (var r = 0u; r < 2u; r = r + 1u) {
        let y0 = code_y(ycc[r * 4u + 0u].x);
        let y1 = code_y(ycc[r * 4u + 1u].x);
        let y2 = code_y(ycc[r * 4u + 2u].x);
        let y3 = code_y(ycc[r * 4u + 3u].x);
        let row = u32(by) + r;
        if p.bits16 == 0u {
            let word = y0 | (y1 << 8u) | (y2 << 16u) | (y3 << 24u);
            y_buf[row * p.y_stride_words + gid.x] = word;
        } else {
            let base = row * p.y_stride_words + gid.x * 2u;
            y_buf[base] = y0 | (y1 << 16u);
            y_buf[base + 1u] = y2 | (y3 << 16u);
        }
    }

    // ---- chroma: left-sited, averaged over the two rows ----
    let cb0 = (ycc[0].y + ycc[4].y) * 0.5;
    let cr0 = (ycc[0].z + ycc[4].z) * 0.5;
    let cb1 = (ycc[2].y + ycc[6].y) * 0.5;
    let cr1 = (ycc[2].z + ycc[6].z) * 0.5;
    let crow = gid.y;
    if p.bits16 == 0u {
        let word = code_c(cb0) | (code_c(cr0) << 8u) | (code_c(cb1) << 16u) | (code_c(cr1) << 24u);
        uv_buf[crow * p.uv_stride_words + gid.x] = word;
    } else {
        let base = crow * p.uv_stride_words + gid.x * 2u;
        uv_buf[base] = code_c(cb0) | (code_c(cr0) << 16u);
        uv_buf[base + 1u] = code_c(cb1) | (code_c(cr1) << 16u);
    }
}
