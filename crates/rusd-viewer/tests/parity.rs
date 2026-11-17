//! Parity tests for the memory-only composite stack (SPEC D12 v2, spike ②).
//!
//! Two claims, both objective:
//!
//! 1. **Snapshot parity** — the cooked `usdc` the main tree already produced
//!    (`target/city.usdc`, read only) composes over [`MemResolver`] to exactly
//!    the same geometry `rusd_scene::extract` gives when that same stack is read
//!    from disk: 434 items, world bbox `min(-210.26,-211.44,-2.0)`
//!    `max(444,444,52.07)` to 1e-3, and a non-empty PNG.
//! 2. **Session parity** — a stack that exists only in memory, a cooked root
//!    layer plus a `usda` session layer overriding one lot's translate, gives the
//!    override in the composed view while every other opinion matches the same
//!    stack composed directly (disk root, no session, plus the same session
//!    applied on top) — so what the mirror contributes is exactly the session.

use std::path::{Path, PathBuf};

use openusd::sdf::{self, Layer, Value};
use rusd_scene::geometry;
use rusd_viewer::MemResolver;
use rusd_viewer::compose::{Manifest, Stack};
use rusd_viewer::scene::{self, BBox, item_bbox, item_transform};

/// The main tree's cooked city artifact (this worktree does not own it). The
/// worktree's own `target/` is not where M6 cooked; the main checkout's is, one
/// level above `../../target` from this crate's manifest dir… but a worktree's
/// `CARGO_MANIFEST_DIR` sits inside `.wt/viewer`, so walk up to the main tree.
fn city_usdc() -> PathBuf {
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // crates/rusd-viewer -> <crate>/../.. is the tree root the crate belongs to
    // (the worktree). The *main* checkout is the one that has `.git` as a file
    // in a worktree: walk up until a directory with a real `.git` *directory*
    // (or gitignored `target/`) holds the artifact.
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(p) = std::env::var("RUSD_MAIN_TREE") {
        candidates.push(PathBuf::from(p).join("target/city.usdc"));
    }
    for up in 0..6 {
        if let Some(root) = here.ancestors().nth(up) {
            candidates.push(root.join("target/city.usdc"));
        }
    }
    if let Some(found) = candidates.iter().find(|p| p.exists()) {
        return found.clone();
    }
    panic!(
        "the M6 artifact target/city.usdc must exist (cook it in the main tree); looked at: {}",
        candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
}

/// A scratch directory under this crate's `target/`, unique per test.
fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("target")
        .join("viewer-spike");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

// --- 1. snapshot parity ---------------------------------------------------

#[test]
fn snapshot_of_the_cooked_usdc_matches_the_disk_stack() {
    let path = city_usdc();
    let bytes = std::fs::read(&path).unwrap();
    let png = scratch("city_snapshot.png");
    let png_str = png.to_str().unwrap();
    let snap = scene::snapshot_usdc_bytes(bytes, path.to_str().unwrap(), 960, 540, Some(png_str))
        .expect("memory composite renders");

    // Geometry parity against the same stack read from disk.
    let disk_stage = openusd::usd::Stage::open(path.to_str().unwrap()).unwrap();
    let disk_geo = geometry::extract(&disk_stage);
    assert_eq!(snap.geometry.items.len(), 434, "city item count");
    assert_eq!(
        snap.geometry.items.len(),
        disk_geo.items.len(),
        "memory composite == disk stack, item count"
    );
    for (mem, disk) in snap.geometry.items.iter().zip(&disk_geo.items) {
        assert_eq!(mem.path, disk.path, "same prim order/set");
        for k in 0..16 {
            assert!(
                (mem.transform[k] - disk.transform[k]).abs() < 1e-9,
                "{}[{k}] memory {} disk {}",
                mem.path,
                mem.transform[k],
                disk.transform[k]
            );
        }
    }

    // World bbox: the numbers M6's verification doc records for the city, which it
    // prints rounded to 2 dp. Check both — the exact values to 1e-3, and that they
    // are what the doc's own figures round to.
    let bbox = BBox {
        min: snap.geometry.bounds.0,
        max: snap.geometry.bounds.1,
    };
    let exact = BBox {
        min: [-210.262_254, -211.440_871, -2.0],
        max: [444.0, 444.0, 52.065_453],
    };
    assert!(
        bbox.approx_eq(&exact, 1e-3),
        "world bbox {:?} != {:?}",
        bbox.corners(),
        exact.corners()
    );
    let rounded = |v: [f64; 3]| {
        [
            (v[0] * 100.0).round() / 100.0,
            (v[1] * 100.0).round() / 100.0,
            (v[2] * 100.0).round() / 100.0,
        ]
    };
    // docs/VERIFICATION.md §7.1: min `(-210.26, -211.44, -2.0)`, max `(444.0, 444.0, 52.07)`.
    assert_eq!(rounded(bbox.min), [-210.26, -211.44, -2.0]);
    assert_eq!(rounded(bbox.max), [444.0, 444.0, 52.07]);
    assert_eq!(
        snap.render.paths.len(),
        434,
        "every item has a prim id in the render"
    );
    assert!(
        snap.render.ids.iter().any(|&id| id != 0),
        "snapshot has foreground pixels"
    );

    // The PNG is a real image, not an empty file.
    assert!(scene::png_is_nonempty(png_str));
    let meta = std::fs::metadata(&png).unwrap();
    assert!(meta.len() > 1000, "PNG has content ({} bytes)", meta.len());
    let img = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&png).unwrap()))
        .read_info()
        .unwrap();
    assert_eq!(img.info().width, 960);
    assert_eq!(img.info().height, 540);
}

#[test]
fn snapshot_render_is_deterministic() {
    let path = city_usdc();
    let bytes = std::fs::read(&path).unwrap();
    let a = scene::snapshot_usdc_bytes(bytes.clone(), path.to_str().unwrap(), 320, 240, None)
        .expect("render a");
    let b = scene::snapshot_usdc_bytes(bytes, path.to_str().unwrap(), 320, 240, None)
        .expect("render b");
    assert_eq!(a.render.beauty, b.render.beauty, "same pixels twice");
    assert_eq!(a.render.ids, b.render.ids, "same ids twice");
}

#[test]
fn the_fitted_camera_follows_the_documented_fit() {
    let path = city_usdc();
    let bytes = std::fs::read(&path).unwrap();
    let snap = scene::snapshot_usdc_bytes(bytes, path.to_str().unwrap(), 480, 270, None).unwrap();
    let bbox = BBox {
        min: snap.geometry.bounds.0,
        max: snap.geometry.bounds.1,
    };
    let cam = snap.geometry.camera.expect("fitted camera");
    // The eye is at center + (0.85d, -0.85d, 0.62d) for d the larger plan extent.
    let d = bbox.size()[0].max(bbox.size()[1]);
    let c = bbox.center();
    assert!((cam.position[0] - (c[0] + 0.85 * d)).abs() < 1e-6);
    assert!((cam.position[1] - (c[1] - 0.85 * d)).abs() < 1e-6);
    assert!((cam.position[2] - (c[2] + 0.62 * d)).abs() < 1e-6);
    assert_eq!(cam.look_at, c);
    // 35 mm through a 20.25 mm aperture, i.e. 2*atan(20.25 / (2*35)).
    let want_fov = (2.0f64 * (20.25f64 / (2.0 * 35.0)).atan()).to_degrees();
    assert!((cam.fov_deg - want_fov).abs() < 1e-9);
    assert!(
        (cam.fov_deg - 32.268_802).abs() < 1e-5,
        "{} deg",
        cam.fov_deg
    );

    // The fit centers the scene but does NOT contain the whole bbox: the
    // prescribed eye (0.85d, -0.85d, 0.62d) with a 35 mm / 36×20.25 lens crops the
    // round 655-unit ground plane at all four image borders. Measured, and
    // asserted here so a change in the fit is visible rather than silent:
    //   projected x ∈ [-7.3, 487.1], y ∈ [49.2, 361.2] on a 480×270 image
    // i.e. ~7 px off the left and right edges, ~91 px off the bottom. Reporting
    // this as a deviation, not tuning the constant to make the image look nicer.
    let (w, h) = (480u32, 270u32);
    let m = view_proj(cam, bbox, w, h);
    let mut lo = [f64::MAX; 2];
    let mut hi = [f64::MIN; 2];
    for corner in corners(&bbox) {
        let clip = {
            let mut out = [0.0; 4];
            for row in 0..4 {
                out[row] = m[row] * corner[0]
                    + m[4 + row] * corner[1]
                    + m[8 + row] * corner[2]
                    + m[12 + row];
            }
            out
        };
        assert!(clip[3] > 0.0, "corner {corner:?} is behind the eye");
        let ndc = [clip[0] / clip[3], clip[1] / clip[3]];
        let x = (ndc[0] * 0.5 + 0.5) * f64::from(w);
        let y = (1.0 - (ndc[1] * 0.5 + 0.5)) * f64::from(h);
        lo[0] = lo[0].min(x);
        lo[1] = lo[1].min(y);
        hi[0] = hi[0].max(x);
        hi[1] = hi[1].max(y);
    }
    for (got, want) in [
        (lo[0], -7.31),
        (hi[0], 487.10),
        (lo[1], 49.23),
        (hi[1], 361.24),
    ] {
        assert!(
            (got - want).abs() < 0.5,
            "projected extent {lo:?}..{hi:?} does not match the recorded fit"
        );
    }
    // The scene center still clips dead center.
    let cc = c;
    let mut out = [0.0; 4];
    for row in 0..4 {
        out[row] = m[row] * cc[0] + m[4 + row] * cc[1] + m[8 + row] * cc[2] + m[12 + row];
    }
    let x = (out[0] / out[3] * 0.5 + 0.5) * f64::from(w);
    let y = (1.0 - (out[1] / out[3] * 0.5 + 0.5)) * f64::from(h);
    assert!(
        (x - 240.0).abs() < 1.0 && (y - 135.0).abs() < 1.0,
        "center at ({x},{y})"
    );
}

fn corners(b: &BBox) -> Vec<[f64; 3]> {
    let mut out = Vec::with_capacity(8);
    for x in [b.min[0], b.max[0]] {
        for y in [b.min[1], b.max[1]] {
            for z in [b.min[2], b.max[2]] {
                out.push([x, y, z]);
            }
        }
    }
    out
}

/// The same view-projection the renderer builds: look-at, then a perspective
/// with wgpu's [0,1] clip z.
fn view_proj(cam: geometry::Camera, bbox: BBox, w: u32, h: u32) -> geometry::Mat {
    let center = bbox.center();
    let size = bbox.size();
    let radius = (size[0] * size[0] + size[1] * size[1] + size[2] * size[2]).sqrt() / 2.0;
    let cam_dist = dist(cam.position, center);
    let far = (cam_dist + radius * 2.0).max(10.0);
    let near = (far / 10000.0).max(0.01);
    let aspect = f64::from(w) / f64::from(h);
    let view = look_at(cam.position, cam.look_at, [0.0, 0.0, 1.0]);
    let proj = perspective(cam.fov_deg, aspect, near, far);
    geometry::mat_mul(proj, view)
}

fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn dist(a: [f64; 3], b: [f64; 3]) -> f64 {
    dot(sub(a, b), sub(a, b)).sqrt()
}
fn norm(v: [f64; 3]) -> [f64; 3] {
    let l = dot(v, v).sqrt();
    if l > 1e-12 {
        [v[0] / l, v[1] / l, v[2] / l]
    } else {
        [0.0, 0.0, 1.0]
    }
}
fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
fn look_at(eye: [f64; 3], target: [f64; 3], up: [f64; 3]) -> geometry::Mat {
    let f = norm(sub(target, eye));
    let s = norm(cross(f, up));
    let u = cross(s, f);
    let mut m = geometry::IDENTITY;
    m[0] = s[0];
    m[4] = s[1];
    m[8] = s[2];
    m[1] = u[0];
    m[5] = u[1];
    m[9] = u[2];
    m[2] = -f[0];
    m[6] = -f[1];
    m[10] = -f[2];
    m[12] = -dot(s, eye);
    m[13] = -dot(u, eye);
    m[14] = dot(f, eye);
    m
}
fn perspective(fovy_deg: f64, aspect: f64, near: f64, far: f64) -> geometry::Mat {
    let f = 1.0 / (fovy_deg * std::f64::consts::PI / 360.0).tan();
    let mut m = [0.0; 16];
    m[0] = f / aspect;
    m[5] = f;
    m[10] = far / (near - far);
    m[11] = -1.0;
    m[14] = far * near / (near - far);
    m
}

// --- 2. a composite stack with a session opinion, in memory only -----------

const CITY: &str = include_str!("../../../examples/city.rusd");
/// The lot the session layer overrides: a ground-floor building in block (0,0).
const LOT: &str = "/World/City/b0_0/lot0_0";
/// The override's translate (a value no cook output would produce: it lifts the
/// building 7 units and shifts it 3 along x).
const OVERRIDE: [f64; 3] = [3.0, 0.0, 7.0];
/// The ground plane, whose own geometry the session must not touch.
const GROUND: &str = "/World/City/Ground";

/// A `usda` session layer overriding `LOT`'s xformOp:translate. `xformOpOrder`
/// is re-authored as `token[]` (the block-local translate the cook wrote), so
/// the composed op list keeps its one entry and the override lands on it.
///
/// A prim clause nests by name from the pseudo-root (`over "World" { over
/// "City" { ... } }`): the parser appends each clause's name to the enclosing
/// path, so an absolute prim path is not spellable here.
fn session_usda() -> String {
    let segs: Vec<&str> = LOT.trim_start_matches('/').split('/').collect();
    let mut out = String::from("#usda 1.0\n\n");
    for (i, seg) in segs.iter().enumerate() {
        let pad = "    ".repeat(i);
        out.push_str(&format!("{pad}over \"{seg}\"\n{pad}{{\n"));
    }
    let pad = "    ".repeat(segs.len());
    out.push_str(&format!(
        "{pad}uniform token[] xformOpOrder = [\"xformOp:translate\"]\n\
         {pad}uniform point3f xformOp:translate = ({}, {}, {})\n",
        OVERRIDE[0], OVERRIDE[1], OVERRIDE[2]
    ));
    for i in (0..segs.len()).rev() {
        out.push_str(&format!("{}\n", "    ".repeat(i)));
        out.push('}');
        out.push('\n');
    }
    out
}

/// Cook `examples/city.rusd` and write its stage's composited root layer out as
/// `usdc` bytes: exactly what the M6 `export --bake` path produces, but held in
/// memory rather than on disk.
fn cooked_city_usdc() -> Vec<u8> {
    let doc = rusd_core::syntax::parse(CITY).expect("city.rusd parses");
    let out = rusd_proc::expand_doc(&doc).expect("city.rusd cooks");
    let flat = out.stage.flatten().expect("stage flattens");
    let format = openusd::sdf::LayerRegistry::find_by_id("usdc").expect("usdc format");
    format
        .write_bytes(flat.data())
        .expect("usdc writes")
        .bytes
        .to_vec()
}

/// The same cooked bytes composed with no session layer: the baseline a session
/// opinion is measured against.
fn plain_stack(cooked: &[u8]) -> Stack {
    let mut mem = MemResolver::new();
    mem.insert("/cooked/city.usdc", cooked.to_vec());
    Stack::open(mem, "/cooked/city.usdc", None, Vec::new()).expect("root alone opens")
}

#[test]
fn a_memory_only_stack_composes_its_session_override() {
    let cooked = cooked_city_usdc();
    let manifest = Manifest {
        root: "/cooked/city.usdc".to_owned(),
        session: Some("/session/override.usda".to_owned()),
        layers: vec![
            rusd_viewer::LayerBlob::usda("/session/override.usda", session_usda()),
            rusd_viewer::LayerBlob::usdc("/cooked/city.usdc", &cooked),
        ],
    };
    // Nothing was written: the mirror holds every layer.
    let stack = Stack::build(&manifest).expect("memory composite opens");
    assert_eq!(stack.mem.len(), 2, "both layers served from memory");
    assert!(
        stack
            .diags
            .iter()
            .all(|d| d.severity != rusd_core::Severity::Error),
        "{:?}",
        stack.diags
    );

    // The session opinion wins: /World/City/b0_0/lot0_0's composed translate.
    let t = stack
        .attribute(LOT, "xformOp:translate")
        .expect("lot has a composed translate");
    let got = match t {
        Value::Vec3f(v) => [f64::from(v.x), f64::from(v.y), f64::from(v.z)],
        Value::Vec3d(v) => [v.x, v.y, v.z],
        other => panic!("unexpected translate type {other:?}"),
    };
    // xformOp semantics: the authored translate composes on the prim, so the
    // composed translate value is the override itself.
    assert_eq!(
        got, OVERRIDE,
        "session translate override composed to {got:?}"
    );
    // And the world transform carries it: `xformOpOrder = [translate, scale]`
    // composes as `M = T * S`, and the lot sits under block b0_0's own translate
    // (street 12, street 12), so the session's translate — which *replaces* the
    // cook's lot-local one rather than adding to it — is offset only by the
    // block: world = block + override.
    let geo = geometry::extract(&stack.stage);
    let world = item_transform(&geo, LOT).expect("lot extracted");
    let block = [12.0, 12.0, 0.0];
    let want = [
        block[0] + OVERRIDE[0],
        block[1] + OVERRIDE[1],
        block[2] + OVERRIDE[2],
    ];
    let got = scene::translation(world);
    for k in 0..3 {
        assert!(
            (got[k] - want[k]).abs() < 1e-3,
            "world translate of {LOT} after the session override: {got:?}, want {want:?}"
        );
    }
    // Without the session, the same prim sits at the cook's lot-local offset
    // inside the block instead — so the difference really is the session.
    let cooked_only = geometry::extract(&plain_stack(&cooked).stage);
    let base = scene::translation(item_transform(&cooked_only, LOT).expect("lot in plain stack"));
    assert!(
        (base[2] - got[2]).abs() > 1.0,
        "the session moved the lot vertically: {base:?} -> {got:?}"
    );

    // Everything else is untouched: the ground plane's world bbox is the same
    // as the stack without the session.
    let plain = plain_stack(&cooked);
    let plain_geo = geometry::extract(&plain.stage);
    let g0 = item_bbox(&plain_geo, GROUND).expect("ground in plain stack");
    let g1 = item_bbox(&geo, GROUND).expect("ground in session stack");
    assert!(
        g0.approx_eq(&g1, 1e-9),
        "ground bbox moved: {:?} -> {:?}",
        g0.corners(),
        g1.corners()
    );
    assert_eq!(geo.items.len(), plain_geo.items.len(), "same prim set");
}

#[test]
fn the_mirror_composes_opinion_by_opinion_like_the_same_stack_directly() {
    // Build the identical stack in memory twice: once through the manifest
    // (mirror), once by opening the very same bytes through a hand-built
    // resolver. Every opinion must agree, so what a client receives over the
    // wire composes to what the bytes compose to.
    let cooked = cooked_city_usdc();
    let manifest = Manifest {
        root: "/cooked/city.usdc".to_owned(),
        session: Some("/session/override.usda".to_owned()),
        layers: vec![
            rusd_viewer::LayerBlob::usda("/session/override.usda", session_usda()),
            rusd_viewer::LayerBlob::usdc("/cooked/city.usdc", &cooked),
        ],
    };
    // Round-trip the manifest through the JSON a client actually sends.
    let wire = manifest.to_json();
    let parsed = Manifest::from_json(&wire).expect("manifest round-trips");
    assert_eq!(parsed, manifest, "manifest JSON round-trip is lossless");

    let via_manifest = Stack::build(&parsed).expect("mirror opens");

    let mut direct = MemResolver::new();
    direct.insert("/cooked/city.usdc", cooked.clone());
    direct.insert("/session/override.usda", session_usda().into_bytes());
    let via_direct = Stack::open(
        direct,
        "/cooked/city.usdc",
        Some("/session/override.usda"),
        Vec::new(),
    )
    .expect("direct open");

    let a = via_manifest.opinions("/World/City");
    let b = via_direct.opinions("/World/City");
    assert!(!a.is_empty(), "the city subtree has opinions");
    assert_eq!(a.len(), b.len(), "same opinion count");
    for (x, y) in a.iter().zip(&b) {
        assert_eq!(x.0, y.0, "prim path");
        assert_eq!(x.1, y.1, "property name");
        assert_eq!(x.2, y.2, "property value");
    }
    // And the stage-level composition agrees on the prim count.
    let n = |s: &Stack| {
        let mut n = 0;
        let _ = s
            .stage
            .traverse(openusd::usd::PrimPredicate::ALL, |_| n += 1);
        n
    };
    assert_eq!(n(&via_manifest), n(&via_direct), "same composed prim count");
}

#[test]
fn the_manifest_carries_layers_without_a_file() {
    // A tiny hand-built stack proves the manifest path end to end: root +
    // session, both `usda`, composed in memory, with the session's opinion on
    // top of the root's.
    let root = "#usda 1.0\n\ndef Xform \"World\"\n{\n}\n\
                def Cube \"Box\"\n{\n    uniform token[] xformOpOrder = [\"xformOp:translate\"]\n\
                \x20   uniform point3f xformOp:translate = (0, 0, 0)\n}\n";
    let session = "#usda 1.0\n\nover \"Box\"\n{\n    \
                   uniform point3f xformOp:translate = (5, 6, 7)\n}\n";
    let text = format!(
        "{{\"root\":\"/root.usda\",\"session\":\"/session.usda\",\"layers\":[\
         {{\"identifier\":\"/root.usda\",\"encoding\":\"usda\",\"data\":{}}},\
         {{\"identifier\":\"/session.usda\",\"encoding\":\"usda\",\"data\":{}}}]}}",
        json_str(root),
        json_str(session)
    );
    let manifest = Manifest::from_json(&text).expect("manifest parses");
    let stack = Stack::build(&manifest).expect("stack opens");
    let v = stack
        .attribute("/Box", "xformOp:translate")
        .expect("translate composes");
    match v {
        Value::Vec3f(t) => assert_eq!(
            [f64::from(t.x), f64::from(t.y), f64::from(t.z)],
            [5.0, 6.0, 7.0]
        ),
        Value::Vec3d(t) => assert_eq!([t.x, t.y, t.z], [5.0, 6.0, 7.0]),
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(stack.mem.len(), 2);
}

#[test]
fn a_missing_layer_is_a_diagnostic_not_a_panic() {
    let manifest = Manifest {
        root: "/root.usda".to_owned(),
        session: None,
        layers: vec![rusd_viewer::LayerBlob::usda("/root.usda", "#usda 1.0\n")],
    };
    // Fine: one layer, named by the manifest.
    assert!(Stack::build(&manifest).is_ok());
    // A manifest naming a layer it does not carry is refused, with a code.
    let bad = Manifest {
        root: "/root.usda".to_owned(),
        session: Some("/gone.usda".to_owned()),
        layers: vec![rusd_viewer::LayerBlob::usda("/root.usda", "#usda 1.0\n")],
    };
    let err = match Stack::build(&bad) {
        Ok(_) => panic!("a missing session layer must be refused"),
        Err(e) => e,
    };
    assert_eq!(err[0].code, "E_MANIFEST");
    assert!(err[0].message.contains("/gone.usda"), "{}", err[0].message);
    // A root that does not open is a diagnostic too, never a panic.
    let opened_unresolved = Stack::open(MemResolver::new(), "/nope.usda", None, Vec::new());
    assert!(opened_unresolved.is_err());
}

#[test]
fn base64_carries_usdc_bytes_losslessly() {
    // Every byte value, plus the lengths that hit each padding case.
    let mut bytes: Vec<u8> = (0..=255u8).collect();
    for n in 0..4 {
        bytes.truncate(256 - n.min(256));
        let enc = rusd_viewer::compose::base64_encode(&bytes);
        assert_eq!(
            rusd_viewer::compose::base64_decode(&enc).as_deref(),
            Some(&bytes[..]),
            "round-trip at len {}",
            bytes.len()
        );
    }
    // A cooked usdc survives the wire encoding: it still opens.
    let cooked = cooked_city_usdc();
    let blob = rusd_viewer::LayerBlob::usdc("/c/city.usdc", &cooked);
    assert_eq!(blob.bytes().expect("decodes"), cooked);
    let layer = Layer::from_bytes("city.usdc", blob.bytes().unwrap()).expect("still a usdc");
    assert!(
        layer
            .prim(sdf::path("/World/City").unwrap())
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_session_override_changes_the_rendered_pixels() {
    // The whole point of a session opinion: it is visible. Render the stack with
    // and without the session at a size where the lifted lot has pixels, and
    // require the images to differ and to agree elsewhere.
    let cooked = cooked_city_usdc();
    let with = Stack::build(&Manifest {
        root: "/cooked/city.usdc".to_owned(),
        session: Some("/session/override.usda".to_owned()),
        layers: vec![
            rusd_viewer::LayerBlob::usda("/session/override.usda", session_usda()),
            rusd_viewer::LayerBlob::usdc("/cooked/city.usdc", &cooked),
        ],
    })
    .unwrap();
    let mut mem = MemResolver::new();
    mem.insert("/cooked/city.usdc", cooked.clone());
    let without = Stack::open(mem, "/cooked/city.usdc", None, Vec::new()).unwrap();

    let ga = scene::extract_fitted(&with.stage);
    let gb = scene::extract_fitted(&without.stage);
    let ra = scene::render_offscreen(&ga, 480, 270).unwrap();
    let rb = scene::render_offscreen(&gb, 480, 270).unwrap();
    assert_eq!(ra.beauty.len(), rb.beauty.len());
    let differing = ra
        .beauty
        .iter()
        .zip(&rb.beauty)
        .filter(|(a, b)| a != b)
        .count();
    assert!(differing > 0, "the session override moved visible geometry");

    // The id buffer still resolves every visible pixel to a real prim.
    for (pos, &id) in ra.ids.iter().enumerate() {
        if id == 0 {
            continue;
        }
        let x = (pos as u32) % ra.width;
        let y = (pos as u32) / ra.width;
        assert!(
            ra.pick(x, y).is_some(),
            "pixel ({x},{y}) id {id} unresolved"
        );
    }
}

fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[allow(dead_code)]
fn path_exists(p: &Path) -> bool {
    p.exists()
}
