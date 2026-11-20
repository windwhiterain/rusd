//! Schema registry (SPEC §3): typed property definitions for builtin and
//! generator schemas. The registry is the single source of truth for validation
//! and (later) coercion / USD schema mapping. Unknown schemas are tolerated
//! (forward compatible, like USD's schema-less prims) with a warning.

/// A property's value type, driving validation and coercion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropType {
    Bool,
    I64,
    U64,
    F64,
    String,
    Token,
    Path,
    Asset,
    Vec2,
    Vec3,
    /// `point3f`: a position, as `Xformable`'s `xformOp:translate` is authored
    /// (v1.9). Its only difference from `Vec3` is the type name it writes —
    /// `point3f` rather than `vector3f` — and a `.rusd` product has to write back
    /// the type it was read as, so the two spellings have to be two types.
    Point3f,
    Vec4,
    /// `point3f[]` / `vector3f[]`: an array of 3-vectors, one USD array value
    /// (D14: a `Points` prim's `points`, which a scatter reads as geometry in).
    Vec3Array,
    /// `int[]`: an array of integers, one USD array value (a `Mesh`'s
    /// `faceVertexCounts` / `faceVertexIndices`).
    IntArray,
    /// `token[]`: `Xformable`'s `xformOpOrder` (v1.9). A list of bare words, which
    /// no other array type may be mistaken for: `value inference` is exactly what
    /// the schema type table exists to replace.
    TokenArray,
    /// `color3f[]`: `primvars:displayColor` (v1.9), one colour per element.
    Color3Array,
    Color3f,
    Array,
    Dict,
    Distribution,
    Any,
}

impl PropType {
    /// Type name used in diagnostics and schema declarations.
    pub fn name(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::I64 => "i64",
            Self::U64 => "u64",
            Self::F64 => "f64",
            Self::String => "string",
            Self::Token => "token",
            Self::Path => "path",
            Self::Asset => "asset",
            Self::Vec2 => "vec2",
            Self::Vec3 => "vec3",
            Self::Point3f => "point3f",
            Self::Vec4 => "vec4",
            Self::Vec3Array => "vec3[]",
            Self::IntArray => "int[]",
            Self::TokenArray => "token[]",
            Self::Color3Array => "color3f[]",
            Self::Color3f => "color3f",
            Self::Array => "array",
            Self::Dict => "dict",
            Self::Distribution => "distribution",
            Self::Any => "any",
        }
    }
}

/// One property definition.
#[derive(Clone, Copy, Debug)]
pub struct PropDef {
    pub name: &'static str,
    pub ty: PropType,
    pub required: bool,
}

/// Whether a schema is a builtin scene schema or a Rusd generator node type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchemaKind {
    Builtin,
    Generator,
}

/// A schema definition: its name, kind, and property table.
#[derive(Clone, Copy, Debug)]
pub struct SchemaDef {
    pub name: &'static str,
    pub kind: SchemaKind,
    pub props: &'static [PropDef],
}

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

static XFORM: &[PropDef] = &[p("kind", Token), p("active", Bool), p("instanceable", Bool)];
/// The attributes **every** prim carries whatever its schema: `UsdGeomXformable`'s
/// `xformOp*` and `UsdGeomImageable`'s `visibility` (v1.9).
///
/// They are not on `XFORM` because they are not the `Xform` schema's: an instance
/// prim authored under a `references` arc has **no** `typeName` of its own (its type
/// comes through the arc), and it still carries a transform. Keeping them in a second
/// table is what lets a `.rusd` product write its own transform back with the type it
/// was read as instead of guessing it from the value's shape.
static COMMON: &[PropDef] = &[
    p("xformOp:translate", Point3f),
    p("xformOp:scale", Vec3),
    p("xformOp:rotateXYZ", Vec3),
    p("xformOpOrder", TokenArray),
    p("visibility", Token),
    p("primvars:displayColor", Color3Array),
];
static SPHERE: &[PropDef] = &[p("radius", F64), p("extent", Array)];
static CUBE: &[PropDef] = &[p("size", F64), p("extent", Array)];
static CYLINDER: &[PropDef] = &[p("radius", F64), p("height", F64)];
static MESH: &[PropDef] = &[
    // D14.1: a mesh had no usable surface spelling before this — `points` as a
    // plain `Array` coerced to nothing (`[[x,y,z], …]` is not a flat number list)
    // and the indices went to `int64[]`, which the mesh reader does not accept. A
    // mesh a scatter can sample has to be authorable first.
    p("points", Vec3Array),
    p("faceVertexCounts", IntArray),
    p("faceVertexIndices", IntArray),
    p("normals", Array),
    p("extent", Array),
];
// D14 (geometry in, geometry out): a bare point cloud. `UsdGeom.Points` in USD;
// the renderer draws each point as a small marker so a pipeline prefix that ends
// before a scatter still shows something.
static POINTS: &[PropDef] = &[
    p("points", Vec3Array),
    p("widths", Array),
    p("ids", Array),
    p("extent", Array),
];
static CAMERA: &[PropDef] = &[
    p("fov", F64),
    p("position", Vec3),
    p("look_at", Vec3),
    p("focal_length", F64),
];
static MATERIAL: &[PropDef] = &[
    p("diffuse", Color3f),
    p("roughness", F64),
    p("metallic", F64),
];
static DIRECTIONAL_LIGHT: &[PropDef] = &[p("intensity", F64), p("color", Color3f), p("angle", F64)];
static POINT_LIGHT: &[PropDef] = &[p("intensity", F64), p("color", Color3f), p("radius", F64)];

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
// Recognized generator names whose detailed params land in later milestones.
static GENERIC_GENERATOR: &[PropDef] = &[p("seed", U64), p("max_prims", U64)];
static GRID: &[PropDef] = &[
    p("count_x", U64),
    p("count_y", U64),
    p("spacing", F64),
    p("radius", F64),
    p("seed", U64),
];

/// The type `name` is authored with under `schema`: the schema's own table first,
/// then [`COMMON`]. `None` means the schema table does not know the name — a
/// `.rusd` writer must then **not write it** (the document carries values, and a
/// value the reader has no type for is a value that would come back as a different
/// type).
pub fn prop_type(schema: &str, name: &str) -> Option<PropType> {
    let own = lookup(schema).and_then(|s| {
        s.props
            .iter()
            .find(|p| p.name == name)
            .map(|p| p.ty)
    });
    own.or_else(|| {
        COMMON
            .iter()
            .find(|p| p.name == name)
            .map(|p| p.ty)
    })
}

/// Look up a schema by name. `None` means unknown (tolerated with a warning).
pub fn lookup(name: &str) -> Option<SchemaDef> {
    let (sname, kind, props): (&'static str, SchemaKind, &'static [PropDef]) = match name {
        "Xform" => ("Xform", SchemaKind::Builtin, XFORM),
        "Sphere" => ("Sphere", SchemaKind::Builtin, SPHERE),
        "Cube" => ("Cube", SchemaKind::Builtin, CUBE),
        "Cylinder" => ("Cylinder", SchemaKind::Builtin, CYLINDER),
        "Mesh" => ("Mesh", SchemaKind::Builtin, MESH),
        "Camera" => ("Camera", SchemaKind::Builtin, CAMERA),
        "Material" => ("Material", SchemaKind::Builtin, MATERIAL),
        "DirectionalLight" => ("DirectionalLight", SchemaKind::Builtin, DIRECTIONAL_LIGHT),
        "PointLight" => ("PointLight", SchemaKind::Builtin, POINT_LIGHT),
        "Points" => ("Points", SchemaKind::Builtin, POINTS),
        "RusdScatter" => ("RusdScatter", SchemaKind::Generator, SCATTER),
        "RusdSamplePoints" => ("RusdSamplePoints", SchemaKind::Generator, SAMPLE_POINTS),
        "RusdInstances" => ("RusdInstances", SchemaKind::Generator, INSTANCES),
        "RusdStreetGrid" => ("RusdStreetGrid", SchemaKind::Generator, STREET_GRID),
        "RusdBlockFill" => ("RusdBlockFill", SchemaKind::Generator, BLOCK_FILL),
        "RusdBuilding" => ("RusdBuilding", SchemaKind::Generator, BUILDING),
        "RusdGrid" => ("RusdGrid", SchemaKind::Generator, GRID),
        "RusdInstance" => ("RusdInstance", SchemaKind::Generator, GENERIC_GENERATOR),
        "RusdLSystem" => ("RusdLSystem", SchemaKind::Generator, GENERIC_GENERATOR),
        "RusdTerrain" => ("RusdTerrain", SchemaKind::Generator, GENERIC_GENERATOR),
        "RusdWfc" => ("RusdWfc", SchemaKind::Generator, GENERIC_GENERATOR),
        _ => return None,
    };
    Some(SchemaDef {
        name: sname,
        kind,
        props,
    })
}
