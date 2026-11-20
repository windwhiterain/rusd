//! **A typed handle to a prim** — what a script wires (SPEC D15).
//!
//! A `Prim<T>` is a path, the USD it lives in, and the type it is. The type is not
//! checked at runtime: it is part of the value's Rust type, so an operator that asks
//! for a `Gprim` cannot be handed an `Xform` — the call does not compile. What the
//! handle does *not* do is read the file: a handle into a material layer the script did
//! not author is a claim, and the cook still checks that claim against the USD
//! ([`rusd_scene::operator::unmet`]). A handle the script authored itself
//! ([`crate::usd::Layer::cube`] and friends) is true by construction.

use std::marker::PhantomData;

use crate::schema::{ApiSchema, Applied, IsA, Schema};

/// A prim of type `T`: its absolute path, and the USD file it lives in (`None` for a
/// prim of the cook stack — an operator's product, which every later node reads).
pub struct Prim<T: Schema> {
    asset: Option<String>,
    path: String,
    ty: PhantomData<T>,
}

impl<T: Schema> Clone for Prim<T> {
    fn clone(&self) -> Self {
        Self { asset: self.asset.clone(), path: self.path.clone(), ty: PhantomData }
    }
}

impl<T: Schema> std::fmt::Debug for Prim<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Prim<{}>({})", T::NAME, self.reference())
    }
}

impl<T: Schema> Prim<T> {
    /// A prim of the cook stack: an operator's product, read by a later node.
    pub fn new(path: &str) -> Self {
        Self { asset: None, path: path.to_owned(), ty: PhantomData }
    }

    /// A prim of a named USD file — the material an operator reads.
    pub fn in_file(file: &str, path: &str) -> Self {
        Self { asset: Some(file.to_owned()), path: path.to_owned(), ty: PhantomData }
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn asset(&self) -> Option<&str> {
        self.asset.as_deref()
    }

    /// How a document names this prim: `volume.rusd:/World/Volume` across a file
    /// boundary, `/World/Samples/points` inside the stack.
    pub fn reference(&self) -> String {
        match &self.asset {
            Some(file) => format!("{file}:{}", self.path),
            None => self.path.clone(),
        }
    }

    /// **Upcast**: this prim as one of its base classes. The bound is USD's `IsA`, so
    /// `cube.up::<Gprim>()` compiles and `cube.up::<Xform>()` does not.
    pub fn up<Base: Schema>(self) -> Prim<Base>
    where
        T: IsA<Base>,
    {
        Prim { asset: self.asset, path: self.path, ty: PhantomData }
    }

    /// **Apply an API schema**: the same prim, now carrying an interface. Its class is
    /// unchanged — `Applied<A, T>` still answers every `IsA<T>` answer.
    pub fn apply<A: ApiSchema>(self) -> Prim<Applied<A, T>> {
        Prim { asset: self.asset, path: self.path, ty: PhantomData }
    }

    /// A child of this prim, of the type the caller names. An operator uses it to name
    /// the port it wrote; the claim is what the cook's output check verifies.
    pub fn child<U: Schema>(&self, name: &str) -> Prim<U> {
        Prim {
            asset: self.asset.clone(),
            path: format!("{}/{name}", self.path),
            ty: PhantomData,
        }
    }
}

/// **What can be wired into a port that asks for `Base`.** A `Prim<T>` can, when
/// `T: IsA<Base>` — the class half — and the blanket impl is the only one, so nothing
/// else sneaks in. An operator's parameter is `impl Wire<Gprim>`; a `Prim<Cube>`
/// satisfies it, a `Prim<Xform>` does not, and the error is rustc's at the call site.
pub trait Wire<Base: Schema> {
    fn wire(self) -> Prim<Base>;
}

impl<T: Schema + IsA<Base>, Base: Schema> Wire<Base> for Prim<T> {
    fn wire(self) -> Prim<Base> {
        self.up()
    }
}
