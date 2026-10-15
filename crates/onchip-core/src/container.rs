//! Minimal MP4 demux / mux and H.264 bitstream helpers.
//!
//! Only compressed data passes through here, so it is CPU code by design. Built on the
//! `mp4` crate for the box parsing; this module adds the parameter-set plumbing that a
//! hardware decoder/encoder needs (avcC ⇄ Annex B).
//!
//! Scope (M1): one H.264 video track in, one H.264 video track out. HEVC is tracked as
//! ROADMAP RM-011 — the `mp4` crate does not expose `hvcC`, so it needs its own parser.

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;

use crate::backend::{Codec, Packet, StreamInfo};
use crate::color::ColorSpec;

#[derive(Debug, thiserror::Error)]
pub enum ContainerError {
    #[error("{path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("{path}: {source}")]
    Mp4 { path: String, source: mp4::Error },
    #[error("{0}: no video track found")]
    NoVideoTrack(String),
    #[error("{0}: codec {1:?} is not supported here yet (M1 is H.264 only; HEVC is RM-011)")]
    UnsupportedCodec(String, String),
    #[error("invalid avcC: {0}")]
    InvalidAvcc(&'static str),
    #[error("H.264 stream has no SPS/PPS before the first keyframe")]
    MissingParameterSets,
}

/// Parsed `AVCDecoderConfigurationRecord`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvcConfig {
    pub nal_length_size: usize,
    pub sps: Vec<Vec<u8>>,
    pub pps: Vec<Vec<u8>>,
}

impl AvcConfig {
    /// Parse an `avcC` payload (the box contents, without the box header).
    pub fn parse(avcc: &[u8]) -> Result<AvcConfig, ContainerError> {
        if avcc.len() < 7 || avcc[0] != 1 {
            return Err(ContainerError::InvalidAvcc("too short or bad version"));
        }
        let nal_length_size = (avcc[4] & 0x03) as usize + 1;
        let mut pos = 5;
        let num_sps = (avcc[pos] & 0x1f) as usize;
        pos += 1;
        let mut sps = Vec::with_capacity(num_sps);
        for _ in 0..num_sps {
            let len = read_u16(avcc, pos)? as usize;
            pos += 2;
            let end = pos + len;
            let nal = avcc
                .get(pos..end)
                .ok_or(ContainerError::InvalidAvcc("sps overrun"))?;
            sps.push(nal.to_vec());
            pos = end;
        }
        let num_pps = *avcc
            .get(pos)
            .ok_or(ContainerError::InvalidAvcc("missing pps count"))?
            as usize;
        pos += 1;
        let mut pps = Vec::with_capacity(num_pps);
        for _ in 0..num_pps {
            let len = read_u16(avcc, pos)? as usize;
            pos += 2;
            let end = pos + len;
            let nal = avcc
                .get(pos..end)
                .ok_or(ContainerError::InvalidAvcc("pps overrun"))?;
            pps.push(nal.to_vec());
            pos = end;
        }
        Ok(AvcConfig {
            nal_length_size,
            sps,
            pps,
        })
    }

    /// Build an `avcC` payload (4-byte NAL lengths).
    pub fn build(&self) -> Vec<u8> {
        let sps0 = self
            .sps
            .first()
            .map(|s| s.as_slice())
            .unwrap_or(&[0x67, 0x42, 0x00, 0x1e]);
        let mut out = vec![
            1,
            sps0.get(1).copied().unwrap_or(66),
            sps0.get(2).copied().unwrap_or(0),
            sps0.get(3).copied().unwrap_or(30),
            0xfc | ((self.nal_length_size as u8).saturating_sub(1) & 0x03),
            0xe0 | (self.sps.len() as u8 & 0x1f),
        ];
        for s in &self.sps {
            out.extend_from_slice(&(s.len() as u16).to_be_bytes());
            out.extend_from_slice(s);
        }
        out.push(self.pps.len() as u8);
        for p in &self.pps {
            out.extend_from_slice(&(p.len() as u16).to_be_bytes());
            out.extend_from_slice(p);
        }
        out
    }

    /// SPS + PPS as one Annex B chunk, to prepend to keyframes for a decoder.
    pub fn parameter_sets_annexb(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in self.sps.iter().chain(self.pps.iter()) {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal);
        }
        out
    }
}

fn fourcc(b: &[u8; 4]) -> mp4::FourCC {
    mp4::FourCC { value: *b }
}

fn read_u16(b: &[u8], pos: usize) -> Result<u16, ContainerError> {
    let s = b
        .get(pos..pos + 2)
        .ok_or(ContainerError::InvalidAvcc("truncated length"))?;
    Ok(u16::from_be_bytes([s[0], s[1]]))
}

/// Convert length-prefixed NAL units (as stored in MP4) to Annex B start codes.
pub fn avcc_to_annexb(data: &[u8], nal_length_size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 16);
    let mut pos = 0;
    while pos + nal_length_size <= data.len() {
        let mut len = 0usize;
        for i in 0..nal_length_size {
            len = (len << 8) | data[pos + i] as usize;
        }
        pos += nal_length_size;
        let end = (pos + len).min(data.len());
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[pos..end]);
        pos = end;
    }
    out
}

/// Split an Annex B stream into NAL units (without start codes).
pub fn annexb_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut i = 0;
    let mut start: Option<usize> = None;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(s) = start {
                // trailing zero of a 4-byte start code belongs to the next code
                let mut e = i;
                while e > s && data[e - 1] == 0 {
                    e -= 1;
                }
                nals.push(&data[s..e]);
            }
            i += 3;
            start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = start
        && s < data.len()
    {
        nals.push(&data[s..]);
    }
    nals
}

/// Convert an Annex B access unit to 4-byte length-prefixed NAL units, pulling out any
/// SPS (7) / PPS (8) and dropping AUDs (9). Returns `(avcc_data, sps, pps)`.
pub fn annexb_to_avcc(data: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let mut out = Vec::with_capacity(data.len());
    let mut sps = Vec::new();
    let mut pps = Vec::new();
    for nal in annexb_nals(data) {
        if nal.is_empty() {
            continue;
        }
        match nal[0] & 0x1f {
            7 => sps.push(nal.to_vec()),
            8 => pps.push(nal.to_vec()),
            9 => {}
            _ => {
                out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                out.extend_from_slice(nal);
            }
        }
    }
    (out, sps, pps)
}

/// Whether an H.264 access unit (either framing) contains an IDR slice.
pub fn annexb_is_idr(data: &[u8]) -> bool {
    annexb_nals(data)
        .iter()
        .any(|n| !n.is_empty() && n[0] & 0x1f == 5)
}

/// Reads one H.264 video track from an MP4 file.
pub struct Mp4Demuxer {
    reader: mp4::Mp4Reader<BufReader<File>>,
    track_id: u32,
    sample_count: u32,
    next: u32,
    pub info: StreamInfo,
    pub avc: AvcConfig,
    /// Frames per second as (num, den), from the track's sample durations.
    pub framerate: (u32, u32),
}

impl Mp4Demuxer {
    pub fn open(path: &Path) -> Result<Mp4Demuxer, ContainerError> {
        let p = path.display().to_string();
        let file = File::open(path).map_err(|e| ContainerError::Io {
            path: p.clone(),
            source: e,
        })?;
        let size = file
            .metadata()
            .map_err(|e| ContainerError::Io {
                path: p.clone(),
                source: e,
            })?
            .len();
        let reader = mp4::Mp4Reader::read_header(BufReader::new(file), size).map_err(|e| {
            ContainerError::Mp4 {
                path: p.clone(),
                source: e,
            }
        })?;

        let track = reader
            .tracks()
            .values()
            .find(|t| matches!(t.track_type(), Ok(mp4::TrackType::Video)))
            .ok_or_else(|| ContainerError::NoVideoTrack(p.clone()))?;
        let media = track.media_type().map_err(|e| ContainerError::Mp4 {
            path: p.clone(),
            source: e,
        })?;
        if media != mp4::MediaType::H264 {
            return Err(ContainerError::UnsupportedCodec(p, format!("{media:?}")));
        }
        let sps = track
            .sequence_parameter_set()
            .map_err(|e| ContainerError::Mp4 {
                path: p.clone(),
                source: e,
            })?
            .to_vec();
        let pps = track
            .picture_parameter_set()
            .map_err(|e| ContainerError::Mp4 {
                path: p.clone(),
                source: e,
            })?
            .to_vec();
        let avc = AvcConfig {
            nal_length_size: 4,
            sps: vec![sps],
            pps: vec![pps],
        };
        let timescale = track.timescale();
        let fps = track.frame_rate();
        let framerate = if (fps - fps.round()).abs() < 1e-6 && fps > 0.0 {
            (fps.round() as u32, 1)
        } else if fps > 0.0 {
            ((fps * 1001.0).round() as u32, 1001)
        } else {
            (30, 1)
        };
        let info = StreamInfo {
            codec: Codec::H264,
            width: track.width() as u32,
            height: track.height() as u32,
            color: ColorSpec::BT709_LIMITED,
            extradata: avc.build(),
            timebase: (1, timescale),
        };
        let track_id = track.track_id();
        let sample_count = track.sample_count();
        Ok(Mp4Demuxer {
            reader,
            track_id,
            sample_count,
            next: 1,
            info,
            avc,
            framerate,
        })
    }

    pub fn sample_count(&self) -> u32 {
        self.sample_count
    }

    /// Next packet, NAL units length-prefixed as stored. `dts` is the decode time and
    /// `pts = dts + composition offset`, both in `info.timebase` ticks.
    pub fn next_packet(&mut self) -> Result<Option<Packet>, ContainerError> {
        if self.next > self.sample_count {
            return Ok(None);
        }
        let s = self
            .reader
            .read_sample(self.track_id, self.next)
            .map_err(|e| ContainerError::Mp4 {
                path: "<input>".into(),
                source: e,
            })?;
        self.next += 1;
        Ok(s.map(|s| Packet {
            data: s.bytes.to_vec(),
            dts: s.start_time as i64,
            pts: s.start_time as i64 + s.rendering_offset as i64,
            keyframe: s.is_sync,
        }))
    }

    /// Same packet converted to Annex B, with SPS/PPS prepended on keyframes — what a
    /// Media Foundation / VideoToolbox decoder wants to see.
    pub fn to_annexb(&self, packet: &Packet) -> Vec<u8> {
        let body = avcc_to_annexb(&packet.data, self.avc.nal_length_size);
        if packet.keyframe {
            let mut out = self.avc.parameter_sets_annexb();
            out.extend_from_slice(&body);
            out
        } else {
            body
        }
    }
}

/// Writes one H.264 video track to an MP4 file.
pub struct Mp4Muxer {
    writer: mp4::Mp4Writer<BufWriter<File>>,
    track_id: u32,
    path: String,
    timescale: u32,
    frame_duration: u32,
    written: u64,
}

impl Mp4Muxer {
    /// `timescale` ticks per second; `frame_duration` ticks per frame. `avc` must carry
    /// the encoder's SPS/PPS (take them from the first keyframe with [`annexb_to_avcc`]).
    pub fn create(
        path: &Path,
        width: u32,
        height: u32,
        timescale: u32,
        frame_duration: u32,
        avc: &AvcConfig,
    ) -> Result<Mp4Muxer, ContainerError> {
        let p = path.display().to_string();
        if avc.sps.is_empty() || avc.pps.is_empty() {
            return Err(ContainerError::MissingParameterSets);
        }
        let file = File::create(path).map_err(|e| ContainerError::Io {
            path: p.clone(),
            source: e,
        })?;
        let cfg = mp4::Mp4Config {
            major_brand: fourcc(b"isom"),
            minor_version: 512,
            compatible_brands: vec![
                fourcc(b"isom"),
                fourcc(b"iso2"),
                fourcc(b"avc1"),
                fourcc(b"mp41"),
            ],
            timescale: 1000,
        };
        let mut writer = mp4::Mp4Writer::write_start(BufWriter::new(file), &cfg).map_err(|e| {
            ContainerError::Mp4 {
                path: p.clone(),
                source: e,
            }
        })?;
        let track = mp4::TrackConfig {
            track_type: mp4::TrackType::Video,
            timescale,
            language: "und".into(),
            media_conf: mp4::MediaConfig::AvcConfig(mp4::AvcConfig {
                width: width as u16,
                height: height as u16,
                seq_param_set: avc.sps[0].clone(),
                pic_param_set: avc.pps[0].clone(),
            }),
        };
        writer.add_track(&track).map_err(|e| ContainerError::Mp4 {
            path: p.clone(),
            source: e,
        })?;
        Ok(Mp4Muxer {
            writer,
            track_id: 1,
            path: p,
            timescale,
            frame_duration,
            written: 0,
        })
    }

    pub fn timescale(&self) -> u32 {
        self.timescale
    }

    /// Write one access unit already in length-prefixed form (see [`annexb_to_avcc`]).
    /// `pts`/`dts` are in this muxer's timescale.
    pub fn write_packet(&mut self, packet: &Packet) -> Result<(), ContainerError> {
        let sample = mp4::Mp4Sample {
            start_time: packet.dts.max(0) as u64,
            duration: self.frame_duration,
            rendering_offset: (packet.pts - packet.dts) as i32,
            is_sync: packet.keyframe,
            bytes: packet.data.clone().into(),
        };
        self.writer
            .write_sample(self.track_id, &sample)
            .map_err(|e| ContainerError::Mp4 {
                path: self.path.clone(),
                source: e,
            })?;
        self.written += 1;
        Ok(())
    }

    pub fn written(&self) -> u64 {
        self.written
    }

    pub fn finish(mut self) -> Result<u64, ContainerError> {
        self.writer.write_end().map_err(|e| ContainerError::Mp4 {
            path: self.path.clone(),
            source: e,
        })?;
        Ok(self.written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn avcc_round_trip() {
        let cfg = AvcConfig {
            nal_length_size: 4,
            sps: vec![vec![0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9]],
            pps: vec![vec![0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0]],
        };
        let bytes = cfg.build();
        assert_eq!(AvcConfig::parse(&bytes).unwrap(), cfg);
        assert!(AvcConfig::parse(&bytes[..3]).is_err());
    }

    #[test]
    fn framing_conversions() {
        let avcc = [0, 0, 0, 2, 0x65, 0xaa, 0, 0, 0, 1, 0x41];
        let annexb = avcc_to_annexb(&avcc, 4);
        assert_eq!(annexb, [0, 0, 0, 1, 0x65, 0xaa, 0, 0, 0, 1, 0x41]);
        assert!(annexb_is_idr(&annexb));
        let with_ps = [
            &[0, 0, 0, 1, 0x67, 1, 2][..],
            &[0, 0, 0, 1, 0x68, 3][..],
            &[0, 0, 1, 0x09, 0xf0][..],
            &annexb[..],
        ]
        .concat();
        let (data, sps, pps) = annexb_to_avcc(&with_ps);
        assert_eq!(sps, vec![vec![0x67, 1, 2]]);
        assert_eq!(pps, vec![vec![0x68, 3]]);
        assert_eq!(data, avcc);
        let nals = annexb_nals(&with_ps);
        assert_eq!(nals.len(), 5);
    }

    fn ffmpeg_available() -> bool {
        Command::new("ffmpeg").arg("-version").output().is_ok()
            && Command::new("ffprobe").arg("-version").output().is_ok()
    }

    fn make_fixture(dir: &Path, frames: u32) -> std::path::PathBuf {
        let path = dir.join("fixture_bars_tiny.mp4");
        let st = Command::new("ffmpeg")
            .args([
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x180:rate=30",
                "-frames:v",
                &frames.to_string(),
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-g",
                "10",
                "-bf",
                "2",
            ])
            .arg(&path)
            .status()
            .unwrap();
        assert!(st.success());
        path
    }

    fn ffprobe_frames(path: &Path) -> Vec<(bool, i64)> {
        let out = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "frame=key_frame,pkt_dts",
                "-of",
                "csv=p=0",
            ])
            .arg(path)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let mut it = l.split(',');
                let key = it.next().unwrap().trim() == "1";
                let dts: i64 = it.next().unwrap().trim().parse().unwrap_or(0);
                (key, dts)
            })
            .collect()
    }

    #[test]
    fn demux_matches_ffprobe_and_mux_round_trips() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join(format!("onchip-container-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fixture = make_fixture(&dir, 25);

        let mut demux = Mp4Demuxer::open(&fixture).unwrap();
        assert_eq!((demux.info.width, demux.info.height), (320, 180));
        assert_eq!(demux.framerate, (30, 1));
        assert_eq!(demux.avc.sps.len(), 1);
        let mut packets = Vec::new();
        while let Some(p) = demux.next_packet().unwrap() {
            packets.push(p);
        }
        assert_eq!(packets.len(), 25);
        // ffprobe reports frames in presentation order; compare keyframe count and that
        // decode timestamps are monotonic in our (decode) order.
        let probe = ffprobe_frames(&fixture);
        assert_eq!(probe.len(), 25);
        let probe_keys = probe.iter().filter(|(k, _)| *k).count();
        let our_keys = packets.iter().filter(|p| p.keyframe).count();
        assert_eq!(our_keys, probe_keys);
        assert!(packets.windows(2).all(|w| w[1].dts > w[0].dts));
        // keyframes carry parameter sets in Annex B form
        let first = demux.to_annexb(&packets[0]);
        let (_, sps, pps) = annexb_to_avcc(&first);
        assert_eq!(sps, demux.avc.sps);
        assert_eq!(pps, demux.avc.pps);
        assert!(annexb_is_idr(&first));

        // mux it back without touching the bitstream
        let out = dir.join("remux.mp4");
        let ts = demux.info.timebase.1;
        let mut mux = Mp4Muxer::create(&out, 320, 180, ts, ts / 30, &demux.avc).unwrap();
        for p in &packets {
            mux.write_packet(p).unwrap();
        }
        assert_eq!(mux.finish().unwrap(), 25);
        let probe2 = ffprobe_frames(&out);
        assert_eq!(probe2.len(), 25, "remuxed file should decode to 25 frames");
        assert_eq!(probe2.iter().filter(|(k, _)| *k).count(), probe_keys);
        // and it decodes cleanly end to end
        let st = Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&out)
            .args(["-f", "null", "-"])
            .status()
            .unwrap();
        assert!(st.success());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
