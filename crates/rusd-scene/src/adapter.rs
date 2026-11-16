//! openusd Stage adapter (SPEC §4, D3): lower a parsed `.rusd` document into a
//! composed openusd Stage, query it, and map composition diagnostics back to
//! Rusd JSON. The stage is Z-up right-handed (D7); USD export converts to Y-up.
//!
//! v0: builtin geometry props become typed USD attributes; generator params are
//! authored as custom attributes; distribution dicts are consumed by the cook
//! engine (M3) rather than authored as USD values.

use openusd::gf;
use openusd::pcp::CompositionDiagnostic;
use openusd::sdf::{self, AttributeSpec, PrimSpec, Specifier, Value as SdfValue, Variability};
use openusd::usd::Stage;
use rusd_core::syntax::ParsedDocument;
use rusd_core::{Diagnostic, Value};

use crate::schema::{self, PropType, SchemaKind};

/// Lower a parsed document into a composed openusd Stage (anonymous root layer).
pub fn lower(doc: &ParsedDocument) -> Result<Stage, Vec<Diagnostic>> {
    let default_prim = stage_string(doc, "default_prim").unwrap_or_else(|| "World".to_owned());
    let meters = match doc.stage.get("meters_per_unit") {
        Some(Value::Float(f)) => *f,
        Some(Value::Int(i)) => *i as f64,
        _ => 1.0,
    };

    let stage = Stage::builder()
        .in_memory("rusd_root.usda")
        .map_err(|e| vec![Diagnostic::error("E_OPEN", e.to_string())])?;
    let root_id = stage.root_layer().identifier().to_owned();
    {
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
    }
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
}
