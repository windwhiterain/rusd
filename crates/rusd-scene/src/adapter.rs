//! openusd Stage adapter (SPEC §4, D3): lower a parsed `.rusd` document into a
//! composed openusd Stage, query it, map composition diagnostics back to Rusd
//! JSON, and provide a non-destructive session [`Overlay`] for agent edits.
//! The stage is Z-up right-handed (D7); USD export converts to Y-up.

use openusd::gf;
use openusd::pcp::CompositionDiagnostic;
use openusd::sdf::{self, AttributeSpec, PrimSpec, Specifier, Value as SdfValue, Variability};
use openusd::usd::Stage;
use rusd_core::syntax::ParsedDocument;
use rusd_core::{Diagnostic, Value};

use crate::schema::{self, PropType, SchemaKind};

/// Author the document's stage metadata and prims into the stage's root layer.
fn author_base(stage: &Stage, doc: &ParsedDocument) -> Result<(), Vec<Diagnostic>> {
    let root_id = stage.root_layer().identifier().to_owned();
    let mut root = stage.layer_mut(&root_id).expect("root layer exists");
    // A document's diagnostics are richer than the single `AuthoringError` an edit can
    // carry, so the edit is the mechanism and this is the answer: the first failure is the
    // document's own, and the edit's error is only the fact that it stopped.
    let mut failure: Option<Vec<Diagnostic>> = None;
    let edited = root.edit(|l| match author_into(l.data_mut(), doc) {
        Ok(()) => Ok(()),
        Err(diags) => {
            failure = Some(diags);
            Err(openusd::sdf::AuthoringError::InvalidPath {
                path: sdf::Path::abs_root(),
                reason: "the document could not be authored into this layer",
            })
        }
    });
    match failure {
        Some(diags) => Err(diags),
        None => edited
            .map(|_| ())
            .map_err(|e| vec![Diagnostic::error("E_LOWER", e.to_string())]),
    }
}

/// Author a parsed document into **any** layer's data (v1.9).
///
/// This is the one definition of what a `.rusd` document means, and it has two callers
/// that must not drift: [`lower`], which authors it into a stage's root layer, and
/// [`crate::document`]'s decoder, which the registered `.rusd` file format calls when
/// the composition opens a document as a layer. One function, so a document reads the
/// same whether it is the root of a stage or a layer an arc reached.
pub(crate) fn author_into(
    d: &mut dyn sdf::AbstractData,
    doc: &ParsedDocument,
) -> Result<(), Vec<Diagnostic>> {
    let default_prim = stage_string(doc, "default_prim").unwrap_or_else(|| "World".to_owned());
    let meters = match doc.stage.get("meters_per_unit") {
        Some(Value::Float(f)) => *f,
        Some(Value::Int(i)) => *i as f64,
        _ => 1.0,
    };
    // Every `references` value resolved **before** the layer is edited: a missing
    // layer is a diagnostic about a document, not an authoring failure, and the
    // edit closure can only carry an `AuthoringError`. Frame the answer to the same
    // `Vec<Diagnostic>` the rest of lowering answers with.
    let mut unresolved: Vec<Diagnostic> = Vec::new();
    let references: Vec<Option<crate::reference::AssetRef>> = doc
        .prims
        .iter()
        .map(|prim| {
            let raw = prim.references.as_deref()?;
            match crate::reference::parse(raw, doc.doc_dir.as_deref()) {
                Ok(parsed) => Some(parsed),
                Err(e) => {
                    unresolved.push(crate::validate::reference_diag(&e, prim.path.as_str(), raw));
                    None
                }
            }
        })
        .collect();
    if !unresolved.is_empty() {
        return Err(unresolved);
    }
    author_prims(d, doc, &default_prim, meters, &references)
        .map_err(|e| vec![Diagnostic::error("E_LOWER", e.to_string())])
}

/// The authoring proper: stage metadata, then every prim.
fn author_prims(
    d: &mut dyn sdf::AbstractData,
    doc: &ParsedDocument,
    default_prim: &str,
    meters: f64,
    references: &[Option<crate::reference::AssetRef>],
) -> Result<(), openusd::sdf::AuthoringError> {
    {
        let pseudo = sdf::Path::abs_root();
        // A layer read from a file has a pseudo-root spec from its reader; a `Data` the
        // decoder builds starts empty, so it gets one here. Either way the metadata below
        // has a spec to sit on.
        if d.spec_type(&pseudo).is_none() {
            d.create_spec(pseudo.clone(), sdf::SpecType::PseudoRoot);
        }
        d.set_field(
            &pseudo,
            "defaultPrim",
            SdfValue::Token(default_prim.to_owned().into()),
        );
        d.set_field(&pseudo, "upAxis", SdfValue::Token("Z".into()));
        d.set_field(&pseudo, "metersPerUnit", SdfValue::Double(meters));

        // **A graph document authors no prims** (v1.9): the document is an operator graph,
        // not USD. Its prims are the operators the cook runs — the picture is made from the
        // layers those operators write, not from the document. Only the stage metadata just
        // above is authored.
        let authored: &[rusd_core::syntax::PrimBlock] = if doc.graph { &[] } else { &doc.prims };
        for (index, prim) in authored.iter().enumerate() {
            let schema = schema::lookup(&prim.schema);
            let custom = schema
                .map(|s| s.kind == SchemaKind::Generator)
                .unwrap_or(true);
            // v1.5.10: a prim with no declared schema composes its type from the
            // layer its `references` names, so its own spec declares none.
            PrimSpec::new(d, prim.path.as_str(), Specifier::Def, prim.schema.clone())?;
            // The `references` arc, authored as a composition field on this very
            // spec (the way `RusdInstances` authors an instance's arc onto the
            // prototype). It is **not** an attribute: nothing of it is written as a
            // property, so it never reaches the geometry layer. The key already
            // resolved to an absolute layer above, which is what makes the
            // anonymous root layer able to name it at all.
            if let Some(reference) = &references[index] {
                author_reference(d, &prim.path, reference)?;
            }
            for (name, val) in &prim.props {
                // v1.5.9: a generator node's `description` is metadata about the
                // node, not geometry — the tree reports it beside the layer, and the
                // layer itself must not gain an attribute nobody asked for.
                if name == "description" && schema.is_some_and(|s| s.kind == SchemaKind::Generator)
                {
                    continue;
                }
                let ty = schema::prop_type(prim.schema.as_str(), name);
                if let Some((sval, tname)) = to_usd(val, ty) {
                    let attr_path = format!("{}.{}", prim.path.as_str(), name);
                    let mut attr = AttributeSpec::new(
                        d,
                        attr_path.as_str(),
                        tname,
                        Variability::Uniform,
                        custom,
                    )?;
                    attr.set_default(sval)?;
                }
            }
        }
        Ok(())
    }
}

/// Author one prim's `references` arc (v1.5.10) onto its own spec: the layer the
/// key names, as authored, and the prim inside it.
///
/// The arc is a **composition field**, written the way `RusdInstances` writes an
/// instance's arc (`sdf::ReferenceListOp::explicit` + `FieldKey::References`), so a
/// reader of the layer sees a reference and not an attribute that happens to be
/// named like one. An internal reference (a prim of this stage) leaves
/// `asset_path` empty, which is USD's spelling for it.
fn author_reference(
    d: &mut dyn sdf::AbstractData,
    prim: &rusd_core::PrimPath,
    reference: &crate::reference::AssetRef,
) -> Result<(), openusd::sdf::AuthoringError> {
    let path = sdf::path(prim.as_str())?;
    let op = sdf::ReferenceListOp::explicit([sdf::Reference {
        // **As authored** (v1.9): a relative `../box.rusd` stays relative, so the layer
        // resolves from wherever the file is read — the record can be copied to another
        // machine and still open.
        asset_path: reference
            .authored
            .clone()
            .or_else(|| reference.asset.clone())
            .unwrap_or_default(),
        prim_path: reference.path.clone(),
        ..Default::default()
    }]);
    d.set_field(
        &path,
        sdf::FieldKey::References.as_str(),
        SdfValue::ReferenceListOp(op),
    );
    Ok(())
}

/// Lower a parsed document into a composed openusd Stage (anonymous root layer).
pub fn lower(doc: &ParsedDocument) -> Result<Stage, Vec<Diagnostic>> {
    let stage = crate::document::stage_builder()
        .in_memory("rusd_root.rusd")
        .map_err(|e| vec![Diagnostic::error("E_OPEN", e.to_string())])?;
    author_base(&stage, doc)?;
    Ok(stage)
}

/// Like [`lower`], but adds an empty session layer for non-destructive agent edits.
static SESSION_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A unique on-disk session layer path (the builder resolves session layers via
/// the asset resolver, which requires a real path; edits stay in memory).
fn new_session_file() -> std::path::PathBuf {
    let n = SESSION_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("rusd_session_{}_{n}.usda", std::process::id()))
}

pub fn lower_with_session(doc: &ParsedDocument) -> Result<Stage, Vec<Diagnostic>> {
    let session_path = new_session_file();
    std::fs::write(&session_path, "#usda 1.0\n")
        .map_err(|e| vec![Diagnostic::error("E_SESSION", e.to_string())])?;
    // The session layer is the one layer the engine really does read from disk before
    // there is any document to compile, so it keeps the `usda` spelling: it is a scratch
    // file nothing names, and its row in the tree is labelled `session`, not by its path.
    let stage = crate::document::stage_builder()
        .session_layer(session_path.to_str().expect("session path utf8"))
        .in_memory("rusd_root.rusd")
        .map_err(|e| vec![Diagnostic::error("E_OPEN", e.to_string())])?;
    author_base(&stage, doc)?;
    Ok(stage)
}

fn stage_string(doc: &ParsedDocument, key: &str) -> Option<String> {
    match doc.stage.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// Map openusd composition diagnostics to stable-code Rusd diagnostics.
pub fn composition_diags(stage: &Stage) -> Vec<Diagnostic> {
    stage
        .composition_errors()
        .iter()
        .map(|d| {
            let code = match d {
                CompositionDiagnostic::ArcCycle(_) => "E_ARC_CYCLE",
                CompositionDiagnostic::UnresolvedLayer { .. } => "E_UNRESOLVED_LAYER",
                CompositionDiagnostic::MalformedLayer { .. } => "E_MALFORMED_LAYER",
                CompositionDiagnostic::MutedAssetPath { .. } => "E_MUTED_ASSET",
                _ => "E_COMPOSE",
            };
            Diagnostic::error(code, d.to_string())
        })
        .collect()
}

/// A composed prim's query result.
#[derive(Clone, Debug)]
pub struct PrimInfo {
    pub path: String,
    pub type_name: String,
    pub props: Vec<(String, String)>,
}

/// Query the composed stage for each prim in the document.
pub fn query(stage: &Stage, doc: &ParsedDocument) -> Vec<PrimInfo> {
    let mut out = Vec::new();
    for p in &doc.prims {
        let Ok(path) = sdf::path(p.path.as_str()) else {
            continue;
        };
        let Ok(prim) = stage.prim(path) else { continue };
        let type_name = prim
            .type_name()
            .ok()
            .flatten()
            .map(|t| t.to_string())
            .unwrap_or_default();
        let mut props = Vec::new();
        for name in p.props.keys() {
            let attr = prim.attribute(name.as_str());
            if let Ok(Some(v)) = attr.get::<SdfValue>() {
                props.push((name.clone(), format!("{v:?}")));
            }
        }
        out.push(PrimInfo {
            path: p.path.as_str().to_owned(),
            type_name,
            props,
        });
    }
    out
}

/// Count composed prims in the stage (all predicates).
pub fn prim_count(stage: &Stage) -> usize {
    let mut n = 0;
    let _ = stage.traverse(openusd::usd::PrimPredicate::ALL, |_| n += 1);
    n
}

/// One audited agent edit (Rusd-side provenance; openusd StageSink/Diff is the
/// richer transferable mechanism available for cross-process replay).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditRecord {
    pub author: String,
    pub op: String,
    pub prim: String,
    pub property: Option<String>,
}

/// A non-destructive editing session over a lowered stage (SPEC §4).
/// Agent edits author into the session layer (stronger than base); the base is
/// untouched. `undo_all` mutes the session layer, restoring base opinions.
pub struct Overlay {
    stage: Stage,
    session_id: String,
    provenance: Vec<EditRecord>,
}

impl Overlay {
    /// Lower `doc` with a session layer ready for edits.
    pub fn new(doc: &ParsedDocument) -> Result<Self, Vec<Diagnostic>> {
        let stage = lower_with_session(doc)?;
        let session_id = stage
            .session_layer()
            .expect("session layer present")
            .identifier()
            .to_owned();
        Ok(Self {
            stage,
            session_id,
            provenance: Vec::new(),
        })
    }

    pub fn stage(&self) -> &Stage {
        &self.stage
    }

    pub fn provenance(&self) -> &[EditRecord] {
        &self.provenance
    }

    /// Set an attribute opinion in the session layer (overrides base).
    pub fn set_attr(
        &mut self,
        author: &str,
        prim: &str,
        attr: &str,
        value: Value,
    ) -> Result<(), Vec<Diagnostic>> {
        let (sval, infer_type) = to_usd(&value, None)
            .ok_or_else(|| vec![Diagnostic::error("E_VALUE", "unsupported attribute value")])?;
        let ap = sdf::path(format!("{prim}.{attr}"))
            .map_err(|e| vec![Diagnostic::error("E_PATH", e.to_string())])?;
        let tname = self
            .stage
            .prim(sdf::path(prim).map_err(|e| vec![Diagnostic::error("E_PATH", e.to_string())])?)
            .ok()
            .and_then(|p| p.attribute(attr).type_name().ok().flatten())
            .map(|t| t.to_string())
            .unwrap_or_else(|| infer_type.to_owned());
        {
            let mut session = self
                .stage
                .layer_mut(&self.session_id)
                .ok_or_else(|| vec![Diagnostic::error("E_SESSION", "no session layer")])?;
            session
                .edit(|l| {
                    let d = l.data_mut();
                    if d.has_spec(&ap) {
                        d.set_field(&ap, "default", sval);
                        Ok(())
                    } else {
                        let mut a = AttributeSpec::new(
                            d,
                            ap.as_str(),
                            tname.as_str(),
                            Variability::Uniform,
                            false,
                        )?;
                        a.set_default(sval)?;
                        Ok(())
                    }
                })
                .map_err(|e| vec![Diagnostic::error("E_EDIT", e.to_string())])?;
        }
        self.provenance.push(EditRecord {
            author: author.to_owned(),
            op: "set_attr".to_owned(),
            prim: prim.to_owned(),
            property: Some(attr.to_owned()),
        });
        Ok(())
    }

    /// Define a new prim in the session layer.
    pub fn define_prim(
        &mut self,
        author: &str,
        path: &str,
        schema: &str,
    ) -> Result<(), Vec<Diagnostic>> {
        {
            let mut session = self
                .stage
                .layer_mut(&self.session_id)
                .ok_or_else(|| vec![Diagnostic::error("E_SESSION", "no session layer")])?;
            session
                .edit(|l| {
                    PrimSpec::new(l.data_mut(), path, Specifier::Def, schema)?;
                    Ok(())
                })
                .map_err(|e| vec![Diagnostic::error("E_EDIT", e.to_string())])?;
        }
        self.provenance.push(EditRecord {
            author: author.to_owned(),
            op: "define_prim".to_owned(),
            prim: path.to_owned(),
            property: None,
        });
        Ok(())
    }

    /// Read a composed `double` attribute (base + session).
    pub fn read_f64(&self, prim: &str, attr: &str) -> f64 {
        crate::adapter::read_attr_f64_helper(&self.stage, prim, attr)
    }

    /// Undo all session edits by muting the session layer (base shows through).
    pub fn undo_all(&mut self) {
        self.stage.mute_layer(self.session_id.as_str());
    }

    /// Re-apply session edits after an undo.
    pub fn redo_all(&mut self) {
        self.stage.unmute_layer(self.session_id.as_str());
    }
}

fn read_attr_f64_helper(stage: &Stage, prim: &str, attr: &str) -> f64 {
    let Ok(p) = sdf::path(prim) else { return 0.0 };
    let Ok(pr) = stage.prim(p) else { return 0.0 };
    pr.attribute(attr)
        .get::<f64>()
        .ok()
        .flatten()
        .unwrap_or(0.0)
}

/// Coerce a Rusd value to a USD value + type name, guided by the schema type.
fn to_usd(val: &Value, ty: Option<PropType>) -> Option<(SdfValue, &'static str)> {
    use PropType::*;
    match val {
        Value::Bool(b) => Some((SdfValue::Bool(*b), "bool")),
        Value::Int(i) => match ty {
            // A `f64` property written as a whole number (`size = 6`) is still an
            // `f64` in the document's terms: the schema says so, and `int64` would be
            // a different type (v1.9 — the type comes from the schema, not the literal).
            Some(F64) => Some((SdfValue::Double(*i as f64), "double")),
            Some(U64) => Some((SdfValue::Uint64(*i as u64), "uint64")),
            _ => Some((SdfValue::Int64(*i), "int64")),
        },
        Value::Float(f) => Some((SdfValue::Double(*f), "double")),
        Value::String(s) => match ty {
            Some(Token) | Some(Path) => Some((SdfValue::Token(s.clone().into()), "token")),
            Some(Asset) => Some((SdfValue::AssetPath(s.clone().into()), "asset")),
            _ => Some((SdfValue::String(s.clone()), "string")),
        },
        Value::Token(t) => Some((SdfValue::Token(t.clone().into()), "token")),
        Value::Path(p) => Some((SdfValue::Token(p.as_str().to_owned().into()), "token")),
        Value::Asset(a) => Some((SdfValue::AssetPath(a.clone().into()), "asset")),
        Value::Vec2(v) => Some((SdfValue::DoubleVec(vec![v[0], v[1]]), "double[]")),
        Value::Vec3(v) => Some((SdfValue::Vec3f(vec3f(v)), vec3_type(ty))),
        Value::Vec4(v) => Some((SdfValue::DoubleVec(v.to_vec()), "double[]")),
        Value::Array(items) => array_to_usd(items, ty),
        Value::Dict(_) => None,
    }
}

fn vec3f(v: &[f64; 3]) -> gf::Vec3f {
    gf::vec3f(v[0] as f32, v[1] as f32, v[2] as f32)
}

fn vec3_type(ty: Option<PropType>) -> &'static str {
    if matches!(ty, Some(PropType::Color3f)) {
        "color3f"
    } else {
        "vector3f"
    }
}

fn as_nums(items: &[Value]) -> Option<Vec<f64>> {
    items
        .iter()
        .map(|v| match v {
            Value::Float(f) => Some(*f),
            Value::Int(i) => Some(*i as f64),
            _ => None,
        })
        .collect()
}

fn array_to_usd(items: &[Value], ty: Option<PropType>) -> Option<(SdfValue, &'static str)> {
    use PropType::*;
    // A `point3f[]` (D14's `Points.points`, a `Mesh`'s vertices): each element is
    // one point, written as `[x, y, z]` (or `vec3`). This must be tested **before**
    // the flat-number path, which would otherwise try to read the nested arrays as
    // numbers and give up.
    if ty == Some(Vec3Array) || ty == Some(Color3Array) {
        let mut points = Vec::with_capacity(items.len());
        for item in items {
            let v = match item {
                Value::Vec3(v) => *v,
                Value::Array(nums) if nums.len() == 3 => {
                    let a = as_nums(nums)?;
                    [a[0], a[1], a[2]]
                }
                _ => return None,
            };
            points.push(vec3f(&v));
        }
        let tname = if ty == Some(Color3Array) {
            "color3f[]"
        } else {
            "point3f[]"
        };
        return Some((SdfValue::Vec3fVec(points), tname));
    }
    // `token[]`: `xformOpOrder`. Tested before the string-array fallback below so a
    // schema type decides it rather than the shape of the value (v1.9).
    if ty == Some(TokenArray) {
        let toks: Option<Vec<openusd::tf::Token>> = items
            .iter()
            .map(|v| match v {
                Value::String(s) => Some(s.clone().into()),
                Value::Token(t) => Some(t.clone().into()),
                _ => None,
            })
            .collect();
        return toks.map(|v| (SdfValue::TokenVec(v), "token[]"));
    }
    // `int[]`: a mesh's face counts and vertex indices. Before the generic number
    // path as well — that one writes `int64[]` for integer lists, which the USD
    // topology is not authored as.
    if ty == Some(IntArray) {
        let ints: Option<Vec<i32>> = items
            .iter()
            .map(|v| match v {
                Value::Int(i) => i32::try_from(*i).ok(),
                _ => None,
            })
            .collect();
        return ints.map(|v| (SdfValue::IntVec(v), "int[]"));
    }
    if let Some(nums) = as_nums(items) {
        match ty {
            Some(Vec3) if nums.len() == 3 => {
                return Some((
                    SdfValue::Vec3f(gf::vec3f(nums[0] as f32, nums[1] as f32, nums[2] as f32)),
                    "vector3f",
                ));
            }
            Some(Point3f) if nums.len() == 3 => {
                return Some((
                    SdfValue::Vec3f(gf::vec3f(nums[0] as f32, nums[1] as f32, nums[2] as f32)),
                    "point3f",
                ));
            }
            Some(Color3f) if nums.len() == 3 => {
                return Some((
                    SdfValue::Vec3f(gf::vec3f(nums[0] as f32, nums[1] as f32, nums[2] as f32)),
                    "color3f",
                ));
            }
            _ => {
                if items.iter().all(|v| matches!(v, Value::Int(_))) {
                    let ints: Vec<i64> = items
                        .iter()
                        .map(|v| if let Value::Int(i) = v { *i } else { 0 })
                        .collect();
                    return Some((SdfValue::Int64Vec(ints), "int64[]"));
                }
                return Some((SdfValue::DoubleVec(nums), "double[]"));
            }
        }
    }
    if items
        .iter()
        .all(|v| matches!(v, Value::String(_) | Value::Token(_)))
    {
        let toks: Vec<openusd::tf::Token> = items
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone().into(),
                Value::Token(t) => t.clone().into(),
                _ => unreachable!(),
            })
            .collect();
        return Some((SdfValue::TokenVec(toks), "token[]"));
    }
    None
}
