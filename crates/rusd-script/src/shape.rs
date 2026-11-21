//! **An operator's I/O as Rust types** — Struct, Array, Vec (SPEC D15).
//!
//! The shape vocabulary is `rusd-scene`'s ([`Shape`], [`Port`]); what lives here is the
//! bridge from a Rust type to that description. An operator's ports are declared once,
//! as the fields of a Rust struct:
//!
//! ```
//! use rusd_script::ports;
//! use rusd_script::prim::Prim;
//! use rusd_script::schema::{Cube, Sphere};
//! use rusd_script::shape::{List, ShapeOf};
//!
//! ports! {
//!     /// A block: a run of lots, each a cube.
//!     pub struct Block {
//!         #[part("lot*")]
//!         lots: List<Prim<Cube>>,
//!     }
//! }
//! ports! {
//!     /// A street grid's output: the ground, and a run of blocks.
//!     pub struct StreetGrid {
//!         #[part("Ground")]
//!         ground: Prim<Cube>,
//!         #[part("b*")]
//!         blocks: List<Block>,
//!     }
//! }
//! // The description the cook checks is built from those types, not written beside them.
//! let shape = StreetGrid::shape();
//! assert!(matches!(shape, rusd_script::shape::Shape::Struct(_)));
//! ```
//!
//! `List<T>` is a **Vec whose length is the data's** — an operator writes one element
//! per sample, so the script cannot hold the elements, only the run. `Arr<T, N>` is an
//! array of exactly `N`. A struct is named parts. Any of them holds any of the others,
//! as deep as the operator needs, and the USD nesting the cook writes mirrors it.

use std::marker::PhantomData;

use crate::prim::Prim;
use crate::schema::Schema;

pub use rusd_scene::operator::{Port, Shape, SlotSchema};

/// **The shape a Rust type describes.** Implemented for the leaves and the containers,
/// and by [`ports!`] for every named struct — so an operator's declaration is its types.
pub trait ShapeOf {
    fn shape() -> Shape;
}

impl<T: Schema> ShapeOf for Prim<T> {
    fn shape() -> Shape {
        Shape::Prim(SlotSchema { ty: T::NAME.to_owned(), apis: T::apis() })
    }
}

/// **A named struct of ports** — what [`ports!`] implements, and what an operator's
/// input or output declaration is. The macro also implements [`ShapeOf`] for the
/// struct: a struct's shape is its ports.
pub trait Ports {
    fn ports() -> Vec<Port>;
}

/// **A run**: any number of `T`, named `<prefix>{k:06}` under one parent. The length is
/// the data's — a sampler's cloud decides how many instances there are — so this is the
/// Vec an operator *writes*, held as a handle rather than as elements.
pub struct List<T: ShapeOf> {
    parent: String,
    prefix: String,
    ty: PhantomData<T>,
}

impl<T: ShapeOf> Clone for List<T> {
    fn clone(&self) -> Self {
        Self { parent: self.parent.clone(), prefix: self.prefix.clone(), ty: PhantomData }
    }
}

impl<T: ShapeOf> List<T> {
    pub fn new(parent: &str, prefix: &str) -> Self {
        Self { parent: parent.to_owned(), prefix: prefix.to_owned(), ty: PhantomData }
    }

    /// The run's parent prim path, and the prefix its elements' names start with.
    pub fn parent(&self) -> &str {
        &self.parent
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }
}

impl<T: ShapeOf> ShapeOf for List<T> {
    fn shape() -> Shape {
        Shape::List(Box::new(T::shape()))
    }
}

/// **A slot wired with one prim or a list of them**, each of type `T`. The declared
/// shape is the element contract — `Prim<T>`, what every wired prim must be — while
/// the multiplicity is the author's wiring choice: a sampler may draw from several
/// domains, an instances node copy along several clouds. Unlike [`List`], the script
/// holds the elements (it names them), so this is a `Vec` of handles, not a run whose
/// length the data decides.
pub struct Many<T: Schema>(pub Vec<Prim<T>>);

impl<T: Schema> Clone for Many<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T: Schema> ShapeOf for Many<T> {
    fn shape() -> Shape {
        Prim::<T>::shape()
    }
}

/// **An array**: exactly `N` of `T`, named `<prefix>{k:06}` under one parent. The length
/// is the schema's, which is the one difference from [`List`].
pub struct Arr<T: ShapeOf, const N: usize> {
    parent: String,
    prefix: String,
    ty: PhantomData<T>,
}

impl<T: ShapeOf, const N: usize> Clone for Arr<T, N> {
    fn clone(&self) -> Self {
        Self { parent: self.parent.clone(), prefix: self.prefix.clone(), ty: PhantomData }
    }
}

impl<T: ShapeOf, const N: usize> Arr<T, N> {
    pub fn new(parent: &str, prefix: &str) -> Self {
        Self { parent: parent.to_owned(), prefix: prefix.to_owned(), ty: PhantomData }
    }

    pub fn len(&self) -> usize {
        N
    }

    pub fn is_empty(&self) -> bool {
        N == 0
    }
}

impl<T: ShapeOf, const N: usize> ShapeOf for Arr<T, N> {
    fn shape() -> Shape {
        Shape::Array { len: N as u32, of: Box::new(T::shape()) }
    }
}

/// Declare a struct of ports: the struct itself, its [`Ports`] description, and its
/// [`ShapeOf`]. A field's port name is the field's own name, or the literal in
/// `#[part("…")]` when the wire spells it differently (`Ground`, `i*`, `lot*`).
///
/// One declaration, three readers: the script's types, the cook's runtime check, and
/// the composition tree's report of what an operator reads and writes.
#[macro_export]
macro_rules! ports {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident {
            $( $(#[part($part:literal)])? $field:ident : $ty:ty ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        $vis struct $name {
            $( $vis $field : $ty ),*
        }

        impl $crate::shape::Ports for $name {
            fn ports() -> ::std::vec::Vec<$crate::shape::Port> {
                ::std::vec![
                    $( $crate::shape::Port::new(
                        $crate::ports!(@name $( $part )? $field),
                        <$ty as $crate::shape::ShapeOf>::shape(),
                    ) ),*
                ]
            }
        }

        impl $crate::shape::ShapeOf for $name {
            fn shape() -> $crate::shape::Shape {
                $crate::shape::Shape::Struct(<Self as $crate::shape::Ports>::ports())
            }
        }
    };
    (@name $part:literal $field:ident) => { $part };
    (@name $field:ident) => { ::std::stringify!($field) };
}
