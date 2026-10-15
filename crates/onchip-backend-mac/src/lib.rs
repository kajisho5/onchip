//! macOS backend: VideoToolbox decode/encode with Metal zero-copy import/export.
//!
//! Implemented in M2 (see `ROADMAP.md` RM-020..RM-024). The skeleton exists so the
//! workspace, CI and `onchip probe` compile on every platform.

use onchip_core::GpuContext;
use onchip_core::backend::{BackendError, BackendInfo, CodecInfo};

/// Name reported by `onchip probe`.
pub const NAME: &str = "videotoolbox+metal";

/// Hardware codec enumeration. Returns an empty list until M2.
pub fn enumerate_hw_codecs(_ctx: &GpuContext) -> (Vec<CodecInfo>, Vec<CodecInfo>) {
    (Vec::new(), Vec::new())
}

pub fn backend_info(ctx: &GpuContext) -> BackendInfo {
    let (hw_decoders, hw_encoders) = enumerate_hw_codecs(ctx);
    BackendInfo {
        name: NAME.into(),
        adapter: ctx.info.clone(),
        hw_decoders,
        hw_encoders,
    }
}

/// Placeholder until M2.
pub fn open() -> Result<(), BackendError> {
    Err(BackendError::Unavailable("macOS backend lands in M2"))
}
