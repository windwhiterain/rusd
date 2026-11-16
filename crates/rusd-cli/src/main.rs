//! `rusd` command line entry point. Subcommands land per docs/ROADMAP.md:
//! fmt/parse (M1), validate/query (M2), expand (M3), render/pick (M4),
//! serve (M5), export (M6).

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
                eprintln!("usage: rusd render <file.rusd> [--out PREFIX] [--width W] [--height H]");
                std::process::exit(2);
            };
            let width = flag_val(&args, "--width")
                .and_then(|v| v.parse().ok())
                .unwrap_or(800u32);
            let height = flag_val(&args, "--height")
                .and_then(|v| v.parse().ok())
                .unwrap_or(600u32);
            let prefix = flag_val(&args, "--out").unwrap_or_else(|| default_prefix(path));
            let geo = load_scene(path);
            match rusd_render::render(&geo, width, height) {
                Ok(r) => {
                    for d in &r.diags {
                        println!("{}", d.to_json());
                    }
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
                        "render: {}x{} items={} foreground={} -> {prefix}_beauty.png / {prefix}_ids.png / {prefix}.json",
                        r.width,
                        r.height,
                        r.paths.len(),
                        fg
                    );
                }
                Err(diags) => print_diags(&diags),
            }
        }
        Some("pick") => {
            let Some(path) = args.get(1) else {
                eprintln!("usage: rusd pick <file.rusd> --x X --y Y [--width W] [--height H]");
                std::process::exit(2);
            };
            let Some(x) = flag_val(&args, "--x").and_then(|v| v.parse::<u32>().ok()) else {
                eprintln!("rusd pick: --x <pixel> required");
                std::process::exit(2);
            };
            let Some(y) = flag_val(&args, "--y").and_then(|v| v.parse::<u32>().ok()) else {
                eprintln!("rusd pick: --y <pixel> required");
                std::process::exit(2);
            };
            let width = flag_val(&args, "--width")
                .and_then(|v| v.parse().ok())
                .unwrap_or(800u32);
            let height = flag_val(&args, "--height")
                .and_then(|v| v.parse().ok())
                .unwrap_or(600u32);
            let geo = load_scene(path);
            match rusd_render::render(&geo, width, height) {
                Ok(r) => match r.pick(x, y) {
                    Some(p) => println!("{p}"),
                    None => println!("background"),
                },
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
