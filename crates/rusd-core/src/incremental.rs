//! Block-level incremental parser for the flat `.rusd` profile (SPEC §6, D2).
//!
//! Flat TOML tables are independent blocks. On re-parse we split the text into
//! header-delimited blocks, hash each block's text, and re-parse only the blocks
//! whose hash changed; unchanged blocks reuse their cached parse. The merged
//! result is guaranteed equal to a full [`crate::syntax::parse`] (the
//! "incremental ≡ full" invariant), verified by tests below.
//!
//! v0 limitation: header detection assumes bare-key headers and single-line
//! values (the canonical form). Multi-line arrays / quoted headers are parsed
//! correctly by the full parser but may defeat block splitting; revisit with
//! CST spans when those enter the profile.

use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};
use std::hash::{DefaultHasher, Hash, Hasher};

use crate::diag::Diagnostic;
use crate::path::PrimPath;
use crate::syntax::{self, ParsedDocument, PrimBlock};

/// Counters proving incrementality: how many blocks were (re)parsed vs reused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub parsed: usize,
    pub reused: usize,
}

struct Cached {
    hash: u64,
    doc: ParsedDocument,
}

/// A reusable incremental parser holding a per-block cache.
#[derive(Default)]
pub struct IncrementalParser {
    cache: BTreeMap<String, Cached>,
    pub stats: Stats,
}

struct Block {
    key: String,
    text: String,
}

impl IncrementalParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse `text`, reusing cached blocks whose text is unchanged.
    pub fn parse(&mut self, text: &str) -> Result<ParsedDocument, Vec<Diagnostic>> {
        self.stats = Stats::default();
        let blocks = split_blocks(text);
        let mut stage = BTreeMap::new();
        let mut prims: BTreeMap<PrimPath, PrimBlock> = BTreeMap::new();
        let mut diags = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();

        for b in blocks {
            seen.insert(b.key.clone());
            let hash = hash_text(&b.text);
            let cached_hit = matches!(self.cache.get(&b.key), Some(c) if c.hash == hash);
            let partial = if cached_hit {
                self.stats.reused += 1;
                self.cache[&b.key].doc.clone()
            } else {
                self.stats.parsed += 1;
                match syntax::parse(&b.text) {
                    Ok(d) => {
                        self.cache.insert(
                            b.key.clone(),
                            Cached {
                                hash,
                                doc: d.clone(),
                            },
                        );
                        d
                    }
                    Err(e) => {
                        diags.extend(e);
                        continue;
                    }
                }
            };
            for (k, v) in partial.stage {
                stage.insert(k, v);
            }
            for p in partial.prims {
                merge_prim(&mut prims, p);
            }
        }

        // Forget blocks that disappeared from the document.
        self.cache.retain(|k, _| seen.contains(k));

        if !diags.is_empty() {
            return Err(diags);
        }
        let mut prims: Vec<PrimBlock> = prims.into_values().collect();
        prims.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(ParsedDocument { stage, prims })
    }
}

/// Merge a prim, preferring the more-defined opinion on path collision.
/// Implicit ancestors are empty `Xform`s; an explicit table carries schema/props.
fn merge_prim(map: &mut BTreeMap<PrimPath, PrimBlock>, p: PrimBlock) {
    match map.entry(p.path.clone()) {
        Entry::Vacant(v) => {
            v.insert(p);
        }
        Entry::Occupied(mut o) => {
            if definition_score(&p) > definition_score(o.get()) {
                o.insert(p);
            }
        }
    }
}

fn definition_score(p: &PrimBlock) -> usize {
    p.props.len() + if p.schema != "Xform" { 1000 } else { 0 }
}

fn hash_text(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Split text into header-delimited blocks. The preamble (before the first
/// header) is a block with key `""`; `[stage]` has key `"stage"`; a prim table
/// `[World.Forest]` has key `"World.Forest"`.
fn split_blocks(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut key = String::new();
    let mut buf = String::new();
    for line in text.lines() {
        if let Some(k) = header_key(line) {
            blocks.push(Block {
                key: std::mem::take(&mut key),
                text: std::mem::take(&mut buf),
            });
            key = k;
        }
        buf.push_str(line);
        buf.push('\n');
    }
    blocks.push(Block { key, text: buf });
    blocks
}

/// Detect a bare-key table header line, returning its dotted path key.
fn header_key(line: &str) -> Option<String> {
    let t = line.trim_start();
    if !t.starts_with('[') || t.starts_with("[[") {
        return None;
    }
    let end = t.find(']')?;
    let inner = &t[1..end];
    let rest = t[end + 1..].trim();
    if !rest.is_empty() && !rest.starts_with('#') {
        return None;
    }
    if inner.is_empty() || !inner.split('.').all(valid_component) {
        return None;
    }
    Some(inner.to_owned())
}

fn valid_component(c: &str) -> bool {
    let mut chs = c.chars();
    match chs.next() {
        Some(a) if a.is_ascii_alphabetic() || a == '_' => {}
        _ => return false,
    }
    chs.all(|a| a.is_ascii_alphanumeric() || a == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOREST: &str = include_str!("../../../examples/forest.rusd");

    #[test]
    fn cold_incremental_equals_full() {
        let full = syntax::parse(FOREST).unwrap();
        let mut inc = IncrementalParser::new();
        let got = inc.parse(FOREST).unwrap();
        assert_eq!(full, got);
        assert_eq!(inc.stats.reused, 0);
        assert!(inc.stats.parsed > 0);
    }

    #[test]
    fn noop_reparse_reuses_all_blocks() {
        let mut inc = IncrementalParser::new();
        inc.parse(FOREST).unwrap();
        let first = inc.stats;
        let again = inc.parse(FOREST).unwrap();
        assert_eq!(syntax::parse(FOREST).unwrap(), again);
        assert_eq!(inc.stats.parsed, 0, "no block should re-parse on no-op");
        assert_eq!(inc.stats.reused, first.parsed + first.reused);
    }

    #[test]
    fn single_block_edit_reparses_only_that_block() {
        let mut inc = IncrementalParser::new();
        inc.parse(FOREST).unwrap();
        let edited = FOREST.replace("count = 500", "count = 600");
        let full = syntax::parse(&edited).unwrap();
        let got = inc.parse(&edited).unwrap();
        assert_eq!(full, got, "incremental must equal full after edit");
        assert_eq!(inc.stats.parsed, 1, "only the Forest block changed");
        assert!(inc.stats.reused >= 5);
        let forest = got
            .prims
            .iter()
            .find(|p| p.path.as_str() == "/World/Forest")
            .unwrap();
        assert_eq!(forest.props.get("count"), Some(&crate::Value::Int(600)));
    }

    #[test]
    fn add_and_remove_block_equal_full() {
        let mut inc = IncrementalParser::new();
        inc.parse(FOREST).unwrap();
        // add a block
        let added = format!("{FOREST}\n[World.Extra]\nschema = \"Sphere\"\nradius = 2.0\n");
        assert_eq!(syntax::parse(&added).unwrap(), inc.parse(&added).unwrap());
        // remove it again
        assert_eq!(syntax::parse(FOREST).unwrap(), inc.parse(FOREST).unwrap());
        assert!(!inc.cache.contains_key("World.Extra"));
    }

    #[test]
    fn explicit_parent_props_survive_merge() {
        // [World] carries a prop; [World.Child] implies an empty /World ancestor.
        let text = "[World.Child]\nschema = \"Sphere\"\nradius = 1.0\n\n[World]\nschema = \"Xform\"\nlabel = \"root\"\n";
        let full = syntax::parse(text).unwrap();
        let mut inc = IncrementalParser::new();
        let got = inc.parse(text).unwrap();
        assert_eq!(full, got);
        let world = got
            .prims
            .iter()
            .find(|p| p.path.as_str() == "/World")
            .unwrap();
        assert_eq!(
            world.props.get("label"),
            Some(&crate::Value::String("root".into()))
        );
    }

    #[test]
    fn header_detection() {
        assert_eq!(
            header_key("[World.Forest]").as_deref(),
            Some("World.Forest")
        );
        assert_eq!(header_key("[stage] # c").as_deref(), Some("stage"));
        assert_eq!(header_key("count = 500"), None);
        assert_eq!(header_key("[[World.X]]"), None);
        assert_eq!(header_key("[1,2]"), None); // array element, not a path
    }
}
