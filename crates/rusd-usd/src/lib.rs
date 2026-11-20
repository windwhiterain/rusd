//! # rusd-usd
//!
//! Rusd ↔ USD interop bridge (M6, SPEC §9/D3). openusd does the format work:
//! export lowers/expand a `.rusd` document to a composed Stage, flattens it to a
//! single layer, and writes `usda` / `usdc` / `usdz` (chosen by file extension).
//!
//! - `bake = true` (default): cook the generators first, so the USD contains real
//!   geometry that opens in usdview / Blender / Omniverse.
//! - `bake = false`: export generator prims as custom schemas (params preserved,
//!   not renderable in a DCC without the Rusd runtime).
//!
//! An `out` spelled `.rusd` is a **document** export (v1.9): the flattened stage is
//! written back as TOML, values only, with every type taken from the schema table —
//! so it round-trips through `parse_in`/`lower` like any other `.rusd`. Properties the
//! table cannot type are left out and reported, never guessed at.
//!
//! The stage is Z-up (D7) and declares `upAxis = "Z"`; DCCs honor the declared axis.
//! (A baked Y-up conversion is a later refinement.)

use rusd_core::{Diagnostic, Severity};

/// What was exported.
#[derive(Clone, Debug)]
pub struct ExportInfo {
    pub path: String,
    pub format: String,
    pub prim_count: usize,
    pub baked: bool,
    /// What the export left out, if anything (v1.9: a `.rusd` export cannot carry a
    /// property the schema table has no type for).
    pub warnings: Vec<Diagnostic>,
}

/// Export a `.rusd` document (source text) to `out` (`.usda`/`.usdc`/`.usdz`).
///
/// `doc_dir` is the directory the text came from (v1.5.8): a relative asset path in
/// a generator parameter resolves against it. `None` for text with no file behind
/// it. The export between files passes the document's own directory.
pub fn export_text(
    text: &str,
    doc_dir: Option<&str>,
    out: &str,
    bake: bool,
) -> Result<ExportInfo, Vec<Diagnostic>> {
    let doc = rusd_core::syntax::parse_in(text, doc_dir)?;
    let vdiags = rusd_scene::validate(&doc);
    if vdiags.iter().any(|d| d.severity == Severity::Error) {
        return Err(vdiags);
    }
    let stage = if bake {
        rusd_proc::expand_doc(&doc, doc_dir)?.stage
    } else {
        rusd_scene::lower(&doc)?
    };
    let prim_count = rusd_scene::prim_count(&stage);
    let flat = stage
        .flatten()
        .map_err(|e| vec![Diagnostic::error("E_FLATTEN", e.to_string())])?;
    let format = std::path::Path::new(out)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("usda")
        .to_ascii_lowercase();
    let mut diags: Vec<Diagnostic> = Vec::new();
    if format == "rusd" {
        // A flattened stage is one layer; a document is that layer's prims.
        let rendered = rusd_scene::document::layer_to_document(&flat, &|_, _| None)
            .map_err(|e| vec![Diagnostic::error("E_EXPORT", e)])?;
        if !rendered.skipped.is_empty() {
            diags.push(Diagnostic::warning(
                "W_EXPORT_RUSD",
                format!(
                    "{} properties have no type in the schema table and are not in the \
                     document: {}",
                    rendered.skipped.len(),
                    rendered.skipped.join(", ")
                ),
            ));
        }
        std::fs::write(out, rendered.text)
            .map_err(|e| vec![Diagnostic::error("E_EXPORT", format!("{out}: {e}"))])?;
    } else {
        flat.export(out)
            .map_err(|e| vec![Diagnostic::error("E_EXPORT", e.to_string())])?;
    }
    Ok(ExportInfo {
        path: out.to_owned(),
        format,
        prim_count,
        baked: bake,
        warnings: diags,
    })
}

/// Export a `.rusd` file to `out`.
pub fn export_file(path: &str, out: &str, bake: bool) -> Result<ExportInfo, Vec<Diagnostic>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| vec![Diagnostic::error("E_READ", format!("{path}: {e}"))])?;
    let dir = std::path::Path::new(path)
        .parent()
        .map(|d| d.to_string_lossy().into_owned());
    export_text(&text, dir.as_deref(), out, bake)
}
