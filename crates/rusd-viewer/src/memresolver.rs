//! Client-side, fully in-memory asset resolution for the USD-layer remote
//! (SPEC D12 v2, spike ②). Nothing here touches the filesystem or the network:
//! a stack of layer bytes is handed over in memory and composed exactly as the
//! same stack on disk would be, so a remote client can render a composite stage
//! it never wrote anywhere.
//!
//! [`MemResolver`] is the one seam openusd gives a host for this: a
//! [`Resolver`] maps authored asset paths to identifiers, and hands back the
//! bytes for those identifiers. Identifiers are virtual POSIX paths under `/`
//! (`/root.usda`, `/cooks/city.usdc`), so relative `subLayers` / reference
//! paths anchor the same way they would between real files.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;

use openusd::ar::{Asset, AssetBuffer, ResolvedPath, Resolver, SharedBuffer};
use openusd::sdf::Layer;

/// The identity token of this resolver configuration (SPEC D3: two resolvers
/// that resolve the same paths identically must report the same token, and a
/// different backend must not collide with the empty default).
pub const RESOLVER_IDENTITY: &str = "rusd-remote/1";

/// The key a path is stored and looked up under: an anonymous layer identifier
/// is its own key (it names no asset and must not be folded into a path),
/// anything else is canonicalized.
fn key(path: &str) -> String {
    if Layer::is_anonymous_identifier(path) {
        path.to_owned()
    } else {
        canonical(path)
    }
}

/// An asset resolver over bytes held in memory, keyed by virtual path.
///
/// Every identifier is canonicalized to a `/`-rooted, `.`/`..`-free spelling,
/// so `./a.usda`, `a.usda` and `/a.usda` opened from the same anchor are one
/// layer and one opinion stack. Bytes are shared (`Arc<[u8]>`), so serving the
/// same layer to several opens copies nothing.
#[derive(Clone, Debug, Default)]
pub struct MemResolver {
    layers: BTreeMap<String, Arc<[u8]>>,
}

impl MemResolver {
    /// An empty resolver: every path is unresolved.
    pub fn new() -> Self {
        Self::default()
    }

    /// Serve `bytes` as the layer at `identifier` (canonicalized unless it is an
    /// anonymous layer identifier). Re-inserting the same identifier replaces the
    /// bytes, which is what an incremented layer looks like to a rebuilt stage.
    pub fn insert(&mut self, identifier: &str, bytes: impl Into<Arc<[u8]>>) {
        self.layers.insert(key(identifier), bytes.into());
    }

    /// Whether `identifier` is served by this resolver.
    pub fn contains(&self, identifier: &str) -> bool {
        self.layers.contains_key(&key(identifier))
    }

    /// The identifiers this resolver serves, sorted (deterministic: never
    /// depends on insertion order).
    pub fn identifiers(&self) -> impl Iterator<Item = &str> {
        self.layers.keys().map(String::as_str)
    }

    /// The bytes served at `identifier`, if any.
    pub fn bytes(&self, identifier: &str) -> Option<&[u8]> {
        self.layers.get(&key(identifier)).map(|b| &**b)
    }

    /// The number of layers served.
    pub fn len(&self) -> usize {
        self.layers.len()
    }

    /// Whether no layer is served.
    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }
}

impl Resolver for MemResolver {
    /// Canonicalize `asset_path` against `anchor`, a virtual path.
    ///
    /// An anonymous layer identifier (`anon:<n>:<tag>`) names no asset, so it
    /// passes through unchanged — the rule `LayerRegistry` itself applies
    /// before consulting a resolver.
    fn create_identifier(&self, asset_path: &str, anchor: Option<&ResolvedPath>) -> String {
        if asset_path.is_empty() {
            return String::new();
        }
        if Layer::is_anonymous_identifier(asset_path) {
            return asset_path.to_owned();
        }
        // An already-rooted path is its own identifier.
        if asset_path.starts_with('/') {
            return canonical(asset_path);
        }
        match anchor {
            // Anchor relative paths at the anchoring layer's directory, exactly
            // as `DefaultResolver` anchors at a real file's directory.
            Some(anchor) => {
                let anchor_str = anchor.to_string_lossy();
                let dir = anchor_str.rsplit_once('/').map_or("", |(dir, _)| dir);
                canonical(&format!("{dir}/{asset_path}"))
            }
            // No anchor: the virtual root is the "working directory".
            None => canonical(&format!("/{asset_path}")),
        }
    }

    fn resolve(&self, asset_path: &str) -> Option<ResolvedPath> {
        let k = key(asset_path);
        self.layers.contains_key(&k).then(|| ResolvedPath::new(k))
    }

    fn resolve_for_new_asset(&self, asset_path: &str) -> Option<ResolvedPath> {
        // A new asset is never created in a read-only mirror.
        let _ = asset_path;
        None
    }

    fn open_asset(&self, resolved_path: &ResolvedPath) -> io::Result<Box<dyn Asset>> {
        let k = key(&resolved_path.to_string_lossy());
        let bytes = self
            .layers
            .get(&k)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, k.clone()))?;
        let buffer = AssetBuffer::Shared(SharedBuffer::new(Arc::clone(bytes)));
        Ok(Box::new(io::Cursor::new(buffer)))
    }

    fn in_memory_identifier<'a>(&self, asset_path: &'a str) -> Cow<'a, str> {
        if Layer::is_anonymous_identifier(asset_path) {
            return Cow::Borrowed(asset_path);
        }
        Cow::Owned(canonical(asset_path))
    }

    fn identity(&self) -> String {
        RESOLVER_IDENTITY.to_owned()
    }
}

/// Canonical `/`-rooted spelling of a virtual path: `.` and empty segments
/// dropped, `..` folded, `/`-separated. Never depends on the host's filesystem.
pub fn canonical(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    let mut out = String::with_capacity(path.len() + 1);
    for part in &parts {
        out.push('/');
        out.push_str(part);
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use openusd::sdf;
    use openusd::usd::Stage;

    #[test]
    fn canonical_folds_dot_segments() {
        assert_eq!(canonical("/a//b/./c.usda"), "/a/b/c.usda");
        assert_eq!(canonical("/a/b/../c.usda"), "/a/c.usda");
        assert_eq!(canonical("c.usda"), "/c.usda");
        assert_eq!(canonical("/"), "/");
    }

    #[test]
    fn identifiers_anchor_at_the_anchoring_layer() {
        let r = MemResolver::new();
        let anchor = ResolvedPath::new("/cooks/city.usdc");
        assert_eq!(
            r.create_identifier("over.usda", Some(&anchor)),
            "/cooks/over.usda"
        );
        assert_eq!(
            r.create_identifier("./over.usda", Some(&anchor)),
            "/cooks/over.usda"
        );
        assert_eq!(
            r.create_identifier("../top.usda", Some(&anchor)),
            "/top.usda"
        );
        assert_eq!(r.create_identifier("over.usda", None), "/over.usda");
        assert_eq!(r.create_identifier("/abs.usda", None), "/abs.usda");
        assert_eq!(r.identity(), RESOLVER_IDENTITY);
    }

    #[test]
    fn anonymous_identifiers_pass_through() {
        let r = MemResolver::new();
        let a = Layer::new_anonymous("scene.usda");
        let id = a.identifier().to_owned();
        assert_eq!(r.create_identifier(&id, None), id);
    }

    #[test]
    fn a_single_layer_opens_straight_from_bytes() {
        let text = "#usda 1.0\n\ndef Xform \"World\"\n{\n}\n";
        let layer = Layer::from_bytes("scene.usda", text.as_bytes().to_vec()).unwrap();
        let mut mem = MemResolver::new();
        mem.insert(layer.identifier(), text.as_bytes().to_vec());
        let stage = Stage::builder()
            .resolver(mem)
            .open(layer.identifier())
            .expect("stage opens over the in-memory layer");
        assert!(stage.prim(sdf::path("/World").unwrap()).is_ok());
    }

    #[test]
    fn sublayers_resolve_inside_the_mirror() {
        // The root names its sublayer relative to itself; the mirror serves both
        // from memory, and the sublayer's opinion composes.
        let root = "#usda 1.0\n(\n    subLayers = [@./over.usda@]\n)\n\n\
                    def Xform \"World\"\n{\n}\n";
        let over = "#usda 1.0\nover \"World\"\n{\n    string label = \"from-sublayer\"\n}\n";
        let mut mem = MemResolver::new();
        mem.insert("/root.usda", root.as_bytes().to_vec());
        mem.insert("/over.usda", over.as_bytes().to_vec());
        let stage = Stage::builder().resolver(mem).open("/root.usda").unwrap();
        let prim = stage.prim(sdf::path("/World").unwrap()).unwrap();
        let v = prim.attribute("label").get::<String>().unwrap();
        assert_eq!(v.as_deref(), Some("from-sublayer"));
    }

    #[test]
    fn an_unknown_layer_is_unresolved_not_an_error() {
        let mem = MemResolver::new();
        assert!(mem.resolve("/missing.usda").is_none());
        assert!(Layer::open_with(mem, "/missing.usda").is_err());
    }
}
