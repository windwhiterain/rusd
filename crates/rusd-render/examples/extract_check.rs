//! M4 extraction check: expand city/forest, extract geometry, report what the
//! renderer would consume. Run: `cargo run -p rusd-render --example extract_check`

use rusd_scene::geometry;

fn check(name: &str, src: &str) {
    let doc = rusd_core::syntax::parse(src).expect("parse");
    let out = rusd_proc::expand_doc(&doc, None).expect("expand");
    let geo = geometry::extract(&out.stage);
    println!("== {name} ==");
    println!("composed prims: {}", out.prim_count);
    println!("geo items: {}", geo.items.len());
    println!("camera: {:?}", geo.camera);
    println!("bounds: min={:?} max={:?}", geo.bounds.0, geo.bounds.1);
    println!("diags: {}", geo.diags.len());
    for d in &geo.diags {
        println!("  {}", d.code);
    }
    let mut cubes = 0;
    let mut spheres = 0;
    let mut other = 0;
    for it in &geo.items {
        match it.mesh.positions.len() {
            24 => cubes += 1,
            425 => spheres += 1,
            _ => other += 1,
        }
    }
    println!("cubes(24v)={cubes} spheres(425v)={spheres} other={other}");
    for it in geo.items.iter().take(3) {
        println!(
            "  {} verts={} translate=[{:.2},{:.2},{:.2}] color={:?}",
            it.path,
            it.mesh.positions.len(),
            it.transform[12],
            it.transform[13],
            it.transform[14],
            it.color
        );
    }
    let geo2 = geometry::extract(&out.stage);
    let same = geo.items.len() == geo2.items.len()
        && geo
            .items
            .iter()
            .zip(&geo2.items)
            .all(|(a, b)| a.path == b.path && a.transform == b.transform);
    println!("deterministic: {same}");
}

fn main() {
    check("forest", include_str!("../../../examples/forest.rusd"));
    check("city", include_str!("../../../examples/city.rusd"));
}
