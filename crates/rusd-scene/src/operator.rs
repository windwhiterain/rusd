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
//!
//! A node's slot constraint is the same two halves a USD prim's type has: the typed
//! schema it must be (the class) and the API schemas it must have applied (the
//! interfaces), which is what [`SlotSchema`] spells and [`unmet`] answers.

use crate::schema::PropDef;

pub use crate::schema::PropType;

use openusd::tf;
use openusd::usd::{Prim, SchemaRegistry};

/// **What one port's prim must be** — a USD type, and the API schemas on it.
///
/// A prim's type is not one name: it is the `typeName` **and** the API schemas applied
/// to it — a class and its interfaces. `IsA` answers the class half and the prim's
/// applied-schema list the interface half, so a prim claim is the pair:
///
/// ```text
/// SlotSchema { ty: "Xformable", apis: &["MaterialBindingAPI"] }
/// ```
///
/// `ty` is a typed schema, concrete or abstract, and a prim satisfies it by **being** it
/// or deriving from it; an empty `ty` claims nothing about the prim's own type (an
/// instance prim authors none — its type composes through its arc).
#[derive(Clone, Copy, Debug)]
pub struct SlotSchema {
    /// The typed schema — the class.
    pub ty: &'static str,
    /// The applied API schemas — the interfaces. Empty when the port asks for none.
    pub apis: &'static [&'static str],
}

/// **The shape of one port's payload** — what an operator reads or writes.
///
/// An operator is a typed node: it has named input ports and named output ports, and
/// each port carries a payload of some shape. A shape composes, so a port can be a
/// struct of named parts, a list of any number, an array of exactly *n*, one prim, or a
/// leaf value — and any of these can hold any of the others, as deep as the operator
/// needs:
///
/// ```text
/// blocks: List(Struct([ lots: List(Prim(Cube)) ]))    // a list of blocks, each a
///                                                     // list of lots
/// ```
///
/// This is the **operator's** schema, not a prim's: the `Prim` leaf says what USD type
/// the port's prim must be, and the composition around it says how many parts there are
/// and how they nest. Nothing here describes a prim's attributes; a shape says what
/// flows, and the one who reads the flow checks it.
#[derive(Clone, Copy, Debug)]
pub enum Shape {
    /// One prim: the type it must be, and the API schemas it must have.
    Prim(SlotSchema),
    /// A leaf value, in the document's own type system — `point3f[]`, `token`, `f64`.
    Value(PropType),
    /// **A struct**: named parts, each its own shape. On the wire a part is a child
    /// prim; a name ending in `*` is a prefix and matches every part that starts with
    /// it, whose shape must then be a [`Shape::List`].
    Struct(&'static [Port]),
    /// **A list**: any number of `shape`. The length is the data's, not the schema's.
    List(&'static Shape),
    /// **An array**: exactly `len` of `shape`.
    Array { len: u32, of: &'static Shape },
}

/// One named port of an operator, and the shape of what it carries.
#[derive(Clone, Copy, Debug)]
pub struct Port {
    /// The port's own name — `domain`, `points`, `prototype`, `blocks`.
    pub name: &'static str,
    pub shape: Shape,
}

/// The ports `operator` reads.
pub fn inputs(operator: &str) -> &'static [Port] {
    match operator {
        "RusdSamplePoints" => SAMPLE_POINTS_INPUTS,
        "RusdInstances" => INSTANCES_INPUTS,
        _ => &[],
    }
}

/// The parts `operator` writes under the prim it anchors — its output ports.
pub fn outputs(operator: &str) -> &'static [Port] {
    match operator {
        "RusdSamplePoints" => SAMPLE_POINTS_OUTPUTS,
        "RusdInstances" => INSTANCES_OUTPUTS,
        "RusdScatter" => SCATTER_OUTPUTS,
        "RusdGrid" => GRID_OUTPUTS,
        "RusdStreetGrid" => STREET_GRID_OUTPUTS,
        _ => &[],
    }
}

/// One input port of `operator`, by name.
pub fn input(operator: &str, name: &str) -> Option<&'static Port> {
    inputs(operator).iter().find(|port| port.name == name)
}

/// Which part of a port's shape a wired prim failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Miss {
    /// Its `typeName` is not `declared`, nor any schema derived from it.
    Type {
        declared: &'static str,
        actual: std::string::String,
    },
    /// It does not have `missing` applied.
    Api { missing: &'static str },
    /// The structure is not what the shape says: a named part is missing, a run is the
    /// wrong length, or a part is not a prim at all.
    Shape { message: std::string::String },
}

impl Miss {
    /// One sentence, for a diagnostic.
    pub fn describe(&self) -> std::string::String {
        match self {
            Miss::Type { declared, actual } => {
                format!("declares a `{declared}` but the wire is a `{actual}`")
            }
            Miss::Api { missing } => format!(
                "declares `{missing}` applied, and neither the prim nor its type has it"
            ),
            Miss::Shape { message } => message.clone(),
        }
    }
}

/// **The answer to a prim's claim**, given what the wire actually is: the prim's
/// `typeName` and the API schemas it has, read the way `UsdPrim` reads them.
///
/// `None` means the prim is what the port asked for. Otherwise the first half that
/// failed, so one diagnostic names the thing that is wrong rather than both.
pub fn unmet(
    declared: &SlotSchema,
    actual: &str,
    applied: &[&str],
    registry: &SchemaRegistry,
) -> Option<Miss> {
    if !declared.ty.is_empty()
        && !registry.is_a(&tf::Token::from(actual), &tf::Token::from(declared.ty))
    {
        return Some(Miss::Type {
            declared: declared.ty,
            actual: actual.to_owned(),
        });
    }
    declared
        .apis
        .iter()
        .find(|api| !applied.iter().any(|has| *has == **api))
        .map(|missing| Miss::Api { missing })
}

/// **Does `prim`, and everything under it the shape names, hold what `shape` says?**
///
/// The shape is the operator's; the prims are where it lands. A struct names parts of
/// the prim, a list is any number of them, an array exactly *n*, and a `Prim` leaf
/// checks the type and the API schemas the way `UsdPrim` answers them.
pub fn conform(shape: &Shape, prim: &Prim, registry: &SchemaRegistry) -> Result<(), Miss> {
    match shape {
        Shape::Prim(declared) => {
            let actual = prim
                .type_name()
                .ok()
                .flatten()
                .map(|t| t.as_str().to_owned())
                .unwrap_or_default();
            let applied: Vec<std::string::String> = prim
                .api_schemas()
                .map(|schemas| {
                    schemas
                        .into_iter()
                        .map(|schema| schema.as_str().to_owned())
                        .collect()
                })
                .unwrap_or_default();
            let applied: Vec<&str> = applied.iter().map(|schema| schema.as_str()).collect();
            match unmet(declared, &actual, &applied, registry) {
                None => Ok(()),
                Some(miss) => Err(miss),
            }
        }
        // A leaf value is checked where the value is read; a prim is not one.
        Shape::Value(_) => Ok(()),
        Shape::Struct(fields) => {
            let children = children(prim)?;
            for field in fields.iter() {
                let Some(prefix) = field.name.strip_suffix('*') else {
                    let Some(child) = children.iter().find(|child| leaf(child) == field.name) else {
                        return Err(Miss::Shape {
                            message: format!("`{}` has no part named `{}`", prim.path(), field.name),
                        });
                    };
                    conform(&field.shape, child, registry)?;
                    continue;
                };
                let Shape::List(element) = &field.shape else {
                    return Err(Miss::Shape {
                        message: format!("`{}*` is a run, so its shape must be a list", prefix),
                    });
                };
                let mut matched = 0;
                for child in children.iter().filter(|c| leaf(c).starts_with(prefix)) {
                    matched += 1;
                    conform(element, child, registry)?;
                }
                if matched == 0 {
                    return Err(Miss::Shape {
                        message: format!("`{}` has no part named `{prefix}…`", prim.path()),
                    });
                }
            }
            Ok(())
        }
        Shape::List(element) => {
            for child in children(prim)?.iter() {
                conform(element, child, registry)?;
            }
            Ok(())
        }
        Shape::Array { len, of } => {
            let children = children(prim)?;
            if children.len() != *len as usize {
                return Err(Miss::Shape {
                    message: format!(
                        "`{}` holds {} parts, and the shape says {len}",
                        prim.path(),
                        children.len()
                    ),
                });
            }
            for child in children.iter() {
                conform(of, child, registry)?;
            }
            Ok(())
        }
    }
}

/// The child prims of `prim`, or the reason there are none to look at.
fn children(prim: &Prim) -> Result<Vec<Prim>, Miss> {
    prim.children().map_err(|error| Miss::Shape {
        message: format!("`{}` has no readable parts: {error}", prim.path()),
    })
}

/// The last component of a prim's path — its name.
fn leaf(prim: &Prim) -> &str {
    prim.path().as_str().rsplit('/').next().unwrap_or_default()
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
// `PropType` has a `String` variant, so the glob would shadow the type one function's
// signature needs. Only `Miss` names it, and names it in full.

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

// --- The prims the ports' shapes name -------------------------------------

/// A point cloud — what a sampler writes and what an instances node copies along.
static POINTS: SlotSchema = SlotSchema {
    ty: "Points",
    apis: &[],
};
/// Any prim USD can place: every geometry, light and camera derives from it.
static XFORMABLE: SlotSchema = SlotSchema {
    ty: "Xformable",
    apis: &[],
};
/// A prim that authors **no type of its own**: an instance composes its type through
/// its `reference` arc, so the layer it is written in holds nothing to compare.
static ANY_PRIM: SlotSchema = SlotSchema {
    ty: "",
    apis: &[],
};
static CUBE: SlotSchema = SlotSchema {
    ty: "Cube",
    apis: &[],
};
static SPHERE: SlotSchema = SlotSchema {
    ty: "Sphere",
    apis: &[],
};

// --- What each operator reads and writes ----------------------------------

/// **A sampler reads a `domain`** — the geometry it samples. `Gprim` is the class that
/// covers a `Cube` and a `Mesh`; which of them it can draw from is settled where the
/// geometry is read, because `Points` is a `Gprim` too and has neither a volume nor a
/// surface. What it writes is one `points` prim: a point cloud.
static SAMPLE_POINTS_INPUTS: &[Port] = &[Port {
    name: "domain",
    shape: Shape::Prim(SlotSchema {
        ty: "Gprim",
        apis: &[],
    }),
}];
static SAMPLE_POINTS_OUTPUTS: &[Port] = &[Port {
    name: "points",
    shape: Shape::Prim(POINTS),
}];

/// **An instances node reads** the cloud it copies along and the prim it copies, and
/// writes **a list** of instances — one per point, so how many is the data's to say.
static INSTANCES_INPUTS: &[Port] = &[
    Port {
        name: "points",
        shape: Shape::Prim(POINTS),
    },
    Port {
        name: "prototype",
        shape: Shape::Prim(XFORMABLE),
    },
];
static INSTANCES_OUTPUTS: &[Port] = &[Port {
    name: "i*",
    shape: Shape::List(&Shape::Prim(ANY_PRIM)),
}];

/// **A scatter writes a list of spheres**, one per sample.
static SCATTER_OUTPUTS: &[Port] = &[Port {
    name: "i*",
    shape: Shape::List(&Shape::Prim(SPHERE)),
}];

/// **A grid writes a list of spheres**, one per cell.
static GRID_OUTPUTS: &[Port] = &[Port {
    name: "c*",
    shape: Shape::List(&Shape::Prim(SPHERE)),
}];

/// **A street grid writes a struct**: the ground, and a list of blocks each of which is
/// itself a list of lots — a struct in a list in a struct.
static STREET_GRID_OUTPUTS: &[Port] = &[
    Port {
        name: "Ground",
        shape: Shape::Prim(CUBE),
    },
    Port {
        name: "b*",
        shape: Shape::List(&Shape::Struct(&[Port {
            name: "lot*",
            shape: Shape::List(&Shape::Prim(CUBE)),
        }])),
    },
];

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::usd_registry;

    /// **A slot's constraint has two halves** (the user: 一个 prim 的类型由 type 和一组
    /// schema 共同构成，相当于 class 和 interface). The class half is `IsA`, through the
    /// schema graph; the interface half is the prim's applied schemas.
    #[test]
    fn a_slot_asks_for_a_type_and_its_interfaces() {
        let registry = usd_registry();
        let bare = |ty: &'static str| SlotSchema { ty, apis: &[] };

        // The class half: a subtype passes, a sibling does not.
        assert_eq!(unmet(&bare("Xformable"), "Mesh", &[], registry), None);
        assert_eq!(
            unmet(&bare("Xform"), "Mesh", &[], registry),
            Some(Miss::Type {
                declared: "Xform",
                actual: "Mesh".to_owned()
            })
        );

        // The interface half: the same type, with and without the API schema.
        let bound = SlotSchema {
            ty: "Xformable",
            apis: &["MaterialBindingAPI"],
        };
        assert_eq!(
            unmet(&bound, "Mesh", &["MaterialBindingAPI"], registry),
            None
        );
        assert_eq!(
            unmet(&bound, "Mesh", &[], registry),
            Some(Miss::Api {
                missing: "MaterialBindingAPI"
            })
        );
        // The type is settled first, so one diagnostic names one thing.
        assert_eq!(
            unmet(&bound, "Material", &["MaterialBindingAPI"], registry),
            Some(Miss::Type {
                declared: "Xformable",
                actual: "Material".to_owned()
            })
        );
    }

    /// **The operators' own schemas**, which is where the constraint lives: what each
    /// port reads, and what shape its output takes. A grid reads nothing (it grows out
    /// of its parameters) and writes a list of spheres; an instances node writes a list
    /// of prims that carry no type of their own; a street grid writes a struct holding a
    /// list of blocks, each of which is a list of lots.
    #[test]
    fn the_operators_declare_their_io() {
        let prim_ty = |ports: &'static [Port], name: &str| match input_ty(ports, name) {
            Some(ty) => ty,
            None => "",
        };
        assert_eq!(prim_ty(inputs("RusdSamplePoints"), "domain"), "Gprim");
        assert_eq!(prim_ty(inputs("RusdInstances"), "points"), "Points");
        assert_eq!(prim_ty(inputs("RusdInstances"), "prototype"), "Xformable");
        assert!(inputs("RusdGrid").is_empty());

        // A list is a list, however deep it sits.
        assert!(matches!(outputs("RusdInstances")[0].shape, Shape::List(_)));
        let Shape::List(element) = &outputs("RusdStreetGrid")[1].shape else {
            panic!("blocks are a list");
        };
        let Shape::Struct(lots) = **element else {
            panic!("a block is a struct");
        };
        assert!(matches!(lots[0].shape, Shape::List(_)), "a block holds lots");
    }

    /// The `ty` an input port asks for, if it asks for one prim.
    fn input_ty(ports: &'static [Port], name: &str) -> Option<&'static str> {
        match ports.iter().find(|port| port.name == name)?.shape {
            Shape::Prim(schema) => Some(schema.ty),
            _ => None,
        }
    }
}
