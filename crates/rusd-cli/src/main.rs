//! `rusd` command line entry point. Subcommands land per docs/ROADMAP.md:
//! fmt/parse (M1), validate/query (M2), expand (M3), render/pick (M4),
//! serve (M5), export (M6).

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("version") => {
            println!("rusd {}", env!("CARGO_PKG_VERSION"));
            println!("M0 scaffold — see docs/ROADMAP.md for what lands when");
        }
        Some(other) => {
            eprintln!("rusd: '{other}' is not implemented yet — see docs/ROADMAP.md");
            std::process::exit(2);
        }
    }
}
