//! City generator suite (SPEC §5, D4; docs/GENERATORS.md).
//!
//! M3 v0: `RusdStreetGrid` is a cohesive city generator (block layout + lot
//! subdivision + building massing as internal helpers); `RusdBuilding` and
//! `RusdScatter` are standalone. The fully recursive cook-graph instancing
//! (a separate BlockFill node spawned per parcel) is the P6 refinement.
//! Buildings are box massing (Cube + translate/scale); roofs/setbacks later.

use std::collections::BTreeMap;

use openusd::gf;
use openusd::sdf::{self, AttributeSpec, PrimSpec, Specifier, Value as SdfValue, Variability};
use rusd_core::{Diagnostic, Value};

use crate::generator::{GenContext, Generator};
use crate::rng::Rng;

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Float(f) => Some(*f),
        Value::Int(i) => Some(*i as f64),
        _ => None,
    }
}

fn dict_f(m: &BTreeMap<String, Value>, k: &str, d: f64) -> f64 {
    m.get(k).and_then(num).unwrap_or(d)
}

/// Sample a distribution value: `{dist="uniform",min,max}` / `{dist="normal",mu,sigma}`,
/// or a constant number.
fn sample_dist(rng: &mut Rng, v: Option<&Value>, default: f64) -> f64 {
    match v {
        Some(Value::Dict(m)) => match m.get("dist").and_then(|d| match d {
            Value::String(s) => Some(s.as_str()),
            _ => None,
        }) {
            Some("uniform") => rng.uniform(dict_f(m, "min", default), dict_f(m, "max", default)),
            Some("normal") => rng.normal(dict_f(m, "mu", default), dict_f(m, "sigma", 1.0)),
            _ => default,
        },
        Some(other) => num(other).unwrap_or(default),
        None => default,
    }
}

/// Zone presets: (height mean, height std-dev, occupancy).
fn zone_defaults(zone: &str) -> (f64, f64, f64) {
    match zone {
        "residential" => (12.0, 4.0, 0.85),
        "downtown" => (60.0, 25.0, 0.95),
        "industrial" => (10.0, 3.0, 0.8),
        _ => (24.0, 10.0, 0.9), // commercial
    }
}

type AuthResult = Result<(), sdf::AuthoringError>;

/// Author a box building: Cube massing scaled to (w, depth, height), positioned
/// at (cx, cy) rising in Z, with a display color.
fn author_building(
    d: &mut dyn sdf::AbstractData,
    path: &str,
    size: (f64, f64, f64),
    pos: (f64, f64),
    color: [f32; 3],
) -> AuthResult {
    let (w, depth, height) = size;
    let (cx, cy) = pos;
    PrimSpec::new(d, path, Specifier::Def, "Cube")?;

    let tp = format!("{path}.xformOp:translate");
    let mut ta = AttributeSpec::new(d, tp.as_str(), "point3f", Variability::Uniform, false)?;
    ta.set_default(SdfValue::Vec3f(gf::vec3f(
        cx as f32,
        cy as f32,
        (height / 2.0) as f32,
    )))?;

    let sp = format!("{path}.xformOp:scale");
    let mut sa = AttributeSpec::new(d, sp.as_str(), "float3", Variability::Uniform, false)?;
    sa.set_default(SdfValue::Vec3f(gf::vec3f(
        (w / 2.0) as f32,
        (depth / 2.0) as f32,
        (height / 2.0) as f32,
    )))?;

    let op = format!("{path}.xformOpOrder");
    let mut oa = AttributeSpec::new(d, op.as_str(), "token[]", Variability::Uniform, false)?;
    oa.set_default(SdfValue::TokenVec(vec![
        "xformOp:translate".into(),
        "xformOp:scale".into(),
    ]))?;

    let cp = format!("{path}.primvars:displayColor");
    let mut ca = AttributeSpec::new(d, cp.as_str(), "color3f[]", Variability::Varying, false)?;
    ca.set_default(SdfValue::Vec3fVec(vec![gf::vec3f(
        color[0], color[1], color[2],
    )]))?;
    Ok(())
}

fn author_translate(
    d: &mut dyn sdf::AbstractData,
    path: &str,
    x: f64,
    y: f64,
    z: f64,
) -> AuthResult {
    let tp = format!("{path}.xformOp:translate");
    let mut ta = AttributeSpec::new(d, tp.as_str(), "point3f", Variability::Uniform, false)?;
    ta.set_default(SdfValue::Vec3f(gf::vec3f(x as f32, y as f32, z as f32)))?;
    let op = format!("{path}.xformOpOrder");
    let mut oa = AttributeSpec::new(d, op.as_str(), "token[]", Variability::Uniform, false)?;
    oa.set_default(SdfValue::TokenVec(vec!["xformOp:translate".into()]))?;
    Ok(())
}

/// A city block grid: streets + parcels, each parcel filled with box buildings
/// whose heights are sampled from a zone-driven distribution. Deterministic and
/// budget-truncated (W_BUDGET) in row-major block / lot order.
pub struct RusdStreetGrid;

impl Generator for RusdStreetGrid {
    fn name(&self) -> &'static str {
        "RusdStreetGrid"
    }

    fn eval(&self, ctx: &mut GenContext) -> Result<sdf::Layer, Vec<Diagnostic>> {
        let base = ctx.path.as_str().to_owned();
        let nx = ctx.param_u64("blocks_x", 6);
        let ny = ctx.param_u64("blocks_y", 6);
        let [bw, bh] = ctx.param_vec2("block_size", [60.0, 60.0]);
        let street = ctx.param_f64("street_width", 12.0);
        let lot_width = ctx.param_f64("lot_width", 18.0).max(4.0);
        let margin = ctx.param_f64("margin", 4.0);
        let floor_height = ctx.param_f64("floor_height", 3.2);
        let max_prims = ctx.param_u64("max_prims", 20000);
        let zone = ctx.param_str("zone").unwrap_or("commercial").to_owned();
        let height_dist = ctx.params.get("height").cloned();
        let (zmu, zsigma, zocc) = zone_defaults(&zone);
        let occupancy = ctx.param_f64("occupancy", zocc);
        let mut rng = Rng::seeded(ctx.stage_seed, ctx.path.as_str(), ctx.seed);

        let mut layer = sdf::Layer::new_anonymous("rusd_city.usda");
        let mut warns: Vec<Diagnostic> = Vec::new();
        layer
            .edit(|l| {
                let d = l.data_mut();
                PrimSpec::new(d, base.as_str(), Specifier::Def, "Xform")?;

                let total_w = nx as f64 * bw + (nx as f64 + 1.0) * street;
                let total_h = ny as f64 * bh + (ny as f64 + 1.0) * street;
                let ground = format!("{base}/Ground");
                author_building(
                    d,
                    &ground,
                    (total_w, total_h, 0.5),
                    (total_w / 2.0, total_h / 2.0),
                    [0.2, 0.2, 0.22],
                )?;

                let mut count: u64 = 1;
                let mut truncated = false;
                let lots_x = (((bw - 2.0 * margin) / lot_width).floor() as u64).max(1);
                let lots_y = (((bh - 2.0 * margin) / lot_width).floor() as u64).max(1);

                'blocks: for i in 0..nx {
                    for j in 0..ny {
                        let cx = street + i as f64 * (bw + street);
                        let cy = street + j as f64 * (bh + street);
                        let block = format!("{base}/b{i}_{j}");
                        PrimSpec::new(d, block.as_str(), Specifier::Def, "Xform")?;
                        author_translate(d, &block, cx, cy, 0.0)?;
                        count += 1;

                        for li in 0..lots_x {
                            for lj in 0..lots_y {
                                if count >= max_prims {
                                    truncated = true;
                                    break 'blocks;
                                }
                                if !rng.chance(occupancy) {
                                    continue;
                                }
                                let height = sample_dist(&mut rng, height_dist.as_ref(), zmu)
                                    .max(floor_height);
                                let _ = zsigma;
                                let fw = lot_width - 2.0;
                                let fd = lot_width - 2.0;
                                // Block-local: the block prim's Xform already carries
                                // (cx, cy), so adding it again here would count the
                                // block origin twice and push the lots out past the
                                // street grid -- off the ground plane.
                                let bx = margin + (li as f64 + 0.5) * lot_width;
                                let by = margin + (lj as f64 + 0.5) * lot_width;
                                let shade = rng.uniform(0.55, 0.8) as f32;
                                let bp = format!("{block}/lot{li}_{lj}");
                                author_building(
                                    d,
                                    &bp,
                                    (fw, fd, height),
                                    (bx, by),
                                    [shade, shade, shade * 0.95],
                                )?;
                                count += 1;
                            }
                        }
                    }
                }
                if truncated {
                    warns.push(
                        Diagnostic::warning(
                            "W_BUDGET",
                            format!("max_prims={max_prims} reached; truncated deterministically"),
                        )
                        .at_prim(base.as_str()),
                    );
                }
                Ok(())
            })
            .map_err(|e| {
                vec![Diagnostic::error("E_GEN", e.to_string()).at_prim(ctx.path.as_str())]
            })?;
        for w in warns {
            ctx.warn(w);
        }
        Ok(layer)
    }
}

/// A standalone parametric box building at the anchor path.
pub struct RusdBuilding;

impl Generator for RusdBuilding {
    fn name(&self) -> &'static str {
        "RusdBuilding"
    }

    fn eval(&self, ctx: &mut GenContext) -> Result<sdf::Layer, Vec<Diagnostic>> {
        let base = ctx.path.as_str().to_owned();
        let [w, depth] = ctx.param_vec2("footprint", [10.0, 10.0]);
        let floor_height = ctx.param_f64("floor_height", 3.2);
        let height = match ctx.params.get("height") {
            Some(v) => num(v).unwrap_or(0.0),
            None => ctx.param_u64("floors", 4) as f64 * floor_height,
        }
        .max(floor_height);
        let mut layer = sdf::Layer::new_anonymous("rusd_building.usda");
        layer
            .edit(|l| {
                author_building(
                    l.data_mut(),
                    base.as_str(),
                    (w, depth, height),
                    (0.0, 0.0),
                    [0.7, 0.68, 0.64],
                )
            })
            .map_err(|e| {
                vec![Diagnostic::error("E_GEN", e.to_string()).at_prim(ctx.path.as_str())]
            })?;
        Ok(layer)
    }
}

/// Scatter prop instances (spheres) across a rectangular domain, reading the
/// prototype's radius. Demonstrates a real read dependency in the cook graph.
pub struct RusdScatter;

impl Generator for RusdScatter {
    fn name(&self) -> &'static str {
        "RusdScatter"
    }

    fn eval(&self, ctx: &mut GenContext) -> Result<sdf::Layer, Vec<Diagnostic>> {
        let base = ctx.path.as_str().to_owned();
        let count = ctx.param_u64("count", 100);
        let max_prims = ctx.param_u64("max_prims", 20000).min(count);
        let (dw, dh) = match ctx.params.get("domain") {
            Some(Value::Dict(m)) => (dict_f(m, "w", 100.0), dict_f(m, "h", 100.0)),
            _ => (100.0, 100.0),
        };
        let prototype = ctx.param_str("prototype").map(|s| s.to_owned());
        let proto_radius = match prototype {
            Some(p) => ctx.read_f64(&p, "radius"),
            None => 1.0,
        }
        .max(0.1);
        let scale_dist = ctx.params.get("scale").cloned();
        let mut rng = Rng::seeded(ctx.stage_seed, ctx.path.as_str(), ctx.seed);

        let mut layer = sdf::Layer::new_anonymous("rusd_scatter.usda");
        let mut warns: Vec<Diagnostic> = Vec::new();
        layer
            .edit(|l| {
                let d = l.data_mut();
                PrimSpec::new(d, base.as_str(), Specifier::Def, "Xform")?;
                for k in 0..max_prims {
                    let x = rng.uniform(-dw / 2.0, dw / 2.0);
                    let y = rng.uniform(-dh / 2.0, dh / 2.0);
                    let s = sample_dist(&mut rng, scale_dist.as_ref(), 1.0);
                    let p = format!("{base}/i{k:06}");
                    PrimSpec::new(d, p.as_str(), Specifier::Def, "Sphere")?;
                    let rp = format!("{p}.radius");
                    let mut ra =
                        AttributeSpec::new(d, rp.as_str(), "double", Variability::Uniform, false)?;
                    ra.set_default(SdfValue::Double(proto_radius * s))?;
                    author_translate(d, &p, x, y, proto_radius * s)?;
                }
                if count > max_prims {
                    warns.push(
                        Diagnostic::warning(
                            "W_BUDGET",
                            format!("max_prims={max_prims} reached; truncated"),
                        )
                        .at_prim(base.as_str()),
                    );
                }
                Ok(())
            })
            .map_err(|e| {
                vec![Diagnostic::error("E_GEN", e.to_string()).at_prim(ctx.path.as_str())]
            })?;
        for w in warns {
            ctx.warn(w);
        }
        Ok(layer)
    }
}

#[cfg(test)]
mod tests {
    use crate::cook::nodes_from_doc;
    use crate::generator::read_attr_f64;
    use crate::generators::default_registry;
    use openusd::usd::Stage;
    use rusd_core::Value;
    use rusd_core::syntax::{self, ParsedDocument};

    const CITY: &str = include_str!("../../../examples/city.rusd");

    fn expand(doc: &ParsedDocument) -> (Stage, crate::cook::CookResult) {
        let stage = rusd_scene::lower(doc).unwrap();
        let root = stage.root_layer().identifier().to_owned();
        let nodes = nodes_from_doc(doc);
        let mut e = default_registry();
        let r = e.cook(&stage, &root, &nodes);
        (stage, r)
    }

    /// World-space bounds of one extracted prim: (min, max) over its vertices.
    fn world_bounds(it: &rusd_scene::GeoItem) -> ([f64; 3], [f64; 3]) {
        let mut lo = [f64::INFINITY; 3];
        let mut hi = [f64::NEG_INFINITY; 3];
        for p in &it.mesh.positions {
            let w = rusd_scene::geometry::mat_transform_point(
                it.transform,
                [p[0] as f64, p[1] as f64, p[2] as f64],
            );
            for k in 0..3 {
                lo[k] = lo[k].min(w[k]);
                hi[k] = hi[k].max(w[k]);
            }
        }
        (lo, hi)
    }

    /// The ground plane spans the whole street grid, and every building lot stands
    /// on it. Both derive their extent from the same grid formula
    /// `blocks * block_size + (blocks + 1) * street_width`, so the ground is exactly
    /// the grid -- no invented margin on top of it.
    #[test]
    fn ground_covers_the_whole_street_grid() {
        let doc = syntax::parse(CITY).unwrap();
        let (stage, r) = expand(&doc);
        assert!(r.diags.iter().all(|d| d.code != "E_GEN"));

        let geo = rusd_scene::extract(&stage);
        let ground = geo
            .items
            .iter()
            .find(|it| it.path == "/World/City/Ground")
            .expect("ground exists");
        let (glo, ghi) = world_bounds(ground);

        // The grid extent, recomputed here from examples/city.rusd's own params --
        // independently of the formula inside `RusdStreetGrid::eval`.
        let city = doc
            .prims
            .iter()
            .find(|p| p.path.as_str() == "/World/City")
            .expect("/World/City exists");
        let param_f64 = |k: &str| match city.props.get(k) {
            Some(Value::Float(f)) => *f,
            Some(Value::Int(i)) => *i as f64,
            _ => panic!("{k} missing from /World/City"),
        };
        let param_vec2 = |k: &str| match city.props.get(k) {
            Some(Value::Array(items)) if items.len() == 2 => {
                let comp = |v: &Value| match v {
                    Value::Float(f) => *f,
                    Value::Int(i) => *i as f64,
                    _ => panic!("{k} component is not a number"),
                };
                (comp(&items[0]), comp(&items[1]))
            }
            _ => panic!("{k} missing from /World/City"),
        };
        let (nx, ny) = (param_f64("blocks_x"), param_f64("blocks_y"));
        let (bw, bh) = param_vec2("block_size");
        let street = param_f64("street_width");
        let grid_w = nx * bw + (nx + 1.0) * street;
        let grid_h = ny * bh + (ny + 1.0) * street;

        assert!(
            glo[0].abs() < 1e-6 && glo[1].abs() < 1e-6,
            "ground starts at the grid origin: {:?}",
            [glo[0], glo[1]]
        );
        assert!(
            (ghi[0] - grid_w).abs() < 1e-6,
            "ground width {} != grid width {grid_w}",
            ghi[0]
        );
        assert!(
            (ghi[1] - grid_h).abs() < 1e-6,
            "ground depth {} != grid depth {grid_h}",
            ghi[1]
        );
        assert!(
            (glo[2] - 0.0).abs() < 1e-6 && (ghi[2] - 0.5).abs() < 1e-6,
            "ground slab sits at z in [0, 0.5]: {:?}",
            [glo[2], ghi[2]]
        );

        // Every building lot must lie within the ground footprint.
        let mut lots = 0;
        for it in geo
            .items
            .iter()
            .filter(|it| it.path.starts_with("/World/City/b") && it.path.contains("/lot"))
        {
            let (lo, hi) = world_bounds(it);
            for k in 0..2 {
                assert!(
                    lo[k] >= glo[k] - 1e-6 && hi[k] <= ghi[k] + 1e-6,
                    "{} escapes the ground on axis {k}: [{}, {}] not within [{}, {}]",
                    it.path,
                    lo[k],
                    hi[k],
                    glo[k],
                    ghi[k]
                );
            }
            assert!(lo[2] >= -1e-6, "{} sits below the ground", it.path);
            lots += 1;
        }
        assert!(lots > 100, "expected a full city, got {lots} lots");
    }

    #[test]
    fn city_expands_deterministically() {
        let doc = syntax::parse(CITY).unwrap();
        let (s1, r1) = expand(&doc);
        let (s2, _) = expand(&doc);
        assert!(r1.diags.iter().all(|d| d.code != "E_GEN"));
        assert_eq!(rusd_scene::prim_count(&s1), rusd_scene::prim_count(&s2));
        for i in 0..5 {
            let p = format!("/World/StreetProps/i{i:06}");
            assert_eq!(
                read_attr_f64(&s1, &p, "radius").to_bits(),
                read_attr_f64(&s2, &p, "radius").to_bits()
            );
        }
    }

    #[test]
    fn city_budget_truncates() {
        let mut doc = syntax::parse(CITY).unwrap();
        doc.prims
            .iter_mut()
            .find(|p| p.path.as_str() == "/World/City")
            .unwrap()
            .props
            .insert("max_prims".to_owned(), Value::Int(40));
        doc.prims
            .iter_mut()
            .find(|p| p.path.as_str() == "/World/StreetProps")
            .unwrap()
            .props
            .insert("max_prims".to_owned(), Value::Int(10));
        let (stage, r) = expand(&doc);
        assert!(r.diags.iter().any(|d| d.code == "W_BUDGET"));
        assert!(
            rusd_scene::prim_count(&stage) < 200,
            "budget should bound prim count"
        );
    }

    #[test]
    fn scatter_reads_prototype_radius() {
        let doc = syntax::parse(CITY).unwrap();
        let (s1, _) = expand(&doc);
        let r1 = read_attr_f64(&s1, "/World/StreetProps/i000000", "radius");
        let mut doc2 = doc.clone();
        doc2.prims
            .iter_mut()
            .find(|p| p.path.as_str() == "/World/Prototypes/Tree")
            .unwrap()
            .props
            .insert("radius".to_owned(), Value::Float(4.0));
        let (s2, _) = expand(&doc2);
        let r2 = read_attr_f64(&s2, "/World/StreetProps/i000000", "radius");
        assert!(
            r2 > r1 * 1.5,
            "doubling prototype radius should grow scatter: {r1} -> {r2}"
        );
    }
}
