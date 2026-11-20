//! **Compiling a script is the type check** (SPEC D15).
//!
//! A script is a Rust program that calls operator functions. It is compiled against
//! this crate, so the wires it typed are checked by rustc — a `Material` where a
//! `Gprim` was asked is `E0308`/`E0277` at the script's own line, before anything runs.
//! This module is the loop that makes that usable from the tool protocol:
//!
//! - [`Runner::check`] compiles without running, and answers rustc's diagnostics as
//!   [`Diagnostic`]s — the same machine-readable form every other check returns;
//! - [`Runner::run`] compiles and runs, and the script writes its own `.ops.toml` and
//!   `.rusd` files ([`crate::graph::Graph::write`]).
//!
//! The script is built as a tiny crate beside the workspace's `target/`, with the
//! workspace's own `Cargo.lock` copied in and `--offline`: the dependencies are the
//! ones already built, so a warm check is a second or two and nothing is fetched. The
//! binary's source is the script file itself (`[[bin]] path = …`), so every span rustc
//! reports names the file the agent edited, not a copy.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use rusd_core::{Diagnostic, Severity};

/// The workspace a script is compiled against.
pub struct Runner {
    pub repo: PathBuf,
}

/// A compiled script: the executable, and whatever rustc said on the way.
pub struct Compiled {
    pub exe: PathBuf,
    pub diags: Vec<Diagnostic>,
}

/// What running a script answered.
pub struct Run {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
    pub diags: Vec<Diagnostic>,
}

impl Runner {
    pub fn new(repo: PathBuf) -> Self {
        Self { repo }
    }

    /// **Find the workspace a script belongs to**: walk up from the script until a
    /// `Cargo.toml` declares `[workspace]`. A script outside any workspace has nothing
    /// to be compiled against, and that is an error rather than a guess.
    pub fn discover(script: &Path) -> Result<Self, String> {
        let start = script
            .canonicalize()
            .map_err(|e| format!("{}: {e}", script.display()))?;
        let mut dir = start.parent();
        while let Some(d) = dir {
            let manifest = d.join("Cargo.toml");
            if let Ok(text) = std::fs::read_to_string(&manifest)
                && text.contains("[workspace]")
            {
                return Ok(Self::new(d.to_path_buf()));
            }
            dir = d.parent();
        }
        Err(format!("{} is not inside a Cargo workspace", script.display()))
    }

    /// Compile the script without running it. `Ok(diags)` when it compiles (warnings
    /// included); `Err(diags)` when it does not — the errors are the type check.
    pub fn check(&self, script: &Path) -> Result<Vec<Diagnostic>, Vec<Diagnostic>> {
        let dir = self.prepare(script).map_err(|e| vec![compile_error(e)])?;
        let out = self
            .cargo(&dir, "check")
            .output()
            .map_err(|e| vec![compile_error(format!("cargo: {e}"))])?;
        let diags = diagnostics(&out.stdout, script);
        if out.status.success() { Ok(diags) } else { Err(errors_or(diags, &out.stderr, script)) }
    }

    /// Compile the script to an executable.
    pub fn build(&self, script: &Path) -> Result<Compiled, Vec<Diagnostic>> {
        let dir = self.prepare(script).map_err(|e| vec![compile_error(e)])?;
        let out = self
            .cargo(&dir, "build")
            .output()
            .map_err(|e| vec![compile_error(format!("cargo: {e}"))])?;
        let diags = diagnostics(&out.stdout, script);
        if !out.status.success() {
            return Err(errors_or(diags, &out.stderr, script));
        }
        let exe = self
            .repo
            .join("target")
            .join("debug")
            .join(format!("{}{}", bin_name(script), std::env::consts::EXE_SUFFIX));
        Ok(Compiled { exe, diags })
    }

    /// Compile and run. The script's cwd is the workspace root, so a path it writes is
    /// the path the repository knows it by.
    pub fn run(&self, script: &Path, args: &[String]) -> Result<Run, Vec<Diagnostic>> {
        let compiled = self.build(script)?;
        let out = Command::new(&compiled.exe)
            .args(args)
            .current_dir(&self.repo)
            .output()
            .map_err(|e| vec![compile_error(format!("{}: {e}", compiled.exe.display()))])?;
        Ok(Run {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            diags: compiled.diags,
        })
    }

    /// The scratch crate this script is built as: stable per script, so cargo's own
    /// cache makes a second check fast.
    fn crate_dir(&self, script: &Path) -> PathBuf {
        self.repo
            .join("target")
            .join("rusd-script")
            .join(hash_path(script))
    }

    fn prepare(&self, script: &Path) -> Result<PathBuf, String> {
        let script = script
            .canonicalize()
            .map_err(|e| format!("{}: {e}", script.display()))?;
        let dir = self.crate_dir(&script);
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        // The script is the binary's own source, by path — rustc's spans name it.
        let rel = pathdiff(&script, &dir);
        let manifest = format!(
            "[package]\nname = \"{}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
             [[bin]]\nname = \"{}\"\npath = \"{}\"\n\n\
             [dependencies]\nrusd-script = {{ path = \"{}\" }}\n\n\
             [patch.crates-io]\nopenusd = {{ path = \"{}\" }}\n\n\
             [workspace]\n",
            bin_name(&script),
            bin_name(&script),
            rel,
            toml_path(&self.repo.join("crates/rusd-script")),
            toml_path(&self.repo.join("vendor/openusd")),
        );
        std::fs::write(dir.join("Cargo.toml"), manifest)
            .map_err(|e| format!("write the script crate: {e}"))?;
        // The workspace's lock, so the dependency versions are the ones already built.
        let lock = self.repo.join("Cargo.lock");
        if lock.is_file() {
            std::fs::copy(&lock, dir.join("Cargo.lock"))
                .map_err(|e| format!("copy Cargo.lock: {e}"))?;
        }
        Ok(dir)
    }

    /// One cargo invocation, offline, against the workspace's own `target/`.
    fn cargo(&self, dir: &Path, subcommand: &str) -> Command {
        let mut cmd = Command::new(cargo_bin());
        cmd.arg(subcommand)
            .arg("--offline")
            .arg("--message-format=json")
            .arg("--manifest-path")
            .arg(dir.join("Cargo.toml"))
            .arg("--target-dir")
            .arg(self.repo.join("target"))
            .current_dir(dir);
        for (key, value) in msvc_env() {
            cmd.env(key, value);
        }
        cmd
    }
}

/// A path as a TOML basic string: forward slashes, and no Windows verbatim prefix.
/// `canonicalize` answers `\\?\C:\…`, and a backslash in a basic string is an escape.
fn toml_path(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    s.strip_prefix("//?/").unwrap_or(&s).to_owned()
}

/// The binary's name: the script's stem, prefixed so it cannot collide with a crate.
fn bin_name(script: &Path) -> String {
    let stem = script
        .file_stem()
        .map(|s| s.to_string_lossy().replace(['-', '.'], "_"))
        .unwrap_or_else(|| "script".to_owned());
    format!("rusd_script_{stem}")
}

/// A stable, short directory name for a script path.
fn hash_path(script: &Path) -> String {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    script.to_string_lossy().to_lowercase().hash(&mut h);
    format!("{:016x}", h.finish())
}

/// `target` relative to `from`, with forward slashes — a TOML string, not a Windows
/// escape problem.
fn pathdiff(target: &Path, from: &Path) -> String {
    let t: Vec<_> = target.components().collect();
    let f: Vec<_> = from.components().collect();
    let common = t.iter().zip(f.iter()).take_while(|(a, b)| a == b).count();
    let mut out = String::new();
    for _ in common..f.len() {
        out.push_str("../");
    }
    for (i, part) in t[common..].iter().enumerate() {
        if i > 0 {
            out.push('/');
        }
        out.push_str(&part.as_os_str().to_string_lossy());
    }
    out
}

fn cargo_bin() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned())
}

/// **The MSVC environment, when the process does not already have it.** Linking a
/// script needs `link.exe` and the SDK lib paths, which a developer shell sets and a
/// service process may not. Read them once from `vcvars64.bat` and hand them to cargo;
/// nothing here disables a check or guesses a path that is not there.
fn msvc_env() -> &'static [(String, String)] {
    static ENV: OnceLock<Vec<(String, String)>> = OnceLock::new();
    ENV.get_or_init(|| {
        if !cfg!(windows) || std::env::var_os("VCINSTALLDIR").is_some() {
            return Vec::new();
        }
        let Some(vcvars) = find_vcvars() else { return Vec::new() };
        // `raw_arg`: cmd.exe does not understand Rust's `\"` escaping, so the command
        // line after `/C` has to reach it verbatim — the same spelling a developer
        // shell uses.
        let mut command = Command::new("cmd");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.raw_arg(format!("/C \"{}\" >nul && set", vcvars.display()));
        }
        #[cfg(not(windows))]
        {
            let _ = vcvars;
            return Vec::new();
        }
        let Ok(out) = command.output() else { return Vec::new() };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect()
    })
}

fn find_vcvars() -> Option<PathBuf> {
    for edition in ["2022", "2019"] {
        for kind in ["Community", "Professional", "Enterprise", "BuildTools"] {
            let p = PathBuf::from(format!(
                "C:/Program Files/Microsoft Visual Studio/{edition}/{kind}/VC/Auxiliary/Build/vcvars64.bat"
            ));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// rustc's messages, as the tool protocol's diagnostics. The primary span's file and
/// line are the script's own, so an agent can go straight to the wire that is wrong.
fn diagnostics(stdout: &[u8], script: &Path) -> Vec<Diagnostic> {
    let script_name = script.file_name().map(|s| s.to_string_lossy().into_owned());
    let mut out = Vec::new();
    for line in String::from_utf8_lossy(stdout).lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if value.get("reason").and_then(|r| r.as_str()) != Some("compiler-message") {
            continue;
        }
        let Some(message) = value.get("message") else { continue };
        let level = message.get("level").and_then(|l| l.as_str()).unwrap_or("error");
        let text = message.get("message").and_then(|m| m.as_str()).unwrap_or("");
        let rustc = message
            .get("code")
            .and_then(|c| c.get("code"))
            .and_then(|c| c.as_str())
            .unwrap_or("");
        let span = message
            .get("spans")
            .and_then(|s| s.as_array())
            .and_then(|spans| {
                spans
                    .iter()
                    .find(|s| s.get("is_primary").and_then(|p| p.as_bool()).unwrap_or(false))
                    .or_else(|| spans.first())
            });
        let (file, line_no, col) = match span {
            Some(s) => (
                clean_path(s.get("file_name").and_then(|f| f.as_str()).unwrap_or("")),
                s.get("line_start").and_then(|l| l.as_u64()).unwrap_or(0),
                s.get("column_start").and_then(|c| c.as_u64()).unwrap_or(0),
            ),
            None => (String::new(), 0, 0),
        };
        // Only the script's own spans are the script's diagnostics; a dependency's
        // warning is not something the author of this script can act on. A message
        // with no span (a linker error, a missing tool) is about this build, so it stays.
        let own = file.is_empty()
            || script_name.as_deref().map(|n| file.ends_with(n)).unwrap_or(true);
        if !own {
            continue;
        }
        let severity = match level {
            "error" | "error: internal compiler error" => Severity::Error,
            "warning" => Severity::Warning,
            _ => Severity::Note,
        };
        let kind = if rustc.starts_with("E0") { "E_SCRIPT_TYPE" } else { "E_SCRIPT_COMPILE" };
        let code = if severity == Severity::Warning { "W_SCRIPT".to_owned() } else { kind.to_owned() };
        let where_at = if line_no > 0 { format!("{file}:{line_no}:{col}: ") } else { String::new() };
        let rendered = message.get("rendered").and_then(|r| r.as_str()).unwrap_or("");
        let mut diag = Diagnostic::new(severity, code, format!("{where_at}{text}"))
            .with_hint(rendered.trim().to_owned());
        if !rustc.is_empty() && severity == Severity::Error {
            diag = diag.at_property(rustc.to_owned());
        }
        out.push(diag);
    }
    out
}

fn compile_error(message: String) -> Diagnostic {
    Diagnostic::error("E_SCRIPT", message)
}

/// A span's file name as a path a reader can use: no Windows verbatim prefix, forward
/// slashes — the same spelling the repository's own paths have.
fn clean_path(file: &str) -> String {
    let s = file.replace('\\', "/");
    s.strip_prefix("//?/").unwrap_or(&s).to_owned()
}

/// When cargo failed without a compiler message (a resolution problem, a missing
/// tool), its stderr is the diagnostic — never a silent failure.
fn errors_or(mut diags: Vec<Diagnostic>, stderr: &[u8], script: &Path) -> Vec<Diagnostic> {
    if diags.iter().any(|d| d.severity == Severity::Error) {
        return diags;
    }
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines[lines.len().saturating_sub(12)..].join("\n");
    diags.push(
        Diagnostic::error("E_SCRIPT", format!("{} did not compile", script.display()))
            .with_hint(tail),
    );
    diags
}
