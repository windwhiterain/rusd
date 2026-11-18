//! v1.5 two-op pipeline wiring for `nodes_from_doc` (docs/PROTOCOL.md v1.5.1):
//! the reads and writes each of the two ops declares, which is what makes the
//! implicit cook DAG (D8) order `sample -> instances` and the memo exact.
//!
//! The rule the memo has to satisfy is "incremental ≡ full" (D2): a re-cook of a
//! changed node must produce what a cold cook of the final document produces. That
//! only holds if every value a node's output actually depends on is declared, so the
//! declarations below are the whole dependency, not a sample of it:
//!
//! - the **input geometry**, in the form each op reads (a surface for the sample, a
//!   cloud for the instances), whose memo value is the world-space geometry itself;
//! - the **anchor's world transform**, because both ops write their output in the
//!   anchor's local space — moving the node, or any parent of it, moves everything it
//!   authored;
//! - for the instances, the **prototype's `radius`**: the instances compose the
//!   prototype through an arc, so a change to the prototype's own numbers has to
//!   reach them.
//!
//! Writes: a sample op owns its anchor **and** the cloud prim it authors
//! ([`crate::pipeline::points_path`]); that is the path an instances op reads, so it
//! is what makes the second node depend on the first.

use rusd_core::Value;

use crate::generator::{SURFACE_READ, WORLD_XFORM_READ};
use crate::pipeline::points_path;

/// One `path`-typed parameter as a string (the surface spells paths as strings).
fn path_param(props: &std::collections::BTreeMap<String, Value>, name: &str) -> Option<String> {
    match props.get(name) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Path(p)) => Some(p.as_str().to_owned()),
        _ => None,
    }
}

/// The declared reads of a generator node, by schema name.
pub(crate) fn reads_for(
    schema: &str,
    props: &std::collections::BTreeMap<String, Value>,
    anchor: &str,
) -> Vec<(String, String)> {
    let mut reads: Vec<(String, String)> = Vec::new();
    let input = path_param(props, "input_points");
    match schema {
        // v1.5: mesh -> points. A surface read only: the op samples a `Mesh`'s
        // triangles, and its bare vertices are deliberately **not** an input (taking
        // them would place points on vertices, which is different geometry).
        "RusdSamplePoints" => {
            if let Some(p) = input {
                reads.push((p, SURFACE_READ.to_owned()));
                reads.push((anchor.to_owned(), WORLD_XFORM_READ.to_owned()));
            }
        }
        // v1.5: points -> instances. A cloud read, plus the prototype's radius (the
        // arc composes the prototype's values into every instance).
        "RusdInstances" => {
            if let Some(p) = input {
                reads.push((p, rusd_scene::geometry::POINTS_ATTR.to_owned()));
                reads.push((anchor.to_owned(), WORLD_XFORM_READ.to_owned()));
            }
            if let Some(p) = path_param(props, "prototype") {
                reads.push((p, "radius".to_owned()));
            }
        }
        _ => {}
    }
    reads
}

/// The prim paths a generator node's output layer authors, by schema name.
pub(crate) fn writes_for(schema: &str, anchor: &str) -> Vec<String> {
    match schema {
        // The anchor itself, plus the cloud prim: the latter is the path the
        // downstream instances node names as its `input_points`, so this is the edge
        // that orders the two ops.
        "RusdSamplePoints" => vec![anchor.to_owned(), points_path(anchor)],
        _ => vec![anchor.to_owned()],
    }
}
