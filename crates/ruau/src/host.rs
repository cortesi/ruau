//! Host API registration with paired analyzer definitions.
#![allow(clippy::missing_docs_in_private_items)]

use std::{fmt::Write as _, rc::Rc};

use crate::{
    FromLuauMulti, Function, IntoLuauMulti, Luau, Result,
    analyzer::{AnalysisError, Checker},
};

type Installer = Box<dyn Fn(&Luau) -> Result<()>>;
type FunctionFactory = Box<dyn Fn(&Luau) -> Result<Function>>;

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
    pub fn add_definition(mut self, definition: impl AsRef<str>) -> Self {
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
        F: Fn(&Luau, A) -> Result<R> + 'static,
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
        F: AsyncFn(&Luau, A) -> Result<R> + 'static,
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

    /// Registers a namespaced bundle of host functions under a single global name.
    ///
    /// The closure receives a [`HostNamespace`] builder; functions and nested namespaces
    /// added through it are bundled into a Luau table assigned to the named global. The
    /// matching `.d.luau` declaration (a single `declare <name>: { ... }` block) is generated
    /// automatically from the function signatures supplied to
    /// [`HostNamespace::function`] and [`HostNamespace::async_function`].
    ///
    /// Function signature strings should be the function-type form Luau expects inside a
    /// table type, e.g. `"(s: string) -> ()"` or `"(a: number, b: number) -> number"`.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use ruau::{HostApi, Luau, Result};
    /// # fn print_fn(_: &Luau, _: String) -> Result<()> { Ok(()) }
    /// # fn clear_fn(_: &Luau, _: ()) -> Result<()> { Ok(()) }
    /// HostApi::new().namespace("term", |ns| {
    ///     ns.function("print", print_fn, "(s: string) -> ()");
    ///     ns.function("clear", clear_fn, "() -> ()");
    /// });
    /// ```
    #[must_use]
    pub fn namespace<F>(mut self, name: impl Into<String>, build: F) -> Self
    where
        F: FnOnce(&mut HostNamespace),
    {
        let name = name.into();
        let mut ns = HostNamespace::default();
        build(&mut ns);

        // Append the declaration text as a single `declare <name>: { ... }` block.
        let mut declaration = format!("declare {name}: ");
        ns.write_table_type(&mut declaration);
        self.push_definition(&declaration);

        // Schedule the runtime install: build the table, mark it read-only, set the global.
        let installer_ns = ns;
        self.installers.push(Box::new(move |lua| {
            let table = installer_ns.build_table(lua)?;
            lua.globals().set(name.as_str(), table)
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

/// Builder for a host-side namespace registered via [`HostApi::namespace`].
///
/// `HostNamespace` collects function and nested-namespace entries in insertion order.
/// `HostApi` generates the matching `.d.luau` declaration and installs a read-only Luau
/// table at install time.
#[derive(Default)]
pub struct HostNamespace {
    entries: Vec<Entry>,
}

enum Entry {
    Function {
        name: String,
        signature: String,
        factory: FunctionFactory,
    },
    Namespace {
        name: String,
        ns: HostNamespace,
    },
}

impl HostNamespace {
    /// Adds a function to this namespace.
    ///
    /// `signature` is the Luau function-type signature inside a table type, e.g.
    /// `"(s: string) -> ()"`.
    pub fn function<F, A, R>(
        &mut self,
        name: impl Into<String>,
        func: F,
        signature: impl Into<String>,
    ) -> &mut Self
    where
        F: Fn(&Luau, A) -> Result<R> + 'static,
        A: FromLuauMulti + 'static,
        R: IntoLuauMulti + 'static,
    {
        let func = Rc::new(func);
        self.entries.push(Entry::Function {
            name: name.into(),
            signature: signature.into(),
            factory: Box::new(move |lua| {
                let func = Rc::clone(&func);
                lua.create_function(move |lua, args| func(lua, args))
            }),
        });
        self
    }

    /// Adds an async function to this namespace.
    pub fn async_function<F, A, R>(
        &mut self,
        name: impl Into<String>,
        func: F,
        signature: impl Into<String>,
    ) -> &mut Self
    where
        F: AsyncFn(&Luau, A) -> Result<R> + 'static,
        A: FromLuauMulti + 'static,
        R: IntoLuauMulti + 'static,
    {
        let func = Rc::new(func);
        self.entries.push(Entry::Function {
            name: name.into(),
            signature: signature.into(),
            factory: Box::new(move |lua| {
                let func = Rc::clone(&func);
                lua.create_async_function(async move |lua, args| func(lua, args).await)
            }),
        });
        self
    }

    /// Adds a nested namespace to this namespace.
    pub fn namespace<F>(&mut self, name: impl Into<String>, build: F) -> &mut Self
    where
        F: FnOnce(&mut Self),
    {
        let mut child = Self::default();
        build(&mut child);
        self.entries.push(Entry::Namespace {
            name: name.into(),
            ns: child,
        });
        self
    }

    /// Writes the `{ ... }` Luau type body for this namespace into `out`.
    fn write_table_type(&self, out: &mut String) {
        out.push_str("{ ");
        for (i, entry) in self.entries.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            match entry {
                Entry::Function {
                    name, signature, ..
                } => {
                    write!(out, "{name}: {signature}").expect("write to String");
                }
                Entry::Namespace { name, ns } => {
                    write!(out, "{name}: ").expect("write to String");
                    ns.write_table_type(out);
                }
            }
        }
        out.push_str(" }");
    }

    /// Builds a Luau table for this namespace, recursively constructing nested namespaces and
    /// function values, and marks the result read-only.
    fn build_table(&self, lua: &Luau) -> Result<crate::Table> {
        let table = lua.create_table()?;
        for entry in &self.entries {
            match entry {
                Entry::Function { name, factory, .. } => {
                    let function = factory(lua)?;
                    table.set(name.as_str(), function)?;
                }
                Entry::Namespace { name, ns } => {
                    let child = ns.build_table(lua)?;
                    table.set(name.as_str(), child)?;
                }
            }
        }
        table.set_readonly(true);
        Ok(table)
    }
}
