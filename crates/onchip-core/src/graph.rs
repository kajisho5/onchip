//! Filter graph: validated DAG built from a [`Job`].
//!
//! Validation happens once, before any GPU work:
//! - every input refers to an existing node, with the arity the op requires
//! - no cycles, no node left dangling (everything must reach the single sink)
//! - exactly one `source` and one `sink`
//! - signal kinds line up (`yuv2rgb` wants Y'CbCr, `scale` wants RGB, ...)
//! - the [`ColorSpec`] that flows along every edge is one the shaders can handle,
//!   and the requested output color matches what the graph actually produces
//!   (there is no color-conversion node yet; that is M3)

use std::collections::{HashMap, HashSet, VecDeque};

use crate::color::ColorSpec;
use crate::job::{Job, Node, Op};

#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    #[error("node {id:?}: input {input:?} does not exist")]
    MissingInput { id: String, input: String },
    #[error("node {id:?} ({op}): expected {expected} input(s), got {got}")]
    Arity {
        id: String,
        op: &'static str,
        expected: usize,
        got: usize,
    },
    #[error("graph has a cycle involving {0:?}")]
    Cycle(Vec<String>),
    #[error("node {0:?} is not connected to the sink")]
    Dangling(String),
    #[error("graph must have exactly one `source` node (found {0})")]
    SourceCount(usize),
    #[error("graph must have exactly one `sink` node (found {0})")]
    SinkCount(usize),
    #[error("node {id:?} ({op}): input {index} is {got} but {expected} is required")]
    SignalMismatch {
        id: String,
        op: &'static str,
        index: usize,
        got: Signal,
        expected: Signal,
    },
    #[error(
        "node {id:?}: input color {got:?} does not match {expected:?} (color conversion is M3)"
    )]
    ColorMismatch {
        id: String,
        got: ColorSpec,
        expected: ColorSpec,
    },
    #[error("color {0:?} uses a transfer function the shaders cannot evaluate yet (M3)")]
    UnsupportedColor(ColorSpec),
    #[error("output color {0:?} is not a known name")]
    UnknownOutputColor(String),
    #[error(
        "output color {requested:?} differs from graph output {produced:?} (color conversion is M3)"
    )]
    OutputColorMismatch {
        requested: ColorSpec,
        produced: ColorSpec,
    },
}

/// What kind of pixels travel along an edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Bi-planar Y'CbCr as produced by a decoder / consumed by an encoder.
    Yuv,
    /// Premultiplied linear RGBA16F working format.
    Rgb,
}

impl std::fmt::Display for Signal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Signal::Yuv => write!(f, "Y'CbCr"),
            Signal::Rgb => write!(f, "RGB"),
        }
    }
}

fn expected_inputs(op: &Op) -> Vec<Signal> {
    match op {
        Op::Source | Op::Image { .. } => vec![],
        Op::Yuv2Rgb => vec![Signal::Yuv],
        Op::Scale { .. } => vec![Signal::Rgb],
        Op::Overlay { .. } => vec![Signal::Rgb, Signal::Rgb],
        Op::Rgb2Yuv => vec![Signal::Rgb],
        Op::Sink => vec![Signal::Yuv],
    }
}

fn output_signal(op: &Op) -> Option<Signal> {
    match op {
        Op::Source => Some(Signal::Yuv),
        Op::Yuv2Rgb | Op::Scale { .. } | Op::Image { .. } | Op::Overlay { .. } => Some(Signal::Rgb),
        Op::Rgb2Yuv => Some(Signal::Yuv),
        Op::Sink => None,
    }
}

/// A validated graph. `order` is a topological order; every id in it is a key of `nodes`.
#[derive(Debug, Clone)]
pub struct Graph {
    pub nodes: HashMap<String, Node>,
    pub order: Vec<String>,
    pub source: String,
    pub sink: String,
    /// Color at each node's output (`None` for the sink).
    pub colors: HashMap<String, ColorSpec>,
    /// Color the sink receives; what the encoder must be opened with.
    pub output_color: ColorSpec,
}

impl Graph {
    /// Validate `job.graph` given the color the source will deliver.
    pub fn build(job: &Job, source_color: ColorSpec) -> Result<Graph, GraphError> {
        let nodes: HashMap<String, Node> = job
            .graph
            .iter()
            .map(|n| (n.id.clone(), n.clone()))
            .collect();

        // arity + existence
        for n in &job.graph {
            let expected = n.op.arity();
            if n.inputs.len() != expected {
                return Err(GraphError::Arity {
                    id: n.id.clone(),
                    op: n.op.name(),
                    expected,
                    got: n.inputs.len(),
                });
            }
            for input in &n.inputs {
                if !nodes.contains_key(input) {
                    return Err(GraphError::MissingInput {
                        id: n.id.clone(),
                        input: input.clone(),
                    });
                }
            }
        }

        let sources: Vec<_> = job
            .graph
            .iter()
            .filter(|n| matches!(n.op, Op::Source))
            .map(|n| &n.id)
            .collect();
        if sources.len() != 1 {
            return Err(GraphError::SourceCount(sources.len()));
        }
        let sinks: Vec<_> = job
            .graph
            .iter()
            .filter(|n| matches!(n.op, Op::Sink))
            .map(|n| &n.id)
            .collect();
        if sinks.len() != 1 {
            return Err(GraphError::SinkCount(sinks.len()));
        }
        let source = sources[0].clone();
        let sink = sinks[0].clone();

        // Kahn topological sort
        let mut indegree: HashMap<&str, usize> = HashMap::new();
        let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
        for n in &job.graph {
            indegree.entry(n.id.as_str()).or_insert(0);
            for input in &n.inputs {
                *indegree.entry(n.id.as_str()).or_insert(0) += 1;
                dependents
                    .entry(input.as_str())
                    .or_default()
                    .push(n.id.as_str());
            }
        }
        let mut queue: VecDeque<&str> = {
            let mut zero: Vec<&str> = indegree
                .iter()
                .filter(|(_, d)| **d == 0)
                .map(|(id, _)| *id)
                .collect();
            zero.sort(); // deterministic order
            zero.into_iter().collect()
        };
        let mut order = Vec::with_capacity(job.graph.len());
        while let Some(id) = queue.pop_front() {
            order.push(id.to_string());
            if let Some(deps) = dependents.get(id) {
                let mut ready = Vec::new();
                for d in deps {
                    let e = indegree.get_mut(d).expect("known node");
                    *e -= 1;
                    if *e == 0 {
                        ready.push(*d);
                    }
                }
                ready.sort();
                queue.extend(ready);
            }
        }
        if order.len() != job.graph.len() {
            let seen: HashSet<&str> = order.iter().map(String::as_str).collect();
            let mut stuck: Vec<String> = job
                .graph
                .iter()
                .filter(|n| !seen.contains(n.id.as_str()))
                .map(|n| n.id.clone())
                .collect();
            stuck.sort();
            return Err(GraphError::Cycle(stuck));
        }

        // reachability: everything must feed the sink
        let mut reach: HashSet<String> = HashSet::new();
        let mut stack = vec![sink.clone()];
        while let Some(id) = stack.pop() {
            if reach.insert(id.clone()) {
                for input in &nodes[&id].inputs {
                    stack.push(input.clone());
                }
            }
        }
        for id in &order {
            if !reach.contains(id) {
                return Err(GraphError::Dangling(id.clone()));
            }
        }

        // signal + color propagation
        if !source_color.transfer.is_supported() {
            return Err(GraphError::UnsupportedColor(source_color));
        }
        let mut signals: HashMap<String, Signal> = HashMap::new();
        let mut colors: HashMap<String, ColorSpec> = HashMap::new();
        let working = source_color.working_space();
        let mut output_color = source_color;
        for id in &order {
            let n = &nodes[id];
            let expected = expected_inputs(&n.op);
            for (index, input) in n.inputs.iter().enumerate() {
                let got = signals[input];
                if got != expected[index] {
                    return Err(GraphError::SignalMismatch {
                        id: id.clone(),
                        op: n.op.name(),
                        index,
                        got,
                        expected: expected[index],
                    });
                }
            }
            let out_color = match &n.op {
                Op::Source => source_color,
                Op::Yuv2Rgb => working,
                Op::Image { .. } => working,
                Op::Scale { .. } => colors[&n.inputs[0]],
                Op::Overlay { .. } => {
                    let a = colors[&n.inputs[0]];
                    let b = colors[&n.inputs[1]];
                    if a != b {
                        return Err(GraphError::ColorMismatch {
                            id: id.clone(),
                            got: b,
                            expected: a,
                        });
                    }
                    a
                }
                Op::Rgb2Yuv => source_color,
                Op::Sink => {
                    output_color = colors[&n.inputs[0]];
                    source_color
                }
            };
            if let Some(sig) = output_signal(&n.op) {
                signals.insert(id.clone(), sig);
                colors.insert(id.clone(), out_color);
            }
        }

        if let Some(name) = &job.output.color {
            let requested = ColorSpec::from_short_name(name)
                .ok_or_else(|| GraphError::UnknownOutputColor(name.clone()))?;
            if requested != output_color {
                return Err(GraphError::OutputColorMismatch {
                    requested,
                    produced: output_color,
                });
            }
        }

        Ok(Graph {
            nodes,
            order,
            source,
            sink,
            colors,
            output_color,
        })
    }

    pub fn node(&self, id: &str) -> &Node {
        &self.nodes[id]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Codec;
    use crate::job::{Job, ScaleFilter};

    fn sample() -> Job {
        Job::shorthand(
            "in/",
            "out/",
            Codec::Png,
            Some((1280, 720, ScaleFilter::Lanczos3)),
            Some(("logo_sample.png".into(), 32, 32, 0.9)),
        )
    }

    #[test]
    fn builds_and_orders() {
        let g = Graph::build(&sample(), ColorSpec::BT709_LIMITED).unwrap();
        let pos = |id: &str| g.order.iter().position(|x| x == id).unwrap();
        assert!(pos("src") < pos("rgb"));
        assert!(pos("rgb") < pos("sc"));
        assert!(pos("sc") < pos("ov"));
        assert!(pos("logo") < pos("ov"));
        assert!(pos("ov") < pos("yuv"));
        assert!(pos("yuv") < pos("out"));
        assert_eq!(g.output_color, ColorSpec::BT709_LIMITED);
        assert_eq!(g.colors["sc"], ColorSpec::BT709_LIMITED.working_space());
    }

    #[test]
    fn detects_cycle() {
        let mut job = sample();
        job.graph.iter_mut().find(|n| n.id == "rgb").unwrap().inputs = vec!["sc".into()];
        let err = Graph::build(&job, ColorSpec::BT709_LIMITED).unwrap_err();
        assert!(matches!(err, GraphError::Cycle(_)), "{err}");
    }

    #[test]
    fn detects_dangling() {
        let mut job = sample();
        job.graph.push(Node {
            id: "orphan".into(),
            inputs: vec!["rgb".into()],
            op: Op::Scale {
                width: 10,
                height: 10,
                filter: ScaleFilter::Bilinear,
            },
        });
        let err = Graph::build(&job, ColorSpec::BT709_LIMITED).unwrap_err();
        assert!(
            matches!(err, GraphError::Dangling(ref id) if id == "orphan"),
            "{err}"
        );
    }

    #[test]
    fn detects_missing_input() {
        let mut job = sample();
        job.graph.iter_mut().find(|n| n.id == "sc").unwrap().inputs = vec!["nope".into()];
        let err = Graph::build(&job, ColorSpec::BT709_LIMITED).unwrap_err();
        assert!(matches!(err, GraphError::MissingInput { .. }), "{err}");
    }

    #[test]
    fn detects_signal_mismatch() {
        // scale directly on the Y'CbCr source
        let mut job = sample();
        job.graph.iter_mut().find(|n| n.id == "sc").unwrap().inputs = vec!["src".into()];
        job.graph.retain(|n| n.id != "rgb");
        let err = Graph::build(&job, ColorSpec::BT709_LIMITED).unwrap_err();
        assert!(matches!(err, GraphError::SignalMismatch { .. }), "{err}");
    }

    #[test]
    fn detects_arity() {
        let mut job = sample();
        job.graph.iter_mut().find(|n| n.id == "ov").unwrap().inputs = vec!["sc".into()];
        let err = Graph::build(&job, ColorSpec::BT709_LIMITED).unwrap_err();
        assert!(
            matches!(
                err,
                GraphError::Arity {
                    expected: 2,
                    got: 1,
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn rejects_pq_until_m3() {
        let pq = ColorSpec::from_short_name("bt2020_pq").unwrap();
        let err = Graph::build(&sample(), pq).unwrap_err();
        assert!(matches!(err, GraphError::UnsupportedColor(_)), "{err}");
    }

    #[test]
    fn output_color_must_match() {
        let mut job = sample();
        job.output.color = Some("bt709_full".into());
        let err = Graph::build(&job, ColorSpec::BT709_LIMITED).unwrap_err();
        assert!(
            matches!(err, GraphError::OutputColorMismatch { .. }),
            "{err}"
        );
        job.output.color = Some("bt709".into());
        assert!(Graph::build(&job, ColorSpec::BT709_LIMITED).is_ok());
    }
}
