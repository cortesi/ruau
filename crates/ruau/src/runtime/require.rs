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
pub type SharedResolver = Rc<dyn ModuleResolver>;
/// Per-resolver module cache shared across requesters.
pub type RuntimeModuleCache = Rc<RefCell<HashMap<ModuleId, RuntimeModuleState>>>;

/// Runtime loading state for one resolved module.
#[derive(Clone)]
pub enum RuntimeModuleState {
    /// The module is currently executing.
    Loading,
    /// The module finished and returned this cached value.
    Loaded(Value),
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
    if !module.is_executable() {
        return Err(Error::runtime(format!(
            "module is not executable: {}; register declaration-only modules with ModuleInterfaceSet",
            module.id()
        )));
    }
    let module_id = module.id().clone();

    {
        let cache = cache.borrow();
        match cache.get(&module_id) {
            Some(RuntimeModuleState::Loaded(value)) => return Ok(value.clone()),
            Some(RuntimeModuleState::Loading) => {
                return Err(Error::runtime(format!(
                    "cyclic module require: {module_id}"
                )));
            }
            None => {}
        }
    }

    cache
        .borrow_mut()
        .insert(module_id.clone(), RuntimeModuleState::Loading);

    let env = match resolver_environment(
        lua,
        Rc::clone(&resolver),
        Rc::clone(&cache),
        Some(module_id.clone()),
    ) {
        Ok(env) => env,
        Err(error) => {
            cache.borrow_mut().remove(&module_id);
            return Err(error);
        }
    };
    let result = lua
        .load(module.source())
        .name(module_name(&module))
        .environment(env)
        .call::<MultiValue>(())
        .await;
    let mut values = match result {
        Ok(values) => values,
        Err(error) => {
            cache.borrow_mut().remove(&module_id);
            return Err(error);
        }
    };

    if values.len() > 1 {
        cache.borrow_mut().remove(&module_id);
        return Err(Error::runtime("module must return a single value"));
    }

    let value = values.pop_front().unwrap_or(Value::Boolean(true));
    cache
        .borrow_mut()
        .insert(module_id, RuntimeModuleState::Loaded(value.clone()));
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
