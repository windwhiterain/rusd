//! # rusd-agent
//!
//! The agent protocol (SPEC §8, decisions D10/D11): a **stateful session** over a
//! `.rusd` document plus a **stdio JSON-RPC 2.0** tool server (newline-delimited
//! JSON, one request/response per line). The document is the source of truth;
//! `patch` edits it (D11), and every tool re-derives the pipeline
//! (parse → validate → expand → extract → render) from the current source.
//!
//! Tools: `open`, `query`, `validate`, `expand`, `render`, `pick`, `patch`,
//! `diff`, `undo`, `redo`, `state`.
//!
//! Error-tolerant: a tool returns a JSON-RPC error or diagnostics, never panics
//! the agent loop. Render/pick write PNGs to a session output dir and return their
//! paths plus machine-readable metadata.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use openusd::sdf;
use openusd::usd::{PrimPredicate, Stage};
use rusd_core::{Diagnostic, Severity};
use rusd_proc::ExpandOutput;
use rusd_render::RenderConfig;
use rusd_render::annotate::{self, Marker, SelectMode, Selection};
use rusd_scene::geometry::{self, SceneGeometry};
use serde_json::{Value, json};

mod capture;
mod composition;
mod patch;
pub mod transport;

/// A live editing session: the current document source + undo history + renders.
pub struct Agent {
    doc: Option<String>,
    /// The directory the open document was read from (v1.5.8), which a relative
    /// asset path in a generator parameter resolves against. `None` for a document
    /// opened as `content`: it has no directory to be relative to.
    doc_dir: Option<String>,
    baseline: Option<String>,
    history: Vec<String>,
    redo_stack: Vec<String>,
    provenance: Vec<Value>,
    out_dir: PathBuf,
    seq: u64,
    /// The live stage the transport owns, when this agent runs as `rusd serve
    /// --listen` (D12): [`scene`](Self::scene) then reads that one stage —
    /// cooked, with the overrides session layer — instead of re-deriving the
    /// pipeline from the document, so a tool call and a published snapshot
    /// describe the same scene. `None` in the stdio session, which re-derives.
    live: Option<ExpandOutput>,
}

struct Scene {
    out: ExpandOutput,
    geo: SceneGeometry,
}

impl Agent {
    pub fn new() -> Self {
        let out_dir = std::env::temp_dir().join(format!("rusd_serve_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&out_dir);
        Self {
            doc: None,
            doc_dir: None,
            baseline: None,
            history: Vec::new(),
            redo_stack: Vec::new(),
            provenance: Vec::new(),
            out_dir,
            seq: 0,
            live: None,
        }
    }

    /// The current document source, or `None` before `open`.
    pub(crate) fn doc_text(&self) -> Option<&str> {
        self.doc.as_deref()
    }

    /// The directory the open document came from, which a relative asset path in a
    /// parameter resolves against (`open{path}` sets it; `open{content}` does not).
    pub(crate) fn doc_dir(&self) -> Option<&str> {
        self.doc_dir.as_deref()
    }

    /// Replace the document without touching the history stacks: the
    /// transport keeps its own undo faces (REMOTE §3: both edit faces undo).
    pub(crate) fn replace_doc(&mut self, text: String) {
        self.doc = Some(text);
        self.redo_stack.clear();
    }

    /// Install (or clear) the live stage [`scene`](Self::scene) reads (D12).
    pub(crate) fn set_live(&mut self, live: Option<ExpandOutput>) {
        self.live = live;
    }

    /// Run the stdio JSON-RPC loop until stdin closes.
    pub fn serve_stdio(&mut self) {
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout().lock();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let resp = match serde_json::from_str::<Value>(&line) {
                Ok(req) => self.handle(&req),
                Err(e) => json!({
                    "jsonrpc": "2.0", "id": Value::Null,
                    "error": {"code": -32700, "message": format!("parse error: {e}")}
                }),
            };
            let _ = writeln!(stdout, "{}", serde_json::to_string(&resp).unwrap());
            let _ = stdout.flush();
        }
    }

    fn handle(&mut self, req: &Value) -> Value {
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
        match self.call(method, &params) {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(message) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": message}})
            }
        }
    }

    fn call(&mut self, method: &str, p: &Value) -> Result<Value, String> {
        match method {
            "open" => self.open(p),
            "query" => self.query(),
            "validate" => self.validate(),
            "expand" => self.expand(),
            "render" => self.render(p),
            "pick" => self.pick(p),
            "patch" => self.patch(p),
            "diff" => self.diff(),
            "undo" => self.undo(),
            "redo" => self.redo(),
            "export" => self.export_usd(p),
            "state" => Ok(json!({"doc": self.doc, "open": self.doc.is_some()})),
            _ => Err(format!("unknown method: {method}")),
        }
    }

    // --- tools ------------------------------------------------------------

    fn open(&mut self, p: &Value) -> Result<Value, String> {
        let (text, dir) = if let Some(content) = p.get("content").and_then(Value::as_str) {
            (content.to_owned(), None)
        } else if let Some(path) = p.get("path").and_then(Value::as_str) {
            let text =
                std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
            let dir = std::path::Path::new(path)
                .parent()
                .map(|d| d.to_string_lossy().into_owned());
            (text, dir)
        } else {
            return Err("open: provide `path` or `content`".into());
        };
        let doc = rusd_core::syntax::parse_in(&text, dir.as_deref()).map_err(diags_msg)?;
        let diags = rusd_scene::validate(&doc);
        self.doc = Some(text.clone());
        self.doc_dir = dir;
        self.baseline = Some(text);
        self.history.clear();
        self.redo_stack.clear();
        self.provenance.clear();
        self.seq = 0;
        Ok(json!({"prim_count": doc.prims.len(), "diags": diags_json(&diags)}))
    }

    fn scene(&self) -> Result<Scene, String> {
        if let Some(live) = &self.live {
            return Ok(Scene {
                out: ExpandOutput {
                    stage: live.stage.clone(),
                    stats: live.stats,
                    diags: live.diags.clone(),
                    prim_count: live.prim_count,
                    node_count: live.node_count,
                    ops: live.ops.clone(),
                },
                geo: geometry::extract(&live.stage),
            });
        }
        let text = self.doc.as_ref().ok_or("no scene open; call open first")?;
        let doc = rusd_core::syntax::parse_in(text, self.doc_dir.as_deref()).map_err(diags_msg)?;
        let vdiags = rusd_scene::validate(&doc);
        if vdiags.iter().any(|d| d.severity == Severity::Error) {
            return Err(diags_msg(vdiags));
        }
        let out = rusd_proc::expand_doc(&doc, self.doc_dir.as_deref()).map_err(diags_msg)?;
        let geo = geometry::extract(&out.stage);
        Ok(Scene { out, geo })
    }

    fn query(&self) -> Result<Value, String> {
        let s = self.scene()?;
        Ok(json!({"prims": query_prims(&s.out.stage), "diags": diags_json(&s.out.diags)}))
    }

    fn validate(&self) -> Result<Value, String> {
        let text = self.doc.as_ref().ok_or("no scene open; call open first")?;
        let doc = rusd_core::syntax::parse_in(text, self.doc_dir.as_deref()).map_err(diags_msg)?;
        let diags = rusd_scene::validate(&doc);
        Ok(json!({"prim_count": doc.prims.len(), "diags": diags_json(&diags)}))
    }

    fn expand(&self) -> Result<Value, String> {
        let s = self.scene()?;
        Ok(json!({
            "prim_count": s.out.prim_count,
            "node_count": s.out.node_count,
            "stats": {"cooked": s.out.stats.cooked, "reused": s.out.stats.reused, "skipped": s.out.stats.skipped},
            "geo_items": s.geo.items.len(),
            "diags": diags_json(&s.out.diags),
        }))
    }

    fn render(&mut self, p: &Value) -> Result<Value, String> {
        let s = self.scene()?;
        let cfg = render_config(p);
        let selections = parse_selections(p);
        let markers = parse_markers(p);
        let mut r = rusd_render::render_with(&s.geo, &cfg).map_err(diags_msg)?;
        annotate::apply_selection(&mut r, &selections, &cfg.style);
        annotate::draw_markers(&mut r, &markers, &cfg.style);
        self.seq += 1;
        let stem = self.out_dir.join(format!("render_{}", self.seq));
        let beauty = format!("{}_beauty.png", stem.display());
        let ids = format!("{}_ids.png", stem.display());
        rusd_render::capture::write_png(&beauty, r.width, r.height, &r.beauty)
            .map_err(|e| e.to_string())?;
        rusd_render::capture::write_png(
            &ids,
            r.width,
            r.height,
            &rusd_render::capture::ids_to_rgba(&r.ids),
        )
        .map_err(|e| e.to_string())?;
        let fg = r.ids.iter().filter(|&&id| id != 0).count();
        Ok(json!({
            "beauty": beauty, "ids": ids,
            "width": r.width, "height": r.height,
            "items": r.paths.len(), "foreground": fg,
            "paths": r.paths, "diags": diags_json(&r.diags),
        }))
    }

    fn pick(&mut self, p: &Value) -> Result<Value, String> {
        let x = p
            .get("x")
            .and_then(Value::as_u64)
            .ok_or("pick: `x` required")? as u32;
        let y = p
            .get("y")
            .and_then(Value::as_u64)
            .ok_or("pick: `y` required")? as u32;
        let s = self.scene()?;
        let cfg = render_config(p);
        let r = rusd_render::render_with(&s.geo, &cfg).map_err(diags_msg)?;
        Ok(json!({"x": x, "y": y, "path": r.pick(x, y)}))
    }

    fn patch(&mut self, p: &Value) -> Result<Value, String> {
        let ops = p
            .get("ops")
            .and_then(Value::as_array)
            .ok_or("patch: `ops` array required")?;
        let current = self.doc.clone().ok_or("no scene open; call open first")?;
        let new_text = patch::apply(&current, ops)?;
        let diags = match rusd_core::syntax::parse_in(&new_text, self.doc_dir.as_deref()) {
            Ok(doc) => rusd_scene::validate(&doc),
            Err(d) => d,
        };
        self.history.push(current);
        self.redo_stack.clear();
        self.doc = Some(new_text);
        self.provenance.extend(ops.iter().cloned());
        Ok(json!({"applied": ops.len(), "diags": diags_json(&diags)}))
    }

    fn undo(&mut self) -> Result<Value, String> {
        let prev = self.history.pop().ok_or("nothing to undo")?;
        if let Some(cur) = self.doc.take() {
            self.redo_stack.push(cur);
        }
        self.doc = Some(prev);
        Ok(json!({"ok": true, "undo_depth": self.history.len()}))
    }

    fn redo(&mut self) -> Result<Value, String> {
        let next = self.redo_stack.pop().ok_or("nothing to redo")?;
        if let Some(cur) = self.doc.take() {
            self.history.push(cur);
        }
        self.doc = Some(next);
        Ok(json!({"ok": true, "redo_depth": self.redo_stack.len()}))
    }

    fn diff(&self) -> Result<Value, String> {
        let doc = self.doc.as_ref().ok_or("no scene open; call open first")?;
        let changed = self.baseline.as_deref() != Some(doc.as_str());
        Ok(json!({
            "changed": changed,
            "ops_applied": self.provenance.len(),
            "provenance": self.provenance,
        }))
    }
    fn export_usd(&self, p: &Value) -> Result<Value, String> {
        let out = p
            .get("out")
            .and_then(Value::as_str)
            .ok_or("export: `out` path required")?;
        let bake = p.get("bake").and_then(Value::as_bool).unwrap_or(true);
        let text = self.doc.as_ref().ok_or("no scene open; call open first")?;
        let info =
            rusd_usd::export_text(text, self.doc_dir.as_deref(), out, bake).map_err(diags_msg)?;
        Ok(
            json!({"path": info.path, "format": info.format, "prim_count": info.prim_count, "baked": info.baked}),
        )
    }
}

impl Default for Agent {
    fn default() -> Self {
        Self::new()
    }
}

// --- helpers --------------------------------------------------------------

pub(crate) fn diags_json(diags: &[Diagnostic]) -> Value {
    Value::Array(
        diags
            .iter()
            .filter_map(|d| serde_json::from_str(&d.to_json()).ok())
            .collect(),
    )
}

fn diags_msg(diags: Vec<Diagnostic>) -> String {
    serde_json::to_string(&diags_json(&diags)).unwrap_or_else(|_| "diagnostics".into())
}

fn query_prims(stage: &Stage) -> Vec<Value> {
    let mut paths: Vec<String> = Vec::new();
    let _ = stage.traverse(PrimPredicate::ALL, |p| paths.push(p.as_str().to_owned()));
    let mut out = Vec::new();
    for path in paths {
        if path == "/" || path.is_empty() {
            continue;
        }
        let Ok(spath) = sdf::path(&path) else {
            continue;
        };
        let Ok(prim) = stage.prim(spath) else {
            continue;
        };
        let ty = prim
            .type_name()
            .ok()
            .flatten()
            .map(|t| t.to_string())
            .unwrap_or_default();
        out.push(json!({"path": path, "type": ty}));
    }
    out
}

fn render_config(p: &Value) -> RenderConfig {
    let mut cfg = RenderConfig::default();
    if let Some(v) = p.get("width").and_then(Value::as_u64) {
        cfg.width = v as u32;
    }
    if let Some(v) = p.get("height").and_then(Value::as_u64) {
        cfg.height = v as u32;
    }
    if let Some(v) = p.get("fov").and_then(Value::as_f64) {
        cfg.fov_deg = Some(v);
    }
    if let Some(v) = p.get("ambient").and_then(Value::as_f64) {
        cfg.ambient = v as f32;
    }
    if let Some(v) = p.get("diffuse").and_then(Value::as_f64) {
        cfg.diffuse = v as f32;
    }
    if let Some(c) = p
        .get("background")
        .and_then(Value::as_str)
        .and_then(annotate::parse_color)
    {
        cfg.background = srgb_to_linear(c);
    }
    if let Some(a) = p.get("light").and_then(Value::as_array)
        && a.len() == 3
        && let (Some(x), Some(y), Some(z)) = (a[0].as_f64(), a[1].as_f64(), a[2].as_f64())
    {
        cfg.light_dir = [x as f32, y as f32, z as f32];
    }
    if let Some(v) = p.get("outline_width").and_then(Value::as_u64) {
        cfg.style.outline_width = v as u32;
    }
    if let Some(v) = p.get("marker_radius").and_then(Value::as_u64) {
        cfg.style.marker_radius = v as u32;
    }
    if let Some(v) = p.get("recolor_shading").and_then(Value::as_f64) {
        cfg.style.recolor_shading = v as f32;
    }
    cfg
}

fn parse_selections(p: &Value) -> Vec<Selection> {
    let mut out = Vec::new();
    if let Some(arr) = p.get("select").and_then(Value::as_array) {
        for s in arr {
            let Some(path) = s.get("path").and_then(Value::as_str) else {
                continue;
            };
            let color = s
                .get("color")
                .and_then(Value::as_str)
                .and_then(annotate::parse_color)
                .unwrap_or([255, 60, 60]);
            let mode = match s.get("mode").and_then(Value::as_str).unwrap_or("outline") {
                "recolor" => SelectMode::Recolor,
                "both" => SelectMode::Both,
                _ => SelectMode::Outline,
            };
            out.push(Selection {
                path: path.to_owned(),
                color,
                mode,
            });
        }
    }
    out
}

fn parse_markers(p: &Value) -> Vec<Marker> {
    let mut out = Vec::new();
    if let Some(arr) = p.get("mark").and_then(Value::as_array) {
        for (i, m) in arr.iter().enumerate() {
            let (Some(x), Some(y)) = (
                m.get("x").and_then(Value::as_u64),
                m.get("y").and_then(Value::as_u64),
            ) else {
                continue;
            };
            let color = m
                .get("color")
                .and_then(Value::as_str)
                .and_then(annotate::parse_color)
                .unwrap_or_else(|| annotate::palette(i));
            out.push(Marker {
                x: x as u32,
                y: y as u32,
                color,
            });
        }
    }
    out
}

fn srgb_to_linear(c: [u8; 3]) -> [f32; 3] {
    fn ch(v: u8) -> f32 {
        let c = v as f32 / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    [ch(c[0]), ch(c[1]), ch(c[2])]
}

/// Entry point for `rusd serve`.
pub fn serve() {
    Agent::new().serve_stdio();
}
