//! Typed property values and their canonical literal rendering (SPEC §2–3).
//!
//! `Display` produces the canonical `.rusd` (TOML profile) literal: floats always
//! carry a decimal point, strings are escaped, vectors render as `[a, b, c]`,
//! dicts as sorted inline tables. The M1 parser must round-trip every value this
//! module can print.

use std::collections::BTreeMap;
use std::fmt;

use crate::path::PrimPath;

/// A typed property value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Token(String),
    Path(PrimPath),
    Asset(String),
    Vec2([f64; 2]),
    Vec3([f64; 3]),
    Vec4([f64; 4]),
    Array(Vec<Value>),
    Dict(BTreeMap<String, Value>),
}

impl Value {
    /// The v0 type name, as used in diagnostics and schema declarations.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Bool(_) => "bool",
            Self::Int(_) => "i64",
            Self::Float(_) => "f64",
            Self::String(_) => "string",
            Self::Token(_) => "token",
            Self::Path(_) => "path",
            Self::Asset(_) => "asset",
            Self::Vec2(_) => "vec2",
            Self::Vec3(_) => "vec3",
            Self::Vec4(_) => "vec4",
            Self::Array(_) => "array",
            Self::Dict(_) => "dict",
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bool(b) => write!(f, "{b}"),
            Self::Int(i) => write!(f, "{i}"),
            Self::Float(v) => write!(f, "{}", canonical_float(*v)),
            Self::String(s) => write!(f, "\"{}\"", escape_string(s)),
            Self::Token(t) => write!(f, "{t}"),
            Self::Path(p) => write!(f, "{p}"),
            Self::Asset(a) => write!(f, "@{a}@"),
            Self::Vec2(v) => write_vec(f, v),
            Self::Vec3(v) => write_vec(f, v),
            Self::Vec4(v) => write_vec(f, v),
            Self::Array(items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, "]")
            }
            Self::Dict(entries) => {
                if entries.is_empty() {
                    return write!(f, "{{}}");
                }
                write!(f, "{{ ")?;
                for (i, (k, v)) in entries.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{k} = {v}")?;
                }
                write!(f, " }}")
            }
        }
    }
}

fn write_vec(f: &mut fmt::Formatter<'_>, v: &[f64]) -> fmt::Result {
    write!(f, "[")?;
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            write!(f, ", ")?;
        }
        write!(f, "{}", canonical_float(*x))?;
    }
    write!(f, "]")
}

/// Canonical float form: integral values keep a `.0` so they stay f64 on reparse.
fn canonical_float(v: f64) -> String {
    if v.is_nan() {
        "nan".to_owned()
    } else if v.is_infinite() {
        if v > 0.0 { "inf" } else { "-inf" }.to_owned()
    } else if v == v.trunc() && v.abs() < 1e15 {
        format!("{v:.1}")
    } else {
        format!("{v}")
    }
}

fn escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
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
    out
}
