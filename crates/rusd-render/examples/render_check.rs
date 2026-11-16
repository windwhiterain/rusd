//! M4 render check: expand + extract + render forest/city, write PNGs, verify the
//! prim-id → path roundtrip and pixel determinism.
//! Run: `cargo run -p rusd-render --example render_check`

use rusd_scene::geometry;

fn run(name: &str, src: &str) {
    let doc = rusd_core::syntax::parse(src).unwrap();
    let out = rusd_proc::expand_doc(&doc).unwrap();
    let geo = geometry::extract(&out.stage);
    let r = rusd_render::render(&geo, 800, 600).expect("render");
    rusd_render::capture::write_png(
        &format!("target/{name}_beauty.png"),
        r.width,
        r.height,
        &r.beauty,
    )
    .unwrap();
    rusd_render::capture::write_png(
        &format!("target/{name}_ids.png"),
        r.width,
        r.height,
        &rusd_render::capture::ids_to_rgba(&r.ids),
    )
    .unwrap();
    let fg = r.ids.iter().filter(|&&id| id != 0).count();
    println!(
        "{name}: {}x{} fg_pixels={} ({:.1}%) items={}",
        r.width,
        r.height,
        fg,
        100.0 * fg as f64 / (r.width * r.height) as f64,
        r.paths.len()
    );
    let mut checked = 0;
    let mut ok = 0;
    for (pos, &id) in r.ids.iter().enumerate() {
        if id == 0 {
            continue;
        }
        let x = (pos as u32) % r.width;
        let y = (pos as u32) / r.width;
        if r.pick(x, y) == Some(r.paths[(id - 1) as usize].as_str()) {
            ok += 1;
        }
        checked += 1;
        if checked >= 2000 {
            break;
        }
    }
    println!("  pick roundtrip: {ok}/{checked} foreground pixels → correct path");
    println!("  center pick = {:?}", r.pick(r.width / 2, r.height / 2));
    let r2 = rusd_render::render(&geo, 800, 600).expect("render2");
    println!(
        "  deterministic pixels: {}",
        r.beauty == r2.beauty && r.ids == r2.ids
    );
}

fn main() {
    run("forest", include_str!("../../../examples/forest.rusd"));
    run("city", include_str!("../../../examples/city.rusd"));
}
