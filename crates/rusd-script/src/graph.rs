//! **The operator graph a script builds** (SPEC D15).
//!
//! A script calls operator functions ([`crate::ops`]); each call appends one node here,
//! and [`Graph::to_ops_toml`] writes the `.ops.toml` the cook already reads. The graph is
//! therefore a *product* of the script, not a second thing to keep in step with it: the
//! wires the script typed are the wires the document declares.
//!
//! ```text
//! let mut g = Graph::new();
//! let volume = g.material::<Cube>("volume.rusd", "/World/Volume");
//! let cloud = ops::sample_points(&mut g, out("Samples", "samples.rusd:/World/Samples"),
//!                                SamplePointsIn::new(volume), SamplePointsParams { count: 60, seed: 11 });
//! g.write(Path::new("examples/box_scatter.ops.toml"))?;
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use rusd_core::Value;

use crate::prim::Prim;
use crate::schema::Schema;
use crate::usd::Layer;

/// **Where an operator's product goes**: the node's name in the graph, and the USD it
/// writes — `out("Samples", "samples.rusd:/World/Samples")`. The file half is optional
/// (a bare anchor lets the cook name the product), and the anchor is the prim every
/// port of the operator hangs under.
#[derive(Clone, Debug)]
pub struct Output {
    pub node: String,
    pub file: Option<String>,
    pub anchor: String,
    pub description: Option<String>,
}

/// Split `file.rusd:/World/Anchor` into its two halves, at the `:` whose right side is a
/// prim path taken from the right (a Windows drive letter contains one too).
pub fn out(node: &str, usd: &str) -> Output {
    let (file, anchor) = match usd.rfind(':') {
        Some(i) if usd[i + 1..].starts_with('/') && i > 0 => {
            (Some(usd[..i].to_owned()), usd[i + 1..].to_owned())
        }
        _ => (None, usd.to_owned()),
    };
    Output { node: node.to_owned(), file, anchor, description: None }
}

impl Output {
    /// The node's own description — metadata about the node, never a parameter.
    pub fn desc(mut self, text: &str) -> Self {
        self.description = Some(text.to_owned());
        self
    }

    /// How the document spells this output.
    pub fn reference(&self) -> String {
        match &self.file {
            Some(file) => format!("{file}:{}", self.anchor),
            None => self.anchor.clone(),
        }
    }
}

/// One node of the graph: what runs, what it writes, what it reads, and its parameters.
#[derive(Clone, Debug)]
pub struct Node {
    pub name: String,
    /// The graph's word for the operator — `sample_points`, `instances`, …
    pub kind: String,
    pub output: Output,
    /// The one USD input (`input = "…"`), when the operator reads material.
    pub input: Option<String>,
    /// The declared wires, by the operator's own port names (`[operator.X.inputs]`).
    pub wires: Vec<(String, String)>,
    pub params: BTreeMap<String, Value>,
}

/// The graph under construction, and the material layers the script authors beside it.
#[derive(Default)]
pub struct Graph {
    stage: BTreeMap<String, Value>,
    nodes: Vec<Node>,
    layers: Vec<Layer>,
}

impl Graph {
    pub fn new() -> Self {
        Self::default()
    }

    /// `[stage] default_prim`.
    pub fn default_prim(&mut self, path: &str) -> &mut Self {
        self.stage.insert("default_prim".to_owned(), Value::String(path.to_owned()));
        self
    }

    /// `[stage] seed` — the stage-wide half of every path-keyed RNG stream.
    pub fn seed(&mut self, seed: u64) -> &mut Self {
        self.stage.insert("seed".to_owned(), Value::Int(seed as i64));
        self
    }

    pub fn up_axis(&mut self, axis: &str) -> &mut Self {
        self.stage.insert("up_axis".to_owned(), Value::String(axis.to_owned()));
        self
    }

    pub fn meters_per_unit(&mut self, m: f64) -> &mut Self {
        self.stage.insert("meters_per_unit".to_owned(), Value::Float(m));
        self
    }

    /// **A typed handle into a USD the script did not author.** The type is a claim
    /// about that file, and the cook checks it against the prim it reads; a handle the
    /// script authored itself ([`Self::layer`]) is true by construction.
    pub fn material<T: Schema>(&self, file: &str, path: &str) -> Prim<T> {
        Prim::in_file(file, path)
    }

    /// The material layer named `file`, created empty on first use. What it authors is
    /// USD nesting: every prim a typed handle, every ancestor an `Xform`.
    pub fn layer(&mut self, file: &str) -> &mut Layer {
        if let Some(at) = self.layers.iter().position(|l| l.file() == file) {
            return &mut self.layers[at];
        }
        self.layers.push(Layer::new(file));
        self.layers.last_mut().expect("just pushed")
    }

    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    pub fn layers(&self) -> &[Layer] {
        &self.layers
    }

    /// Append one node. The operator functions are the only callers.
    pub(crate) fn push(&mut self, node: Node) {
        self.nodes.push(node);
    }

    /// **The `.ops.toml` this graph is.** Nodes in call order — which is the cook's
    /// dependency order, since a wire can only name a product an earlier call wrote.
    pub fn to_ops_toml(&self) -> String {
        let mut text = String::from(
            "# Generated by a Rusd script (SPEC D15). The operators are Rust functions;\n\
             # this graph is what their calls wrote. Edit the script, not this file.\n\n",
        );
        if !self.stage.is_empty() {
            text.push_str("[stage]\n");
            for (key, value) in &self.stage {
                text.push_str(&format!("{key} = {value}\n"));
            }
            text.push('\n');
        }
        for node in &self.nodes {
            text.push_str(&format!("[operator.{}]\n", node.name));
            text.push_str(&format!("operator = {}\n", quote(&node.kind)));
            if let Some(desc) = &node.output.description {
                text.push_str(&format!("description = {}\n", quote(desc)));
            }
            text.push_str(&format!("output = {}\n", quote(&node.output.reference())));
            if let Some(input) = &node.input {
                text.push_str(&format!("input = {}\n", quote(input)));
            }
            for (key, value) in &node.params {
                text.push_str(&format!("{key} = {value}\n"));
            }
            if !node.wires.is_empty() {
                text.push_str(&format!("\n[operator.{}.inputs]\n", node.name));
                for (slot, wire) in &node.wires {
                    text.push_str(&format!("{slot} = {}\n", quote(wire)));
                }
            }
            text.push('\n');
        }
        text
    }

    /// Write the graph to `ops_path`, and every material layer beside it — the directory
    /// the graph's bare file names resolve against.
    pub fn write(&self, ops_path: &Path) -> std::io::Result<()> {
        let dir = ops_path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)?;
        std::fs::write(ops_path, self.to_ops_toml())?;
        for layer in &self.layers {
            layer.write(dir)?;
        }
        Ok(())
    }
}

/// A TOML basic string, minimally escaped.
fn quote(s: &str) -> String {
    let mut out = String::from("\"");
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
