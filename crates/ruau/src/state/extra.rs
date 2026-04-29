use std::{
    any::TypeId,
    cell::{Cell, RefCell, UnsafeCell},
    mem::MaybeUninit,
    os::raw::{c_int, c_void},
    ptr::{self, NonNull},
    rc::Rc,
    task::Waker,
};

use futures_util::task::noop_waker;
use rustc_hash::FxHashMap;

use super::{Luau, WeakLuau};
use crate::{
    chunk::Compiler,
    error::Result,
    state::RawLuau,
    stdlib::StdLib,
    types::{AppData, XRc},
    userdata_impl::{RawUserDataRegistry, UserDataSerializeCallback},
    util::{TypeKey, WrappedFailure, get_internal_metatable},
};

const WRAPPED_FAILURE_POOL_DEFAULT_CAPACITY: usize = 64;
const REF_STACK_RESERVE: c_int = 3;

/// Data associated with the Luau state.
pub struct ExtraData {
    pub(super) lua: MaybeUninit<Luau>,
    pub(super) weak: MaybeUninit<WeakLuau>,
    pub(super) owned: bool,

    pub(super) pending_userdata_reg: FxHashMap<TypeId, RawUserDataRegistry>,
    pub(super) registered_userdata_t: FxHashMap<TypeId, c_int>,
    pub(super) registered_userdata_tags: FxHashMap<c_int, c_int>,
    pub(super) registered_userdata_tag_types: FxHashMap<c_int, TypeId>,
    pub(super) registered_userdata_mt: FxHashMap<*const c_void, Option<TypeId>>,
    pub(super) registered_userdata_serializers: FxHashMap<TypeId, UserDataSerializeCallback>,
    pub(super) last_checked_userdata_mt: (*const c_void, Option<TypeId>),
    pub(super) next_userdata_tag: c_int,

    // When Luau instance dropped, setting `None` would prevent collecting `RegistryKey`s
    pub(super) registry_unref_list: Rc<RefCell<Option<Vec<c_int>>>>,

    // Containers to store arbitrary data (extensions)
    pub(super) app_data: AppData,
    pub(super) app_data_priv: AppData,

    pub(super) safe: bool,
    pub(super) libs: StdLib,
    // Auxiliary thread to store references
    pub(super) ref_thread: *mut ffi::lua_State,
    pub(super) ref_stack_size: c_int,
    pub(super) ref_stack_top: c_int,
    pub(super) ref_free: Vec<c_int>,

    // Pool of `WrappedFailure` enums in the ref thread (as userdata)
    pub(super) wrapped_failure_pool: Vec<c_int>,
    pub(super) wrapped_failure_top: usize,
    // Pool of `Thread`s (coroutines) for async execution
    pub(super) thread_pool: Vec<crate::types::ValueRefIndex>,

    // Address of `WrappedFailure` metatable
    pub(super) wrapped_failure_mt_ptr: *const c_void,

    // Waker for polling futures
    pub(super) waker: Waker,

    pub(super) interrupt_callback: Option<crate::types::InterruptCallback>,
    pub(super) thread_creation_callback: Option<crate::types::ThreadCreationCallback>,
    pub(super) thread_collection_callback: Option<crate::types::ThreadCollectionCallback>,

    pub(crate) running_gc: bool,
    pub(crate) sandboxed: bool,
    pub(super) compiler: Option<Compiler>,
    pub(super) enable_jit: bool,
    pub(crate) mem_categories: Vec<std::ffi::CString>,
    pub(crate) namecall_atoms: FxHashMap<Vec<u8>, i16>,
    pub(crate) next_namecall_atom: i16,
}

impl Drop for ExtraData {
    fn drop(&mut self) {
        unsafe {
            if !self.owned {
                self.lua.assume_init_drop();
            }

            self.weak.assume_init_drop();
        }
        *self.registry_unref_list.borrow_mut() = None;
    }
}

static EXTRA_TYPE_KEY: u8 = 0;

impl TypeKey for XRc<UnsafeCell<ExtraData>> {
    #[inline(always)]
    fn type_key() -> *const c_void {
        &EXTRA_TYPE_KEY as *const u8 as *const c_void
    }
}

impl ExtraData {
    // Index of `error_traceback` function in auxiliary thread stack
    pub(super) const ERROR_TRACEBACK_IDX: c_int = 1;

    pub(super) unsafe fn init(state: *mut ffi::lua_State, owned: bool) -> XRc<UnsafeCell<Self>> {
        // Create ref stack thread and place it in the registry to prevent it
        // from being garbage collected.
        let ref_thread = ruau_expect!(
            protect_lua!(state, 0, 0, |state| {
                let thread = ffi::lua_newthread(state);
                ffi::luaL_ref(state, ffi::LUA_REGISTRYINDEX);
                thread
            }),
            "Error while creating ref thread",
        );

        let wrapped_failure_mt_ptr = {
            get_internal_metatable::<WrappedFailure>(state);
            let ptr = ffi::lua_topointer(state, -1);
            ffi::lua_pop(state, 1);
            ptr
        };

        // Store `error_traceback` function on the ref stack
        {
            ffi::lua_pushcfunction(ref_thread, crate::util::error_traceback);
            assert_eq!(ffi::lua_gettop(ref_thread), Self::ERROR_TRACEBACK_IDX);
        }

        #[allow(clippy::arc_with_non_send_sync)]
        let extra = XRc::new(UnsafeCell::new(Self {
            lua: MaybeUninit::uninit(),
            weak: MaybeUninit::uninit(),
            owned,
            pending_userdata_reg: FxHashMap::default(),
            registered_userdata_t: FxHashMap::default(),
            registered_userdata_tags: FxHashMap::default(),
            registered_userdata_tag_types: FxHashMap::default(),
            registered_userdata_mt: FxHashMap::default(),
            registered_userdata_serializers: FxHashMap::default(),
            last_checked_userdata_mt: (ptr::null(), None),
            next_userdata_tag: 2,
            registry_unref_list: Rc::new(RefCell::new(Some(Vec::new()))),
            app_data: AppData::default(),
            app_data_priv: AppData::default(),
            safe: false,
            libs: StdLib::NONE,
            ref_thread,
            // We need some reserved stack space to move values in and out of the ref stack.
            ref_stack_size: ffi::LUA_MINSTACK - REF_STACK_RESERVE,
            ref_stack_top: ffi::lua_gettop(ref_thread),
            ref_free: Vec::new(),
            wrapped_failure_pool: Vec::with_capacity(WRAPPED_FAILURE_POOL_DEFAULT_CAPACITY),
            wrapped_failure_top: 0,
            thread_pool: Vec::new(),
            wrapped_failure_mt_ptr,
            waker: noop_waker(),
            interrupt_callback: None,
            thread_creation_callback: None,
            thread_collection_callback: None,
            sandboxed: false,
            compiler: None,
            enable_jit: true,
            running_gc: false,
            mem_categories: vec![std::ffi::CString::new("main").unwrap()],
            namecall_atoms: FxHashMap::default(),
            next_namecall_atom: 0,
        }));

        // Store it in the registry
        ruau_expect!(Self::store(&extra, state), "Error while storing extra data");

        extra
    }

    pub(super) unsafe fn set_lua(&mut self, raw: NonNull<RawLuau>, live: &Rc<Cell<bool>>) {
        self.lua.write(Luau {
            raw,
            live: Rc::clone(live),
            collect_garbage: false,
            _not_send_sync: std::marker::PhantomData,
        });
        self.weak.write(WeakLuau {
            raw,
            live: Rc::downgrade(live),
            _not_send_sync: std::marker::PhantomData,
        });
    }

    pub(crate) unsafe fn get(state: *mut ffi::lua_State) -> *mut Self {
        (*ffi::lua_callbacks(state)).userdata as *mut _
    }

    unsafe fn store(extra: &XRc<UnsafeCell<Self>>, state: *mut ffi::lua_State) -> Result<()> {
        (*ffi::lua_callbacks(state)).userdata = extra.get() as *mut _;
        Ok(())
    }

    #[inline(always)]
    pub(super) unsafe fn lua(&self) -> &Luau {
        self.lua.assume_init_ref()
    }

    #[inline(always)]
    pub(crate) unsafe fn raw_luau(&self) -> &RawLuau {
        self.lua.assume_init_ref().raw.as_ref()
    }

    #[inline(always)]
    pub(super) unsafe fn weak(&self) -> &WeakLuau {
        self.weak.assume_init_ref()
    }

    pub(crate) fn register_namecall_atom(&mut self, name: &[u8]) -> Option<i16> {
        if let Some(&atom) = self.namecall_atoms.get(name) {
            return Some(atom);
        }
        if self.next_namecall_atom == i16::MAX {
            return None;
        }
        let atom = self.next_namecall_atom;
        self.next_namecall_atom += 1;
        self.namecall_atoms.insert(name.to_vec(), atom);
        Some(atom)
    }

    pub(crate) fn namecall_atom(&self, name: &[u8]) -> i16 {
        self.namecall_atoms.get(name).copied().unwrap_or(-1)
    }

    /// Pops a reference from top of the auxiliary stack and move it to a first free slot.
    pub(super) unsafe fn ref_stack_pop(&mut self) -> c_int {
        if let Some(free) = self.ref_free.pop() {
            ffi::lua_replace(self.ref_thread, free);
            return free;
        }

        // Try to grow max stack size
        if self.ref_stack_top >= self.ref_stack_size {
            let mut inc = self.ref_stack_size; // Try to double stack size
            while inc > 0 && ffi::lua_checkstack(self.ref_thread, inc + REF_STACK_RESERVE) == 0 {
                inc /= 2;
            }
            if inc == 0 {
                // Pop item on top of the stack to avoid stack leaking and successfully run destructors
                // during unwinding.
                ffi::lua_pop(self.ref_thread, 1);
                let top = self.ref_stack_top;
                // It is a user error to create too many references to exhaust the Luau max stack size
                // for the ref thread.
                panic!(
                    "cannot create a Luau reference, out of auxiliary stack space (used {top} slots)"
                );
            }
            self.ref_stack_size += inc;
        }
        self.ref_stack_top += 1;
        self.ref_stack_top
    }
}
