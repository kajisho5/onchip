// Shared helpers, prepended to every onchip shader at pipeline build time.
// Transfer function ids (must match onchip_core::color::Transfer::id()).
const TRANSFER_LINEAR: u32 = 0u;
const TRANSFER_BT709: u32 = 1u;
const TRANSFER_SRGB: u32 = 2u;
// PQ / HLG are reserved for M3; the graph validator rejects them before we get here.

// Opto-electronic transfer (linear light -> encoded signal), per channel.
fn oetf(v: f32, transfer: u32) -> f32 {
    let x = clamp(v, 0.0, 1.0);
    if transfer == TRANSFER_BT709 {
        // ITU-R BT.709 OETF
        if x < 0.018 { return 4.5 * x; }
        return 1.099 * pow(x, 0.45) - 0.099;
    }
    if transfer == TRANSFER_SRGB {
        if x <= 0.0031308 { return 12.92 * x; }
        return 1.055 * pow(x, 1.0 / 2.4) - 0.055;
    }
    return x;
}

// Inverse OETF (encoded signal -> linear light), per channel.
fn eotf_inv(v: f32, transfer: u32) -> f32 {
    let x = clamp(v, 0.0, 1.0);
    if transfer == TRANSFER_BT709 {
        if x < 0.081 { return x / 4.5; }
        return pow((x + 0.099) / 1.099, 1.0 / 0.45);
    }
    if transfer == TRANSFER_SRGB {
        if x <= 0.04045 { return x / 12.92; }
        return pow((x + 0.055) / 1.055, 2.4);
    }
    return x;
}

fn oetf3(v: vec3<f32>, transfer: u32) -> vec3<f32> {
    return vec3<f32>(oetf(v.x, transfer), oetf(v.y, transfer), oetf(v.z, transfer));
}

fn eotf_inv3(v: vec3<f32>, transfer: u32) -> vec3<f32> {
    return vec3<f32>(eotf_inv(v.x, transfer), eotf_inv(v.y, transfer), eotf_inv(v.z, transfer));
}
