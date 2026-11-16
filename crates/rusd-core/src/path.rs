//! Absolute prim paths: the addressing scheme every Rusd edit targets.
//!
//! v0 grammar (SPEC §1): `/` for the root, otherwise `/` followed by one or more
//! `/`-separated components, each matching `[A-Za-z_][A-Za-z0-9_]*`. Deliberately
//! stricter than USD so that diffs, sort order and hash keys stay predictable.

use std::fmt;
use std::str::FromStr;

/// Why a string is not a valid v0 prim path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathError {
    /// The input was empty.
    Empty,
    /// The path did not start with `/`.
    NotAbsolute,
    /// A `//` or a trailing `/` produced an empty component.
    EmptyComponent,
    /// A character outside the component grammar, or a digit leading a component.
    InvalidCharacter {
        /// Byte index of the offending character in the input.
        index: usize,
        /// The offending character.
        ch: char,
    },
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "empty path"),
            Self::NotAbsolute => write!(f, "path must start with '/'"),
            Self::EmptyComponent => write!(f, "empty path component ('//' or trailing '/')"),
            Self::InvalidCharacter { index, ch } => {
                write!(f, "invalid character {ch:?} at byte {index}")
            }
        }
    }
}

impl std::error::Error for PathError {}

/// A validated absolute prim path, e.g. `/World/Forest/i000042`.
///
/// Ordering is byte order of the canonical string, which is also the canonical
/// block order of a `.rusd` file (SPEC §2).
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PrimPath {
    inner: String,
}

impl PrimPath {
    /// The root path `/`.
    pub fn root() -> Self {
        Self {
            inner: "/".to_owned(),
        }
    }

    /// Parse and validate a path string.
    pub fn parse(s: &str) -> Result<Self, PathError> {
        if s.is_empty() {
            return Err(PathError::Empty);
        }
        if !s.starts_with('/') {
            return Err(PathError::NotAbsolute);
        }
        if s.len() == 1 {
            return Ok(Self::root());
        }
        let mut index = 1; // byte index just after the leading '/'
        for comp in s[1..].split('/') {
            validate_component(comp, index)?;
            index += comp.len() + 1;
        }
        Ok(Self {
            inner: s.to_owned(),
        })
    }

    /// The canonical string form.
    pub fn as_str(&self) -> &str {
        &self.inner
    }

    /// Whether this is the root path `/`.
    pub fn is_root(&self) -> bool {
        self.inner == "/"
    }

    /// Number of components; the root has depth 0.
    pub fn depth(&self) -> usize {
        self.components().count()
    }

    /// The last component, or `None` for the root.
    pub fn name(&self) -> Option<&str> {
        if self.is_root() {
            None
        } else {
            self.inner.rsplit('/').next()
        }
    }

    /// The parent path, or `None` for the root.
    pub fn parent(&self) -> Option<Self> {
        if self.is_root() {
            return None;
        }
        let i = self.inner.rfind('/').expect("non-root path has a slash");
        let inner = if i == 0 {
            "/".to_owned()
        } else {
            self.inner[..i].to_owned()
        };
        Some(Self { inner })
    }

    /// Append one component name. The name must be a single valid component.
    pub fn join(&self, name: &str) -> Result<Self, PathError> {
        validate_component(name, 0)?;
        let inner = if self.is_root() {
            format!("/{name}")
        } else {
            format!("{}/{name}", self.inner)
        };
        Ok(Self { inner })
    }

    /// Iterate components; the root yields none.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.inner.split('/').filter(|s| !s.is_empty())
    }

    /// Component-wise prefix test: `/World2` does **not** start with `/World`.
    pub fn starts_with(&self, prefix: &PrimPath) -> bool {
        self.depth() >= prefix.depth()
            && self
                .components()
                .zip(prefix.components())
                .all(|(a, b)| a == b)
    }
}

fn validate_component(comp: &str, base_index: usize) -> Result<(), PathError> {
    if comp.is_empty() {
        return Err(PathError::EmptyComponent);
    }
    for (i, ch) in comp.char_indices() {
        let ok = if i == 0 {
            ch.is_ascii_alphabetic() || ch == '_'
        } else {
            ch.is_ascii_alphanumeric() || ch == '_'
        };
        if !ok {
            return Err(PathError::InvalidCharacter {
                index: base_index + i,
                ch,
            });
        }
    }
    Ok(())
}

impl fmt::Display for PrimPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.inner)
    }
}

impl fmt::Debug for PrimPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.inner)
    }
}

impl FromStr for PrimPath {
    type Err = PathError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root() {
        let r = PrimPath::root();
        assert!(r.is_root());
        assert_eq!(r.depth(), 0);
        assert_eq!(r.name(), None);
        assert_eq!(r.parent(), None);
        assert_eq!(r.to_string(), "/");
        assert_eq!(PrimPath::parse("/").unwrap(), r);
    }

    #[test]
    fn parse_valid() {
        for s in ["/World", "/World/Forest", "/World/Forest/i000042", "/_x/_9"] {
            assert_eq!(PrimPath::parse(s).unwrap().as_str(), s);
        }
    }

    #[test]
    fn parse_rejects() {
        assert_eq!(PrimPath::parse(""), Err(PathError::Empty));
        assert_eq!(PrimPath::parse("World"), Err(PathError::NotAbsolute));
        assert_eq!(
            PrimPath::parse("/World//Forest"),
            Err(PathError::EmptyComponent)
        );
        assert_eq!(PrimPath::parse("/World/"), Err(PathError::EmptyComponent));
        assert_eq!(
            PrimPath::parse("/World/9lives"),
            Err(PathError::InvalidCharacter { index: 7, ch: '9' })
        );
        assert_eq!(
            PrimPath::parse("/World/for-est"),
            Err(PathError::InvalidCharacter { index: 10, ch: '-' })
        );
        assert!(matches!(
            PrimPath::parse("/World/树"),
            Err(PathError::InvalidCharacter { .. })
        ));
    }

    #[test]
    fn parent_chain() {
        let p = PrimPath::parse("/World/Forest/i000042").unwrap();
        assert_eq!(p.name(), Some("i000042"));
        assert_eq!(p.depth(), 3);
        let f = p.parent().unwrap();
        assert_eq!(f.as_str(), "/World/Forest");
        let w = f.parent().unwrap();
        assert_eq!(w.as_str(), "/World");
        assert_eq!(w.parent().unwrap(), PrimPath::root());
    }

    #[test]
    fn join() {
        let r = PrimPath::root();
        assert_eq!(r.join("World").unwrap().as_str(), "/World");
        let w = r.join("World").unwrap();
        assert_eq!(w.join("Forest").unwrap().as_str(), "/World/Forest");
        assert_eq!(w.join(""), Err(PathError::EmptyComponent));
        assert!(matches!(
            w.join("a/b"),
            Err(PathError::InvalidCharacter { .. })
        ));
    }

    #[test]
    fn starts_with_is_component_wise() {
        let world = PrimPath::parse("/World").unwrap();
        let forest = PrimPath::parse("/World/Forest").unwrap();
        let world2 = PrimPath::parse("/World2").unwrap();
        assert!(forest.starts_with(&world));
        assert!(world.starts_with(&world));
        assert!(world.starts_with(&PrimPath::root()));
        assert!(!world2.starts_with(&world));
        assert!(!world.starts_with(&forest));
    }

    #[test]
    fn canonical_order() {
        let mut paths: Vec<PrimPath> = ["/World2", "/World/Forest", "/World", "/A"]
            .iter()
            .map(|s| PrimPath::parse(s).unwrap())
            .collect();
        paths.sort();
        let strs: Vec<&str> = paths.iter().map(|p| p.as_str()).collect();
        assert_eq!(strs, ["/A", "/World", "/World/Forest", "/World2"]);
    }

    #[test]
    fn display_roundtrip() {
        let p = PrimPath::parse("/World/Forest/i000042").unwrap();
        assert_eq!(PrimPath::parse(&p.to_string()).unwrap(), p);
        assert_eq!(format!("{p:?}"), "/World/Forest/i000042");
    }
}
