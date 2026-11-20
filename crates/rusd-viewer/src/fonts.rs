//! Text is drawn with the face the machine already has; the binary embeds none.
//!
//! egui carries a Latin face behind its `default_fonts` feature, and this viewer turns
//! that feature off (the user's ruling: **不要 embed 字体，直接用用户的系统字体**). So the
//! definitions come from a font file read at startup — `C:\Windows\Fonts` on Windows — and
//! every family holds that face and nothing else: no bundled face is kept in front of it,
//! because a family with a built-in face first is exactly what the ruling rules out.
//!
//! A CJK face is not optional: every label in this window is Chinese, and without one the
//! whole UI is tofu. Candidates, best first (the order `C:\Windows\Fonts` actually offers on
//! a Chinese install):
//!
//! 1. `msyh.ttc` — Microsoft YaHei, the UI font of Windows itself;
//! 2. `simsun.ttc` — SimSun, the older fallback that is nearly always present;
//! 3. `simhei.ttf` — SimHei, a plain black sans;
//! 4. a few POSIX paths, so the same binary is legible off Windows;
//! 5. nothing: the definitions come back empty and the UI draws tofu, which is a cosmetic
//!    loss and not a reason to refuse to start.
//!
//! A `.ttc` is a *collection*: the first face (`index 0`) of YaHei is the regular weight,
//! which is what a status bar wants.

use std::path::PathBuf;
use std::sync::Arc;

use egui::FontDefinitions;
use egui::epaint::text::{FontData, FontFamily};

/// The key the loaded face is registered under in [`FontDefinitions::font_data`].
pub const CJK_KEY: &str = "rusd-cjk";

/// The key a system **fixed-width** face is registered under, when the machine has one.
pub const MONO_KEY: &str = "rusd-mono";

/// Where to look for the face the window reads text in, best first.
///
/// Windows paths are absolute and fixed; the POSIX entries are best-effort so a Linux
/// build is not silent tofu. Nothing here is required to exist.
pub const CJK_CANDIDATES: &[&str] = &[
    r"C:\Windows\Fonts\msyh.ttc",
    r"C:\Windows\Fonts\simsun.ttc",
    r"C:\Windows\Fonts\simhei.ttf",
    "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/System/Library/Fonts/PingFang.ttc",
];

/// Where to look for a **fixed-width** face: the detail panel prints layer text in
/// `monospace()` and a proportional face would lose its alignment. Latin-only, which is
/// fine — anything it lacks falls through to the CJK face in the same family.
pub const MONO_CANDIDATES: &[&str] = &[
    r"C:\Windows\Fonts\consola.ttf",
    r"C:\Windows\Fonts\cour.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/System/Library/Fonts/Menlo.ttc",
];

/// Which face was loaded, and how big it was — the report a caller can log and a test can
/// assert on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontChoice {
    /// The file the bytes came from.
    pub path: PathBuf,
    /// Its size in bytes (never zero: an empty read is a failed read).
    pub bytes: usize,
}

/// The font definitions: one family, one face, and that face is the system's.
///
/// `None` when no candidate file could be read; the definitions returned are then
/// [`FontDefinitions::default`], which with `default_fonts` off holds no face at all — the
/// UI draws tofu and the caller says so, rather than refusing to start.
pub fn definitions_with_cjk() -> (FontDefinitions, Option<FontChoice>) {
    let mut defs = FontDefinitions::default();
    let Some((path, bytes)) = first_readable(CJK_CANDIDATES) else {
        return (defs, None);
    };
    let choice = FontChoice {
        path,
        bytes: bytes.len(),
    };
    // `index 0` is the regular face of a `.ttc` collection.
    // `Blob` is `Arc<dyn AsRef<[u8]> + Send + Sync>`: an `Arc<Vec<u8>>` coerces into
    // it (a `Vec` is sized and implements `AsRef<[u8]>`), so the file's bytes are
    // shared with the atlas rather than copied again.
    let blob: egui::epaint::text::Blob = Arc::new(bytes);
    let data = Arc::new(FontData::from_blob(blob, 0));
    defs.font_data.insert(CJK_KEY.to_owned(), data);
    // **Clear, then push** — not append. A list that still holds `Ubuntu-Light` first is a
    // window drawn in the embedded face, which is what the ruling forbids; the system face
    // is the family.
    let mono = first_readable(MONO_CANDIDATES);
    let has_mono = mono.is_some();
    if let Some((_, bytes)) = mono {
        let blob: egui::epaint::text::Blob = Arc::new(bytes);
        defs.font_data
            .insert(MONO_KEY.to_owned(), Arc::new(FontData::from_blob(blob, 0)));
    }
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        let names = defs.families.entry(family.clone()).or_default();
        names.clear();
        // The system's fixed-width face first where there is one, then the system's text
        // face for everything that face does not cover (CJK).
        if family == FontFamily::Monospace && has_mono {
            names.push(MONO_KEY.to_owned());
        }
        names.push(CJK_KEY.to_owned());
    }
    (defs, Some(choice))
}

/// The first candidate that exists and reads as a non-empty file.
fn first_readable(candidates: &[&str]) -> Option<(PathBuf, Vec<u8>)> {
    for candidate in candidates {
        let path = PathBuf::from(candidate);
        match std::fs::read(&path) {
            Ok(bytes) if !bytes.is_empty() => return Some((path, bytes)),
            _ => continue,
        }
    }
    None
}

/// Install the system face on `ctx`. Returns which font was used, if any.
///
/// Call once, before the first frame: `set_fonts` invalidates the atlas, and a call after
/// text has been laid out costs one rebuild.
pub fn install(ctx: &egui::Context) -> Option<FontChoice> {
    let (defs, choice) = definitions_with_cjk();
    ctx.set_fonts(defs);
    choice
}
