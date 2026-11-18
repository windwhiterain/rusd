//! Machine-readable diagnostics: the feedback half of the agent loop (SPEC §3).
//!
//! Every validation, expansion or render problem is reported as a `Diagnostic`
//! with a stable `code`, never as a panic. `to_json` renders the wire form the
//! agent protocol returns; key order is fixed so outputs diff cleanly.

use std::fmt;

/// How much a diagnostic matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Note,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Note => "note",
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One locatable, machine-readable problem report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// Stable identifier, `E_` / `W_` / `N_` prefixed; never reused once shipped.
    pub code: String,
    pub severity: Severity,
    /// Prim path the problem is about, when known.
    pub prim: Option<String>,
    /// Property name within that prim, when known.
    pub property: Option<String>,
    pub message: String,
    /// A concrete suggestion an agent can act on.
    pub hint: Option<String>,
}

impl Diagnostic {
    pub fn new(severity: Severity, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            severity,
            prim: None,
            property: None,
            message: message.into(),
            hint: None,
        }
    }

    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(Severity::Error, code, message)
    }

    pub fn warning(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(Severity::Warning, code, message)
    }

    pub fn at_prim(mut self, prim: impl Into<String>) -> Self {
        self.prim = Some(prim.into());
        self
    }

    pub fn at_property(mut self, property: impl Into<String>) -> Self {
        self.property = Some(property.into());
        self
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// The wire form returned by the agent protocol. Optional fields are omitted
    /// when absent; key order is fixed.
    pub fn to_json(&self) -> String {
        let mut out = String::from("{");
        out.push_str(&format!("\"code\":{}", json_string(&self.code)));
        out.push_str(&format!(
            ",\"severity\":{}",
            json_string(self.severity.as_str())
        ));
        if let Some(prim) = &self.prim {
            out.push_str(&format!(",\"prim\":{}", json_string(prim)));
        }
        if let Some(property) = &self.property {
            out.push_str(&format!(",\"property\":{}", json_string(property)));
        }
        out.push_str(&format!(",\"message\":{}", json_string(&self.message)));
        if let Some(hint) = &self.hint {
            out.push_str(&format!(",\"hint\":{}", json_string(hint)));
        }
        out.push('}');
        out
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)?;
        if let Some(prim) = &self.prim {
            write!(f, " (at {prim}")?;
            if let Some(property) = &self.property {
                write!(f, ".{property}")?;
            }
            write!(f, ")")?;
        }
        Ok(())
    }
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
