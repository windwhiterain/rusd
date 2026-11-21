//! Schema registry (SPEC §3): typed property definitions for builtin and
//! generator schemas. The registry is the single source of truth for validation
//! and (later) coercion / USD schema mapping. Unknown schemas are tolerated
//! (forward compatible, like USD's schema-less prims) with a warning.
//!
//! Two registries live here, and they answer two different questions. This
//! module's tables answer what a schema's properties are ([`prop_type`]);
//! [`usd_registry`] is USD's own [`SchemaRegistry`], built from the schema data in
//! `schemas/`, and answers what one schema **is** — the subtype relation
//! [`SchemaRegistry::is_a`] walks. A table cannot answer the second question, and
//! a walk cannot type a value, so neither replaces the other.

use std::sync::{Arc, OnceLock};

use openusd::usd::{FamilySource, SchemaRegistry, SchemaRegistryBuilder};

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
    /// A path, or a non-empty list of paths (list input): one prim reference or
    /// several, each a `path`. The sampler's `domain` and the instances node's
    /// `points` take this — wire one geometry, or a list of them.
    PathArray,
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
            Self::PathArray => "path[]",
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

/// A schema definition: its name and property table.
#[derive(Clone, Copy, Debug)]
pub struct SchemaDef {
    pub name: &'static str,
    pub props: &'static [PropDef],
}

const fn p(name: &'static str, ty: PropType) -> PropDef {
    PropDef {
        name,
        ty,
        required: false,
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
    let (sname, props): (&'static str, &'static [PropDef]) = match name {
        "Xform" => ("Xform", XFORM),
        "Sphere" => ("Sphere", SPHERE),
        "Cube" => ("Cube", CUBE),
        "Cylinder" => ("Cylinder", CYLINDER),
        "Mesh" => ("Mesh", MESH),
        "Camera" => ("Camera", CAMERA),
        "Material" => ("Material", MATERIAL),
        "DirectionalLight" => ("DirectionalLight", DIRECTIONAL_LIGHT),
        "PointLight" => ("PointLight", POINT_LIGHT),
        "Points" => ("Points", POINTS),
        _ => return None,
    };
    Some(SchemaDef {
        name: sname,
        props,
    })
}

/// **The schema graph, as USD's own registry holds it.**
///
/// A `.rusd` document authors a USD layer, so the `typeName` in it means what USD
/// says it means, and "is this input the schema the operator declared?" is
/// [`SchemaRegistry::is_a`] — C++ `UsdSchemaRegistry::IsA`, the walk
/// [`UsdPrim::is_a`](openusd::usd::Prim::is_a) answers with. That walk reads data,
/// not code: per schema identifier its kind and its *direct* bases, which is what
/// `plugInfo.json` carries for a C++ schema library, beside a `generatedSchema.usda`
/// of class prims. `schemas/rusd_types_{manifest,schematics}.usda` is that pair, so
/// the subtype relation is the registry's and there is no second table of it.
///
/// The family carries the relation alone: what a schema's attributes are, and what
/// their fallbacks are, stays in the tables above, which is what [`prop_type`]
/// answers from. Reading a fallback from two sources would be two answers to one
/// question.
///
pub fn usd_registry() -> &'static Arc<SchemaRegistry> {
    static REGISTRY: OnceLock<Arc<SchemaRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        // Both halves are compiled in and parsed once, exactly as the layers a
        // C++ library resolves from disk would be: the registry reads them at
        // build time and keeps what they hold.
        let manifest = layer("rusd_types_manifest.usda", MANIFEST);
        let schematics = layer("rusd_types_schematics.usda", SCHEMATICS);
        SchemaRegistryBuilder::default()
            .family(FamilySource {
                name: "rusd",
                manifest: &manifest,
                schematics: &schematics,
            })
            .build()
            .expect("the schema family registers")
    })
}

const MANIFEST: &str = include_str!("../schemas/rusd_types_manifest.usda");
const SCHEMATICS: &str = include_str!("../schemas/rusd_types_schematics.usda");

/// One of the family's two layers, over the text compiled in above.
fn layer(identifier: &str, text: &str) -> openusd::sdf::Layer {
    openusd::sdf::Layer::from_bytes(identifier, text.as_bytes().to_vec())
        .unwrap_or_else(|error| panic!("{identifier} parses: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_a(actual: &str, base: &str) -> bool {
        usd_registry().is_a(
            &openusd::tf::Token::from(actual),
            &openusd::tf::Token::from(base),
        )
    }

    /// **The subtype relation is the one USD's schema data declares** (the user: 就用 USD
    /// 本身的 subtype 机制判断). Every answer below is `UsdSchemaRegistry::IsA`: the walk up
    /// `bases`, read out of the manifest, crossing into the core family at `Typed`.
    #[test]
    fn is_a_walks_the_usd_schema_graph() {
        // A schema is itself.
        assert!(is_a("Mesh", "Mesh"));
        // …and everything its chain names, however far up: Mesh → PointBased → Gprim →
        // Boundable → Xformable → Imageable → Typed.
        assert!(is_a("Mesh", "PointBased"));
        assert!(is_a("Mesh", "Gprim"));
        assert!(is_a("Mesh", "Xformable"));
        assert!(is_a("Mesh", "Typed"), "the chain reaches the core family");
        assert!(is_a("Points", "Gprim"));
        assert!(is_a("Cube", "Boundable"));
        // **Siblings are not bases**: `Mesh` and `Xform` both hang off `Xformable`, and
        // that is the difference a string comparison cannot see.
        assert!(!is_a("Mesh", "Xform"));
        assert!(!is_a("Xform", "Gprim"));
        // A material is neither geometry nor a transform, which is what a slot declared
        // `Xformable` has to reject.
        assert!(!is_a("Material", "Xformable"));
        assert!(is_a("Material", "NodeBase"));
        // Nothing derives from a name the data does not hold — not even itself.
        assert!(!is_a("NoSuchSchema", "Typed"));
    }

    /// An abstract schema is a class, not a type a prim may author — which is why a slot
    /// may *declare* `Xformable` while no wire can be one.
    #[test]
    fn abstract_schemas_are_not_concrete_types() {
        let registry = usd_registry();
        let concrete = |name: &str| {
            registry.is_concrete_type(&openusd::tf::Token::from(name))
        };
        assert!(concrete("Mesh"));
        assert!(concrete("Xform"));
        assert!(!concrete("Xformable"));
        assert!(!concrete("Gprim"));
    }
}

