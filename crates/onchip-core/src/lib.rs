//! onchip core: job definition, validated filter graph, WGSL compute filters and the
//! backend contract the platform crates implement.
//!
//! See `docs/DESIGN.md` for the whole picture. The one rule everything here obeys:
//! from decoder output to encoder input, uncompressed pixels never leave GPU-accessible
//! memory. GPU→GPU copies are fine; a readback is not.

pub mod backend;
pub mod color;
pub mod container;
pub mod exec;
pub mod frame;
pub mod gpu;
pub mod graph;
pub mod job;
pub mod pngio;

pub use backend::{BackendInfo, Codec, Decoder, Encoder, VideoBackend};
pub use color::ColorSpec;
pub use exec::Executor;
pub use frame::{GpuFrame, PixelFormat};
pub use gpu::GpuContext;
pub use graph::Graph;
pub use job::Job;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod gpu_tests;
