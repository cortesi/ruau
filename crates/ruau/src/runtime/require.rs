use std::{cell::RefCell, collections::HashMap, rc::Rc};

use crate::{
    error::{Error, Result},
    function::Function,
    multi::MultiValue,
    resolver::{ModuleId, ModuleResolver, ModuleSource},
    state::Luau,
    value::Value,
};

/// Shared, single-threaded resolver handle used by the runtime `require` plumbing.
pub(crate) type SharedResolver = Rc<dyn ModuleResolver>;

/// Runtime loading state for one resolved module.
#[derive(Clone)]
enum RuntimeModuleState {
    /// The module is currently executing.
    Loading,
    /// The module finished and returned this cached value.
    Loaded(Value),
}

/// Per-resolver module cache shared across requester-specific `require` closures.
#[derive(Clone, Default)]
pub(crate) struct RuntimeModuleCache {
    inner: Rc<RefCell<HashMap<ModuleId, RuntimeModuleState>>>,
}

impl RuntimeModuleCache {
    /// Creates an empty module cache.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Returns a loaded module value, or an error when a cyclic load is in progress.
    fn loaded(&self, module_id: &ModuleId) -> Result<Option<Value>> {
        match self.inner.borrow().get(module_id) {
            Some(RuntimeModuleState::Loaded(value)) => Ok(Some(value.clone())),
            Some(RuntimeModuleState::Loading) => Err(Error::runtime(format!(
                "cyclic module require: {module_id}"
            ))),
            None => Ok(None),
        }
    }

    /// Marks a module as currently loading.
    fn mark_loading(&self, module_id: ModuleId) {
        self.inner
            .borrow_mut()
            .insert(module_id, RuntimeModuleState::Loading);
    }

    /// Stores a loaded module result.
    fn mark_loaded(&self, module_id: ModuleId, value: Value) {
        self.inner
            .borrow_mut()
            .insert(module_id, RuntimeModuleState::Loaded(value));
    }

    /// Removes a module from the cache after load failure.
    fn remove(&self, module_id: &ModuleId) {
        self.inner.borrow_mut().remove(module_id);
    }

    /// Starts loading a module and removes the loading entry again unless it is committed.
    fn start_loading(&self, module_id: ModuleId) -> LoadingModuleGuard {
        self.mark_loading(module_id.clone());
        LoadingModuleGuard {
            cache: self.clone(),
            module_id: Some(module_id),
        }
    }
}

/// Transactional guard for a module cache entry in the `Loading` state.
struct LoadingModuleGuard {
    cache: RuntimeModuleCache,
    module_id: Option<ModuleId>,
}

impl LoadingModuleGuard {
    fn mark_loaded(mut self, value: Value) {
        if let Some(module_id) = self.module_id.take() {
            self.cache.mark_loaded(module_id, value);
        }
    }
}

impl Drop for LoadingModuleGuard {
    fn drop(&mut self) {
        if let Some(module_id) = &self.module_id {
            self.cache.remove(module_id);
        }
    }
}

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

    fn install_module_resolver(&self, resolver: SharedResolver) -> Result<()> {
        let cache = RuntimeModuleCache::new();
        let require = resolver_require_function(self, resolver, cache, None)?;
        self.globals().raw_set("require", require)
    }
}

/// Builds a `require` function that resolves through `resolver` and caches results by `ModuleId`.
fn resolver_require_function(
    lua: &Luau,
    resolver: SharedResolver,
    cache: RuntimeModuleCache,
    requester: Option<ModuleId>,
) -> Result<Function> {
    lua.create_async_function(async move |lua, specifier: String| {
        let resolver = Rc::clone(&resolver);
        let cache = cache.clone();
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
    if !module.is_executable() {
        return Err(Error::runtime(format!(
            "module is not executable: {}; register declaration-only modules with ModuleInterfaceSet",
            module.id()
        )));
    }
    let module_id = module.id().clone();

    if let Some(value) = cache.loaded(&module_id)? {
        return Ok(value);
    }

    let loading = cache.start_loading(module_id.clone());

    let env = resolver_environment(lua, Rc::clone(&resolver), cache.clone(), Some(module_id))?;
    let result = lua
        .load(module.source())
        .name(module_name(&module))
        .environment(env)
        .call::<MultiValue>(())
        .await;
    let mut values = result?;

    if values.len() > 1 {
        return Err(Error::runtime("module must return a single value"));
    }

    let value = values.pop_front().unwrap_or(Value::Boolean(true));
    loading.mark_loaded(value.clone());
    Ok(value)
}

/// Builds an environment table whose `__index` proxies globals and whose `require` resolves
/// through `resolver`, used for both runtime child-module envs and checked-load chunks.
pub(crate) fn resolver_environment(
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
