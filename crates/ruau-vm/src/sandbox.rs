use ruau_vm_api::RawValue;

use crate::{
    Vm,
    table::{LuaTable, NextStep},
};

/// Why sandboxing failed; the VM is unchanged unless stated otherwise.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SandboxError {
    /// The VM is poisoned; sandboxing refused to touch it.
    Poisoned,
    /// The VM has no global table.
    NoGlobals,
    /// The shared globals are not read-only yet — `sandbox_thread` requires
    /// the [`Vm::sandbox`] lock first (this also rejects a double call, since
    /// the installed proxy is itself writable).
    GlobalsNotSandboxed,
    /// An allocation failed mid-install; the thread's globals pointer was not
    /// swapped, but an orphaned table may await the next collection.
    OutOfMemory,
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Poisoned => "VM is poisoned",
            Self::NoGlobals => "VM has no global table",
            Self::GlobalsNotSandboxed => "shared globals are not sandboxed yet",
            Self::OutOfMemory => "allocation failed while installing the sandbox",
        })
    }
}

impl std::error::Error for SandboxError {}

impl Vm {
    /// Locks the shared environment against tampering — the port of `luaL_sandbox`
    /// (`linit.cpp`). Marks every library table reachable from the global table
    /// read-only, marks the string metatable read-only, and marks the global table
    /// itself read-only and `safeenv`. After this, untrusted bytecode can neither
    /// rebind a stdlib member (`string.format = f`) nor introduce a global
    /// (`x = 1`): both compile to a write that now raises "attempt to modify a
    /// readonly table". Call once, after building the VM and before running
    /// untrusted code.
    ///
    /// This protects the *shared* libraries and globals; to additionally give each
    /// script its own writable global table over them, follow with
    /// [`sandbox_thread`](Self::sandbox_thread). A poisoned VM is left untouched.
    pub fn sandbox(&mut self) {
        if self.poisoned {
            return;
        }
        let Some(globals) = self.heap.thread(self.main_thread).and_then(|t| t.globals) else {
            return;
        };
        // Collect every table-valued global (the library tables) in one immutable
        // walk, then mark them read-only in a second pass — keeping the walk's
        // borrow of the global table disjoint from the per-library mutation.
        let mut libraries = Vec::new();
        let mut key = RawValue::Nil;
        while let NextStep::Pair(next_key, value) = self
            .heap
            .table(globals)
            .map_or(NextStep::Done, |table| table.next(key))
        {
            if let RawValue::Table(library) = value {
                libraries.push(library);
            }
            key = next_key;
        }
        for library in libraries {
            if let Some(table) = self.heap.table_mut(library) {
                table.readonly = true;
            }
        }
        // The string metatable is the one basic-type metatable today; lock it so
        // `("").format = f` cannot reach the shared `string` library either.
        if let Some(metatable) = self.heap.string_metatable()
            && let Some(table) = self.heap.table_mut(metatable)
        {
            table.readonly = true;
        }
        // Finally lock the global table itself and flag it `safeenv`, since the
        // environment is now immutable.
        if let Some(table) = self.heap.table_mut(globals) {
            table.readonly = true;
            table.safeenv = true;
        }
    }

    /// Gives the main thread its own writable global table that proxies reads to
    /// the shared globals — the port of `luaL_sandboxthread` (`linit.cpp`). The new
    /// table's read-only metatable `__index`-points at the current globals, so a
    /// script's global *writes* (`x = 1`) land in its private table while *reads*
    /// of `print`, `string`, etc. fall through to the shared libraries.
    ///
    /// **Call [`sandbox`](Self::sandbox) first.** This requires the current globals
    /// to already be read-only and returns `false` otherwise — proxying *unlocked*
    /// globals is pointless (writes would still reach the shared libraries), and the
    /// requirement rejects the two misuse hazards: calling this before `sandbox()`
    /// (which would then lock the empty proxy, leaving the real libraries writable)
    /// and calling it twice (the freshly installed proxy is writable, so a second
    /// call sees a non-read-only environment and refuses, rather than chaining one
    /// script's proxy onto another's private globals).
    ///
    /// Returns `false` without installing the proxy if the VM is poisoned, has no
    /// globals, the globals are not yet sandboxed, or an allocation fails. On an
    /// allocation failure mid-build the heap may carry an orphaned table for the
    /// next collection to reclaim; the thread's globals pointer is only swapped
    /// on full success.
    pub fn sandbox_thread(&mut self) -> Result<(), SandboxError> {
        if self.poisoned {
            return Err(SandboxError::Poisoned);
        }
        let Some(shared) = self.heap.thread(self.main_thread).and_then(|t| t.globals) else {
            return Err(SandboxError::NoGlobals);
        };
        // The shared globals must be sandboxed (read-only) first — see the contract
        // above. This single check rejects both the wrong-order and double-call
        // misuses, since the proxy this installs is itself writable.
        if !self.heap.table(shared).is_some_and(|table| table.readonly) {
            return Err(SandboxError::GlobalsNotSandboxed);
        }
        let Some(index_key) = self.heap.intern_str(b"__index") else {
            return Err(SandboxError::OutOfMemory);
        };
        // metatable = { __index = <shared globals> }, itself read-only.
        let Some(metatable) = self.heap.alloc_table(LuaTable::new()) else {
            return Err(SandboxError::OutOfMemory);
        };
        match self.heap.table_mut(metatable) {
            Some(table) => {
                table.set(RawValue::String(index_key), RawValue::Table(shared));
                table.readonly = true;
            }
            None => return Err(SandboxError::OutOfMemory),
        }
        // The fresh per-thread global table proxies reads through that metatable.
        // `safeenv` mirrors upstream, which flags the sandboxed thread's env safe;
        // it is inert here until the import fast-path reads it, and a host that
        // reloads code into the same thread must clear it (upstream's caveat).
        let Some(proxy) = self.heap.alloc_table(LuaTable::new()) else {
            return Err(SandboxError::OutOfMemory);
        };
        match self.heap.table_mut(proxy) {
            Some(table) => {
                table.set_metatable(Some(metatable));
                table.safeenv = true;
            }
            None => return Err(SandboxError::OutOfMemory),
        }
        if let Some(thread) = self.heap.thread_mut(self.main_thread) {
            thread.globals = Some(proxy);
        }
        Ok(())
    }

    /// Locks the shared environment **and** gives the main thread its own
    /// writable global proxy — [`Vm::sandbox`] followed by
    /// [`Vm::sandbox_thread`] as one fallible operation. This is the
    /// untrusted-code posture: call once after building, before running
    /// tenant code.
    ///
    /// # Errors
    /// Returns the first [`SandboxError`] the sequence hits; on error the
    /// thread proxy is not installed.
    #[must_use = "an unsandboxed VM must not run untrusted code"]
    pub fn sandbox_for_untrusted(&mut self) -> Result<(), SandboxError> {
        self.sandbox();
        self.sandbox_thread()
    }
}
