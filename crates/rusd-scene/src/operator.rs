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
use openusd::usd::SchemaRegistry;

/// **What a slot asks of the prim wired to it** — a USD type, and the API schemas on it.
///
/// A USD prim's type is not one name: it is the `typeName` **and** the API schemas
/// applied to it — a class and its interfaces. `UsdPrim::IsA` answers the class half
/// and `UsdPrim::HasAPI` the interface half, so a slot's constraint is the pair, not a
/// single schema name:
///
/// ```text
/// SlotSchema { ty: "Xformable", apis: &["MaterialBindingAPI"] }
/// ```
///
/// `ty` is a typed schema, concrete or abstract, and a prim satisfies it by **being**
/// it or deriving from it. `apis` are applied API schemas; a prim satisfies each one
/// by having it applied — authored in `apiSchemas`, or built in by the schema that
/// types it, which is what `UsdPrim::GetAppliedSchemas` reports.
#[derive(Clone, Copy, Debug)]
pub struct SlotSchema {
    /// The typed schema — the class.
    pub ty: &'static str,
    /// The applied API schemas — the interfaces. Empty when the slot asks for none.
    pub apis: &'static [&'static str],
}

/// One slot an operator reads, and what it asks of the USD wired to it.
#[derive(Clone, Copy, Debug)]
pub struct Slot {
    /// The slot's own name — `domain`, `points`, `prototype` — which is also the name
    /// the product authors and the tree prints.
    pub name: &'static str,
    pub schema: SlotSchema,
}

/// The slots `operator` reads. An operator with no USD input has none.
pub fn slots(operator: &str) -> &'static [Slot] {
    match operator {
        "RusdSamplePoints" => SAMPLE_POINTS_SLOTS,
        "RusdInstances" => INSTANCES_SLOTS,
        _ => &[],
    }
}

/// One slot of `operator`, by the name the product and the tree use.
pub fn slot(operator: &str, name: &str) -> Option<&'static Slot> {
    slots(operator).iter().find(|slot| slot.name == name)
}

/// Which half of a slot's constraint a wired prim failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Miss {
    /// Its `typeName` is not `declared`, nor any schema derived from it.
    Type {
        declared: &'static str,
        actual: std::string::String,
    },
    /// It does not have `missing` applied.
    Api { missing: &'static str },
}

/// **The answer to a slot's constraint**, given what the wire actually is: the prim's
/// `typeName` and the API schemas it has, read the way `UsdPrim` reads them.
///
/// `None` means the prim is what the slot asked for. Otherwise the first half that
/// failed, so one diagnostic names the thing that is wrong rather than both.
pub fn unmet(
    declared: &SlotSchema,
    actual: &str,
    applied: &[&str],
    registry: &SchemaRegistry,
) -> Option<Miss> {
    if !registry.is_a(&tf::Token::from(actual), &tf::Token::from(declared.ty)) {
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

/// **What a sampler reads**: a `domain` — the geometry it samples. `Gprim` is the
/// class that covers a `Cube` and a `Mesh`; which of them it can draw from is settled
/// where the geometry is read, because `Points` is a `Gprim` too and has neither a
/// volume nor a surface.
static SAMPLE_POINTS_SLOTS: &[Slot] = &[Slot {
    name: "domain",
    schema: SlotSchema {
        ty: "Gprim",
        apis: &[],
    },
}];

/// **What an instances node reads**: the cloud it copies along, and the prim it copies.
/// The cloud is a `Points`; the prototype is anything USD can place — an `Xformable`,
/// which every geometry, light and camera derives from.
static INSTANCES_SLOTS: &[Slot] = &[
    Slot {
        name: "points",
        schema: SlotSchema {
            ty: "Points",
            apis: &[],
        },
    },
    Slot {
        name: "prototype",
        schema: SlotSchema {
            ty: "Xformable",
            apis: &[],
        },
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

    /// **The operators' own declarations**, which is where the constraint lives: a
    /// sampler reads a `domain`, an instances node a `points` cloud and the `prototype`
    /// it copies. An operator with no USD input declares no slot.
    #[test]
    fn the_operators_declare_their_slots() {
        assert_eq!(
            slot("RusdSamplePoints", "domain").map(|slot| slot.schema.ty),
            Some("Gprim")
        );
        assert_eq!(
            slot("RusdInstances", "points").map(|slot| slot.schema.ty),
            Some("Points")
        );
        assert_eq!(
            slot("RusdInstances", "prototype").map(|slot| slot.schema.ty),
            Some("Xformable")
        );
        assert!(slots("RusdGrid").is_empty());
    }
}
