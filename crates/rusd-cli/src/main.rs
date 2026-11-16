//! `rusd` command line entry point. Subcommands land per docs/ROADMAP.md:
//! fmt/parse (M1), validate/query (M2), expand (M3), render/pick (M4),
//! serve (M5), export (M6).

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("version") => {
            println!("rusd {}", env!("CARGO_PKG_VERSION"));
            println!("M1 in progress — see docs/ROADMAP.md for what lands when");
        }
        Some("parse") => {
            let Some(path) = args.get(1) else {
                eprintln!("usage: rusd parse <file.rusd>");
                std::process::exit(2);
            };
            let text = match std::fs::read_to_string(path) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("rusd: cannot read {path}: {e}");
                    std::process::exit(2);
                }
            };
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
                Err(diags) => {
                    for d in &diags {
                        println!("{}", d.to_json());
                    }
                    std::process::exit(1);
                }
            }
        }
        Some(other) => {
            eprintln!("rusd: '{other}' is not implemented yet — see docs/ROADMAP.md");
            std::process::exit(2);
        }
    }
}
