//! A layer synced from the server as **blocks**, opened from a local file (v1.9).
//!
//! The wire carries no geometry: a layer is a `.rusd` file, and the client keeps a copy of
//! it. What travels is the **block manifest** (one entry per TOML table: its key and a hash
//! of its text) and then only the blocks whose hash moved. A re-cooked product that changed
//! one instance therefore costs one table, not a megabyte.
//!
//! The block unit is [`rusd_core::incremental::text_blocks`]' — the same split the
//! incremental parser caches — so the client can hand the reassembled text straight to an
//! [`IncrementalParser`](rusd_core::IncrementalParser) and re-parse only what changed.
//!
//! What is stored beside the layer:
//!
//! - `<dir>/<name>` — the reassembled text, which is what `Stage::open` reads;
//! - `<dir>/<name>.blocks.json` — the blocks it was assembled from, so the next sync can
//!   tell which ones it already has.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One entry of a layer's block manifest, as the server reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockRef {
    /// The table's dotted key (`World.Boxes`), `""` for the preamble.
    pub key: String,
    /// The server's hash of that block's text.
    pub hash: String,
}

/// A cached block: what was fetched, and what it said.
#[derive(Clone, Debug)]
struct CachedBlock {
    hash: String,
    text: String,
}

/// The local copy of every layer this viewer has seen, under one directory.
#[derive(Clone, Debug)]
pub struct LayerCache {
    dir: PathBuf,
}

impl LayerCache {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The default cache: `%TEMP%/rusd-layers`.
    pub fn default_dir() -> PathBuf {
        std::env::temp_dir().join("rusd-layers")
    }

    /// Where a remote layer's text lands locally.
    ///
    /// The cache **mirrors the layout the products assume** (v1.9): a product sits in
    /// `<dir>/out/`, its materials in `<dir>/`, so `../box.rusd` means the same thing in the
    /// cache as it does on the server. No hashing, no rewriting — the relative arc is the
    /// whole mapping.
    pub fn local_path(&self, remote: &str) -> PathBuf {
        let slashed = remote.replace('\\', "/");
        let name = slashed
            .rsplit('/')
            .next()
            .unwrap_or("layer.rusd")
            .to_owned();
        let in_out = slashed
            .rsplit_once('/')
            .map(|(parent, _)| parent.ends_with("/out"))
            .unwrap_or(false);
        if in_out {
            self.dir.join("out").join(name)
        } else {
            self.dir.join(name)
        }
    }

    fn blocks_path(&self, remote: &str) -> PathBuf {
        let mut path = self.local_path(remote);
        let name = format!(
            "{}.blocks.json",
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        );
        path.set_file_name(name);
        path
    }

    /// The blocks this cache already holds for `remote`.
    fn held(&self, remote: &str) -> BTreeMap<String, CachedBlock> {
        let Ok(text) = std::fs::read_to_string(self.blocks_path(remote)) else {
            return BTreeMap::new();
        };
        let Ok(value) = text.parse::<serde_json::Value>() else {
            return BTreeMap::new();
        };
        let mut out = BTreeMap::new();
        for entry in value.as_array().into_iter().flatten() {
            let key = entry.get("key").and_then(|k| k.as_str()).unwrap_or("");
            let hash = entry.get("hash").and_then(|k| k.as_str()).unwrap_or("");
            let text = entry.get("text").and_then(|k| k.as_str()).unwrap_or("");
            out.insert(
                format!("{key}#{hash}"),
                CachedBlock {
                    hash: hash.to_owned(),
                    text: text.to_owned(),
                },
            );
        }
        out
    }

    /// Which blocks of `manifest` this cache does not have (a position in `manifest`, so the
    /// caller can ask for exactly those).
    pub fn missing(&self, remote: &str, manifest: &[BlockRef]) -> Vec<usize> {
        let held = self.held(remote);
        manifest
            .iter()
            .enumerate()
            .filter(|(_, b)| {
                !held
                    .values()
                    .any(|h| h.hash == b.hash && h.text.starts_with(&header_of(&b.key)))
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Write the layer: `texts` supplies the blocks `missing` asked for, by position.
    ///
    /// The text is assembled from the cache for everything else, so a sync that fetched one
    /// block still produces the whole file — and the sidecar keeps the blocks it came from.
    pub fn store(
        &self,
        remote: &str,
        manifest: &[BlockRef],
        fetched: &BTreeMap<usize, String>,
    ) -> Result<PathBuf, String> {
        // The **parent of the file**, not just the cache root: a product lives in
        // `<dir>/out/` (v1.9, the flat layout that makes `../box.rusd` true locally), and a
        // fresh cache has no `out/` yet — writing into it must make it.
        let path = self.local_path(remote);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        let held = self.held(remote);
        let mut text = String::new();
        let mut sidecar = Vec::new();
        for (i, block) in manifest.iter().enumerate() {
            let body = match fetched.get(&i) {
                Some(t) => t.clone(),
                None => held
                    .values()
                    .find(|h| h.hash == block.hash)
                    .map(|h| h.text.clone())
                    .ok_or_else(|| {
                        format!("block {i} ({}) was neither fetched nor cached", block.key)
                    })?,
            };
            text.push_str(&body);
            sidecar.push(serde_json::json!({
                "key": block.key,
                "hash": block.hash,
                "text": body,
            }));
        }
        std::fs::write(&path, &text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        let blocks = serde_json::Value::Array(sidecar).to_string();
        std::fs::write(self.blocks_path(remote), blocks)
            .map_err(|e| format!("cannot write the block cache: {e}"))?;
        Ok(path)
    }

    /// Whether this cache holds `remote` as it is now.
    pub fn is_current(&self, remote: &str, manifest: &[BlockRef]) -> bool {
        self.local_path(remote).is_file() && self.missing(remote, manifest).is_empty()
    }
}

/// A short, stable key for a remote path: the same string always lands in the same local
/// file, and two paths that differ land in two.
fn key_of(remote: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    remote.replace('\\', "/").to_lowercase().hash(&mut h);
    format!("{:016x}", h.finish())
}

/// The header line a block's key implies, for matching a cached block to a manifest entry
/// whose hash agrees: `World.Boxes` starts its text with `[World.Boxes]`, the preamble with
/// nothing in particular.
fn header_of(key: &str) -> String {
    if key.is_empty() {
        String::new()
    } else {
        format!("[{key}]")
    }
}

/// The resolver a locally-opened layer uses: a path this machine does not have is looked
/// up in the cache by its **file name** (v1.9).
///
/// A product written on the server names its materials by the server's absolute paths
/// (`C:/resource/rusd/examples/box.rusd`). The client holds the same layers under
/// `%TEMP%/rusd-layers/`, block-synced, so the arc resolves to that copy and the layer opens
/// without a single byte of geometry crossing the wire.
#[derive(Default)]
pub struct CacheResolver {
    inner: openusd::ar::DefaultResolver,
    dir: PathBuf,
}

impl CacheResolver {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            inner: openusd::ar::DefaultResolver::new(),
            dir: dir.into(),
        }
    }
}

impl openusd::ar::Resolver for CacheResolver {
    fn create_identifier(
        &self,
        asset_path: &str,
        anchor: Option<&openusd::ar::ResolvedPath>,
    ) -> String {
        self.inner.create_identifier(asset_path, anchor)
    }

    fn resolve(&self, asset_path: &str) -> Option<openusd::ar::ResolvedPath> {
        if let Some(found) = self.inner.resolve(asset_path) {
            return Some(found);
        }
        let name = asset_path.rsplit(['/', '\\']).next().unwrap_or(asset_path);
        let local = LayerCache::new(self.dir.clone())
            .local_path(asset_path)
            .to_owned();
        let _ = name;
        if local.is_file() {
            return self.inner.resolve(&local.to_string_lossy().replace('\\', "/"));
        }
        None
    }

    fn resolve_for_new_asset(&self, asset_path: &str) -> Option<openusd::ar::ResolvedPath> {
        self.inner.resolve_for_new_asset(asset_path)
    }

    fn open_asset(
        &self,
        resolved_path: &openusd::ar::ResolvedPath,
    ) -> std::io::Result<Box<dyn openusd::ar::Asset>> {
        self.inner.open_asset(resolved_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_of(text: &str) -> Vec<BlockRef> {
        rusd_core::incremental::text_blocks(text)
            .into_iter()
            .map(|b| BlockRef {
                key: b.key,
                hash: hash(&b.text),
            })
            .collect()
    }

    fn hash(text: &str) -> String {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        text.as_bytes().hash(&mut h);
        format!("{:016x}", h.finish())
    }

    fn fetch(text: &str, manifest: &[BlockRef], want: &[usize]) -> BTreeMap<usize, String> {
        let blocks = rusd_core::incremental::text_blocks(text);
        want.iter()
            .map(|i| (*i, blocks[*i].text.clone()))
            .collect()
    }

    const BEFORE: &str = "[World]\nschema = \"Xform\"\n\n[World.Boxes]\nschema = \"Xform\"\n\n[World.Boxes.i0]\nschema = \"\"\n\"xformOp:translate\" = [1.0, 2.0, 3.0]\n";
    const AFTER: &str = "[World]\nschema = \"Xform\"\n\n[World.Boxes]\nschema = \"Xform\"\n\n[World.Boxes.i0]\nschema = \"\"\n\"xformOp:translate\" = [4.0, 5.0, 6.0]\n";

    #[test]
    fn the_first_sync_fetches_everything_and_the_next_fetches_only_what_moved() {
        let dir = std::env::temp_dir().join(format!("rusd-cache-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = LayerCache::new(&dir);
        let remote = "C:/remote/a.rusd";

        let first = manifest_of(BEFORE);
        assert_eq!(cache.missing(remote, &first).len(), first.len());
        let want = cache.missing(remote, &first);
        let path = cache
            .store(remote, &first, &fetch(BEFORE, &first, &want))
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), BEFORE);
        assert!(cache.is_current(remote, &first));

        // One table edited: exactly one block is fetched.
        let second = manifest_of(AFTER);
        let want = cache.missing(remote, &second);
        assert_eq!(want.len(), 1, "only the edited table moved");
        let path = cache
            .store(remote, &second, &fetch(AFTER, &second, &want))
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), AFTER);
        assert!(cache.is_current(remote, &second));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
