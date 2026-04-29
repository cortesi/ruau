use std::ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign, BitXor, BitXorAssign};

/// Flags describing the set of Luau standard libraries to load.
#[derive(Copy, Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct StdLib(u32);

impl StdLib {
    /// [`coroutine`](https://luau.org/library#coroutine-library) library
    pub const COROUTINE: Self = Self(1);

    /// [`table`](https://luau.org/library#table-library) library
    pub const TABLE: Self = Self(1 << 1);

    /// [`os`](https://luau.org/library#os-library) library
    ///
    /// Luau's `os` library is the sandboxed subset documented by Luau: `clock`, `date`,
    /// `difftime`, and `time`.
    pub const OS: Self = Self(1 << 3);

    /// [`string`](https://luau.org/library#string-library) library
    pub const STRING: Self = Self(1 << 4);

    /// [`utf8`](https://luau.org/library#utf8-library) library
    pub const UTF8: Self = Self(1 << 5);

    /// [`bit32`](https://luau.org/library#bit32-library) library
    pub const BIT: Self = Self(1 << 6);

    /// [`math`](https://luau.org/library#math-library) library
    pub const MATH: Self = Self(1 << 7);

    /// [`buffer`](https://luau.org/library#buffer-library) library
    pub const BUFFER: Self = Self(1 << 9);

    /// [`vector`](https://luau.org/library#vector-library) library
    pub const VECTOR: Self = Self(1 << 10);

    /// [`integer`](https://luau.org/library#integer-library) library
    pub const INTEGER: Self = Self(1 << 11);

    /// (**unsafe**) [`debug`](https://luau.org/library#debug-library) library
    ///
    /// Luau's sandbox documentation treats most debug APIs as isolation-breaking, so this library
    /// is excluded from [`StdLib::ALL_SAFE`].
    pub const DEBUG: Self = Self(1 << 31);

    /// No libraries.
    pub const NONE: Self = Self(0);
    /// (**unsafe**) All standard libraries.
    pub const ALL: Self = Self(u32::MAX);
    /// All standard libraries that are safe in Luau's sandboxed runtime.
    ///
    /// This includes Luau's sandboxed `os` library and excludes the `debug` library.
    pub const ALL_SAFE: Self = Self(
        Self::COROUTINE.0
            | Self::TABLE.0
            | Self::OS.0
            | Self::STRING.0
            | Self::UTF8.0
            | Self::BIT.0
            | Self::MATH.0
            | Self::BUFFER.0
            | Self::VECTOR.0
            | Self::INTEGER.0,
    );

    /// Returns `true` if this set includes `lib`.
    pub fn contains(self, lib: Self) -> bool {
        (self & lib).0 != 0
    }
}

impl BitAnd for StdLib {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self::Output {
        Self(self.0 & rhs.0)
    }
}

impl BitAndAssign for StdLib {
    fn bitand_assign(&mut self, rhs: Self) {
        *self = Self(self.0 & rhs.0)
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
        *self = Self(self.0 | rhs.0)
    }
}

impl BitXor for StdLib {
    type Output = Self;
    fn bitxor(self, rhs: Self) -> Self::Output {
        Self(self.0 ^ rhs.0)
    }
}

impl BitXorAssign for StdLib {
    fn bitxor_assign(&mut self, rhs: Self) {
        *self = Self(self.0 ^ rhs.0)
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
            StdLib::BIT,
            StdLib::MATH,
            StdLib::BUFFER,
            StdLib::VECTOR,
            StdLib::INTEGER,
        ] {
            assert!(StdLib::ALL_SAFE.contains(lib));
        }

        assert!(!StdLib::ALL_SAFE.contains(StdLib::DEBUG));
    }
}
