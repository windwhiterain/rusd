//! Startup milestones: what the viewer had finished, and when.
//!
//! The window's first seconds are the one part of this crate that no test could
//! see. A viewer that never shows a window prints `N_FONT` and then nothing —
//! every step after the font is silent, so "it stalled" was the whole of the
//! evidence, and three rounds of it were spent guessing which step.
//!
//! This module makes that sequence observable without a screenshot:
//!
//! - [`Startup`] records a named [`Mark`] with the milliseconds since the process
//!   started. The marks are a fixed, documented list ([`WINDOW_CREATED`],
//!   [`SHELL_BUILT`], [`FIRST_PAINT`], …), so a test asserts "startup reached
//!   `SHELL_BUILT`" instead of scanning log text.
//! - With `RUSD_VIEWER_STARTUP_LOG` set to a path, every mark is also appended to
//!   that file **as it happens**, flushed per line. That is the channel that
//!   survives a run whose stdout/stderr nobody drains, and whose window is on
//!   another Windows session: the file says how far startup got.
//! - [`assemble`] is the headless half of [`crate::app::Shell::new`]: everything
//!   the shell builds *except* the window and the GPU surface, which need a
//!   display. A test drives it against a mock server, so the network side of
//!   startup is checked with no window, no GPU and no human
//!   (`tests/startup.rs`).
//!
//! The invariant these marks exist to protect: **startup never waits on the
//! server.** A quiet server, a server that greets and says nothing else, and a
//! server that never answers at all must all leave the window built and painted;
//! only the picture waits.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use rusd_core::Diagnostic;

use crate::net::{NetWorker, ProtocolClient};
use crate::session::ClientModel;

/// The winit shell exists and `Shell::new` has returned.
pub const SHELL_BUILT: &str = "shell_built";
/// `winit` handed us an event loop (the step that aborts on a session with no
/// window station, which is the one failure no `Result` can report).
pub const EVENT_LOOP: &str = "event_loop";
/// The window object exists. Everything before this is "no window at all".
pub const WINDOW_CREATED: &str = "window_created";
/// `egui_wgpu::winit::Painter::new` returned: a wgpu instance, adapter and device.
pub const PAINTER_READY: &str = "painter_ready";
/// The painter has a configured surface for the window.
pub const SURFACE_READY: &str = "surface_ready";
/// The render worker thread was started (it owns the viewport on egui's device).
pub const RENDER_WORKER: &str = "render_worker";
/// The network worker thread was started. **Not** "the server answered": the
/// greeting happens on that thread, and startup does not wait for it.
pub const NET_WORKER: &str = "net_worker";
/// The first frame was drawn into the window.
pub const FIRST_PAINT: &str = "first_paint";
/// The first `stage_snapshot` produced a scene. Arrives whenever the server gets
/// round to it; startup must not be waiting on it.
pub const FIRST_SNAPSHOT: &str = "first_snapshot";

/// One milestone, in milliseconds since the process started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mark {
    pub name: &'static str,
    pub at_ms: u128,
}

/// The marks a startup produced, and the file the env var asked them to be
/// appended to.
#[derive(Debug)]
pub struct Startup {
    started: Instant,
    marks: Vec<Mark>,
    sink: Option<PathBuf>,
}

impl Startup {
    /// A recorder that writes to no file.
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            marks: Vec::new(),
            sink: None,
        }
    }

    /// A recorder that appends every mark to `path` as it happens.
    pub fn to_file(path: impl Into<PathBuf>) -> Self {
        Self {
            started: Instant::now(),
            marks: Vec::new(),
            sink: Some(path.into()),
        }
    }

    /// A recorder for the binary: a file when `RUSD_VIEWER_STARTUP_LOG` names one,
    /// nothing otherwise.
    ///
    /// This is the diagnostic a run on a machine with no console uses: the file
    /// says how far startup got, where stderr says only what happened to be
    /// flushed.
    pub fn from_env() -> Self {
        match std::env::var_os("RUSD_VIEWER_STARTUP_LOG") {
            Some(path) => Self::to_file(PathBuf::from(path)),
            None => Self::new(),
        }
    }

    /// Record `name`. Called at most once per milestone; a repeat is kept, because
    /// "painted twice" is a fact worth having.
    pub fn mark(&mut self, name: &'static str) {
        let at_ms = self.started.elapsed().as_millis();
        if let Some(path) = &self.sink {
            // Append and flush per line: the whole point is that the file is
            // readable *while* a startup is stuck, and after a kill that left no
            // stderr behind.
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = writeln!(f, "{name} at {at_ms}ms");
                let _ = f.flush();
            }
        }
        self.marks.push(Mark { name, at_ms });
    }

    /// Every mark so far, oldest first.
    pub fn marks(&self) -> &[Mark] {
        &self.marks
    }

    /// Whether `name` was reached.
    pub fn reached(&self, name: &str) -> bool {
        self.marks.iter().any(|m| m.name == name)
    }

    /// When `name` was reached, if it was.
    pub fn at(&self, name: &str) -> Option<Duration> {
        self.marks
            .iter()
            .find(|m| m.name == name)
            .map(|m| Duration::from_millis(m.at_ms as u64))
    }

    /// How long ago this recorder was created.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

impl Default for Startup {
    fn default() -> Self {
        Self::new()
    }
}

/// The shell's state that startup builds **without a window**: the client model
/// and the network worker.
///
/// [`crate::app::Shell::new`] builds these in the same order, on the same calls,
/// so what this exercises is the real thing and not a rehearsal of it.
pub struct Assembled {
    /// The marks this assembly produced (it shares the caller's recorder).
    pub net: NetWorker,
    pub model: ClientModel,
}

/// Build what startup builds, headless: a [`ClientModel`] and a [`NetWorker`],
/// marking [`NET_WORKER`] and [`SHELL_BUILT`].
///
/// `connect` is the server address. Both steps are non-blocking by construction —
/// the connection and its greeting run on the worker thread — which is exactly
/// what the marker order asserts and what a blocking startup would break.
pub fn assemble(connect: &str, startup: &mut Startup) -> Assembled {
    let model = ClientModel::new();
    let net = NetWorker::spawn(connect.to_owned(), Box::new(|| {}));
    startup.mark(NET_WORKER);
    // The window, in the real shell, is between these two: `Shell::new` returns
    // after the workers are up. Headless, "the shell is built" is the same fact.
    startup.mark(SHELL_BUILT);
    Assembled { net, model }
}

/// The deadline a greeting is given before the viewer gives up on a server that
/// will not answer.
///
/// Not a network timeout in disguise: a server that accepted the connection and
/// then says nothing is a server with a problem, and waiting forever for it is
/// how a client hangs with no diagnostic. Five seconds is far longer than a city
/// snapshot takes (~0.2 s of cook, measured) and short enough that a user sees
/// the fallback scene rather than a blank window.
pub const GREETING_DEADLINE: Duration = Duration::from_secs(5);

/// The deadline one request gets for its reply.
///
/// A `composition_tree` on city is a structural read of the authority stack; the
/// panel degrades when it fails, so a slow answer must end as one diagnostic, not
/// as a network thread parked forever.
pub const REPLY_DEADLINE: Duration = Duration::from_secs(10);

/// Connect and greet with a deadline, so a silent peer ends in a diagnostic.
pub fn open_within(
    addr: &str,
    deadline: Duration,
) -> Result<(ProtocolClient, Vec<Notification>), Vec<Diagnostic>> {
    ProtocolClient::open_within(addr, deadline)
}

use crate::protocol::Notification;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mark_keeps_its_order_and_name() {
        let mut s = Startup::new();
        s.mark(EVENT_LOOP);
        s.mark(SHELL_BUILT);
        assert!(s.reached(SHELL_BUILT));
        assert!(!s.reached(FIRST_PAINT));
        assert_eq!(s.marks().len(), 2);
        assert!(s.at(EVENT_LOOP).expect("marked") <= s.at(SHELL_BUILT).expect("marked"));
    }

    #[test]
    fn a_file_sink_has_the_marks_while_the_process_is_still_running() {
        let dir = std::env::temp_dir().join(format!("rusd-startup-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("marks.log");
        let _ = std::fs::remove_file(&path);
        let mut s = Startup::to_file(&path);
        s.mark(EVENT_LOOP);
        let first = std::fs::read_to_string(&path).expect("the file exists already");
        assert!(first.contains(EVENT_LOOP), "{first}");
        s.mark(SHELL_BUILT);
        let both = std::fs::read_to_string(&path).expect("still there");
        assert!(both.contains(SHELL_BUILT), "{both}");
        assert!(both.lines().count() >= 2, "{both}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
