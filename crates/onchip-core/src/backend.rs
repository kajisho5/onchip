//! Platform backend contract. `onchip-backend-win` and `onchip-backend-mac` implement it.
//!
//! The contract is deliberately narrow: push compressed packets in, pull GPU frames
//! out (decoder); push GPU frames in, pull compressed packets out (encoder). Demux and
//! mux are the core's job and only ever touch compressed data.

use serde::{Deserialize, Serialize};

use crate::color::ColorSpec;
use crate::frame::{GpuFrame, PixelFormat};

pub type Result<T> = std::result::Result<T, BackendError>;

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("no backend is available on this platform ({0})")]
    Unavailable(&'static str),
    #[error("codec not supported by this backend: {0:?}")]
    UnsupportedCodec(Codec),
    #[error("pixel format not supported by this backend: {0:?}")]
    UnsupportedFormat(PixelFormat),
    #[error("GPU interop failed: {0}")]
    Interop(String),
    #[error("platform error: {0}")]
    Platform(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    H264,
    Hevc,
    /// Stand-in used by the PNG-sequence source/sink until M1.
    Png,
}

/// Static description of a backend, as printed by `onchip probe`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendInfo {
    pub name: String,
    pub adapter: AdapterInfo,
    #[serde(default)]
    pub hw_decoders: Vec<CodecInfo>,
    #[serde(default)]
    pub hw_encoders: Vec<CodecInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdapterInfo {
    pub name: String,
    pub backend: String,
    pub device_type: String,
    pub driver: String,
    pub driver_info: String,
    pub vendor: u32,
    pub device: u32,
    /// `true` on unified-memory systems (Apple Silicon, integrated GPUs): there is no
    /// separate GPU memory, so "on-chip" means "never copied to a CPU-visible buffer".
    pub unified_memory: bool,
    /// Whether the adapter can filter 16-bit normalized textures (needed for P010).
    pub texture_format_16bit_norm: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodecInfo {
    pub codec: Codec,
    /// Platform name of the component (MFT friendly name, VideoToolbox encoder id, ...).
    pub name: String,
    #[serde(default)]
    pub formats: Vec<PixelFormat>,
}

/// What the demuxer knows about a video stream.
#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub color: ColorSpec,
    /// Codec private data (avcC / hvcC).
    pub extradata: Vec<u8>,
    /// Timebase as (numerator, denominator) seconds per tick.
    pub timebase: (u32, u32),
}

#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub color: ColorSpec,
    pub bitrate_bps: u64,
    pub gop_frames: u32,
    pub framerate: (u32, u32),
}

/// A compressed access unit.
#[derive(Debug, Clone)]
pub struct Packet {
    pub data: Vec<u8>,
    pub pts: i64,
    pub dts: i64,
    pub keyframe: bool,
}

pub trait Decoder {
    /// Feed one compressed packet. May buffer internally.
    fn push(&mut self, packet: Packet) -> Result<()>;
    /// Pull the next decoded frame, if one is ready. Frames live in GPU-accessible memory.
    fn pull(&mut self) -> Result<Option<GpuFrame>>;
    /// Signal end of stream; subsequent `pull`s drain the decoder.
    fn flush(&mut self) -> Result<()>;
}

pub trait Encoder {
    /// Feed one frame. The frame must have the `PixelFormat` the encoder was opened with.
    fn push(&mut self, frame: GpuFrame) -> Result<()>;
    /// Pull the next compressed packet, if one is ready.
    fn pull(&mut self) -> Result<Option<Packet>>;
    /// Signal end of stream; subsequent `pull`s drain the encoder.
    fn flush(&mut self) -> Result<()>;
}

pub trait VideoBackend {
    type Decoder: Decoder;
    type Encoder: Encoder;

    fn probe(&self) -> BackendInfo;
    fn open_decoder(&self, stream: &StreamInfo) -> Result<Self::Decoder>;
    fn open_encoder(&self, cfg: &EncoderConfig) -> Result<Self::Encoder>;
}
