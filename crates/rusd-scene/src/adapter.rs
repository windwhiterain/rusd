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
    let default_prim = stage_string(doc, "default_prim").unwrap_or_else(|| "World".to_owned());
    let meters = match doc.stage.get("meters_per_unit") {
        Some(Value::Float(f)) => *f,
        Some(Value::Int(i)) => *i as f64,
        _ => 1.0,
    };
    let root_id = stage.root_layer().identifier().to_owned();
    let mut root = stage.layer_mut(&root_id).expect("root layer exists");
    root.edit(|l| {
        let d = l.data_mut();
        let pseudo = sdf::Path::abs_root();
        d.set_field(
            &pseudo,
            "defaultPrim",
            SdfValue::Token(default_prim.clone().into()),
        );
        d.set_field(&pseudo, "upAxis", SdfValue::Token("Z".into()));
        d.set_field(&pseudo, "metersPerUnit", SdfValue::Double(meters));

        for prim in &doc.prims {
            let schema = schema::lookup(&prim.schema);
            let custom = schema
                .map(|s| s.kind == SchemaKind::Generator)
                .unwrap_or(true);
            PrimSpec::new(d, prim.path.as_str(), Specifier::Def, prim.schema.clone())?;
            for (name, val) in &prim.props {
                let ty = schema
                    .and_then(|s| s.props.iter().find(|p| p.name == name))
                    .map(|p| p.ty);
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
    })
    .map_err(|e| vec![Diagnostic::error("E_LOWER", e.to_string())])?;
    Ok(())
}

/// Lower a parsed document into a composed openusd Stage (anonymous root layer).
pub fn lower(doc: &ParsedDocument) -> Result<Stage, Vec<Diagnostic>> {
    let stage = Stage::builder()
        .in_memory("rusd_root.usda")
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
    let stage = Stage::builder()
        .session_layer(session_path.to_str().expect("session path utf8"))
        .in_memory("rusd_root.usda")
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
    if let Some(nums) = as_nums(items) {
        match ty {
            Some(Vec3) if nums.len() == 3 => {
                return Some((
                    SdfValue::Vec3f(gf::vec3f(nums[0] as f32, nums[1] as f32, nums[2] as f32)),
                    "vector3f",
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

#[cfg(test)]
mod tests {
    use super::*;
    use rusd_core::syntax;

    const FOREST: &str = include_str!("../../../examples/forest.rusd");
    const CITY: &str = include_str!("../../../examples/city.rusd");

    #[test]
    fn lower_forest_opens_and_composes() {
        let doc = syntax::parse(FOREST).unwrap();
        let stage = lower(&doc).expect("forest should lower");
        assert!(composition_diags(&stage).is_empty());
        let pine = stage
            .prim(sdf::path("/World/Prototypes/Pine").unwrap())
            .unwrap();
        assert_eq!(pine.type_name().unwrap().unwrap().to_string(), "Sphere");
        let radius = pine.attribute("radius").get::<f64>().unwrap();
        assert_eq!(radius, Some(0.5));
    }

    #[test]
    fn lower_city_opens() {
        let doc = syntax::parse(CITY).unwrap();
        let stage = lower(&doc).expect("city should lower");
        assert!(composition_diags(&stage).is_empty());
        let grid = stage.prim(sdf::path("/World/City").unwrap()).unwrap();
        assert_eq!(
            grid.type_name().unwrap().unwrap().to_string(),
            "RusdStreetGrid"
        );
    }

    #[test]
    fn query_forest_reports_prims() {
        let doc = syntax::parse(FOREST).unwrap();
        let stage = lower(&doc).unwrap();
        let infos = query(&stage, &doc);
        assert_eq!(infos.len(), 5);
        let pine = infos
            .iter()
            .find(|i| i.path == "/World/Prototypes/Pine")
            .unwrap();
        assert_eq!(pine.type_name, "Sphere");
        assert!(pine.props.iter().any(|(n, _)| n == "radius"));
    }

    #[test]
    fn zup_metadata_authored() {
        let doc = syntax::parse(FOREST).unwrap();
        let stage = lower(&doc).unwrap();
        let root = stage.root_layer();
        let up = root.data().get_field(&sdf::Path::abs_root(), "upAxis");
        assert!(matches!(up, Ok(v) if matches!(&*v, SdfValue::Token(t) if t.as_str() == "Z")));
    }

    #[test]
    fn overlay_overrides_base() {
        let doc = syntax::parse(FOREST).unwrap();
        let mut ov = Overlay::new(&doc).unwrap();
        assert_eq!(ov.read_f64("/World/Prototypes/Pine", "radius"), 0.5);
        ov.set_attr(
            "agent",
            "/World/Prototypes/Pine",
            "radius",
            Value::Float(2.0),
        )
        .unwrap();
        assert_eq!(ov.read_f64("/World/Prototypes/Pine", "radius"), 2.0);
        // base layer untouched
        let root = ov.stage().root_layer();
        let base = root
            .data()
            .get_field(
                &sdf::path("/World/Prototypes/Pine.radius").unwrap(),
                "default",
            )
            .unwrap();
        assert!(matches!(&*base, SdfValue::Double(d) if *d == 0.5));
    }

    #[test]
    fn overlay_undo_restores_base() {
        let doc = syntax::parse(FOREST).unwrap();
        let mut ov = Overlay::new(&doc).unwrap();
        ov.set_attr(
            "agent",
            "/World/Prototypes/Pine",
            "radius",
            Value::Float(2.0),
        )
        .unwrap();
        assert_eq!(ov.read_f64("/World/Prototypes/Pine", "radius"), 2.0);
        ov.undo_all();
        assert_eq!(ov.read_f64("/World/Prototypes/Pine", "radius"), 0.5);
        ov.redo_all();
        assert_eq!(ov.read_f64("/World/Prototypes/Pine", "radius"), 2.0);
    }

    #[test]
    fn overlay_provenance_recorded() {
        let doc = syntax::parse(FOREST).unwrap();
        let mut ov = Overlay::new(&doc).unwrap();
        ov.set_attr(
            "agent-1",
            "/World/Prototypes/Pine",
            "radius",
            Value::Float(2.0),
        )
        .unwrap();
        ov.define_prim("agent-1", "/World/Landmark", "Sphere")
            .unwrap();
        let prov = ov.provenance();
        assert_eq!(prov.len(), 2);
        assert_eq!(prov[0].author, "agent-1");
        assert_eq!(prov[0].op, "set_attr");
        assert_eq!(prov[0].property.as_deref(), Some("radius"));
        assert_eq!(prov[1].op, "define_prim");
        assert!(ov.read_f64("/World/Landmark", "radius") == 0.0); // new prim exists
        assert!(
            ov.stage()
                .prim(sdf::path("/World/Landmark").unwrap())
                .is_ok()
        );
    }

    #[test]
    fn overlay_define_prim_undo() {
        let doc = syntax::parse(FOREST).unwrap();
        let mut ov = Overlay::new(&doc).unwrap();
        let before = prim_count(ov.stage());
        ov.define_prim("agent", "/World/Extra", "Cube").unwrap();
        assert_eq!(prim_count(ov.stage()), before + 1);
        ov.undo_all();
        assert_eq!(prim_count(ov.stage()), before);
    }
}
