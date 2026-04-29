bitflags::bitflags! {
    /// Flags describing the set of Luau standard libraries to load.
    #[derive(Copy, Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
    pub struct StdLib: u32 {
        /// No libraries.
        const NONE = 0;
        /// [`coroutine`](https://luau.org/library#coroutine-library) library
        const COROUTINE = 1;
        /// [`table`](https://luau.org/library#table-library) library
        const TABLE = 1 << 1;
        /// [`os`](https://luau.org/library#os-library) library
        ///
        /// Luau's `os` library is the sandboxed subset documented by Luau: `clock`, `date`,
        /// `difftime`, and `time`.
        const OS = 1 << 3;
        /// [`string`](https://luau.org/library#string-library) library
        const STRING = 1 << 4;
        /// [`utf8`](https://luau.org/library#utf8-library) library
        const UTF8 = 1 << 5;
        /// [`bit32`](https://luau.org/library#bit32-library) library
        const BIT = 1 << 6;
        /// [`math`](https://luau.org/library#math-library) library
        const MATH = 1 << 7;
        /// [`buffer`](https://luau.org/library#buffer-library) library
        const BUFFER = 1 << 9;
        /// [`vector`](https://luau.org/library#vector-library) library
        const VECTOR = 1 << 10;
        /// [`integer`](https://luau.org/library#integer-library) library
        const INTEGER = 1 << 11;
        /// (**unsafe**) [`debug`](https://luau.org/library#debug-library) library
        ///
        /// Luau's sandbox documentation treats most debug APIs as isolation-breaking, so this
        /// library is excluded from [`StdLib::ALL_SAFE`].
        const DEBUG = 1 << 31;
        /// All standard libraries that are safe in Luau's sandboxed runtime.
        ///
        /// This includes Luau's sandboxed `os` library and excludes the `debug` library.
        const ALL_SAFE = Self::COROUTINE.bits()
            | Self::TABLE.bits()
            | Self::OS.bits()
            | Self::STRING.bits()
            | Self::UTF8.bits()
            | Self::BIT.bits()
            | Self::MATH.bits()
            | Self::BUFFER.bits()
            | Self::VECTOR.bits()
            | Self::INTEGER.bits();
        /// (**unsafe**) All standard libraries.
        const ALL = u32::MAX;
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
