use std::{
    any::TypeId,
    ffi::CStr,
    os::raw::{c_int, c_void},
    ptr,
};

use rustc_hash::FxHashMap;

use super::UserDataStorage;
use crate::{
    error::{Error, Result},
    state::{ExtraData, callback_error_ext},
    types::CallbackPtr,
    util::{get_userdata, push_userdata, rawget_field, rawset_field, take_userdata},
};

// Userdata type hints,  used to match types of wrapped userdata
#[derive(Clone, Copy)]
pub struct TypeIdHints {
    t: TypeId,
}

impl TypeIdHints {
    pub(crate) fn new<T: 'static>() -> Self {
        Self {
            t: TypeId::of::<T>(),
        }
    }

    #[inline(always)]
    pub(crate) fn type_id(&self) -> TypeId {
        self.t
    }
}

pub(crate) unsafe fn borrow_userdata_scoped<T, R>(
    state: *mut ffi::lua_State,
    idx: c_int,
    type_id: Option<TypeId>,
    type_hints: TypeIdHints,
    f: impl FnOnce(&T) -> R,
) -> Result<R> {
    match type_id {
        Some(type_id) if type_id == type_hints.t => {
            let ud = get_userdata::<UserDataStorage<T>>(state, idx);
            (*ud).try_borrow_scoped(|ud| f(ud))
        }

        _ => Err(Error::UserDataTypeMismatch),
    }
}

pub(crate) unsafe fn borrow_userdata_scoped_mut<T, R>(
    state: *mut ffi::lua_State,
    idx: c_int,
    type_id: Option<TypeId>,
    type_hints: TypeIdHints,
    f: impl FnOnce(&mut T) -> R,
) -> Result<R> {
    match type_id {
        Some(type_id) if type_id == type_hints.t => {
            let ud = get_userdata::<UserDataStorage<T>>(state, idx);
            (*ud).try_borrow_scoped_mut(|ud| f(ud))
        }

        _ => Err(Error::UserDataTypeMismatch),
    }
}

// Populates the given table with the appropriate members to be a userdata metatable for the given
// type. This function takes the given table at the `metatable` index, and adds an appropriate
// `__gc` member to it for the given type and a `__metatable` entry to protect the table from script
// access. The function also, if given a `field_getters` or `methods` tables, will create an
// `__index` metamethod (capturing previous one) to lookup in `field_getters` first, then `methods`
// and falling back to the captured `__index` if no matches found.
// The same is also applicable for `__newindex` metamethod and `field_setters` table.
// Internally uses 9 stack spaces and does not call checkstack.
pub(crate) unsafe fn init_userdata_metatable(
    state: *mut ffi::lua_State,
    metatable: c_int,
    field_getters: Option<c_int>,
    field_setters: Option<c_int>,
    methods: Option<c_int>,
    _methods_map: Option<FxHashMap<Vec<u8>, CallbackPtr>>, // Used only in Luau for `__namecall`
) -> Result<()> {
    if field_getters.is_some() || methods.is_some() {
        // Push `__index` generator function
        init_userdata_metatable_index(state)?;

        let index_type = rawget_field(state, metatable, "__index")?;
        match index_type {
            ffi::LUA_TNIL | ffi::LUA_TTABLE | ffi::LUA_TFUNCTION => {
                for &idx in &[field_getters, methods] {
                    if let Some(idx) = idx {
                        ffi::lua_pushvalue(state, idx);
                    } else {
                        ffi::lua_pushnil(state);
                    }
                }

                // Generate `__index`
                protect_lua!(state, 4, 1, fn(state) ffi::lua_call(state, 3, 1))?;
            }
            _ => ruau_panic!("improper `__index` type: {}", index_type),
        }

        rawset_field(state, metatable, "__index")?;
        if let Some(methods_map) = _methods_map {
            // In Luau we can speedup method calls by providing a dedicated `__namecall` metamethod
            push_userdata_metatable_namecall(state, methods_map)?;
            rawset_field(state, metatable, "__namecall")?;
        }
    }

    if let Some(field_setters) = field_setters {
        // Push `__newindex` generator function
        init_userdata_metatable_newindex(state)?;

        let newindex_type = rawget_field(state, metatable, "__newindex")?;
        match newindex_type {
            ffi::LUA_TNIL | ffi::LUA_TTABLE | ffi::LUA_TFUNCTION => {
                ffi::lua_pushvalue(state, field_setters);
                // Generate `__newindex`
                protect_lua!(state, 3, 1, fn(state) ffi::lua_call(state, 2, 1))?;
            }
            _ => ruau_panic!("improper `__newindex` type: {}", newindex_type),
        }

        rawset_field(state, metatable, "__newindex")?;
    }

    ffi::lua_pushboolean(state, 0);
    rawset_field(state, metatable, "__metatable")?;

    Ok(())
}

unsafe extern "C-unwind" fn lua_error_impl(state: *mut ffi::lua_State) -> c_int {
    ffi::lua_error(state);
}

unsafe extern "C-unwind" fn lua_isfunction_impl(state: *mut ffi::lua_State) -> c_int {
    ffi::lua_pushboolean(state, ffi::lua_isfunction(state, -1));
    1
}

unsafe extern "C-unwind" fn lua_istable_impl(state: *mut ffi::lua_State) -> c_int {
    ffi::lua_pushboolean(state, ffi::lua_istable(state, -1));
    1
}

unsafe fn init_userdata_metatable_index(state: *mut ffi::lua_State) -> Result<()> {
    let index_key = &USERDATA_METATABLE_INDEX as *const u8 as *const _;
    if ffi::lua_rawgetp(state, ffi::LUA_REGISTRYINDEX, index_key) == ffi::LUA_TFUNCTION {
        return Ok(());
    }
    ffi::lua_pop(state, 1);

    // Create and cache `__index` generator
    let code = cr#"
        local error, isfunction, istable = ...
        return function (__index, field_getters, methods)
            -- Common case: has field getters and index is a table
            if field_getters ~= nil and methods == nil and istable(__index) then
                return function (self, key)
                    local field_getter = field_getters[key]
                    if field_getter ~= nil then
                        return field_getter(self)
                    end
                    return __index[key]
                end
            end

            return function (self, key)
                if field_getters ~= nil then
                    local field_getter = field_getters[key]
                    if field_getter ~= nil then
                        return field_getter(self)
                    end
                end

                if methods ~= nil then
                    local method = methods[key]
                    if method ~= nil then
                        return method
                    end
                end

                if isfunction(__index) then
                    return __index(self, key)
                elseif __index == nil then
                    error("attempt to get an unknown field '"..key.."'")
                else
                    return __index[key]
                end
            end
        end
    "#;
    protect_lua!(state, 0, 1, |state| {
        let ret = ffi::luaL_loadbuffer(
            state,
            code.as_ptr(),
            code.count_bytes(),
            cstr!("=__ruau_index"),
        );
        if ret != ffi::LUA_OK {
            ffi::lua_error(state);
        }
        ffi::lua_pushcfunction(state, lua_error_impl);
        ffi::lua_pushcfunction(state, lua_isfunction_impl);
        ffi::lua_pushcfunction(state, lua_istable_impl);
        ffi::lua_call(state, 3, 1);
        if ffi::luau_codegen_supported() != 0 {
            ffi::luau_codegen_compile(state, -1);
        }

        // Store in the registry
        ffi::lua_pushvalue(state, -1);
        ffi::lua_rawsetp(state, ffi::LUA_REGISTRYINDEX, index_key);
    })
}

unsafe fn init_userdata_metatable_newindex(state: *mut ffi::lua_State) -> Result<()> {
    let newindex_key = &USERDATA_METATABLE_NEWINDEX as *const u8 as *const _;
    if ffi::lua_rawgetp(state, ffi::LUA_REGISTRYINDEX, newindex_key) == ffi::LUA_TFUNCTION {
        return Ok(());
    }
    ffi::lua_pop(state, 1);

    // Create and cache `__newindex` generator
    let code = cr#"
        local error, isfunction = ...
        return function (__newindex, field_setters)
            return function (self, key, value)
                if field_setters ~= nil then
                    local field_setter = field_setters[key]
                    if field_setter ~= nil then
                        field_setter(self, value)
                        return
                    end
                end

                if isfunction(__newindex) then
                    __newindex(self, key, value)
                elseif __newindex == nil then
                    error("attempt to set an unknown field '"..key.."'")
                else
                    __newindex[key] = value
                end
            end
        end
    "#;
    protect_lua!(state, 0, 1, |state| {
        let code_len = code.count_bytes();
        let ret = ffi::luaL_loadbuffer(state, code.as_ptr(), code_len, cstr!("=__ruau_newindex"));
        if ret != ffi::LUA_OK {
            ffi::lua_error(state);
        }
        ffi::lua_pushcfunction(state, lua_error_impl);
        ffi::lua_pushcfunction(state, lua_isfunction_impl);
        ffi::lua_call(state, 2, 1);
        if ffi::luau_codegen_supported() != 0 {
            ffi::luau_codegen_compile(state, -1);
        }

        // Store in the registry
        ffi::lua_pushvalue(state, -1);
        ffi::lua_rawsetp(state, ffi::LUA_REGISTRYINDEX, newindex_key);
    })
}
unsafe fn push_userdata_metatable_namecall(
    state: *mut ffi::lua_State,
    methods_map: FxHashMap<Vec<u8>, CallbackPtr>,
) -> Result<()> {
    struct NamecallMethods {
        atoms: FxHashMap<i16, CallbackPtr>,
        names: FxHashMap<Vec<u8>, CallbackPtr>,
    }

    unsafe extern "C-unwind" fn namecall(state: *mut ffi::lua_State) -> c_int {
        let mut atom = -1;
        let name = ffi::lua_namecallatom(state, &mut atom);
        if name.is_null() {
            ffi::luaL_error(state, cstr!("attempt to call an unknown method"));
        }
        let name_cs = CStr::from_ptr(name);
        let methods = get_userdata::<NamecallMethods>(state, ffi::lua_upvalueindex(1));
        let callback_ptr = match (i16::try_from(atom).ok())
            .and_then(|atom| (*methods).atoms.get(&atom))
        {
            Some(ptr) => *ptr,
            None => match (*methods).names.get(name_cs.to_bytes()) {
                Some(ptr) => *ptr,
                #[rustfmt::skip]
                None => ffi::luaL_error(state, cstr!("attempt to call an unknown method '%s'"), name),
            },
        };
        callback_error_ext(state, ptr::null_mut(), true, |extra, nargs| {
            let rawlua = (*extra).raw_luau();
            (*callback_ptr)(rawlua, nargs)
        })
    }

    let mut atoms = FxHashMap::default();
    let extra = (*ffi::lua_callbacks(state)).userdata as *mut ExtraData;
    if !extra.is_null() {
        for (name, callback) in &methods_map {
            if let Some(atom) = (*extra).register_namecall_atom(name) {
                atoms.insert(atom, *callback);
            }
        }
    }
    let methods = NamecallMethods {
        atoms,
        names: methods_map,
    };

    // Automatic destructor is provided for any Luau userdata
    push_userdata(state, methods, true)?;
    protect_lua!(state, 1, 1, |state| {
        ffi::lua_pushcclosured(state, namecall, cstr!("__namecall"), 1);
    })
}

// This method is called by Luau GC when it's time to collect the userdata.
pub(crate) unsafe extern "C" fn collect_userdata<T>(state: *mut ffi::lua_State, ud: *mut c_void) {
    // Almost none Luau operations are allowed when destructor is running,
    // so we need to set a flag to prevent calling any Luau functions
    let extra = (*ffi::lua_callbacks(state)).userdata as *mut ExtraData;
    (*extra).running_gc = true;
    // Luau does not support _any_ panics in destructors (they are declared as "C", NOT as "C-unwind"),
    // so any panics will trigger `abort()`.
    ptr::drop_in_place(ud as *mut T);
    (*extra).running_gc = false;
}

// This method can be called by user or Luau GC to destroy the userdata.
// It checks if the userdata is safe to destroy and sets the "destroyed" metatable
// to prevent further GC collection.
pub(super) unsafe extern "C-unwind" fn destroy_userdata_storage<T>(
    state: *mut ffi::lua_State,
) -> c_int {
    let ud = get_userdata::<UserDataStorage<T>>(state, 1);
    if (*ud).is_safe_to_destroy() {
        take_userdata::<UserDataStorage<T>>(state, 1);
        ffi::lua_pushboolean(state, 1);
    } else {
        ffi::lua_pushboolean(state, 0);
    }
    1
}

static USERDATA_METATABLE_INDEX: u8 = 0;
static USERDATA_METATABLE_NEWINDEX: u8 = 0;
