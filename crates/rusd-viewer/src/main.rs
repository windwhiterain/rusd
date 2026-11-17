//! `rusd-viewer` — the spike-② client: compose a USD stack that lives only in
//! memory and render it headless. No network, no egui, no server.
//!
//! ```text
//! rusd-viewer snapshot --usdc <path> --out <png> [--width 960 --height 540]
//! rusd-viewer compose  --manifest <json> --out <png> [--width 960 --height 540]
//! ```
//!
//! Both print one JSON line (size, items, bbox, camera, foreground pixels,
//! PNG path) and any diagnostics as JSON lines before it. A failure prints its
//! diagnostics and exits 1; it never panics.

use rusd_core::Diagnostic;
use rusd_viewer::compose::{Manifest, Stack};
use rusd_viewer::scene::{self, BBox};

const DEFAULT_WIDTH: u32 = 960;
const DEFAULT_HEIGHT: u32 = 540;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("view") => view_cmd(&args[1..]),
        Some("snapshot") => snapshot_cmd(&args),
        Some("compose") => compose_cmd(&args),
        Some("help") | Some("--help") | Some("-h") => {
            usage();
            0
        }
        _ => {
            usage();
            2
        }
    };
    std::process::exit(code);
}

fn usage() {
    eprintln!(
        "usage:\n  \
         rusd-viewer view     --connect <addr:port> [--usdc <fallback>]\n  \
         rusd-viewer snapshot --usdc <path> --out <png> [--width {DEFAULT_WIDTH} --height {DEFAULT_HEIGHT}]\n  \
         rusd-viewer compose  --manifest <json> --out <png> [--width {DEFAULT_WIDTH} --height {DEFAULT_HEIGHT}]"
    );
}

/// `view`: the interactive client (D12, `docs/REMOTE.md`): a winit + egui window
/// that connects to a server, shows its snapshots, and keeps orbit / pan / zoom /
/// pick / selection local.
fn view_cmd(args: &[String]) -> i32 {
    let args = match rusd_viewer::app::ViewArgs::parse(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("rusd-viewer view: {e}");
            usage();
            return 2;
        }
    };
    rusd_viewer::app::run(args)
}

/// `snapshot`: a single `.usdc` read from disk, composed in memory, rendered.
fn snapshot_cmd(args: &[String]) -> i32 {
    let (Some(usdc), Some(out)) = (flag_val(args, "--usdc"), flag_val(args, "--out")) else {
        eprintln!("rusd-viewer snapshot: --usdc and --out are required");
        usage();
        return 2;
    };
    let (w, h) = size_flags(args);
    match scene::snapshot_png(&usdc, &out, w, h) {
        Ok(snap) => {
            print_diags(&snap.diags);
            println!("{}", summary_json(&snap, &out));
            if snap.diags.iter().any(is_error) {
                1
            } else {
                0
            }
        }
        Err(diags) => {
            print_diags(&diags);
            1
        }
    }
}

/// `compose`: a manifest of in-memory layers (root + sublayers + session),
/// composed through `MemResolver`, rendered. Proves a composite stack with
/// session opinions renders without any file on disk.
fn compose_cmd(args: &[String]) -> i32 {
    let (Some(manifest), Some(out)) = (flag_val(args, "--manifest"), flag_val(args, "--out"))
    else {
        eprintln!("rusd-viewer compose: --manifest and --out are required");
        usage();
        return 2;
    };
    let (w, h) = size_flags(args);
    let text = match std::fs::read_to_string(&manifest) {
        Ok(t) => t,
        Err(e) => {
            print_diags(&[Diagnostic::error("E_READ", format!("{manifest}: {e}"))]);
            return 1;
        }
    };
    match compose_to_png(&text, &out, w, h) {
        Ok((summary, diags)) => {
            print_diags(&diags);
            println!("{summary}");
            if diags.iter().any(is_error) { 1 } else { 0 }
        }
        Err(diags) => {
            print_diags(&diags);
            1
        }
    }
}

/// Compose a manifest's stack in memory and write its snapshot PNG. Split out so
/// a test can call it without a process.
pub fn compose_to_png(
    manifest_json: &str,
    out: &str,
    width: u32,
    height: u32,
) -> Result<(String, Vec<Diagnostic>), Vec<Diagnostic>> {
    let manifest = Manifest::from_json(manifest_json)?;
    let stack = Stack::build(&manifest)?;
    let mut diags = stack.diags.clone();
    // The headless snapshot's frame is the output image: fit to the image the
    // PNG will be, so a `compose` of a wide scene is framed like the same scene
    // in a wide window.
    let aspect = f64::from(width) / f64::from(height.max(1));
    let geo = scene::extract_fitted(&stack.stage, aspect);
    diags.extend(geo.diags.iter().cloned());
    let render = scene::write_snapshot(&geo, width, height, out)?;
    let bbox = BBox {
        min: geo.bounds.0,
        max: geo.bounds.1,
    };
    let cam = geo.camera.expect("extract_fitted always sets a camera");
    let fg = render.ids.iter().filter(|&&id| id != 0).count();
    let summary = format!(
        "{{\"ok\":true,\"width\":{},\"height\":{},\"items\":{},\"prims\":{},\
         \"bbox\":{{\"min\":[{},{},{}],\"max\":[{},{},{}]}},\
         \"camera\":{{\"position\":[{},{},{}],\"look_at\":[{},{},{}],\"fov_deg\":{}}},\
         \"foreground\":{},\"layers\":{},\"png\":\"{}\"}}",
        render.width,
        render.height,
        geo.items.len(),
        render.paths.len(),
        bbox.min[0],
        bbox.min[1],
        bbox.min[2],
        bbox.max[0],
        bbox.max[1],
        bbox.max[2],
        cam.position[0],
        cam.position[1],
        cam.position[2],
        cam.look_at[0],
        cam.look_at[1],
        cam.look_at[2],
        cam.fov_deg,
        fg,
        stack.mem.len(),
        out.replace('\\', "/"),
    );
    Ok((summary, diags))
}

/// One JSON line describing a snapshot (size, items, bbox, camera, PNG).
pub fn summary_json(snap: &scene::Snapshot, out: &str) -> String {
    let bbox = BBox {
        min: snap.geometry.bounds.0,
        max: snap.geometry.bounds.1,
    };
    let cam = snap
        .geometry
        .camera
        .expect("extract_fitted always sets a camera");
    let fg = snap.render.ids.iter().filter(|&&id| id != 0).count();
    format!(
        "{{\"ok\":true,\"width\":{},\"height\":{},\"items\":{},\"prims\":{},\
         \"bbox\":{{\"min\":[{},{},{}],\"max\":[{},{},{}]}},\
         \"camera\":{{\"position\":[{},{},{}],\"look_at\":[{},{},{}],\"fov_deg\":{}}},\
         \"foreground\":{},\"png\":\"{}\"}}",
        snap.render.width,
        snap.render.height,
        snap.geometry.items.len(),
        snap.render.paths.len(),
        bbox.min[0],
        bbox.min[1],
        bbox.min[2],
        bbox.max[0],
        bbox.max[1],
        bbox.max[2],
        cam.position[0],
        cam.position[1],
        cam.position[2],
        cam.look_at[0],
        cam.look_at[1],
        cam.look_at[2],
        cam.fov_deg,
        fg,
        out.replace('\\', "/"),
    )
}

fn print_diags(diags: &[Diagnostic]) {
    for d in diags {
        eprintln!("{}", d.to_json());
    }
}

fn is_error(d: &Diagnostic) -> bool {
    d.severity == rusd_core::Severity::Error
}

fn size_flags(args: &[String]) -> (u32, u32) {
    let w = flag_val(args, "--width")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_WIDTH);
    let h = flag_val(args, "--height")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_HEIGHT);
    (w, h)
}

fn flag_val(args: &[String], flag: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == flag {
            return it.next().cloned();
        }
        if let Some(rest) = a.strip_prefix(&format!("{flag}=")) {
            return Some(rest.to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_flags_default_and_override() {
        let base = vec!["compose".to_owned(), "--out".to_owned(), "x.png".to_owned()];
        assert_eq!(size_flags(&base), (960, 540));
        let mut v = base.clone();
        v.extend(["--width".to_owned(), "320".to_owned()]);
        v.extend(["--height=240".to_owned()]);
        assert_eq!(size_flags(&v), (320, 240));
    }

    #[test]
    fn snapshot_is_reachable_from_the_library() {
        // A compile-time check that the binary and the library agree on the
        // entry point the tests use.
        let _ = scene::snapshot_usdc_bytes;
        let _ = compose_to_png;
    }
}
