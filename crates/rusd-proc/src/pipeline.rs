//! The v1.5 two-op pipeline (docs/PROTOCOL.md v1.5 / v1.5.1): `mesh -> points ->
//! instances`, as **two explicit generators** with a real layer in between.
//!
//! Why two ops and not one: a pipeline prefix can only show a state that exists as
//! a layer in the stack. A single `RusdScatter` over a mesh jumps from "mesh" to
//! "instances" with nothing in between; splitting the step makes the intermediate
//! point cloud a genuine layer, so `preview{upto}` has three readable stages:
//!
//! ```text
//!   root (author layer)   -> the input mesh
//!   RusdSamplePoints      -> a `Points` prim: `count` area-weighted samples
//!   RusdInstances         -> one prototype instance per sample
//! ```
//!
//! Replacement semantics hold **per op** (v1.5.1): each op hides exactly the
//! geometry it consumed, by authoring `visibility = "invisible"` over it in its own
//! output layer. The consumed spec is never deleted, so muting an op's layer brings
//! its input back. The three prefixes are therefore mutually exclusive in what they
//! draw: mesh only, cloud only, instances only.
//!
//! Paths (the proc-internal convention, kept consistent with `RusdScatter`): a node
//! anchored at `<node>` authors
//! - `RusdSamplePoints`: one cloud at `<node>/points`;
//! - `RusdInstances`: one instance per point at `<node>/i000000`, `<node>/i000001`, …
//!
//! Spaces: the input is read in **world** space (the geometry itself, ancestors
//! included) and written back in the anchor's **local** space
//! (`inverse(anchor world) · point`), exactly as `RusdScatter` does — a world value
//! written as a local op would apply the parent's transform twice.
//!
//! Determinism: the stream is `Rng::seeded(stage.seed, node path, local seed)`, path
//! keyed, so editing one node never reshuffles another's samples.

use openusd::gf;
use openusd::sdf::{self, AttributeSpec, PrimSpec, Specifier, Value as SdfValue, Variability};
use rusd_core::Diagnostic;

use crate::city::{AuthResult, author_reference, sample_dist, surface_samples};
use crate::generator::{GenContext, Generator};
use crate::rng::Rng;

/// The prim path a `RusdSamplePoints` node anchored at `anchor` writes its cloud to.
///
/// This is the only thing the two ops have to agree on: the sample node authors it,
/// the instances node reads it, and a prefix can stop between them.
pub fn points_path(anchor: &str) -> String {
    format!("{anchor}/points")
}

/// The `input_points` parameter both ops share, which is required by both (v1.5.1:
/// neither has a meaningful default — a sample op with no surface has nothing to
/// sample, and an instances op with no cloud has nothing to instantiate).
fn require_input(ctx: &GenContext, generator: &str, base: &str) -> Result<String, Vec<Diagnostic>> {
    ctx.param_path("input_points")
        .map(str::to_owned)
        .ok_or_else(|| {
            vec![
                Diagnostic::error(
                    "E_INPUT_GEOMETRY",
                    format!("{generator} needs `input_points` (the prim whose geometry it reads)"),
                )
                .at_prim(base),
            ]
        })
}

/// `inverse(anchor world)`: what a world point is undone by before it is authored as
/// this node's local value.
///
/// `None` only when the anchor has no world transform at all (it is not in the stage
/// yet), which reads as identity — the same rule `RusdScatter` follows. A singular
/// anchor transform is a diagnostic rather than an inverted-into-nonsense matrix.
fn inverse_anchor(
    ctx: &mut GenContext,
    base: &str,
) -> Result<Option<rusd_scene::Mat>, Vec<Diagnostic>> {
    match ctx.read_world_xform(base) {
        Some(m) => match rusd_scene::geometry::mat_inverse_affine(m) {
            Some(inv) => Ok(Some(inv)),
            None => Err(vec![
                Diagnostic::error(
                    "E_INPUT_GEOMETRY",
                    "the node anchor's world transform is not invertible (zero scale)",
                )
                .at_prim(base),
            ]),
        },
        None => Ok(None),
    }
}

fn to_local(inv_anchor: Option<rusd_scene::Mat>, world: [f64; 3]) -> [f64; 3] {
    match inv_anchor {
        Some(inv) => rusd_scene::geometry::mat_transform_point(inv, world),
        None => world,
    }
}

/// Author runtime geometry into this node's own output layer.
///
/// A `def` with a **local** typeName is what makes the prim readable as target
/// geometry: a `Points` prim is drawn as markers by `rusd_scene::geometry::extract`,
/// so a prefix that ends here shows a cloud rather than an empty picture.
fn author_cloud(d: &mut dyn sdf::AbstractData, prim: &str, local: &[[f64; 3]]) -> AuthResult {
    PrimSpec::new(d, prim, Specifier::Def, "Points")?;
    let ap = format!("{prim}.points");
    let mut a = AttributeSpec::new(d, ap.as_str(), "point3f[]", Variability::Varying, false)?;
    a.set_default(SdfValue::Vec3fVec(
        local
            .iter()
            .map(|p| gf::vec3f(p[0] as f32, p[1] as f32, p[2] as f32))
            .collect(),
    ))?;
    Ok(())
}

/// The replacement, per op (v1.5.1): hide the consumed input where it is, without
/// touching the layer that authored it. An `over` plus a `visibility` token — never a
/// delete and never `active = false` (which a weaker layer cannot win back).
fn author_hide(d: &mut dyn sdf::AbstractData, prim: &str) -> AuthResult {
    if let Ok(target) = sdf::path(prim) {
        PrimSpec::over(d, target)?;
        let mut vis = AttributeSpec::new(
            d,
            format!("{prim}.visibility").as_str(),
            "token",
            Variability::Varying,
            false,
        )?;
        vis.set_default(SdfValue::Token("invisible".into()))?;
    }
    Ok(())
}

/// `RusdSamplePoints` (v1.5): a **mesh surface** becomes a point cloud.
///
/// Parameters (v1.5.1): `input_points` (a `Mesh` prim), `count`, `seed`.
///
/// It samples **only** a surface: a `Mesh`'s bare `points` are a readable cloud, and
/// taking that path would place points on the *vertices* rather than on the surface —
/// a different geometry, silently. So an input with no mesh topology is
/// `E_INPUT_GEOMETRY`, not a fallback. (A cloud input is `RusdInstances`' business.)
///
/// Output: `<node>/points`, a `Points` prim whose `points` are the samples in the
/// anchor's local space, plus a `visibility = "invisible"` over on the input mesh.
pub struct RusdSamplePoints;

impl Generator for RusdSamplePoints {
    fn name(&self) -> &'static str {
        "RusdSamplePoints"
    }

    fn eval(&self, ctx: &mut GenContext) -> Result<sdf::Layer, Vec<Diagnostic>> {
        let base = ctx.path.as_str().to_owned();
        let input = require_input(ctx, "RusdSamplePoints", &base)?;
        let tris = match ctx.read_triangles(&input) {
            Some(t) if !t.is_empty() => t,
            _ => {
                return Err(vec![
                    Diagnostic::error(
                        "E_INPUT_GEOMETRY",
                        format!(
                            "input_points {input} has no readable mesh surface (it needs \
                             `points` plus `faceVertexCounts`/`faceVertexIndices`); \
                             RusdSamplePoints samples a surface and does not fall back to \
                             the vertices"
                        ),
                    )
                    .at_prim(&base),
                ]);
            }
        };
        let count = ctx.param_u64("count", 100) as usize;
        let mut rng = Rng::seeded(ctx.stage_seed, &base, ctx.seed);
        let sampled = surface_samples(&tris, count, &mut rng);
        let inv = inverse_anchor(ctx, &base)?;
        let local: Vec<[f64; 3]> = sampled.iter().map(|w| to_local(inv, *w)).collect();
        let cloud = points_path(&base);

        let mut layer = sdf::Layer::new_anonymous("rusd_sample_points.usda");
        layer
            .edit(|l| {
                let d = l.data_mut();
                PrimSpec::new(d, base.as_str(), Specifier::Def, "Xform")?;
                author_cloud(d, &cloud, &local)?;
                author_hide(d, &input)?;
                Ok(())
            })
            .map_err(|e| vec![Diagnostic::error("E_GEN", e.to_string()).at_prim(&base)])?;
        Ok(layer)
    }
}

/// `RusdInstances` (v1.5): one prototype instance per input point.
///
/// Parameters (v1.5.1): `input_points` (a `Points` prim or a `point3f[]` cloud),
/// `prototype`, `scale`, `max_prims`. There is deliberately **no `count`**: the cloud
/// says how many instances there are, and `max_prims` only bounds that (the first
/// `max_prims` points, in the cloud's own order, plus `W_BUDGET`).
///
/// It reads a cloud, never a surface: a `Mesh` input is `E_INPUT_GEOMETRY` here — a
/// mesh is `RusdSamplePoints`' input, and sampling vertices into instances is exactly
/// the "not geometry-driven" shortcut D14.1 ruled out.
///
/// Output: `<node>/i{k:06}` instances, each a same-stack reference to the prototype
/// with its own `xformOp:translate` in the anchor's local space, plus a
/// `visibility = "invisible"` over on the input cloud.
pub struct RusdInstances;

impl Generator for RusdInstances {
    fn name(&self) -> &'static str {
        "RusdInstances"
    }

    fn eval(&self, ctx: &mut GenContext) -> Result<sdf::Layer, Vec<Diagnostic>> {
        let base = ctx.path.as_str().to_owned();
        let input = require_input(ctx, "RusdInstances", &base)?;
        let cloud = match ctx.read_points(&input) {
            Some(pts) if !pts.is_empty() => pts,
            Some(_) => {
                return Err(vec![
                    Diagnostic::error(
                        "E_INPUT_GEOMETRY",
                        format!("input_points {input} authors an empty `points` array"),
                    )
                    .at_prim(&base),
                ]);
            }
            None => {
                return Err(vec![
                    Diagnostic::error(
                        "E_INPUT_GEOMETRY",
                        format!(
                            "input_points {input} authors no `points` cloud; RusdInstances \
                             instantiates one per point and reads no surface"
                        ),
                    )
                    .at_prim(&base),
                ]);
            }
        };
        let prototype = ctx
            .param_str("prototype")
            .map(str::to_owned)
            .ok_or_else(|| {
                vec![
                    Diagnostic::error("E_MISSING_PROTOTYPE", "RusdInstances needs `prototype`")
                        .at_prim(&base),
                ]
            })?;
        let proto_path = sdf::path(&prototype).map_err(|e| {
            vec![
                Diagnostic::error(
                    "E_INPUT_GEOMETRY",
                    format!("prototype {prototype:?} is not a prim path: {e}"),
                )
                .at_prim(&base),
            ]
        })?;
        // The prototype's own values are what the arc composes; reading its radius
        // records the dependency that a change there reaches these instances.
        let _ = ctx.read_f64(&prototype, "radius");
        let limit = ctx.param_u64("max_prims", 20000).max(1) as usize;
        let inv = inverse_anchor(ctx, &base)?;
        let scale_dist = ctx.params.get("scale").cloned();
        let root_id = ctx.stage().root_layer().identifier().to_owned();

        let mut warns: Vec<Diagnostic> = Vec::new();
        if cloud.len() > limit {
            warns.push(
                Diagnostic::warning(
                    "W_BUDGET",
                    format!(
                        "max_prims={limit} reached; {} of {} points truncated",
                        cloud.len() - limit,
                        cloud.len()
                    ),
                )
                .at_prim(&base),
            );
        }

        let mut rng = Rng::seeded(ctx.stage_seed, &base, ctx.seed);
        let mut layer = sdf::Layer::new_anonymous("rusd_instances.usda");
        layer
            .edit(|l| {
                let d = l.data_mut();
                PrimSpec::new(d, base.as_str(), Specifier::Def, "Xform")?;
                for (k, world) in cloud.iter().take(limit).enumerate() {
                    let p = format!("{base}/i{k:06}");
                    let pp = sdf::path(&p)?;
                    // No local typeName: the type comes through the arc. A local
                    // opinion would be the strongest one and would win over the
                    // prototype, leaving an empty `Xform` in every instance's place.
                    PrimSpec::new(d, p.as_str(), Specifier::Def, "")?;
                    let s = sample_dist(&mut rng, scale_dist.as_ref(), 1.0);
                    author_reference(d, &pp, &root_id, &proto_path, to_local(inv, *world), s)?;
                }
                author_hide(d, &input)?;
                Ok(())
            })
            .map_err(|e| vec![Diagnostic::error("E_GEN", e.to_string()).at_prim(&base)])?;
        for w in warns {
            ctx.warn(w);
        }
        Ok(layer)
    }
}
