//! Luau runtime extensions and types.
//!
use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::{CStr, CString},
    os::raw::c_int,
    ptr,
    rc::Rc,
};

pub use heap_dump::HeapDump;

use crate::{
    error::{Error, Result},
    function::Function,
    multi::MultiValue,
    resolver::{ModuleId, ModuleResolver, ModuleSource},
    state::{ExtraData, Luau, callback_error_ext},
    traits::{FromLuauMulti, IntoLuau},
    value::Value,
};

// Since Luau has some missing standard functions, we re-implement them here

/// Shared, single-threaded resolver handle used by the runtime `require` plumbing.
pub type SharedResolver = Rc<dyn ModuleResolver>;
/// Per-resolver module cache shared across requesters.
pub type RuntimeModuleCache = Rc<RefCell<HashMap<ModuleId, Value>>>;

impl Luau {
    /// Installs a global `require` function backed by a [`ModuleResolver`].
    ///
    /// The installed loader resolves every requested specifier through `resolver`, caches module
    /// results by [`ModuleId`], and loads child modules in an environment whose `require` function
    /// resolves relative to that child module. Application-level policies such as aliases or
    /// project configuration files belong in the resolver implementation.
    pub fn set_module_resolver<R>(&self, resolver: R) -> Result<()>
    where
        R: ModuleResolver,
    {
        self.install_module_resolver(Rc::new(resolver))
    }

    /// Set the memory category for subsequent allocations from this Luau state.
    ///
    /// The category "main" is reserved for the default memory category.
    /// Maximum of 255 categories can be registered.
    /// The category is set per Luau thread (state) and affects all allocations made from that
    /// thread.
    ///
    /// Return error if too many categories are registered or if the category name is invalid.
    ///
    /// See [`Luau::heap_dump`] for tracking memory usage by category.
    pub fn set_memory_category(&self, category: &str) -> Result<()> {
        let lua = self.raw();

        if category.contains(|c| !matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_')) {
            return Err(Error::runtime("invalid memory category name"));
        }
        let cat_id = unsafe {
            let extra = ExtraData::get(lua.state());
            match ((*extra).mem_categories.iter().enumerate())
                .find(|&(_, name)| name.as_bytes() == category.as_bytes())
            {
                Some((id, _)) => id as u8,
                None => {
                    let new_id = (*extra).mem_categories.len() as u8;
                    if new_id == 255 {
                        return Err(Error::runtime("too many memory categories registered"));
                    }
                    (*extra)
                        .mem_categories
                        .push(CString::new(category).unwrap());
                    new_id
                }
            }
        };
        unsafe { ffi::lua_setmemcat(lua.state(), cat_id as i32) };

        Ok(())
    }

    /// Dumps the current Luau VM heap state.
    ///
    /// The returned `HeapDump` can be used to analyze memory usage.
    /// It's recommended to call [`Luau::gc_collect`] before dumping the heap.
    pub fn heap_dump(&self) -> Result<HeapDump> {
        let lua = self.raw();
        unsafe {
            heap_dump::HeapDump::new(lua.state())
                .ok_or_else(|| Error::runtime("unable to dump heap"))
        }
    }

    pub(crate) unsafe fn configure_luau(&self) -> Result<()> {
        let globals = self.globals();

        globals.raw_set(
            "collectgarbage",
            self.create_c_function(lua_collectgarbage)?,
        )?;
        globals.raw_set("loadstring", self.create_c_function(lua_loadstring)?)?;

        // Set `_VERSION` global to include version number
        // The environment variable `LUAU_VERSION` set by the build script
        if let Some(version) = ffi::luau_version() {
            globals.raw_set("_VERSION", format!("Luau {version}"))?;
        }

        // No default `require` implementation — embedders pick a resolver explicitly via
        // `Luau::set_module_resolver`. This avoids surprising the host filesystem on
        // `Luau::new()`. Calls to `require` without a resolver installed will fail with a
        // clear error message.
        let no_resolver = self.create_function(|_, specifier: String| -> Result<()> {
            Err(Error::runtime(format!(
                "no module resolver installed; cannot require `{specifier}`. Use \
                 Luau::set_module_resolver(...) to install one."
            )))
        })?;
        globals.raw_set("require", no_resolver)?;

        Ok(())
    }

    fn install_module_resolver(&self, resolver: SharedResolver) -> Result<()> {
        let cache = Rc::new(RefCell::new(HashMap::new()));
        let require = resolver_require_function(self, resolver, cache, None)?;
        self.globals().raw_set("require", require)
    }
}

/// Builds a `require` function that resolves through `resolver` and caches results by `ModuleId`.
pub fn resolver_require_function(
    lua: &Luau,
    resolver: SharedResolver,
    cache: RuntimeModuleCache,
    requester: Option<ModuleId>,
) -> Result<Function> {
    lua.create_async_function(async move |lua, specifier: String| {
        let resolver = Rc::clone(&resolver);
        let cache = Rc::clone(&cache);
        let requester = requester.clone();
        resolver_require(lua, resolver, cache, requester, specifier).await
    })
}

async fn resolver_require(
    lua: &Luau,
    resolver: SharedResolver,
    cache: RuntimeModuleCache,
    requester: Option<ModuleId>,
    specifier: String,
) -> Result<Value> {
    let module = resolver
        .resolve(requester.as_ref(), &specifier)
        .await
        .map_err(|error| Error::runtime(error.to_string()))?;

    if let Some(value) = cache.borrow().get(module.id()).cloned() {
        return Ok(value);
    }

    let env = resolver_environment(
        lua,
        Rc::clone(&resolver),
        Rc::clone(&cache),
        Some(module.id().clone()),
    )?;
    let mut values = lua
        .load(module.source())
        .name(module_name(&module))
        .environment(env)
        .call::<MultiValue>(())
        .await?;

    if values.len() > 1 {
        return Err(Error::runtime("module must return a single value"));
    }

    let value = values.pop_front().unwrap_or(Value::Boolean(true));
    cache
        .borrow_mut()
        .insert(module.id().clone(), value.clone());
    Ok(value)
}

/// Builds an environment table whose `__index` proxies globals and whose `require` resolves
/// through `resolver`, used for both runtime child-module envs and checked-load chunks.
pub fn resolver_environment(
    lua: &Luau,
    resolver: SharedResolver,
    cache: RuntimeModuleCache,
    requester: Option<ModuleId>,
) -> Result<crate::Table> {
    let env = lua.create_table()?;
    let metatable = lua.create_table()?;
    metatable.raw_set("__index", lua.globals())?;
    env.set_metatable(Some(metatable))?;
    env.raw_set(
        "require",
        resolver_require_function(lua, resolver, cache, requester)?,
    )?;
    Ok(env)
}

fn module_name(module: &ModuleSource) -> String {
    module
        .path()
        .map(|path| format!("@{}", path.display()))
        .unwrap_or_else(|| format!("={}", module.id()))
}

unsafe extern "C-unwind" fn lua_collectgarbage(state: *mut ffi::lua_State) -> c_int {
    let option = ffi::luaL_optstring(state, 1, cstr!("collect"));
    let option = CStr::from_ptr(option);
    let arg = ffi::luaL_optinteger(state, 2, 0);
    let is_sandboxed = (*ExtraData::get(state)).sandboxed;
    match option.to_str() {
        Ok("collect") if !is_sandboxed => {
            ffi::lua_gc(state, ffi::LUA_GCCOLLECT, 0);
            0
        }
        Ok("stop") if !is_sandboxed => {
            ffi::lua_gc(state, ffi::LUA_GCSTOP, 0);
            0
        }
        Ok("restart") if !is_sandboxed => {
            ffi::lua_gc(state, ffi::LUA_GCRESTART, 0);
            0
        }
        Ok("count") => {
            let kbytes = ffi::lua_gc(state, ffi::LUA_GCCOUNT, 0) as ffi::lua_Number;
            let kbytes_rem = ffi::lua_gc(state, ffi::LUA_GCCOUNTB, 0) as ffi::lua_Number;
            ffi::lua_pushnumber(state, kbytes + kbytes_rem / 1024.0);
            1
        }
        Ok("step") if !is_sandboxed => {
            let res = ffi::lua_gc(state, ffi::LUA_GCSTEP, arg as _);
            ffi::lua_pushboolean(state, res);
            1
        }
        Ok("isrunning") if !is_sandboxed => {
            let res = ffi::lua_gc(state, ffi::LUA_GCISRUNNING, 0);
            ffi::lua_pushboolean(state, res);
            1
        }
        _ => ffi::luaL_error(state, cstr!("collectgarbage called with invalid option")),
    }
}

unsafe extern "C-unwind" fn lua_loadstring(state: *mut ffi::lua_State) -> c_int {
    callback_error_ext(state, ptr::null_mut(), false, move |extra, nargs| {
        let rawlua = (*extra).raw_luau();
        let (chunk, chunk_name) = <(String, Option<String>)>::from_stack_args(
            nargs,
            1,
            Some("loadstring"),
            &rawlua.ctx(),
        )?;
        let chunk_name = chunk_name.as_deref().unwrap_or("=(loadstring)");
        (rawlua.lua())
            .load(chunk)
            .name(chunk_name)
            .text_mode()
            .into_function()?
            .push_into_stack(&rawlua.ctx())?;
        Ok(1)
    })
}

mod heap_dump;
