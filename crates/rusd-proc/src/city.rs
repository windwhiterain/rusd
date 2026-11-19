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
pub(crate) fn sample_dist(rng: &mut Rng, v: Option<&Value>, default: f64) -> f64 {
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

pub(crate) type AuthResult = Result<(), sdf::AuthoringError>;

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

/// Scatter prop instances across a **parameter domain** and along **input
/// geometry** (D14 A / D14.1). Demonstrates a real read dependency in the cook
/// graph.
///
/// ## Three modes
///
/// - **parametric** (no `input_points`): sample positions from `domain` × `count`.
///   `RusdScatter`'s original behaviour, kept byte-for-byte so an existing document
///   (`examples/forest.rusd`, `examples/city.rusd`) cooks to the same scene.
/// - **point cloud** (`input_points` names a prim whose `points` is a
///   `point3f[]`): one instance per point, in the cloud's own order, at that point.
///   Nothing is invented and nothing is dropped.
/// - **surface** (`input_points` names a `Mesh` with face topology): exactly
///   `count` instances, sampled **area-weighted** — the triangle comes from a
///   cumulative-area table and the point inside it from a uniform barycentric pair —
///   so a dense part of the mesh receives proportionally more instances and every
///   instance lands **on** the surface.
///
/// A mesh wins over its own vertices when both are readable: sampling a `Mesh`'s
/// `points` alone would put instances on triangle corners in file order, which is
/// neither "on the surface" nor area-weighted.
///
/// ## Replacement semantics
///
/// The output layer writes the instances **and** an `over` on the input prim with
/// `visibility = "invisible"`. The input's own spec is untouched — it keeps its
/// points and its faces in its own layer — so a pipeline **prefix** that stops
/// before this node still shows the geometry, while the full stack shows the
/// instances that replaced it. Visibility rather than `active`: an inactive prim is
/// suppressed by composition and a weaker layer cannot bring it back, whereas
/// visibility is an ordinary overridable attribute — exactly "override, do not
/// delete".
///
/// ## Spaces and determinism
///
/// Points and triangles are read in **world** space (the input's and every
/// ancestor's transform), and each instance's `xformOp:translate` is that world
/// position converted into this node's **local** space
/// (`inverse(anchor world) · point`). Writing a world value as a local op would
/// apply the parent's transform twice — the `fix/ground-size` bug class. The
/// sampling stream is `Rng::seeded(stage.seed, anchor path, seed)`: path-keyed, so
/// editing another node never reshuffles these instances.
pub struct RusdScatter;

impl Generator for RusdScatter {
    fn name(&self) -> &'static str {
        "RusdScatter"
    }

    fn eval(&self, ctx: &mut GenContext) -> Result<sdf::Layer, Vec<Diagnostic>> {
        let base = ctx.path.as_str().to_owned();
        if let Some(input) = ctx.param_path("input_points").map(str::to_owned) {
            return scatter_from_geometry(ctx, &base, &input);
        }
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

/// A prototype instance: a **reference** to the prototype prim plus the
/// instance's own transform.
///
/// The reference reaches the prototype two ways (v1.5.8), and which one is the
/// honest description of the scene is the document's choice:
///
/// - **same-stack** — `asset` is the stage's root layer identifier and
///   `prototype` is a prim of this stage;
/// - **asset** — `asset` is another layer and the arcs cross the asset boundary,
///   which is what instancing *material* looks like: the layer is opened by the
///   reference and is never on this stage's own stack. A sublayer would say the
///   other thing — that the material is part of this document's authoring — and
///   that claim is what makes it wrong for a library layer.
///
/// Either spelling is what `Stage::flatten` → `usdc` → `Layer::from_bytes` also
/// survives: the Tier A snapshot a viewer receives still composes the instance's
/// prototype, so geometry out is real USD instancing on both ends, not a copy of
/// the prototype's numbers.
///
/// The instance carries **no type name of its own**. A `def` with a local typeName
/// is the strongest opinion about the prim's type and would win over the arc,
/// turning every instance into an empty `Xform`; the type must come from the
/// prototype.
///
/// `xformOpOrder` is `[translate, scale]` (USD's order: scale applied first), and
/// the whole op stack is authored in one pass because an attribute spec can only be
/// created once per path.
pub(crate) fn author_reference(
    d: &mut dyn sdf::AbstractData,
    prim: &sdf::Path,
    asset: &str,
    prototype: &sdf::Path,
    local_translate: [f64; 3],
    scale: f64,
) -> AuthResult {
    let op = sdf::ReferenceListOp::explicit([sdf::Reference {
        asset_path: asset.to_owned(),
        prim_path: prototype.clone(),
        ..Default::default()
    }]);
    d.set_field(
        prim,
        sdf::FieldKey::References.as_str(),
        SdfValue::ReferenceListOp(op),
    );
    let tp = format!("{}.xformOp:translate", prim.as_str());
    let mut ta = AttributeSpec::new(d, tp.as_str(), "point3f", Variability::Uniform, false)?;
    ta.set_default(SdfValue::Vec3f(gf::vec3f(
        local_translate[0] as f32,
        local_translate[1] as f32,
        local_translate[2] as f32,
    )))?;
    let mut ops = vec!["xformOp:translate".to_owned()];
    if (scale - 1.0).abs() > 1e-12 {
        let s = scale as f32;
        let sp = format!("{}.xformOp:scale", prim.as_str());
        let mut sa = AttributeSpec::new(d, sp.as_str(), "float3", Variability::Uniform, false)?;
        sa.set_default(SdfValue::Vec3f(gf::vec3f(s, s, s)))?;
        ops.push("xformOp:scale".to_owned());
    }
    let op_path = format!("{}.xformOpOrder", prim.as_str());
    let mut oa = AttributeSpec::new(d, op_path.as_str(), "token[]", Variability::Uniform, false)?;
    oa.set_default(SdfValue::TokenVec(
        ops.into_iter().map(Into::into).collect(),
    ))?;
    Ok(())
}

/// The two forms of input geometry a scatter consumes (D14.1).
enum InputGeometry {
    /// A surface: sample `count` points on it, area-weighted.
    Surface(Vec<rusd_scene::geometry::Triangle>),
    /// A cloud: one instance per point, in the cloud's own order.
    Cloud(Vec<[f64; 3]>),
}

/// Everything one geometry-mode scatter needs, read once.
struct GeometryInput {
    geometry: InputGeometry,
    /// `inverse(anchor world)`; `None` only when the anchor has no world transform
    /// at all (it is not in the stage yet), which reads as identity.
    inv_anchor: Option<rusd_scene::Mat>,
    proto_path: sdf::Path,
    /// The layer the `prototype` reference names: this stage's root layer for a
    /// same-stack prototype, or the asset the document pointed at (v1.5.8).
    proto_asset: String,
}

impl GeometryInput {
    /// A world point as the instance's authored local `translate`.
    fn to_local(&self, world: [f64; 3]) -> [f64; 3] {
        match self.inv_anchor {
            Some(inv) => rusd_scene::geometry::mat_transform_point(inv, world),
            None => world,
        }
    }
}

/// Read the geometry an `input_points` names, in its own form (D14.1), recording
/// every dependency the answer depends on so the cook memo is exact.
fn read_input_geometry(
    ctx: &mut GenContext,
    base: &str,
    input: &str,
) -> Result<GeometryInput, Vec<Diagnostic>> {
    // A `Mesh`'s surface first: its `points` are also a readable cloud, and a
    // surface is the stronger claim about what the input *is*.
    let geometry = match ctx.read_triangles(input) {
        Some(tris) => InputGeometry::Surface(tris),
        None => match ctx.read_points(input) {
            Some(pts) if !pts.is_empty() => InputGeometry::Cloud(pts),
            Some(_) => {
                return Err(vec![
                    Diagnostic::error(
                        "E_INPUT_GEOMETRY",
                        format!("input_points {input} authors an empty `points` array"),
                    )
                    .at_prim(base),
                ]);
            }
            None => {
                return Err(vec![
                    Diagnostic::error(
                        "E_INPUT_GEOMETRY",
                        format!(
                            "input_points {input} has neither readable mesh topology nor a `points` cloud"
                        ),
                    )
                    .at_prim(base),
                ]);
            }
        },
    };
    let prototype = ctx
        .param_str("prototype")
        .map(str::to_owned)
        .ok_or_else(|| {
            vec![
                Diagnostic::error("E_MISSING_PROTOTYPE", "RusdScatter needs `prototype`")
                    .at_prim(base),
            ]
        })?;
    // v1.5.8: the parameter accepts a prim path in this stack or `asset:prim`
    // naming a prototype in another layer. Which one it is decides whether the
    // instances' arcs stay inside the stack or cross the asset boundary.
    let proto = crate::generator::parse_prototype(&prototype, ctx.doc_dir).map_err(|e| {
        vec![
            Diagnostic::error(e.code, e.message)
                .at_prim(base)
                .at_property("prototype"),
        ]
    })?;
    let proto_path = proto.path.clone();
    let proto_asset = proto
        .asset
        .unwrap_or_else(|| ctx.stage().root_layer().identifier().to_owned());
    // The anchor's own transform: what a world position is undone by. Read as a
    // dependency, so moving the anchor (or a parent of it) re-cooks this node.
    let anchor_world = ctx.read_world_xform(base);
    let inv_anchor = anchor_world.and_then(rusd_scene::geometry::mat_inverse_affine);
    if anchor_world.is_some() && inv_anchor.is_none() {
        return Err(vec![
            Diagnostic::error(
                "E_INPUT_GEOMETRY",
                "the scatter anchor's world transform is not invertible (zero scale)",
            )
            .at_prim(base),
        ]);
    }
    Ok(GeometryInput {
        geometry,
        inv_anchor,
        proto_path,
        proto_asset,
    })
}

/// The **geometry mode** of [`RusdScatter`] (D14 A / D14.1): the instances come
/// from input geometry. See the type's own docs for the contract.
///
/// Diagnostics:
/// - `E_INPUT_GEOMETRY` — no readable surface and no `points` cloud, an empty
///   cloud, a singular anchor transform, or a prototype that is not a prim path.
///   Never a panic, and never a silently empty layer.
/// - `W_BUDGET` — a cloud larger than `max_prims` (the first `max_prims`, in the
///   cloud's order, become instances), or a `count` that exceeds it.
fn scatter_from_geometry(
    ctx: &mut GenContext,
    base: &str,
    input: &str,
) -> Result<sdf::Layer, Vec<Diagnostic>> {
    let read = read_input_geometry(ctx, base, input)?;
    let limit = ctx.param_u64("max_prims", 20000).max(1) as usize;
    let count = ctx.param_u64("count", 100) as usize;
    let scale_dist = ctx.params.get("scale").cloned();
    let mut rng = Rng::seeded(ctx.stage_seed, base, ctx.seed);

    let mut warns: Vec<Diagnostic> = Vec::new();
    let placements: Vec<[f64; 3]> = match &read.geometry {
        InputGeometry::Cloud(points) => {
            if points.len() > limit {
                warns.push(
                    Diagnostic::warning(
                        "W_BUDGET",
                        format!(
                            "max_prims={limit} reached; {} of {} points truncated",
                            points.len() - limit,
                            points.len()
                        ),
                    )
                    .at_prim(base),
                );
            }
            points.iter().take(limit).copied().collect()
        }
        InputGeometry::Surface(tris) => {
            let kept = count.min(limit);
            if count > kept {
                warns.push(
                    Diagnostic::warning(
                        "W_BUDGET",
                        format!("max_prims={limit} reached; {count} instances truncated to {kept}"),
                    )
                    .at_prim(base),
                );
            }
            surface_samples(tris, kept, &mut rng)
        }
    };

    let mut layer = sdf::Layer::new_anonymous("rusd_scatter.usda");
    layer
        .edit(|l| {
            let d = l.data_mut();
            PrimSpec::new(d, base, Specifier::Def, "Xform")?;
            // The replacement: hide the input geometry where it is, without touching
            // the layer that authored it.
            if let Ok(target) = sdf::path(input) {
                PrimSpec::over(d, target.clone())?;
                let mut vis = AttributeSpec::new(
                    d,
                    format!("{input}.visibility").as_str(),
                    "token",
                    Variability::Varying,
                    false,
                )?;
                vis.set_default(SdfValue::Token("invisible".into()))?;
            }
            for (k, world) in placements.iter().enumerate() {
                let p = format!("{base}/i{k:06}");
                let pp = sdf::path(&p)?;
                PrimSpec::new(d, p.as_str(), Specifier::Def, "")?;
                let s = sample_dist(&mut rng, scale_dist.as_ref(), 1.0);
                author_reference(
                    d,
                    &pp,
                    &read.proto_asset,
                    &read.proto_path,
                    read.to_local(*world),
                    s,
                )?;
            }
            Ok(())
        })
        .map_err(|e| vec![Diagnostic::error("E_GEN", e.to_string()).at_prim(base)])?;
    for w in warns {
        ctx.warn(w);
    }
    Ok(layer)
}

/// `count` points on `tris`, sampled by area: pick a triangle in proportion to its
/// area from a cumulative table, then a point inside it uniformly — the
/// square-root barycentric pair, which is the uniform distribution over a triangle
/// (a plain `(u, v)` would pile the instances towards the first corner).
pub(crate) fn surface_samples(
    tris: &[rusd_scene::geometry::Triangle],
    count: usize,
    rng: &mut Rng,
) -> Vec<[f64; 3]> {
    let mut cumulative = Vec::with_capacity(tris.len());
    let mut total = 0.0_f64;
    for t in tris {
        total += rusd_scene::geometry::triangle_area(*t);
        cumulative.push(total);
    }
    if total <= 0.0 {
        return Vec::new();
    }
    (0..count)
        .map(|_| {
            let r = rng.next_f64() * total;
            let idx = cumulative
                .iter()
                .position(|c| *c > r)
                .unwrap_or(tris.len() - 1);
            let u = rng.next_f64().sqrt();
            let v = rng.next_f64();
            rusd_scene::geometry::triangle_point(tris[idx], u * (1.0 - v), u * v)
        })
        .collect()
}
