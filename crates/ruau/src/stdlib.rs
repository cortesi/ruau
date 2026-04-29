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
/// Start with [`StdLib::empty`] and add the libraries your VM needs:
///
/// ```
/// # use ruau::StdLib;
/// let libs = StdLib::empty().math().string().table();
/// ```
///
/// For the default sandbox-friendly set, use [`StdLib::ALL_SAFE`] or [`StdLib::all_safe`].
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

    /// Creates an empty library set.
    #[must_use]
    pub const fn empty() -> Self {
        Self::NONE
    }

    /// Creates the default sandbox-friendly library set.
    #[must_use]
    pub const fn all_safe() -> Self {
        Self::ALL_SAFE
    }

    /// Creates a set containing every known library, including unsafe libraries.
    #[must_use]
    pub const fn all() -> Self {
        Self::ALL
    }

    /// Returns `true` if this set includes all libraries from `other`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0.contains(other.0)
    }

    /// Adds the `coroutine` library.
    #[must_use]
    pub const fn coroutine(self) -> Self {
        self.with(Self::COROUTINE)
    }

    /// Adds the `table` library.
    #[must_use]
    pub const fn table(self) -> Self {
        self.with(Self::TABLE)
    }

    /// Adds the sandboxed `os` library.
    #[must_use]
    pub const fn os(self) -> Self {
        self.with(Self::OS)
    }

    /// Adds the `string` library.
    #[must_use]
    pub const fn string(self) -> Self {
        self.with(Self::STRING)
    }

    /// Adds the `utf8` library.
    #[must_use]
    pub const fn utf8(self) -> Self {
        self.with(Self::UTF8)
    }

    /// Adds the `bit32` library.
    #[must_use]
    pub const fn bit32(self) -> Self {
        self.with(Self::BIT32)
    }

    /// Adds the `math` library.
    #[must_use]
    pub const fn math(self) -> Self {
        self.with(Self::MATH)
    }

    /// Adds the `buffer` library.
    #[must_use]
    pub const fn buffer(self) -> Self {
        self.with(Self::BUFFER)
    }

    /// Adds the `vector` library.
    #[must_use]
    pub const fn vector(self) -> Self {
        self.with(Self::VECTOR)
    }

    /// Adds the `integer` library.
    #[must_use]
    pub const fn integer(self) -> Self {
        self.with(Self::INTEGER)
    }

    /// Adds the unsafe `debug` library.
    #[must_use]
    pub const fn debug(self) -> Self {
        self.with(Self::DEBUG)
    }

    #[must_use]
    pub(crate) const fn with(self, other: Self) -> Self {
        Self(self.0.union(other.0))
    }

    pub(crate) fn insert(&mut self, other: Self) {
        self.0.insert(other.0);
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
    fn builder_methods_select_libraries() {
        let libs = StdLib::empty().math().string().bit32();

        assert!(libs.contains(StdLib::MATH));
        assert!(libs.contains(StdLib::STRING));
        assert!(libs.contains(StdLib::BIT32));
        assert!(!libs.contains(StdLib::TABLE));
        assert_eq!(StdLib::all_safe(), StdLib::ALL_SAFE);
        assert!(StdLib::all().contains(StdLib::ALL_SAFE));
        assert!(StdLib::all().contains(StdLib::DEBUG));
    }
}
