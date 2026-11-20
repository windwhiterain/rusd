//! **The generated `IsA` is the registry's `IsA`** (SPEC D15).
//!
//! `build.rs` reads `rusd_types_manifest.usda` and writes one `impl IsA<Base> for
//! Derived` per ancestor pair; `rusd-scene` reads the *same file* into
//! `UsdSchemaRegistry`, whose `is_a` walks it at runtime. This test is the loop closed:
//! every pair of schemas the family declares, asked both ways, must agree. If they
//! ever disagree, the compile-time check and the runtime check are two relations, and
//! the one a script trusts is the wrong one.

use rusd_script::schema::{APIS, IS_A, TYPED};
use rusd_scene::schema::usd_registry;

fn is_a(actual: &str, base: &str) -> bool {
    usd_registry().is_a(&openusd::tf::Token::from(actual), &openusd::tf::Token::from(base))
}

#[test]
fn the_generated_relation_is_the_registrys_relation() {
    let mut pairs = 0;
    for actual in TYPED {
        for base in TYPED {
            let generated = IS_A.contains(&(*actual, *base));
            assert_eq!(
                generated,
                is_a(actual, base),
                "`{actual}: IsA<{base}>` is {generated} in Rust and {} in USD's registry",
                is_a(actual, base)
            );
            pairs += 1;
        }
    }
    // The family is small but the closure is not: every typed schema on the geom/shade
    // spine reaches `Typed`. `APISchemaBase` is the API side's root, not that spine —
    // the core family derives it from `SchemaBase`, and the pair loop above already
    // checked that both readers agree about it.
    assert!(pairs > 100, "the cross-check covers {pairs} pairs");
    for actual in TYPED {
        if *actual == "APISchemaBase" {
            continue;
        }
        assert!(IS_A.contains(&(*actual, "Typed")), "{actual} reaches the core family");
    }
}

#[test]
fn concrete_is_the_registrys_concrete() {
    // A concrete type is one a prim may author; an abstract one is a class to derive
    // from. The manifest's `schemaKind` says which, and both readers must say the same.
    use rusd_script::schema::{Cube, Gprim, Mesh, Schema, Xform, Xformable};
    let concrete = |name: &str| usd_registry().is_concrete_type(&openusd::tf::Token::from(name));
    assert_eq!(Cube::CONCRETE, concrete(Cube::NAME));
    assert_eq!(Mesh::CONCRETE, concrete(Mesh::NAME));
    assert_eq!(Xform::CONCRETE, concrete(Xform::NAME));
    assert_eq!(Gprim::CONCRETE, concrete(Gprim::NAME));
    assert_eq!(Xformable::CONCRETE, concrete(Xformable::NAME));
    assert!(Cube::CONCRETE && !Gprim::CONCRETE);
}

#[test]
fn the_api_schemas_are_the_registrys_api_schemas() {
    use rusd_script::schema::{ApiSchema, MaterialBindingAPI};
    assert!(APIS.contains(&MaterialBindingAPI::NAME));
    let info = usd_registry()
        .schema_info(&openusd::tf::Token::from(MaterialBindingAPI::NAME))
        .expect("the family declares MaterialBindingAPI");
    assert!(format!("{info:?}").contains("API"), "it is an API schema: {info:?}");
}

/// **A prim's type is its class and its interfaces, in the type itself.** Applying an
/// API schema changes what the prim *has*, never what it *is*: the wrapped type still
/// answers every `IsA` its tail answers, and `ShapeOf` reports both halves.
#[test]
fn an_applied_api_keeps_the_class_and_adds_the_interface() {
    use rusd_script::prim::Prim;
    use rusd_script::schema::{Applied, Cube, Gprim, MaterialBindingAPI, Xformable};
    use rusd_script::shape::{Shape, ShapeOf};

    let bound = Prim::<Cube>::new("/World/Box").apply::<MaterialBindingAPI>();
    // The class half survives the wrapper: it is still a Cube, still a Gprim.
    let _: Prim<Gprim> = bound.clone().up();
    let _: Prim<Xformable> = bound.clone().up();
    let Shape::Prim(declared) = <Prim<Applied<MaterialBindingAPI, Cube>> as ShapeOf>::shape() else {
        panic!("a prim's shape is a prim claim");
    };
    assert_eq!(declared.ty, "Cube");
    assert_eq!(declared.apis, vec!["MaterialBindingAPI".to_owned()]);

    // And the runtime checker agrees with what the type says: the same claim, asked of
    // a `Cube` with the API applied, is met; without it, the interface half is missing.
    let registry = usd_registry();
    assert_eq!(
        rusd_scene::operator::unmet(&declared, "Cube", &["MaterialBindingAPI"], registry),
        None
    );
    assert!(matches!(
        rusd_scene::operator::unmet(&declared, "Cube", &[], registry),
        Some(rusd_scene::operator::Miss::Api { .. })
    ));
}
