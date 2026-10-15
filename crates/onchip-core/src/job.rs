//! Declarative job definition (JSON).
//!
//! ```json
//! {
//!   "input": "in.mp4",
//!   "output": { "path": "out.mp4", "codec": "hevc", "bitrate": "20M", "color": "bt709" },
//!   "graph": [
//!     { "id": "src", "op": "source" },
//!     { "id": "rgb", "op": "yuv2rgb", "in": ["src"] },
//!     { "id": "sc",  "op": "scale", "in": ["rgb"], "width": 1920, "height": 1080, "filter": "lanczos3" },
//!     { "id": "logo","op": "image", "path": "logo_sample.png" },
//!     { "id": "ov",  "op": "overlay", "in": ["sc", "logo"], "x": 32, "y": 32, "opacity": 0.9 },
//!     { "id": "yuv", "op": "rgb2yuv", "in": ["ov"] },
//!     { "id": "out", "op": "sink", "in": ["yuv"] }
//!   ]
//! }
//! ```
//!
//! Unknown fields anywhere are an error: a typo in a parameter must not silently
//! produce a different render.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::backend::Codec;

#[derive(Debug, thiserror::Error)]
pub enum JobError {
    #[error("invalid job JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("node {id:?}: unknown op {op:?}")]
    UnknownOp { id: String, op: String },
    #[error("node {id:?} ({op}): {source}")]
    NodeParams {
        id: String,
        op: String,
        source: serde_json::Error,
    },
    #[error("duplicate node id {0:?}")]
    DuplicateId(String),
    #[error("invalid bitrate {0:?} (expected e.g. \"20M\", \"800k\", \"5000000\")")]
    Bitrate(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub input: String,
    pub output: Output,
    pub graph: Vec<Node>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Output {
    pub path: String,
    pub codec: Codec,
    /// `"20M"`, `"800k"`, or a plain integer string. Ignored for `png`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate: Option<String>,
    /// Short color name (`"bt709"`, `"bt709_full"`, `"bt2020_pq"`). Defaults to the
    /// source's color when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// GOP length in frames. Backend default when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gop: Option<u32>,
}

impl Output {
    pub fn bitrate_bps(&self) -> Result<Option<u64>, JobError> {
        let Some(s) = &self.bitrate else {
            return Ok(None);
        };
        let t = s.trim();
        let (num, mul) = match t.chars().last() {
            Some('k') | Some('K') => (&t[..t.len() - 1], 1_000u64),
            Some('m') | Some('M') => (&t[..t.len() - 1], 1_000_000u64),
            Some(c) if c.is_ascii_digit() => (t, 1u64),
            _ => return Err(JobError::Bitrate(s.clone())),
        };
        let n: f64 = num.parse().map_err(|_| JobError::Bitrate(s.clone()))?;
        if n.is_nan() || n <= 0.0 {
            return Err(JobError::Bitrate(s.clone()));
        }
        Ok(Some((n * mul as f64).round() as u64))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScaleFilter {
    Bilinear,
    Bicubic,
    #[default]
    Lanczos3,
}

impl ScaleFilter {
    /// Id passed to `scale.wgsl`.
    pub fn id(self) -> u32 {
        match self {
            ScaleFilter::Bilinear => 0,
            ScaleFilter::Bicubic => 1,
            ScaleFilter::Lanczos3 => 2,
        }
    }
}

/// Typed parameters of each node kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Source,
    #[serde(rename = "yuv2rgb")]
    Yuv2Rgb,
    Scale {
        width: u32,
        height: u32,
        #[serde(default)]
        filter: ScaleFilter,
    },
    Image {
        path: String,
    },
    Overlay {
        #[serde(default)]
        x: i32,
        #[serde(default)]
        y: i32,
        #[serde(default = "one")]
        opacity: f64,
    },
    #[serde(rename = "rgb2yuv")]
    Rgb2Yuv,
    Sink,
}

fn one() -> f64 {
    1.0
}

impl Op {
    pub fn name(&self) -> &'static str {
        match self {
            Op::Source => "source",
            Op::Yuv2Rgb => "yuv2rgb",
            Op::Scale { .. } => "scale",
            Op::Image { .. } => "image",
            Op::Overlay { .. } => "overlay",
            Op::Rgb2Yuv => "rgb2yuv",
            Op::Sink => "sink",
        }
    }

    /// Number of inputs this op must have.
    pub fn arity(&self) -> usize {
        match self {
            Op::Source | Op::Image { .. } => 0,
            Op::Yuv2Rgb | Op::Scale { .. } | Op::Rgb2Yuv | Op::Sink => 1,
            Op::Overlay { .. } => 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub id: String,
    pub inputs: Vec<String>,
    pub op: Op,
}

/// The on-disk shape of a node. `op` selects the variant; the remaining fields are
/// re-parsed with `deny_unknown_fields` per op so typos are rejected.
#[derive(Deserialize)]
struct RawNode {
    id: String,
    op: String,
    #[serde(default, rename = "in")]
    inputs: Vec<String>,
    #[serde(flatten)]
    rest: Map<String, Value>,
}

macro_rules! typed_params {
    ($id:expr, $op:expr, $rest:expr, $ty:ty) => {{
        let v = Value::Object($rest);
        serde_json::from_value::<$ty>(v).map_err(|e| JobError::NodeParams {
            id: $id.clone(),
            op: $op.clone(),
            source: e,
        })?
    }};
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoParams {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScaleParams {
    width: u32,
    height: u32,
    #[serde(default)]
    filter: ScaleFilter,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageParams {
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OverlayParams {
    #[serde(default)]
    x: i32,
    #[serde(default)]
    y: i32,
    #[serde(default = "one")]
    opacity: f64,
}

impl<'de> Deserialize<'de> for Node {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = RawNode::deserialize(d)?;
        Node::from_raw(raw).map_err(serde::de::Error::custom)
    }
}

impl Serialize for Node {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let op = serde_json::to_value(&self.op).map_err(serde::ser::Error::custom)?;
        let Value::Object(op_fields) = op else {
            unreachable!("op serializes to an object")
        };
        // id, op, in, then the op's own parameters: the order a human writes them in
        let mut obj = Map::new();
        obj.insert("id".into(), Value::String(self.id.clone()));
        obj.insert("op".into(), Value::String(self.op.name().into()));
        if !self.inputs.is_empty() {
            obj.insert(
                "in".into(),
                Value::Array(self.inputs.iter().cloned().map(Value::String).collect()),
            );
        }
        obj.extend(op_fields.into_iter().filter(|(k, _)| k != "op"));
        Value::Object(obj).serialize(s)
    }
}

impl Node {
    fn from_raw(raw: RawNode) -> Result<Node, JobError> {
        let RawNode {
            id,
            op,
            inputs,
            rest,
        } = raw;
        let typed = match op.as_str() {
            "source" => {
                typed_params!(id, op, rest, NoParams);
                Op::Source
            }
            "yuv2rgb" => {
                typed_params!(id, op, rest, NoParams);
                Op::Yuv2Rgb
            }
            "rgb2yuv" => {
                typed_params!(id, op, rest, NoParams);
                Op::Rgb2Yuv
            }
            "sink" => {
                typed_params!(id, op, rest, NoParams);
                Op::Sink
            }
            "scale" => {
                let p = typed_params!(id, op, rest, ScaleParams);
                Op::Scale {
                    width: p.width,
                    height: p.height,
                    filter: p.filter,
                }
            }
            "image" => {
                let p = typed_params!(id, op, rest, ImageParams);
                Op::Image { path: p.path }
            }
            "overlay" => {
                let p = typed_params!(id, op, rest, OverlayParams);
                Op::Overlay {
                    x: p.x,
                    y: p.y,
                    opacity: p.opacity,
                }
            }
            _ => return Err(JobError::UnknownOp { id, op }),
        };
        Ok(Node {
            id,
            inputs,
            op: typed,
        })
    }
}

impl Job {
    pub fn from_json(text: &str) -> Result<Job, JobError> {
        let job: Job = serde_json::from_str(text)?;
        let mut seen = std::collections::HashSet::new();
        for n in &job.graph {
            if !seen.insert(n.id.as_str()) {
                return Err(JobError::DuplicateId(n.id.clone()));
            }
        }
        job.output.bitrate_bps()?;
        Ok(job)
    }

    pub fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(self).expect("job serializes")
    }

    /// Build the canonical `scale + overlay` job used by the CLI shorthand.
    pub fn shorthand(
        input: &str,
        output: &str,
        codec: Codec,
        scale: Option<(u32, u32, ScaleFilter)>,
        overlay: Option<(String, i32, i32, f64)>,
    ) -> Job {
        let mut graph = vec![
            Node {
                id: "src".into(),
                inputs: vec![],
                op: Op::Source,
            },
            Node {
                id: "rgb".into(),
                inputs: vec!["src".into()],
                op: Op::Yuv2Rgb,
            },
        ];
        let mut last = "rgb".to_string();
        if let Some((w, h, f)) = scale {
            graph.push(Node {
                id: "sc".into(),
                inputs: vec![last.clone()],
                op: Op::Scale {
                    width: w,
                    height: h,
                    filter: f,
                },
            });
            last = "sc".into();
        }
        if let Some((path, x, y, opacity)) = overlay {
            graph.push(Node {
                id: "logo".into(),
                inputs: vec![],
                op: Op::Image { path },
            });
            graph.push(Node {
                id: "ov".into(),
                inputs: vec![last.clone(), "logo".into()],
                op: Op::Overlay { x, y, opacity },
            });
            last = "ov".into();
        }
        graph.push(Node {
            id: "yuv".into(),
            inputs: vec![last],
            op: Op::Rgb2Yuv,
        });
        graph.push(Node {
            id: "out".into(),
            inputs: vec!["yuv".into()],
            op: Op::Sink,
        });
        Job {
            input: input.into(),
            output: Output {
                path: output.into(),
                codec,
                bitrate: None,
                color: None,
                gop: None,
            },
            graph,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "input": "in.mp4",
      "output": { "path": "out.mp4", "codec": "hevc", "bitrate": "20M", "color": "bt709" },
      "graph": [
        { "id": "src", "op": "source" },
        { "id": "rgb", "op": "yuv2rgb", "in": ["src"] },
        { "id": "sc",  "op": "scale", "in": ["rgb"], "width": 1920, "height": 1080, "filter": "lanczos3" },
        { "id": "logo","op": "image", "path": "logo_sample.png" },
        { "id": "ov",  "op": "overlay", "in": ["sc", "logo"], "x": 32, "y": 32, "opacity": 0.9 },
        { "id": "yuv", "op": "rgb2yuv", "in": ["ov"] },
        { "id": "out", "op": "sink", "in": ["yuv"] }
      ]
    }"#;

    #[test]
    fn parses_design_doc_example() {
        let job = Job::from_json(SAMPLE).unwrap();
        assert_eq!(job.graph.len(), 7);
        assert_eq!(job.output.bitrate_bps().unwrap(), Some(20_000_000));
        match &job.graph[2].op {
            Op::Scale {
                width,
                height,
                filter,
            } => {
                assert_eq!((*width, *height), (1920, 1080));
                assert_eq!(*filter, ScaleFilter::Lanczos3);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(job.graph[4].inputs, vec!["sc", "logo"]);
    }

    #[test]
    fn round_trips_through_json() {
        let job = Job::from_json(SAMPLE).unwrap();
        let again = Job::from_json(&job.to_json_pretty()).unwrap();
        assert_eq!(job, again);
    }

    #[test]
    fn rejects_unknown_top_level_field() {
        let bad = SAMPLE.replacen("\"input\"", "\"inputs\"", 1);
        assert!(Job::from_json(&bad).is_err());
    }

    #[test]
    fn rejects_unknown_node_param() {
        let bad = SAMPLE.replacen(
            "\"opacity\": 0.9",
            "\"opacity\": 0.9, \"blend\": \"add\"",
            1,
        );
        let err = Job::from_json(&bad).unwrap_err();
        assert!(err.to_string().contains("blend"), "{err}");
    }

    #[test]
    fn rejects_unknown_op() {
        let bad = SAMPLE.replacen("\"op\": \"scale\"", "\"op\": \"resize\"", 1);
        let err = Job::from_json(&bad).unwrap_err();
        assert!(err.to_string().contains("unknown op \"resize\""), "{err}");
    }

    #[test]
    fn rejects_duplicate_id() {
        let bad = SAMPLE.replacen("\"id\": \"sc\"", "\"id\": \"rgb\"", 1);
        assert!(matches!(
            Job::from_json(&bad).unwrap_err(),
            JobError::DuplicateId(_)
        ));
    }

    #[test]
    fn bitrate_suffixes() {
        let mk = |b: &str| Output {
            path: "x".into(),
            codec: Codec::H264,
            bitrate: Some(b.into()),
            color: None,
            gop: None,
        };
        assert_eq!(mk("800k").bitrate_bps().unwrap(), Some(800_000));
        assert_eq!(mk("2.5M").bitrate_bps().unwrap(), Some(2_500_000));
        assert_eq!(mk("5000000").bitrate_bps().unwrap(), Some(5_000_000));
        assert!(mk("fast").bitrate_bps().is_err());
        assert!(mk("-1M").bitrate_bps().is_err());
    }

    #[test]
    fn shorthand_builds_expected_graph() {
        let job = Job::shorthand(
            "in/",
            "out/",
            Codec::Png,
            Some((1280, 720, ScaleFilter::Bicubic)),
            Some(("logo_sample.png".into(), 32, 32, 0.9)),
        );
        let ids: Vec<_> = job.graph.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, ["src", "rgb", "sc", "logo", "ov", "yuv", "out"]);
    }
}
