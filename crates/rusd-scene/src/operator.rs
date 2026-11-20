//! The **operator vocabulary**: what a graph node runs, and what it takes.
//!
//! A generator is an *operator*, not a schema. A schema is what a USD layer's
//! `typeName` names; an operator is what a `.ops.toml` node names with
//! `operator = "instances"`, and a graph document authors no prims at all — so no
//! layer ever carries one of these names, and [`crate::schema::lookup`] answers for
//! none of them.
//!
//! The tables are the parameters each operator takes, typed with the same value
//! vocabulary a `.rusd` property uses ([`PropType`]), because both are typed values
//! in a document. They live beside the schema registry rather than in it: one
//! registry answers "what is this prim", this one answers "what does this node run".

use crate::schema::PropDef;

pub use crate::schema::PropType;

const fn p(name: &'static str, ty: PropType) -> PropDef {
    PropDef {
        name,
        ty,
        required: false,
    }
}

const fn req(name: &'static str, ty: PropType) -> PropDef {
    PropDef {
        name,
        ty,
        required: true,
    }
}

use PropType::*;

/// Whether `name` is an operator this build runs.
pub fn is_operator(name: &str) -> bool {
    parameters(name).is_some()
}

/// The parameters `operator` takes, or `None` when no operator carries that name.
pub fn parameters(operator: &str) -> Option<&'static [PropDef]> {
    Some(match operator {
        "RusdScatter" => SCATTER,
        "RusdSamplePoints" => SAMPLE_POINTS,
        "RusdInstances" => INSTANCES,
        "RusdStreetGrid" => STREET_GRID,
        "RusdBlockFill" => BLOCK_FILL,
        "RusdBuilding" => BUILDING,
        "RusdGrid" => GRID,
        "RusdInstance" | "RusdLSystem" | "RusdTerrain" | "RusdWfc" => GENERIC_GENERATOR,
        _ => return None,
    })
}

static SCATTER: &[PropDef] = &[
    p("count", U64),
    p("seed", U64),
    // Same two spellings as `RusdInstances`' (v1.5.8): a prim path in this stack,
    // or `asset:prim` naming a prototype in another layer.
    req("prototype", String),
    // D14 geometry-in: the prim whose `points` become one instance each. Optional —
    // without it the scatter is parametric, exactly as before.
    p("input_points", Path),
    p("domain", Distribution),
    p("scale", Distribution),
    p("yaw", Distribution),
    p("max_prims", U64),
    p("max_triangles", U64),
];

// v1.5: mesh -> points. The first half of the two-op pipeline build; it reads a
// `Mesh`'s surface (never its bare vertices) and authors a `Points` prim in its own
// output layer, so a pipeline prefix stopped after this op shows the cloud and a
// prefix stopped before it shows the mesh.
static SAMPLE_POINTS: &[PropDef] = &[req("input_points", Path), p("count", U64), p("seed", U64)];

// v1.5: points -> instances. The second half: one prototype instance per input
// point, in the cloud's own order. No `count`: the cloud decides how many there
// are, and `max_prims` is what bounds the result (truncated in order + W_BUDGET).
static INSTANCES: &[PropDef] = &[
    // v1.13: **not required** — an operator has one USD input (`input`), and the cloud an
    // instances node consumes is the graph's data flow (the earlier node's `points`).
    p("input_points", Path),
    // v1.5.8: `prototype` also accepts an **asset** spelling,
    // `"prototypes.usda:/World/Prototypes/Box"`, which is not a prim path — the
    // instances then carry a reference across the asset boundary instead of a
    // same-stack one. The registry types the parameter as `string` and lets the
    // generator decide which of the two spellings it got (one diagnostic, naming
    // the value and, for an asset, the path it looked for): a `Path` type would
    // refuse the asset form at validation time, before the cook could say which
    // layer was missing.
    req("prototype", String),
    p("scale", Distribution),
    p("max_prims", U64),
    // v1.5.8: the same asset-or-path spelling on `RusdScatter`'s own prototype.
    // v1.5.1 补正 (parent ruling): `seed` is the repository-wide generator parameter
    // — every instance samples `scale` from a stream keyed by
    // `(stage.seed, prim path, local seed)` (SPEC §5), so the local seed has to be
    // authorable here too. Only `max_prims` bounds the count; there is no `count`.
    p("seed", U64),
];

static STREET_GRID: &[PropDef] = &[
    p("blocks_x", U64),
    p("blocks_y", U64),
    p("block_size", Vec2),
    p("street_width", F64),
    p("avenue_every", U64),
    p("avenue_width", F64),
    p("jitter", F64),
    p("block_prototype", Path),
    p("lot_width", F64),
    p("margin", F64),
    p("occupancy", F64),
    p("height", Distribution),
    p("floor_height", F64),
    p("zone", Token),
    p("ground_material", Path),
    p("max_prims", U64),
    p("seed", U64),
];

static BLOCK_FILL: &[PropDef] = &[
    p("lot_width", F64),
    p("lots", U64),
    p("margin", F64),
    p("occupancy", F64),
    p("height", Distribution),
    p("zone", Token),
    req("building_prototype", Path),
    p("max_prims", U64),
    p("max_triangles", U64),
    p("seed", U64),
];

static BUILDING: &[PropDef] = &[
    p("footprint", Vec2),
    p("floors", U64),
    p("height", F64),
    p("floor_height", F64),
    p("setback_every", U64),
    p("setback_depth", F64),
    p("roof", Token),
    p("facade", Path),
    p("base_color", Color3f),
    p("max_triangles", U64),
    p("seed", U64),
];

// Recognized operator names whose detailed params land in later milestones.
static GENERIC_GENERATOR: &[PropDef] = &[p("seed", U64), p("max_prims", U64)];

static GRID: &[PropDef] = &[
    p("count_x", U64),
    p("count_y", U64),
    p("spacing", F64),
    p("radius", F64),
    p("seed", U64),
];
