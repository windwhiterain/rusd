//! Generator abstraction (SPEC §5, D5/D9): a generator is a pure layer-op.
//! It reads composed inputs (tracked), computes, and authors a new output layer.
//! Composition operations themselves belong to openusd pcp, not here (D9).

use std::collections::BTreeMap;

use openusd::sdf::{self, Value as SdfValue};
use openusd::usd::Stage;
use rusd_core::{Diagnostic, PrimPath, Value};

/// Read a composed `double` attribute, returning 0.0 when absent.
pub fn read_attr_f64(stage: &Stage, prim: &str, attr: &str) -> f64 {
    let Ok(p) = sdf::path(prim) else { return 0.0 };
    let Ok(pr) = stage.prim(p) else { return 0.0 };
    pr.attribute(attr)
        .get::<f64>()
        .ok()
        .flatten()
        .unwrap_or(0.0)
}

/// Read a composed attribute as a raw USD value (for hashing / inspection).
pub fn read_attr_value(stage: &Stage, prim: &str, attr: &str) -> Option<SdfValue> {
    let p = sdf::path(prim).ok()?;
    let pr = stage.prim(p).ok()?;
    pr.attribute(attr).get::<SdfValue>().ok().flatten()
}

/// Read a prim's authored `points` (`point3f[]` / `point3d[]`) in **world** space:
/// the cloud under the prim's composed world transform, ancestors included.
///
/// This is D14's geometry-in reader: a generator that consumes geometry reads
/// points where they actually are, not where the input layer wrote them.
/// Returns `None` when the prim does not exist or authors no `points`; otherwise
/// the cloud, one `[x, y, z]` per point.
pub fn read_world_points(stage: &Stage, prim: &str) -> Option<Vec<[f64; 3]>> {
    rusd_scene::geometry::world_points(stage, prim)
}

/// The declared-read name for **a prim's composed world transform** (D14
/// geometry-in). It is not an authored attribute; it is what a generator records
/// when its output depends on where an input prim *is* rather than only on its
/// authored values, so the cook memo re-runs the node when a parent moves.
pub const WORLD_XFORM_READ: &str = "world_xform";

/// The declared-read name for **a prim's tessellated world-space triangles**
/// (D14.1 geometry-in). Like [`WORLD_XFORM_READ`] it is not authored: it is the
/// dependency a surface scatter really has — the *surface*, after topology and
/// every ancestor transform.
pub const SURFACE_READ: &str = "surface";

/// The value a declared read stands for, for the cook memo key (D14):
///
/// - `world_xform` is the prim's composed world matrix;
/// - any other name is that attribute's composed value.
///
/// The distinction matters because `points` is authored in the prim's **local**
/// space: two documents whose point clouds are identical but whose parents moved
/// author the same `points` and different geometry. Hashing the composed attribute
/// alone would memo-hit and leave the instances where they used to be, which
/// breaks incremental ≡ full.
pub fn read_value_for_memo(stage: &Stage, prim: &str, attr: &str) -> String {
    if attr == WORLD_XFORM_READ {
        return match rusd_scene::geometry::world_xform_of(stage, prim) {
            Some(m) => m.map(|v| format!("{v:.6}")).join(","),
            None => "none".to_owned(),
        };
    }
    if attr == SURFACE_READ {
        return match rusd_scene::geometry::world_triangles(stage, prim) {
            Some(tris) => tris
                .iter()
                .flat_map(|t| t.iter())
                .map(|p| format!("{:.6},{:.6},{:.6}", p[0], p[1], p[2]))
                .collect::<Vec<_>>()
                .join(";"),
            None => "none".to_owned(),
        };
    }
    if attr == rusd_scene::geometry::POINTS_ATTR {
        return match read_world_points(stage, prim) {
            Some(pts) => pts
                .iter()
                .map(|p| format!("{:.6},{:.6},{:.6}", p[0], p[1], p[2]))
                .collect::<Vec<_>>()
                .join(";"),
            None => "none".to_owned(),
        };
    }
    match read_attr_value(stage, prim, attr) {
        Some(v) => format!("{v:?}"),
        None => "none".to_owned(),
    }
}

/// The context handed to a generator evaluation: read-only composed inputs
/// (with read tracking), the node's params, path-keyed seed, and a warning sink.
pub struct GenContext<'a> {
    stage: &'a Stage,
    /// The generator node's anchor prim path.
    pub path: &'a PrimPath,
    /// The generator's parameters (from the `.rusd` prim table).
    pub params: &'a BTreeMap<String, Value>,
    /// The node's local seed (the `seed` property).
    pub seed: u64,
    /// The stage-wide seed (for path-keyed RNG streams).
    pub stage_seed: u64,
    reads: Vec<(String, String)>,
    warnings: Vec<Diagnostic>,
}

impl<'a> GenContext<'a> {
    pub fn new(
        stage: &'a Stage,
        path: &'a PrimPath,
        params: &'a BTreeMap<String, Value>,
        seed: u64,
        stage_seed: u64,
    ) -> Self {
        Self {
            stage,
            path,
            params,
            seed,
            stage_seed,
            reads: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// The composed stage to read inputs from (read-only).
    pub fn stage(&self) -> &Stage {
        self.stage
    }

    /// Read a composed `double` input, recording the dependency.
    pub fn read_f64(&mut self, prim: &str, attr: &str) -> f64 {
        self.reads.push((prim.to_owned(), attr.to_owned()));
        read_attr_f64(self.stage, prim, attr)
    }

    /// Read a prim's authored point cloud as **geometry in** (D14), in world
    /// space, recording `(prim, "points")` as the dependency so an edit to those
    /// points re-cooks this node and nothing else.
    pub fn read_points(&mut self, prim: &str) -> Option<Vec<[f64; 3]>> {
        self.reads.push((prim.to_owned(), "points".to_owned()));
        read_world_points(self.stage, prim)
    }

    /// A prim's tessellated **world-space triangles** as an input (D14.1): a
    /// surface a scatter can sample. Recorded as [`SURFACE_READ`], whose memo value
    /// is the triangles themselves — moving a parent moves every one of them.
    pub fn read_triangles(&mut self, prim: &str) -> Option<Vec<rusd_scene::geometry::Triangle>> {
        self.reads.push((prim.to_owned(), SURFACE_READ.to_owned()));
        rusd_scene::geometry::world_triangles(self.stage, prim)
    }

    /// A prim's composed world transform as an **input** (D14): recorded as
    /// [`WORLD_XFORM_READ`], so the memo re-runs the node when the prim or any
    /// ancestor moves.
    pub fn read_world_xform(&mut self, prim: &str) -> Option<rusd_scene::Mat> {
        self.reads
            .push((prim.to_owned(), WORLD_XFORM_READ.to_owned()));
        rusd_scene::geometry::world_xform_of(self.stage, prim)
    }

    /// The actual reads performed during this evaluation (for precise memo keys).
    pub fn tracked_reads(&self) -> &[(String, String)] {
        &self.reads
    }

    /// Emit a non-fatal warning (e.g. budget truncation) alongside a successful eval.
    pub fn warn(&mut self, diag: Diagnostic) {
        self.warnings.push(diag);
    }

    /// Drain accumulated warnings (called by the engine after eval).
    pub fn take_warnings(&mut self) -> Vec<Diagnostic> {
        std::mem::take(&mut self.warnings)
    }

    pub fn param_f64(&self, name: &str, default: f64) -> f64 {
        match self.params.get(name) {
            Some(Value::Float(f)) => *f,
            Some(Value::Int(i)) => *i as f64,
            _ => default,
        }
    }

    pub fn param_u64(&self, name: &str, default: u64) -> u64 {
        match self.params.get(name) {
            Some(Value::Int(i)) if *i >= 0 => *i as u64,
            _ => default,
        }
    }

    pub fn param_str(&self, name: &str) -> Option<&str> {
        match self.params.get(name) {
            Some(Value::String(s)) => Some(s.as_str()),
            _ => None,
        }
    }

    /// A `path` parameter, which the surface spells as a string (SPEC §2/§3).
    pub fn param_path(&self, name: &str) -> Option<&str> {
        self.param_str(name)
    }

    pub fn param_vec2(&self, name: &str, default: [f64; 2]) -> [f64; 2] {
        match self.params.get(name) {
            Some(Value::Array(items)) if items.len() == 2 => {
                let a = num(&items[0]).unwrap_or(default[0]);
                let b = num(&items[1]).unwrap_or(default[1]);
                [a, b]
            }
            _ => default,
        }
    }
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Float(f) => Some(*f),
        Value::Int(i) => Some(*i as f64),
        _ => None,
    }
}

/// A generator type: a pure function from (params, seed, composed inputs) to a
/// new output layer. Registered by name in the cook engine.
pub trait Generator {
    /// The schema name this generator handles (e.g. "RusdGrid").
    fn name(&self) -> &'static str;
    /// Produce the output layer. Errors are returned as diagnostics, never panic.
    fn eval(&self, ctx: &mut GenContext) -> Result<sdf::Layer, Vec<Diagnostic>>;
}
