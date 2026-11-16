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

/// The context handed to a generator evaluation: read-only composed inputs
/// (with read tracking), the node's params and path-keyed seed.
pub struct GenContext<'a> {
    stage: &'a Stage,
    /// The generator node's anchor prim path.
    pub path: &'a PrimPath,
    /// The generator's parameters (from the `.rusd` prim table).
    pub params: &'a BTreeMap<String, Value>,
    /// Path-keyed seed: hash(stage.seed, prim_path, local_seed).
    pub seed: u64,
    reads: Vec<(String, String)>,
}

impl<'a> GenContext<'a> {
    pub fn new(
        stage: &'a Stage,
        path: &'a PrimPath,
        params: &'a BTreeMap<String, Value>,
        seed: u64,
    ) -> Self {
        Self {
            stage,
            path,
            params,
            seed,
            reads: Vec::new(),
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

    /// The actual reads performed during this evaluation (for precise memo keys).
    pub fn tracked_reads(&self) -> &[(String, String)] {
        &self.reads
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
}

/// A generator type: a pure function from (params, seed, composed inputs) to a
/// new output layer. Registered by name in the cook engine.
pub trait Generator {
    /// The schema name this generator handles (e.g. "RusdGrid").
    fn name(&self) -> &'static str;
    /// Produce the output layer. Errors are returned as diagnostics, never panic.
    fn eval(&self, ctx: &mut GenContext) -> Result<sdf::Layer, Vec<Diagnostic>>;
}
