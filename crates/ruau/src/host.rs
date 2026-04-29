//! Host API registration with paired analyzer definitions.
#![allow(clippy::missing_docs_in_private_items)]

use std::rc::Rc;

use crate::{
    FromLuauMulti, Function, IntoLuauMulti, Luau, Result,
    analyzer::{AnalysisError, Checker},
};

type Installer = Box<dyn Fn(&Luau) -> Result<()>>;

/// Runtime host registrations plus matching `.d.luau` definitions.
#[derive(Default)]
pub struct HostApi {
    definitions: String,
    installers: Vec<Installer>,
}

impl HostApi {
    /// Creates an empty host API bundle.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds analyzer definitions without installing runtime functions.
    #[must_use]
    pub fn definition(mut self, definition: impl AsRef<str>) -> Self {
        self.push_definition(definition.as_ref());
        self
    }

    /// Registers a global function and its full analyzer definition.
    #[must_use]
    pub fn global_function<F, A, R>(
        mut self,
        name: impl Into<String>,
        func: F,
        definition: impl AsRef<str>,
    ) -> Self
    where
        F: Fn(&Luau, A) -> Result<R> + Clone + 'static,
        A: FromLuauMulti + 'static,
        R: IntoLuauMulti + 'static,
    {
        let name = name.into();
        let func = Rc::new(func);
        self.push_definition(definition.as_ref());
        self.installers.push(Box::new(move |lua| {
            let func = Rc::clone(&func);
            let function: Function = lua.create_function(move |lua, args| func(lua, args))?;
            lua.globals().set(name.as_str(), function)
        }));
        self
    }

    /// Registers a global async function and its full analyzer definition.
    #[must_use]
    pub fn global_async_function<F, A, R>(
        mut self,
        name: impl Into<String>,
        func: F,
        definition: impl AsRef<str>,
    ) -> Self
    where
        F: AsyncFn(&Luau, A) -> Result<R> + Clone + 'static,
        A: FromLuauMulti + 'static,
        R: IntoLuauMulti + 'static,
    {
        let name = name.into();
        let func = Rc::new(func);
        self.push_definition(definition.as_ref());
        self.installers.push(Box::new(move |lua| {
            let func = Rc::clone(&func);
            let function: Function =
                lua.create_async_function(async move |lua, args| func(lua, args).await)?;
            lua.globals().set(name.as_str(), function)
        }));
        self
    }

    /// Returns all analyzer definitions registered on this host API.
    #[must_use]
    pub fn definitions(&self) -> &str {
        &self.definitions
    }

    /// Loads this host API's definitions into a checker.
    pub fn add_definitions_to(
        &self,
        checker: &mut Checker,
    ) -> std::result::Result<(), AnalysisError> {
        checker.add_definitions(self.definitions())
    }

    /// Installs runtime registrations into a Luau VM.
    ///
    /// Installation borrows the bundle so the same host API can be installed into multiple VMs.
    pub fn install(&self, lua: &Luau) -> Result<()> {
        for installer in &self.installers {
            installer(lua)?;
        }
        Ok(())
    }

    /// Appends one normalized definition block.
    fn push_definition(&mut self, definition: &str) {
        let definition = definition.trim();
        if !definition.is_empty() {
            self.definitions.push_str(definition);
            self.definitions.push('\n');
        }
    }
}
