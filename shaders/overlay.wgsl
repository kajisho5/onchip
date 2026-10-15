// overlay: composite a premultiplied RGBA16F foreground over a background at (x, y)
// with a global opacity. "over" operator in linear light:
//   out = fg * opacity + bg * (1 - fg.a * opacity)

struct Params {
    x: i32,
    y: i32,
    fg_w: u32,
    fg_h: u32,
    out_w: u32,
    out_h: u32,
    opacity: f32,
    _pad: u32,
};

@group(0) @binding(0) var bg_tex: texture_2d<f32>;
@group(0) @binding(1) var fg_tex: texture_2d<f32>;
@group(0) @binding(2) var out_tex: texture_storage_2d<rgba16float, write>;
@group(0) @binding(3) var<uniform> p: Params;

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    if gid.x >= p.out_w || gid.y >= p.out_h { return; }
    let pos = vec2<i32>(i32(gid.x), i32(gid.y));
    let bg = textureLoad(bg_tex, pos, 0);

    let fx = pos.x - p.x;
    let fy = pos.y - p.y;
    var result = bg;
    if fx >= 0 && fy >= 0 && fx < i32(p.fg_w) && fy < i32(p.fg_h) {
        let fg = textureLoad(fg_tex, vec2<i32>(fx, fy), 0) * p.opacity;
        result = fg + bg * (1.0 - fg.a);
    }
    textureStore(out_tex, pos, result);
}
