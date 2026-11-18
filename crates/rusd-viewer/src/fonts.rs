//! System-font loading, so the UI's Chinese labels are glyphs instead of tofu.
//!
//! egui ships one Latin face (`Ubuntu-Light` / `Hack`), which has no CJK
//! coverage: every `中文` in the status bar drew as a hollow box until this
//! module handed the context a system font. The rule here is **append, never
//! replace**: the built-in faces stay first in every family, so ASCII keeps the
//! crisp face it had, and only characters the built-in face lacks (CJK, and the
//! mathematical glyphs a status line uses) fall through to the system font.
//!
//! Sources, in order (the order `C:\Windows\Fonts` actually offers on a Chinese
//! Windows install):
//!
//! 1. `msyh.ttc` — Microsoft YaHei, the UI font of Windows itself;
//! 2. `simsun.ttc` — SimSun, the older fallback that is nearly always present;
//! 3. `simhei.ttf` — SimHei, a plain black sans;
//! 4. a few POSIX paths, so the same binary is legible off Windows;
//! 5. nothing: the definitions come back untouched and the UI draws tofu, which
//!    is a cosmetic loss and not a reason to refuse to start.
//!
//! A `.ttc` is a *collection*: the first face (`index 0`) of YaHei is the regular
//! weight, which is what a status bar wants.

use std::path::PathBuf;
use std::sync::Arc;

use egui::FontDefinitions;
use egui::epaint::text::{FontData, FontFamily};

/// The key the loaded face is registered under in [`FontDefinitions::font_data`].
pub const CJK_KEY: &str = "rusd-cjk";

/// Where to look for a CJK face, best first.
///
/// Windows paths are absolute and fixed; the POSIX entries are best-effort so a
/// Linux build is not silent tofu. Nothing here is required to exist.
pub const CJK_CANDIDATES: &[&str] = &[
    r"C:\Windows\Fonts\msyh.ttc",
    r"C:\Windows\Fonts\simsun.ttc",
    r"C:\Windows\Fonts\simhei.ttf",
    "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/System/Library/Fonts/PingFang.ttc",
];

/// Which face was loaded, and how big it was — the report a caller can log and a
/// test can assert on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontChoice {
    /// The file the bytes came from.
    pub path: PathBuf,
    /// Its size in bytes (never zero: an empty read is a failed read).
    pub bytes: usize,
}

/// The font definitions with a system CJK face appended to both families.
///
/// `None` when no candidate file could be read; the definitions returned are then
/// exactly [`FontDefinitions::default`], so a caller can apply them blindly.
pub fn definitions_with_cjk() -> (FontDefinitions, Option<FontChoice>) {
    let mut defs = FontDefinitions::default();
    let Some((path, bytes)) = first_readable() else {
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
    // Appended, not inserted at 0: the built-in faces keep ASCII, and the system
    // font answers only the characters they do not have.
    //
    // The `entry(..).or_insert_with` matters beyond tidiness: egui compiles its
    // bundled faces in only with the `default_fonts` feature. Should that feature
    // ever be off, every family would be an *empty* list, and `push` alone would
    // leave the CJK face as the sole
    // primary — the opposite of this module's promise. This way the built-in names
    // are put back first when they are missing.
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        let names = defs.families.entry(family.clone()).or_default();
        if names.is_empty() {
            *names = builtin_family(family);
        }
        names.push(CJK_KEY.to_owned());
    }
    (defs, Some(choice))
}

/// The bundled faces egui would have registered for `family` with its
/// `default_fonts` feature on, so a build without it still has a Latin face
/// before the CJK one.
fn builtin_family(family: FontFamily) -> Vec<String> {
    match family {
        FontFamily::Proportional => vec!["Ubuntu-Light".to_owned()],
        FontFamily::Monospace => vec!["Hack".to_owned(), "Ubuntu-Light".to_owned()],
        FontFamily::Name(_) => Vec::new(),
    }
}

/// The first candidate that exists and reads as a non-empty file.
fn first_readable() -> Option<(PathBuf, Vec<u8>)> {
    for candidate in CJK_CANDIDATES {
        let path = PathBuf::from(candidate);
        match std::fs::read(&path) {
            Ok(bytes) if !bytes.is_empty() => return Some((path, bytes)),
            _ => continue,
        }
    }
    None
}

/// Install the CJK face on `ctx`. Returns which font was used, if any.
///
/// Call once, before the first frame: `set_fonts` invalidates the atlas, and a
/// call after text has been laid out costs one rebuild.
pub fn install(ctx: &egui::Context) -> Option<FontChoice> {
    let (defs, choice) = definitions_with_cjk();
    ctx.set_fonts(defs);
    choice
}
