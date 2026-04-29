//! Host API registration with paired analyzer definitions.
#![allow(clippy::missing_docs_in_private_items)]

use crate::{FromLuauMulti, Function, IntoLuauMulti, Luau, Result, analyzer::Checker};

type Installer = Box<dyn FnOnce(&Luau) -> Result<()> + Send>;

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

    /// Registers a global function and its analyzer definition.
    #[must_use]
    pub fn global_function<F, A, R>(
        mut self,
        name: impl Into<String>,
        func: F,
        definition: impl AsRef<str>,
    ) -> Self
    where
        F: Fn(&Luau, A) -> Result<R> + Send + 'static,
        A: FromLuauMulti + 'static,
        R: IntoLuauMulti + 'static,
    {
        let name = name.into();
        let definition = definition.as_ref().trim();
        if definition.starts_with("declare ") {
            self.definitions.push_str(definition);
        } else {
            self.definitions
                .push_str(&format!("declare function {name}{definition}"));
        }
        self.definitions.push('\n');
        self.installers.push(Box::new(move |lua| {
            let function: Function = lua.create_function(func)?;
            lua.globals().set(name, function)
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
    ) -> std::result::Result<(), crate::analyzer::Error> {
        checker.add_definitions(self.definitions())
    }

    /// Installs runtime registrations into a Luau VM.
    pub fn install(self, lua: &Luau) -> Result<()> {
        for installer in self.installers {
            installer(lua)?;
        }
        Ok(())
    }
}
