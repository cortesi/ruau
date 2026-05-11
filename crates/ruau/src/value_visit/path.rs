//! Traversal path formatting.

use std::fmt;

/// A path to a value while traversing a host boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValuePath {
    /// Root label, such as `value` or `argument 1`.
    root: String,
    /// Child path segments from root to current value.
    segments: Vec<ValuePathSegment>,
}

impl ValuePath {
    /// Creates a path with a caller-defined root label.
    #[must_use]
    pub fn new(root: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            segments: Vec::new(),
        }
    }

    /// Creates a path rooted at `value`.
    #[must_use]
    pub fn value() -> Self {
        Self::new("value")
    }

    /// Creates a path rooted at a 1-based argument position.
    #[must_use]
    pub fn argument(position: usize) -> Self {
        Self::new(format!("argument {position}"))
    }

    /// Returns a copy of this path with an array index appended.
    #[must_use]
    pub fn indexed(&self, index: usize) -> Self {
        let mut path = self.clone();
        path.push_index(index);
        path
    }

    /// Returns a copy of this path with a map field appended.
    #[must_use]
    pub fn field(&self, field: impl Into<String>) -> Self {
        let mut path = self.clone();
        path.push_field(field);
        path
    }

    /// Appends an array index in place.
    fn push_index(&mut self, index: usize) {
        self.segments.push(ValuePathSegment::Index(index));
    }

    /// Appends a map field in place.
    fn push_field(&mut self, field: impl Into<String>) {
        self.segments.push(ValuePathSegment::Field(field.into()));
    }

    /// Returns the number of child segments from the root to this path.
    pub(super) fn depth(&self) -> usize {
        self.segments.len()
    }
}

impl Default for ValuePath {
    fn default() -> Self {
        Self::value()
    }
}

impl fmt::Display for ValuePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.root)?;
        for segment in &self.segments {
            match segment {
                ValuePathSegment::Index(index) => write!(formatter, "[{index}]")?,
                ValuePathSegment::Field(field) if is_luau_identifier(field) => {
                    write!(formatter, ".{field}")?;
                }
                ValuePathSegment::Field(field) => write!(formatter, "[{field:?}]")?,
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Segment in a [`ValuePath`].
enum ValuePathSegment {
    /// One-based array index.
    Index(usize),
    /// String map key.
    Field(String),
}

/// Returns whether `value` can be displayed as a Luau dotted field.
fn is_luau_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|char| char == '_' || char.is_ascii_alphanumeric())
}
