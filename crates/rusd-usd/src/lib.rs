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
}

/// Export a `.rusd` document (source text) to `out` (`.usda`/`.usdc`/`.usdz`).
pub fn export_text(text: &str, out: &str, bake: bool) -> Result<ExportInfo, Vec<Diagnostic>> {
    let doc = rusd_core::syntax::parse(text)?;
    let vdiags = rusd_scene::validate(&doc);
    if vdiags.iter().any(|d| d.severity == Severity::Error) {
        return Err(vdiags);
    }
    let stage = if bake {
        rusd_proc::expand_doc(&doc)?.stage
    } else {
        rusd_scene::lower(&doc)?
    };
    let prim_count = rusd_scene::prim_count(&stage);
    let flat = stage
        .flatten()
        .map_err(|e| vec![Diagnostic::error("E_FLATTEN", e.to_string())])?;
    flat.export(out)
        .map_err(|e| vec![Diagnostic::error("E_EXPORT", e.to_string())])?;
    let format = std::path::Path::new(out)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("usda")
        .to_owned();
    Ok(ExportInfo {
        path: out.to_owned(),
        format,
        prim_count,
        baked: bake,
    })
}

/// Export a `.rusd` file to `out`.
pub fn export_file(path: &str, out: &str, bake: bool) -> Result<ExportInfo, Vec<Diagnostic>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| vec![Diagnostic::error("E_READ", format!("{path}: {e}"))])?;
    export_text(&text, out, bake)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOREST: &str = include_str!("../../../examples/forest.rusd");
    const CITY: &str = include_str!("../../../examples/city.rusd");

    fn tmp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rusd_usd_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn export_usda_baked_roundtrip() {
        let out = tmp("forest.usda");
        let info = export_text(FOREST, out.to_str().unwrap(), true).unwrap();
        assert!(info.baked);
        assert_eq!(info.format, "usda");
        assert!(info.prim_count >= 505);
        assert!(out.exists());
        // Re-import with openusd: it must compose back to the same prim count.
        let stage = openusd::usd::Stage::open(out.to_str().unwrap()).unwrap();
        assert_eq!(rusd_scene::prim_count(&stage), info.prim_count);
    }

    #[test]
    fn export_usdc_baked_roundtrip() {
        let out = tmp("city.usdc");
        let info = export_text(CITY, out.to_str().unwrap(), true).unwrap();
        assert_eq!(info.format, "usdc");
        assert!(info.prim_count >= 475);
        let stage = openusd::usd::Stage::open(out.to_str().unwrap()).unwrap();
        assert_eq!(rusd_scene::prim_count(&stage), info.prim_count);
    }

    #[test]
    fn export_unbaked_keeps_generator_prims() {
        let out = tmp("forest_raw.usda");
        let info = export_text(FOREST, out.to_str().unwrap(), false).unwrap();
        assert!(!info.baked);
        // Unbaked: only the authored prims, not the 500 generated spheres.
        assert!(info.prim_count < 20);
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(text.contains("RusdScatter"));
    }

    #[test]
    fn export_usda_declares_zup() {
        let out = tmp("zup.usda");
        export_text(FOREST, out.to_str().unwrap(), true).unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(text.contains("upAxis = \"Z\""));
    }
}
