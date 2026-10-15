// yuv2rgb: bi-planar Y'CbCr (NV12 / P010) -> premultiplied linear RGBA16F.
//
// Chroma siting: horizontally "left" (chroma sample co-sited with the left luma of
// each pair, as BT.709 / H.264 default), vertically centered between the two luma rows
// of a 4:2:0 pair. Other sitings are M3 (see ROADMAP RM-033).

struct Params {
    // Row-major Y'CbCr -> R'G'B' matrix, one row per vec4 (w unused).
    m0: vec4<f32>,
    m1: vec4<f32>,
    m2: vec4<f32>,
    // Normalized-value offsets and scales that undo the coded range:
    //   y' = (y - y_offset) * y_scale ; c' = (c - c_offset) * c_scale
    y_offset: f32,
    y_scale: f32,
    c_offset: f32,
    c_scale: f32,
    width: u32,
    height: u32,
    transfer: u32,
    _pad: u32,
};

@group(0) @binding(0) var y_tex: texture_2d<f32>;
@group(0) @binding(1) var uv_tex: texture_2d<f32>;
@group(0) @binding(2) var chroma_sampler: sampler;
@group(0) @binding(3) var out_tex: texture_storage_2d<rgba16float, write>;
@group(0) @binding(4) var<uniform> p: Params;

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    if gid.x >= p.width || gid.y >= p.height { return; }

    let y_raw = textureLoad(y_tex, vec2<i32>(i32(gid.x), i32(gid.y)), 0).r;

    // Chroma: left-sited horizontally, centered vertically. See header comment.
    let uv_dim = vec2<f32>(textureDimensions(uv_tex));
    let cx = f32(gid.x) * 0.5;          // chroma sample index (continuous), x
    let cy = (f32(gid.y) - 0.5) * 0.5;  // chroma sample index (continuous), y
    let uv_coord = (vec2<f32>(cx, cy) + vec2<f32>(0.5, 0.5)) / uv_dim;
    let c_raw = textureSampleLevel(uv_tex, chroma_sampler, uv_coord, 0.0).rg;

    let yp = (y_raw - p.y_offset) * p.y_scale;
    let cb = (c_raw.x - p.c_offset) * p.c_scale;
    let cr = (c_raw.y - p.c_offset) * p.c_scale;
    let ycc = vec3<f32>(yp, cb, cr);

    let rgb_enc = vec3<f32>(dot(p.m0.xyz, ycc), dot(p.m1.xyz, ycc), dot(p.m2.xyz, ycc));
    let rgb_lin = eotf_inv3(rgb_enc, p.transfer);

    textureStore(out_tex, vec2<i32>(i32(gid.x), i32(gid.y)), vec4<f32>(rgb_lin, 1.0));
}
