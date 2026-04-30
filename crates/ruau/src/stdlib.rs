use std::ops::{BitOr, BitOrAssign};

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
    struct StdLibBits: u32 {
        const NONE = 0;
        const COROUTINE = 1;
        const TABLE = 1 << 1;
        const OS = 1 << 3;
        const STRING = 1 << 4;
        const UTF8 = 1 << 5;
        const BIT32 = 1 << 6;
        const MATH = 1 << 7;
        const BUFFER = 1 << 9;
        const VECTOR = 1 << 10;
        const INTEGER = 1 << 11;
        const DEBUG = 1 << 31;
        const ALL_SAFE = Self::COROUTINE.bits()
            | Self::TABLE.bits()
            | Self::OS.bits()
            | Self::STRING.bits()
            | Self::UTF8.bits()
            | Self::BIT32.bits()
            | Self::MATH.bits()
            | Self::BUFFER.bits()
            | Self::VECTOR.bits()
            | Self::INTEGER.bits();
        const ALL = Self::ALL_SAFE.bits() | Self::DEBUG.bits();
    }
}

/// Set of Luau standard libraries to load.
///
/// Combine library constants with bitwise operators:
///
/// ```
/// # use ruau::StdLib;
/// let libs = StdLib::MATH | StdLib::STRING | StdLib::TABLE;
/// ```
///
/// For the default sandbox-friendly set, use [`StdLib::ALL_SAFE`].
#[derive(Copy, Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct StdLib(StdLibBits);

impl StdLib {
    /// No libraries.
    pub const NONE: Self = Self(StdLibBits::NONE);
    /// [`coroutine`](https://luau.org/library#coroutine-library) library.
    pub const COROUTINE: Self = Self(StdLibBits::COROUTINE);
    /// [`table`](https://luau.org/library#table-library) library.
    pub const TABLE: Self = Self(StdLibBits::TABLE);
    /// [`os`](https://luau.org/library#os-library) library.
    ///
    /// Luau's `os` library is the sandboxed subset documented by Luau: `clock`, `date`,
    /// `difftime`, and `time`.
    pub const OS: Self = Self(StdLibBits::OS);
    /// [`string`](https://luau.org/library#string-library) library.
    pub const STRING: Self = Self(StdLibBits::STRING);
    /// [`utf8`](https://luau.org/library#utf8-library) library.
    pub const UTF8: Self = Self(StdLibBits::UTF8);
    /// [`bit32`](https://luau.org/library#bit32-library) library.
    pub const BIT32: Self = Self(StdLibBits::BIT32);
    /// [`math`](https://luau.org/library#math-library) library.
    pub const MATH: Self = Self(StdLibBits::MATH);
    /// [`buffer`](https://luau.org/library#buffer-library) library.
    pub const BUFFER: Self = Self(StdLibBits::BUFFER);
    /// [`vector`](https://luau.org/library#vector-library) library.
    pub const VECTOR: Self = Self(StdLibBits::VECTOR);
    /// [`integer`](https://luau.org/library#integer-library) library.
    pub const INTEGER: Self = Self(StdLibBits::INTEGER);
    /// (**unsafe**) [`debug`](https://luau.org/library#debug-library) library.
    ///
    /// Luau's sandbox documentation treats most debug APIs as isolation-breaking, so this
    /// library is excluded from [`StdLib::ALL_SAFE`].
    pub const DEBUG: Self = Self(StdLibBits::DEBUG);
    /// All standard libraries that are safe in Luau's sandboxed runtime.
    ///
    /// This includes Luau's sandboxed `os` library and excludes the `debug` library.
    pub const ALL_SAFE: Self = Self(StdLibBits::ALL_SAFE);
    /// (**unsafe**) All known standard libraries.
    pub const ALL: Self = Self(StdLibBits::ALL);

    /// Returns `true` if this set includes all libraries from `other`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0.contains(other.0)
    }

    pub(crate) fn insert(&mut self, other: Self) {
        self.0.insert(other.0);
    }
}

impl BitOr for StdLib {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for StdLib {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[cfg(test)]
mod tests {
    use super::StdLib;

    #[test]
    fn all_safe_includes_luau_sandbox_libraries_without_debug() {
        for lib in [
            StdLib::COROUTINE,
            StdLib::TABLE,
            StdLib::OS,
            StdLib::STRING,
            StdLib::UTF8,
            StdLib::BIT32,
            StdLib::MATH,
            StdLib::BUFFER,
            StdLib::VECTOR,
            StdLib::INTEGER,
        ] {
            assert!(StdLib::ALL_SAFE.contains(lib));
        }

        assert!(!StdLib::ALL_SAFE.contains(StdLib::DEBUG));
    }

    #[test]
    fn bitwise_operators_select_libraries() {
        let libs = StdLib::MATH | StdLib::STRING | StdLib::BIT32;

        assert!(libs.contains(StdLib::MATH));
        assert!(libs.contains(StdLib::STRING));
        assert!(libs.contains(StdLib::BIT32));
        assert!(!libs.contains(StdLib::TABLE));
        assert!(StdLib::ALL.contains(StdLib::ALL_SAFE));
        assert!(StdLib::ALL.contains(StdLib::DEBUG));
    }

    #[test]
    fn bitwise_assignment_adds_libraries() {
        let mut libs = StdLib::NONE;
        libs |= StdLib::BUFFER;
        libs |= StdLib::VECTOR;

        assert!(libs.contains(StdLib::BUFFER));
        assert!(libs.contains(StdLib::VECTOR));
        assert!(!libs.contains(StdLib::MATH));
    }
}
