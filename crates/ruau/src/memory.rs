use std::{
    alloc::{self, Layout},
    os::raw::c_void,
    ptr,
};

/// Allocator callback passed to Luau when a VM is created.
pub static ALLOCATOR: ffi::lua_Alloc = allocator;

/// Per-VM allocation accounting used by [`allocator`].
#[repr(C)]
#[derive(Default)]
pub struct MemoryState {
    /// Current number of bytes allocated through the Luau allocator.
    used_memory: isize,
    /// Maximum number of bytes Luau may allocate, or `0` when unlimited.
    memory_limit: isize,
    /// Temporarily bypasses the memory limit for VM operations that must allocate.
    ignore_limit: bool,
    /// Tracks whether the previous allocator call failed because of the configured limit.
    limit_reached: bool,
}

impl MemoryState {
    /// Returns the allocator state stored in a Luau state.
    #[inline]
    pub(crate) unsafe fn get(state: *mut ffi::lua_State) -> *mut Self {
        let mut mem_state = ptr::null_mut();
        ffi::lua_getallocf(state, &mut mem_state);
        ruau_assert!(!mem_state.is_null(), "Luau state has no allocator userdata");
        mem_state as *mut Self
    }

    /// Returns the current number of bytes allocated by the VM.
    #[inline]
    pub(crate) fn used_memory(&self) -> usize {
        self.used_memory as usize
    }

    /// Returns the configured memory limit in bytes, or `0` when unlimited.
    #[inline]
    pub(crate) fn memory_limit(&self) -> usize {
        self.memory_limit as usize
    }

    /// Replaces the configured memory limit and returns the previous limit.
    #[inline]
    pub(crate) fn set_memory_limit(&mut self, limit: usize) -> usize {
        let prev_limit = self.memory_limit;
        self.memory_limit = limit as isize;
        prev_limit as usize
    }

    /// Runs a closure while temporarily bypassing the memory limit.
    #[inline]
    pub(crate) unsafe fn relax_limit_with(state: *mut ffi::lua_State, f: impl FnOnce()) {
        let mem_state = Self::get(state);
        if !mem_state.is_null() {
            (*mem_state).ignore_limit = true;
            f();
            (*mem_state).ignore_limit = false;
        } else {
            f();
        }
    }

    /// Returns `true` if the previous allocator operation hit the configured limit.
    #[inline]
    pub(crate) unsafe fn limit_reached(state: *mut ffi::lua_State) -> bool {
        (*Self::get(state)).limit_reached
    }
}

/// Luau-compatible allocator that enforces the VM memory limit.
unsafe extern "C" fn allocator(
    extra: *mut c_void,
    ptr: *mut c_void,
    osize: usize,
    nsize: usize,
) -> *mut c_void {
    let mem_state = &mut *(extra as *mut MemoryState);
    // Reset the flag
    mem_state.limit_reached = false;

    if nsize == 0 {
        // Free memory
        if !ptr.is_null() {
            let layout = Layout::from_size_align_unchecked(osize, ffi::SYS_MIN_ALIGN);
            alloc::dealloc(ptr as *mut u8, layout);
            mem_state.used_memory -= osize as isize;
        }
        return ptr::null_mut();
    }

    // Do not allocate more than isize::MAX
    if nsize > isize::MAX as usize {
        return ptr::null_mut();
    }

    // Are we fit to the memory limits?
    let mut mem_diff = nsize as isize;
    if !ptr.is_null() {
        mem_diff -= osize as isize;
    }
    let mem_limit = mem_state.memory_limit;
    let new_used_memory = mem_state.used_memory + mem_diff;
    if mem_limit > 0 && new_used_memory > mem_limit && !mem_state.ignore_limit {
        mem_state.limit_reached = true;
        return ptr::null_mut();
    }
    mem_state.used_memory += mem_diff;

    if ptr.is_null() {
        // Allocate new memory
        let new_layout = match Layout::from_size_align(nsize, ffi::SYS_MIN_ALIGN) {
            Ok(layout) => layout,
            Err(_) => return ptr::null_mut(),
        };
        let new_ptr = alloc::alloc(new_layout) as *mut c_void;
        if new_ptr.is_null() {
            alloc::handle_alloc_error(new_layout);
        }
        return new_ptr;
    }

    // Reallocate memory
    let old_layout = Layout::from_size_align_unchecked(osize, ffi::SYS_MIN_ALIGN);
    let new_ptr = alloc::realloc(ptr as *mut u8, old_layout, nsize) as *mut c_void;
    if new_ptr.is_null() {
        alloc::handle_alloc_error(old_layout);
    }
    new_ptr
}
