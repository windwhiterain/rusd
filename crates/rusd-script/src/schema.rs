//! **USD's schema graph as Rust types** — the compile-time half of `IsA` (SPEC D15).
//!
//! A prim's type is two things: the class it is (`typeName`, and everything that class
//! derives from) and the interfaces it has applied (its API schemas). At runtime USD
//! answers both — `UsdSchemaRegistry::IsA` and `UsdPrim::HasAPI`. Here the same two
//! questions are **trait bounds**, so a script that wires a `Material` where an
//! `Xformable` was declared does not compile:
//!
//! ```compile_fail
//! use rusd_script::schema::{Material, Xformable};
//! use rusd_script::prim::{Prim, Wire};
//! fn takes_a_placeable(_: impl Wire<Xformable>) {}
//! takes_a_placeable(Prim::<Material>::new("/World/Red"));
//! ```
//!
//! The types and the `IsA` impls are **generated from the manifest**
//! (`rusd-scene/schemas/rusd_types_manifest.usda`) by `build.rs`, so the relation a
//! bound checks is the relation the registry walks — one declaration, two readers.

use std::marker::PhantomData;

/// A USD typed schema: the class a prim may be.
pub trait Schema {
    /// The identifier USD knows it by — what a layer writes as `typeName`.
    const NAME: &'static str;
    /// Whether a prim may author this name as its own type. An abstract schema
    /// (`Xformable`, `Gprim`) is a class to derive from, never a `typeName`.
    const CONCRETE: bool;
    /// The API schemas this type builds in. A plain type builds in none; an
    /// [`Applied`] wrapper adds the one it names to whatever its tail has.
    fn apis() -> Vec<String> {
        Vec::new()
    }
}

/// **`Derived` is a `Base`** — USD's `IsA`, as a bound. Reflexive and transitive: the
/// generated impls are the closure of the manifest's `bases`, so `Cube: IsA<Gprim>`
/// and `Cube: IsA<Xformable>` both hold, and `Xform: IsA<Gprim>` does not.
pub trait IsA<Base: Schema>: Schema {}

/// A USD API schema: an interface a prim may have applied, not a class it may be.
pub trait ApiSchema {
    const NAME: &'static str;
}

/// **A prim of type `T` with the API schema `A` applied** — the interface half of a
/// prim's type, carried in the type itself. `Prim<Applied<MaterialBindingAPI, Cube>>`
/// is a `Cube` (it still answers `IsA<Gprim>`) and it has `MaterialBindingAPI`.
///
/// Wrappers nest: `Applied<A, Applied<B, T>>` has both, and [`Has`] finds either.
pub struct Applied<A: ApiSchema, T: Schema>(PhantomData<(A, T)>);

impl<A: ApiSchema, T: Schema> Schema for Applied<A, T> {
    const NAME: &'static str = T::NAME;
    const CONCRETE: bool = T::CONCRETE;
    fn apis() -> Vec<String> {
        let mut apis = vec![A::NAME.to_owned()];
        apis.extend(T::apis());
        apis
    }
}

impl<A: ApiSchema, T: Schema, Base: Schema> IsA<Base> for Applied<A, T> where T: IsA<Base> {}

/// The two positions a [`Has`] search can be in: the wrapper's own API, or further
/// down its tail. They are what keep the two impls from overlapping — the usual
/// type-level-list membership encoding, and the reason `Has` carries a second
/// parameter a caller never writes (it is inferred).
pub struct Here;
pub struct There<M>(PhantomData<M>);

/// **The prim has the API schema `A` applied** — `HasAPI`, as a bound. A port that
/// asks for a class *and* an interface bounds both:
///
/// ```
/// use rusd_script::schema::{Applied, Cube, Has, MaterialBindingAPI, Xformable};
/// use rusd_script::prim::{Prim, Wire};
/// fn paint<T, M>(_: T) where T: Wire<Xformable> + Has<MaterialBindingAPI, M> {}
/// let bound = Prim::<Cube>::new("/World/Box").apply::<MaterialBindingAPI>();
/// paint(bound);
/// ```
///
/// The same call with a bare `Prim<Cube>` does not compile:
///
/// ```compile_fail
/// use rusd_script::schema::{Cube, Has, MaterialBindingAPI, Xformable};
/// use rusd_script::prim::{Prim, Wire};
/// fn paint<T, M>(_: T) where T: Wire<Xformable> + Has<MaterialBindingAPI, M> {}
/// paint(Prim::<Cube>::new("/World/Box"));
/// ```
pub trait Has<A: ApiSchema, M = Here> {}

impl<A: ApiSchema, T: Schema> Has<A, Here> for Applied<A, T> {}

impl<A: ApiSchema, Q: ApiSchema, T: Schema, M> Has<Q, There<M>> for Applied<A, T> where
    T: Has<Q, M>
{
}

/// A handle has what its type has: the bound a port writes is on the *wire*, so the
/// answer forwards from the prim's type to the prim itself.
impl<T: Schema + Has<A, M>, A: ApiSchema, M> Has<A, M> for crate::prim::Prim<T> {}

/// **A prim that authors no `typeName` of its own.** An instance composes its type
/// through its `reference` arc, so the layer it is written in holds nothing to
/// compare — which is exactly what the runtime's empty `ty` claims.
pub struct Untyped;

impl Schema for Untyped {
    const NAME: &'static str = "";
    const CONCRETE: bool = false;
}

// The generated half: one struct per schema, `IsA` for every ancestor pair, and the
// data the cross-check test reads. See `build.rs`.
include!(concat!(env!("OUT_DIR"), "/usd_schemas.rs"));
