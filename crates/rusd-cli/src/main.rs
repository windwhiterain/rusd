//! `rusd` command line entry point. Subcommands land per docs/ROADMAP.md:
//! fmt/parse (M1), validate/query (M2), expand (M3), render/pick (M4),
//! serve (M5), export (M6).

use rusd_render::annotate::{self, Marker, SelectMode, Selection};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("version") => {
            println!("rusd {}", env!("CARGO_PKG_VERSION"));
            println!("M4 in progress — see docs/ROADMAP.md for what lands when");
        }
        Some("parse") => {
            let Some(path) = args.get(1) else {
                eprintln!("usage: rusd parse <file.rusd>");
                std::process::exit(2);
            };
            let text = read(path);
            match rusd_core::syntax::parse(&text) {
                Ok(doc) => {
                    println!("[stage]");
                    for (k, v) in &doc.stage {
                        println!("{k} = {v}");
                    }
                    for b in &doc.prims {
                        println!("\n[{}]  # schema = {}", b.path, b.schema);
                        for (k, v) in &b.props {
                            println!("{k} = {v}");
                        }
                    }
                }
                Err(diags) => print_diags(&diags),
            }
        }
        Some("fmt") => {
            let write = args.iter().any(|a| a == "--write" || a == "-w");
            let Some(path) = args.iter().skip(1).find(|a| !a.starts_with('-')) else {
                eprintln!("usage: rusd fmt [--write] <file.rusd>");
                std::process::exit(2);
            };
            let text = read(path);
            match rusd_core::canonicalize(&text) {
                Ok(out) => {
                    if write {
                        if let Err(e) = std::fs::write(path, &out) {
                            eprintln!("rusd: cannot write {path}: {e}");
                            std::process::exit(2);
                        }
                    } else {
                        print!("{out}");
                    }
                }
                Err(diags) => print_diags(&diags),
            }
        }
        Some("validate") => {
            let Some(path) = args.get(1) else {
                eprintln!("usage: rusd validate <file.rusd>");
                std::process::exit(2);
            };
            let text = read(path);
            match rusd_core::syntax::parse(&text) {
                Ok(doc) => {
                    let diags = rusd_scene::validate(&doc);
                    let mut has_error = false;
                    for d in &diags {
                        println!("{}", d.to_json());
                        if d.severity == rusd_core::Severity::Error {
                            has_error = true;
                        }
                    }
                    if diags.is_empty() {
                        println!("ok: {} prims, no diagnostics", doc.prims.len());
                    }
                    if has_error {
                        std::process::exit(1);
                    }
                }
                Err(diags) => print_diags(&diags),
            }
        }
        Some("query") => {
            let Some(path) = args.get(1) else {
                eprintln!("usage: rusd query <file.rusd>");
                std::process::exit(2);
            };
            let text = read(path);
            match rusd_core::syntax::parse(&text) {
                Ok(doc) => match rusd_scene::lower(&doc) {
                    Ok(stage) => {
                        for d in rusd_scene::composition_diags(&stage) {
                            println!("{}", d.to_json());
                        }
                        for info in rusd_scene::query(&stage, &doc) {
                            println!("[{}] type={}", info.path, info.type_name);
                            for (n, v) in &info.props {
                                println!("  {n} = {v}");
                            }
                        }
                    }
                    Err(diags) => print_diags(&diags),
                },
                Err(diags) => print_diags(&diags),
            }
        }
        Some("expand") => {
            let Some(path) = args.get(1) else {
                eprintln!("usage: rusd expand <file.rusd>");
                std::process::exit(2);
            };
            let text = read(path);
            let doc = match rusd_core::syntax::parse(&text) {
                Ok(d) => d,
                Err(diags) => print_diags(&diags),
            };
            let vdiags = rusd_scene::validate(&doc);
            let mut has_error = false;
            for d in &vdiags {
                println!("{}", d.to_json());
                if d.severity == rusd_core::Severity::Error {
                    has_error = true;
                }
            }
            if has_error {
                std::process::exit(1);
            }
            match rusd_proc::expand_doc(&doc) {
                Ok(out) => {
                    for d in &out.diags {
                        println!("{}", d.to_json());
                    }
                    println!(
                        "cook: {} cooked, {} reused, {} skipped ({} generator nodes)",
                        out.stats.cooked, out.stats.reused, out.stats.skipped, out.node_count
                    );
                    println!("composed prims: {}", out.prim_count);
                }
                Err(diags) => print_diags(&diags),
            }
        }
        Some("render") => {
            let Some(path) = args.get(1) else {
                eprintln!(
                    "usage: rusd render <file.rusd> [--out PREFIX] [--width W] [--height H] [--select path[=color][:mode]]... [--mark x,y[,color]]..."
                );
                std::process::exit(2);
            };
            let width = flag_val(&args, "--width")
                .and_then(|v| v.parse().ok())
                .unwrap_or(800u32);
            let height = flag_val(&args, "--height")
                .and_then(|v| v.parse().ok())
                .unwrap_or(600u32);
            let prefix = flag_val(&args, "--out").unwrap_or_else(|| default_prefix(path));
            let selections: Vec<Selection> = flag_vals(&args, "--select")
                .iter()
                .filter_map(|s| parse_select(s))
                .collect();
            let markers: Vec<Marker> = flag_vals(&args, "--mark")
                .iter()
                .enumerate()
                .filter_map(|(i, s)| parse_mark(s, i))
                .collect();
            let geo = load_scene(path);
            match rusd_render::render(&geo, width, height) {
                Ok(mut r) => {
                    for d in &r.diags {
                        println!("{}", d.to_json());
                    }
                    annotate::apply_selection(&mut r, &selections);
                    annotate::draw_markers(&mut r, &markers);
                    if let Err(e) = rusd_render::capture::write_png(
                        &format!("{prefix}_beauty.png"),
                        r.width,
                        r.height,
                        &r.beauty,
                    ) {
                        eprintln!("rusd: {e}");
                        std::process::exit(1);
                    }
                    if let Err(e) = rusd_render::capture::write_png(
                        &format!("{prefix}_ids.png"),
                        r.width,
                        r.height,
                        &rusd_render::capture::ids_to_rgba(&r.ids),
                    ) {
                        eprintln!("rusd: {e}");
                        std::process::exit(1);
                    }
                    write_metadata(&prefix, &r);
                    let fg = r.ids.iter().filter(|&&id| id != 0).count();
                    println!(
                        "render: {}x{} items={} foreground={} selected={} marked={} -> {prefix}_beauty.png / {prefix}_ids.png / {prefix}.json",
                        r.width,
                        r.height,
                        r.paths.len(),
                        fg,
                        selections.len(),
                        markers.len()
                    );
                }
                Err(diags) => print_diags(&diags),
            }
        }
        Some("pick") => {
            let Some(path) = args.get(1) else {
                eprintln!(
                    "usage: rusd pick <file.rusd> --x X --y Y | --point x,y... [--visual] [--out PREFIX] [--width W] [--height H]"
                );
                std::process::exit(2);
            };
            let visual = args.iter().any(|a| a == "--visual");
            let mut points: Vec<(u32, u32)> = flag_vals(&args, "--point")
                .iter()
                .filter_map(|s| parse_point(s))
                .collect();
            if points.is_empty()
                && let (Some(x), Some(y)) = (
                    flag_val(&args, "--x").and_then(|v| v.parse().ok()),
                    flag_val(&args, "--y").and_then(|v| v.parse().ok()),
                )
            {
                points.push((x, y));
            }
            if points.is_empty() {
                eprintln!("rusd pick: --x/--y or --point x,y required");
                std::process::exit(2);
            }
            let width = flag_val(&args, "--width")
                .and_then(|v| v.parse().ok())
                .unwrap_or(800u32);
            let height = flag_val(&args, "--height")
                .and_then(|v| v.parse().ok())
                .unwrap_or(600u32);
            let prefix = flag_val(&args, "--out").unwrap_or_else(|| default_prefix(path));
            let geo = load_scene(path);
            match rusd_render::render(&geo, width, height) {
                Ok(mut r) => {
                    if visual {
                        let mut sels: Vec<Selection> = Vec::new();
                        let mut marks: Vec<Marker> = Vec::new();
                        for (i, &(x, y)) in points.iter().enumerate() {
                            let c = annotate::palette(i);
                            marks.push(Marker { x, y, color: c });
                            if let Some(p) = r.pick(x, y) {
                                sels.push(Selection {
                                    path: p.to_owned(),
                                    color: c,
                                    mode: SelectMode::Outline,
                                });
                            }
                        }
                        annotate::apply_selection(&mut r, &sels);
                        annotate::draw_markers(&mut r, &marks);
                        if let Err(e) = rusd_render::capture::write_png(
                            &format!("{prefix}_beauty.png"),
                            r.width,
                            r.height,
                            &r.beauty,
                        ) {
                            eprintln!("rusd: {e}");
                            std::process::exit(1);
                        }
                        if let Err(e) = rusd_render::capture::write_png(
                            &format!("{prefix}_ids.png"),
                            r.width,
                            r.height,
                            &rusd_render::capture::ids_to_rgba(&r.ids),
                        ) {
                            eprintln!("rusd: {e}");
                            std::process::exit(1);
                        }
                        write_metadata(&prefix, &r);
                    }
                    for &(x, y) in &points {
                        match r.pick(x, y) {
                            Some(p) => println!("{p}"),
                            None => println!("background"),
                        }
                    }
                    if visual {
                        println!(
                            "visual: {prefix}_beauty.png ({} point(s) marked)",
                            points.len()
                        );
                    }
                }
                Err(diags) => print_diags(&diags),
            }
        }
        Some(other) => {
            eprintln!("rusd: '{other}' is not implemented yet — see docs/ROADMAP.md");
            std::process::exit(2);
        }
    }
}

/// Parse → validate → expand → extract, printing diagnostics and exiting on error.
fn load_scene(path: &str) -> rusd_scene::geometry::SceneGeometry {
    let text = read(path);
    let doc = match rusd_core::syntax::parse(&text) {
        Ok(d) => d,
        Err(diags) => print_diags(&diags),
    };
    let vdiags = rusd_scene::validate(&doc);
    let mut has_error = false;
    for d in &vdiags {
        println!("{}", d.to_json());
        if d.severity == rusd_core::Severity::Error {
            has_error = true;
        }
    }
    if has_error {
        std::process::exit(1);
    }
    let out = match rusd_proc::expand_doc(&doc) {
        Ok(o) => o,
        Err(diags) => print_diags(&diags),
    };
    for d in &out.diags {
        println!("{}", d.to_json());
    }
    rusd_scene::geometry::extract(&out.stage)
}

fn flag_val(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// All values of a repeatable flag (`--select a --select b` → [a, b]).
fn flag_vals(args: &[String], flag: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, a) in args.iter().enumerate() {
        if a == flag
            && let Some(v) = args.get(i + 1)
        {
            out.push(v.clone());
        }
    }
    out
}

fn parse_point(s: &str) -> Option<(u32, u32)> {
    let (x, y) = s.split_once(',')?;
    Some((x.parse().ok()?, y.parse().ok()?))
}

/// `path[=color][:mode]` — color defaults red, mode defaults outline.
fn parse_select(s: &str) -> Option<Selection> {
    let (path, value) = match s.split_once('=') {
        Some((p, v)) => (p, Some(v)),
        None => (s, None),
    };
    let mut color = [255, 60, 60];
    let mut mode = SelectMode::Outline;
    if let Some(v) = value {
        let (c, m) = match v.split_once(':') {
            Some((c, m)) => (c, Some(m)),
            None => (v, None),
        };
        if !c.is_empty() {
            color = annotate::parse_color(c)?;
        }
        if let Some(m) = m {
            mode = match m {
                "recolor" => SelectMode::Recolor,
                "both" => SelectMode::Both,
                "outline" => SelectMode::Outline,
                _ => return None,
            };
        }
    }
    Some(Selection {
        path: path.to_owned(),
        color,
        mode,
    })
}

/// `x,y[,color]` — color defaults to the auto palette by index.
fn parse_mark(s: &str, idx: usize) -> Option<Marker> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() < 2 {
        return None;
    }
    let x = parts[0].parse().ok()?;
    let y = parts[1].parse().ok()?;
    let color = if parts.len() >= 3 {
        annotate::parse_color(parts[2])?
    } else {
        annotate::palette(idx)
    };
    Some(Marker { x, y, color })
}

fn default_prefix(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "render".to_owned())
}

/// Machine-readable render metadata: size, id→path table, diagnostics (SPEC §7/§8).
fn write_metadata(prefix: &str, r: &rusd_render::RenderOutput) {
    let mut s = format!(
        "{{\"width\":{},\"height\":{},\"items\":{},\"paths\":[",
        r.width,
        r.height,
        r.paths.len()
    );
    for (i, p) in r.paths.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push('"');
        s.push_str(p); // prim paths are [A-Za-z0-9_/], no escaping needed
        s.push('"');
    }
    s.push_str("],\"diags\":[");
    for (i, d) in r.diags.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&d.to_json());
    }
    s.push_str("]}");
    if let Err(e) = std::fs::write(format!("{prefix}.json"), s) {
        eprintln!("rusd: cannot write {prefix}.json: {e}");
    }
}

fn read(path: &str) -> String {
    match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("rusd: cannot read {path}: {e}");
            std::process::exit(2);
        }
    }
}

fn print_diags(diags: &[rusd_core::Diagnostic]) -> ! {
    for d in diags {
        println!("{}", d.to_json());
    }
    std::process::exit(1);
}
