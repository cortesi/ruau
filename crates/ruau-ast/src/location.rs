//! Source locations for Luau syntax.

use std::ops::Range;

/// A zero-based source position.
///
/// This mirrors upstream Luau's `Position` value from `Ast/include/Luau/Location.h`.
#[derive(
    Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Deserialize, serde::Serialize,
)]
pub struct Position {
    /// Zero-based line number.
    pub line: u32,
    /// Zero-based column number.
    pub column: u32,
}

impl Position {
    /// Creates a source position.
    #[must_use]
    pub const fn new(line: u32, column: u32) -> Self {
        Self { line, column }
    }

    /// Returns upstream's sentinel missing position.
    #[must_use]
    pub const fn missing() -> Self {
        Self::new(u32::MAX, u32::MAX)
    }

    /// Returns whether this position is not the missing-position sentinel.
    ///
    /// Upstream uses `line != UINT_MAX || column != UINT_MAX`; Ruau keeps that
    /// exact predicate.
    #[must_use]
    pub const fn has_value(self) -> bool {
        self.line != u32::MAX || self.column != u32::MAX
    }

    /// Converts this upstream byte-column position into an absolute byte
    /// offset in `source`.
    ///
    /// Luau columns count bytes, not Unicode scalar values. Returns `None` for
    /// missing positions, lines outside the source, and columns past the line's
    /// end position.
    #[must_use]
    pub fn byte_offset(self, source: impl AsRef<[u8]>) -> Option<usize> {
        let source = source.as_ref();
        let line_starts = source_line_starts(source);
        position_to_offset(self, source, &line_starts)
    }
}

/// A half-open source range.
///
/// This mirrors upstream Luau's `Location` value from `Ast/include/Luau/Location.h`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Deserialize)]
pub struct Location {
    /// First position covered by the range.
    pub begin: Position,
    /// First position after the range.
    pub end: Position,
}

impl Location {
    /// Creates a source range from explicit begin and end positions.
    #[must_use]
    pub const fn new(begin: Position, end: Position) -> Self {
        Self { begin, end }
    }

    /// Returns whether this range fully encloses another range.
    #[must_use]
    pub fn encloses(self, other: Self) -> bool {
        self.begin <= other.begin && self.end >= other.end
    }

    /// Returns whether this range overlaps another range.
    #[must_use]
    pub fn overlaps(self, other: Self) -> bool {
        (self.begin <= other.begin && self.end >= other.begin)
            || (self.begin <= other.end && self.end >= other.end)
            || (self.begin >= other.begin && self.end <= other.end)
    }

    /// Returns whether this range contains a position using a half-open end.
    #[must_use]
    pub fn contains(self, position: Position) -> bool {
        self.begin <= position && position < self.end
    }

    /// Converts this half-open source location into an absolute byte range in
    /// `source`.
    ///
    /// Luau locations use zero-based byte columns. The returned range is
    /// therefore suitable for slicing the same UTF-8 source string with
    /// `source.as_bytes()[range]` or `source[range]` when both endpoints are
    /// UTF-8 boundaries. Returns `None` when either endpoint is outside the
    /// source or the location is reversed.
    #[must_use]
    pub fn byte_range(self, source: impl AsRef<[u8]>) -> Option<Range<usize>> {
        let source = source.as_ref();
        let line_starts = source_line_starts(source);
        let begin = position_to_offset(self.begin, source, &line_starts)?;
        let end = position_to_offset(self.end, source, &line_starts)?;
        (begin <= end).then_some(begin..end)
    }

    /// Extends this range to include another range.
    pub fn extend(&mut self, other: Self) {
        if other.begin < self.begin {
            self.begin = other.begin;
        }

        if other.end > self.end {
            self.end = other.end;
        }
    }

    /// Formats this range as upstream AST JSON text.
    #[must_use]
    pub fn to_upstream_string(self) -> String {
        format!(
            "{},{} - {},{}",
            self.begin.line, self.begin.column, self.end.line, self.end.column
        )
    }
}

/// Returns absolute byte offsets for the start of each source line.
fn source_line_starts(source: &[u8]) -> Vec<usize> {
    let mut starts = vec![0];
    for (index, byte) in source.iter().enumerate() {
        if *byte == b'\n' {
            starts.push(index + 1);
        }
    }
    starts
}

/// Converts an upstream byte-column position into an absolute byte offset.
fn position_to_offset(position: Position, source: &[u8], line_starts: &[usize]) -> Option<usize> {
    if !position.has_value() {
        return None;
    }
    let line = usize::try_from(position.line).ok()?;
    let column = usize::try_from(position.column).ok()?;
    let line_start = *line_starts.get(line)?;
    let offset = line_start.checked_add(column)?;
    let line_end = line_starts
        .get(line + 1)
        .map_or(source.len(), |next_start| next_start.saturating_sub(1));
    (offset <= line_end).then_some(offset)
}

impl Default for Location {
    fn default() -> Self {
        Self::new(Position::new(0, 0), Position::new(0, 0))
    }
}

impl serde::Serialize for Location {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_upstream_string())
    }
}

#[cfg(any())]
mod tests {
    use super::{Location, Position};

    #[test]
    fn missing_position_matches_upstream_sentinel() {
        assert!(!Position::missing().has_value());
        assert!(Position::new(u32::MAX, 0).has_value());
    }

    #[test]
    fn position_ordering_is_line_then_column() {
        assert!(Position::new(1, 0) > Position::new(0, 100));
        assert!(Position::new(1, 2) > Position::new(1, 1));
    }

    #[test]
    fn location_queries_match_upstream_shape() {
        let outer = Location::new(Position::new(1, 0), Position::new(3, 5));
        let inner = Location::new(Position::new(1, 2), Position::new(2, 4));
        let adjacent = Location::new(Position::new(3, 5), Position::new(4, 0));

        assert!(outer.encloses(inner));
        assert!(outer.overlaps(inner));
        assert!(outer.contains(Position::new(2, 0)));
        assert!(!outer.contains(Position::new(3, 5)));
        assert!(outer.overlaps(adjacent));
    }

    #[test]
    fn position_byte_offset_uses_luau_byte_columns() {
        let source = "abé\nxyz";

        assert_eq!(Position::new(0, 0).byte_offset(source), Some(0));
        assert_eq!(Position::new(0, 2).byte_offset(source), Some(2));
        assert_eq!(Position::new(0, 4).byte_offset(source), Some(4));
        assert_eq!(Position::new(1, 2).byte_offset(source), Some(7));
        assert_eq!(Position::new(1, 3).byte_offset(source), Some(8));
    }

    #[test]
    fn location_byte_range_spans_lines_and_rejects_invalid_positions() {
        let source = "abé\nxyz";

        let location = Location::new(Position::new(0, 2), Position::new(1, 2));
        let range = location.byte_range(source).expect("valid range");
        assert_eq!(range, 2..7);
        assert_eq!(&source[range], "é\nxy");

        assert_eq!(Position::missing().byte_offset(source), None);
        assert_eq!(Position::new(2, 0).byte_offset(source), None);
        assert_eq!(Position::new(0, 5).byte_offset(source), None);
        assert_eq!(
            Location::new(Position::new(1, 1), Position::new(0, 1)).byte_range(source),
            None
        );
    }

    #[test]
    fn extend_keeps_earliest_begin_and_latest_end() {
        let mut location = Location::new(Position::new(2, 5), Position::new(2, 8));

        location.extend(Location::new(Position::new(1, 4), Position::new(3, 0)));

        assert_eq!(location.begin, Position::new(1, 4));
        assert_eq!(location.end, Position::new(3, 0));
    }
}
