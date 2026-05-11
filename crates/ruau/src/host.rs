//! Host API registration with paired analyzer definitions.

use std::{
    error::Error as StdError,
    fmt::{self, Write as _},
    rc::Rc,
    result::Result as StdResult,
};

use crate::{
    FromLuauMulti, Function, IntoLuauMulti, Luau, Result,
    analyzer::{AnalysisError, Checker},
};

/// Deferred runtime installation callback.
type Installer = Box<dyn Fn(&Luau) -> Result<()>>;
/// Factory for namespace function values.
type FunctionFactory = Box<dyn Fn(&Luau) -> Result<Function>>;

/// Runtime host registrations plus matching `.d.luau` definitions.
#[derive(Default)]
pub struct HostApi {
    /// Concatenated `.d.luau` definitions.
    definitions: String,
    /// Runtime installation callbacks.
    installers: Vec<Installer>,
    /// Top-level globals declared by definitions whose ownership is known.
    declared_globals: Vec<String>,
    /// Top-level globals installed by runtime callbacks whose ownership is known.
    installed_globals: Vec<String>,
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

    /// Adds analyzer definitions for one known top-level global without installing it.
    ///
    /// Use this when the declaration source is authored elsewhere but should still participate in
    /// checked-host drift detection.
    #[must_use]
    pub fn add_definition_for(
        mut self,
        global: impl Into<String>,
        definition: impl AsRef<str>,
    ) -> Self {
        self.push_definition(definition.as_ref());
        push_unique(&mut self.declared_globals, global.into());
        self
    }

    /// Adds a runtime installer for one known top-level global without adding definitions.
    ///
    /// This is useful for declaration-file-backed hosts where the `.d.luau` source and runtime
    /// installation are assembled separately.
    #[must_use]
    pub fn add_installer<F>(mut self, global: impl Into<String>, installer: F) -> Self
    where
        F: Fn(&Luau) -> Result<()> + 'static,
    {
        push_unique(&mut self.installed_globals, global.into());
        self.installers.push(Box::new(installer));
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
        self.push_installed_definition(&name, definition.as_ref());
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
        self.push_installed_definition(&name, definition.as_ref());
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
    /// Namespace and function names must be Luau identifiers. Function signature strings should be
    /// the function-type form Luau expects inside a
    /// table type, e.g. `"(s: string) -> ()"` or `"(a: number, b: number) -> number"`.
    ///
    /// # Panics
    ///
    /// Panics if the namespace name is not a Luau identifier or if any nested function or
    /// namespace registered by `build` has an invalid name or function signature shape.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::error::Error;
    /// # use ruau::{HostApi, Luau, Result as LuauResult};
    /// # fn print_fn(_: &Luau, _: String) -> LuauResult<()> { Ok(()) }
    /// # fn clear_fn(_: &Luau, _: ()) -> LuauResult<()> { Ok(()) }
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// HostApi::new().try_namespace("term", |ns| {
    ///     ns.try_function("print", print_fn, "(s: string) -> ()")?;
    ///     ns.try_function("clear", clear_fn, "() -> ()")?;
    ///     Ok(())
    /// })?;
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    #[track_caller]
    pub fn namespace<F>(self, name: impl Into<String>, build: F) -> Self
    where
        F: FnOnce(&mut HostNamespace),
    {
        self.try_namespace(name, |ns| {
            build(ns);
            Ok(())
        })
        .expect("host namespace should be valid")
    }

    /// Registers a namespaced host table and returns validation errors instead of panicking.
    pub fn try_namespace<F>(
        mut self,
        name: impl Into<String>,
        build: F,
    ) -> StdResult<Self, HostApiError>
    where
        F: FnOnce(&mut HostNamespace) -> StdResult<(), HostApiError>,
    {
        let name = name.into();
        check_luau_identifier("host namespace", &name)?;
        let mut ns = HostNamespace::default();
        build(&mut ns)?;
        let mut declaration = format!("declare {name}: ");
        ns.write_table_type(&mut declaration);
        self.push_installed_definition(&name, &declaration);

        let installer_ns = ns;
        self.installers.push(Box::new(move |lua| {
            let table = installer_ns.build_table(lua)?;
            lua.globals().set(name.as_str(), table)
        }));
        Ok(self)
    }

    /// Returns all analyzer definitions registered on this host API.
    #[must_use]
    pub fn definitions(&self) -> &str {
        &self.definitions
    }

    /// Returns known top-level globals declared by this host API.
    pub fn declared_globals(&self) -> impl Iterator<Item = &str> {
        self.declared_globals.iter().map(String::as_str)
    }

    /// Returns known top-level globals installed by this host API.
    pub fn installed_globals(&self) -> impl Iterator<Item = &str> {
        self.installed_globals.iter().map(String::as_str)
    }

    /// Installs this host API's `.d.luau` declarations into a [`Checker`].
    ///
    /// Mirrors [`HostApi::install`], which installs the runtime registrations into a [`Luau`] VM.
    pub fn install_definitions(&self, checker: &mut Checker) -> StdResult<(), AnalysisError> {
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

    /// Appends one definition and records the matching runtime global.
    fn push_installed_definition(&mut self, name: &str, definition: &str) {
        self.push_definition(definition);
        push_unique(&mut self.declared_globals, name.to_owned());
        push_unique(&mut self.installed_globals, name.to_owned());
    }
}

/// Error returned by fallible host API builders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostApiError {
    /// A namespace or function name is not a Luau identifier.
    InvalidIdentifier {
        /// Human-readable context for the invalid name.
        kind: &'static str,
        /// Rejected name.
        name: String,
    },
    /// A function signature is not shaped like a Luau function type.
    InvalidFunctionSignature {
        /// Rejected signature.
        signature: String,
    },
}

impl fmt::Display for HostApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentifier { kind, name } => {
                write!(formatter, "{kind} name is not a Luau identifier: {name:?}")
            }
            Self::InvalidFunctionSignature { signature } => write!(
                formatter,
                "host function signature must be a Luau function type such as `(s: string) -> ()`: {signature:?}"
            ),
        }
    }
}

impl StdError for HostApiError {}

/// Builder for a host-side namespace registered via [`HostApi::namespace`].
///
/// `HostNamespace` collects function and nested-namespace entries in insertion order.
/// `HostApi` generates the matching `.d.luau` declaration and installs a read-only Luau
/// table at install time.
#[derive(Default)]
pub struct HostNamespace {
    /// Namespace entries in declaration and installation order.
    entries: Vec<Entry>,
}

/// Entry in a host namespace table.
enum Entry {
    /// Function entry with its Luau type signature and runtime factory.
    Function {
        /// Field name.
        name: String,
        /// Luau function type signature.
        signature: String,
        /// Runtime function factory.
        factory: FunctionFactory,
    },
    /// Nested namespace entry.
    Namespace {
        /// Field name.
        name: String,
        /// Nested namespace contents.
        ns: HostNamespace,
    },
}

impl HostNamespace {
    /// Adds a function to this namespace.
    ///
    /// `signature` is the Luau function-type signature inside a table type, e.g.
    /// `"(s: string) -> ()"`.
    ///
    /// # Panics
    ///
    /// Panics if `name` is not a Luau identifier or if `signature` is not shaped like a Luau
    /// function type.
    #[track_caller]
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
        self.try_function(name, func, signature)
            .expect("host function should be valid")
    }

    /// Adds a function to this namespace and returns validation errors instead of panicking.
    pub fn try_function<F, A, R>(
        &mut self,
        name: impl Into<String>,
        func: F,
        signature: impl Into<String>,
    ) -> StdResult<&mut Self, HostApiError>
    where
        F: Fn(&Luau, A) -> Result<R> + 'static,
        A: FromLuauMulti + 'static,
        R: IntoLuauMulti + 'static,
    {
        let name = name.into();
        let signature = signature.into();
        check_luau_identifier("host function", &name)?;
        check_function_signature(&signature)?;
        let func = Rc::new(func);
        self.entries.push(Entry::Function {
            name,
            signature,
            factory: Box::new(move |lua| {
                let func = Rc::clone(&func);
                lua.create_function(move |lua, args| func(lua, args))
            }),
        });
        Ok(self)
    }

    /// Adds an async function to this namespace.
    ///
    /// # Panics
    ///
    /// Panics if `name` is not a Luau identifier or if `signature` is not shaped like a Luau
    /// function type.
    #[track_caller]
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
        self.try_async_function(name, func, signature)
            .expect("host async function should be valid")
    }

    /// Adds an async function and returns validation errors instead of panicking.
    pub fn try_async_function<F, A, R>(
        &mut self,
        name: impl Into<String>,
        func: F,
        signature: impl Into<String>,
    ) -> StdResult<&mut Self, HostApiError>
    where
        F: AsyncFn(&Luau, A) -> Result<R> + 'static,
        A: FromLuauMulti + 'static,
        R: IntoLuauMulti + 'static,
    {
        let name = name.into();
        let signature = signature.into();
        check_luau_identifier("host async function", &name)?;
        check_function_signature(&signature)?;
        let func = Rc::new(func);
        self.entries.push(Entry::Function {
            name,
            signature,
            factory: Box::new(move |lua| {
                let func = Rc::clone(&func);
                lua.create_async_function(async move |lua, args| func(lua, args).await)
            }),
        });
        Ok(self)
    }

    /// Adds a nested namespace to this namespace.
    ///
    /// # Panics
    ///
    /// Panics if `name` is not a Luau identifier or if `build` registers an invalid child.
    #[track_caller]
    pub fn namespace<F>(&mut self, name: impl Into<String>, build: F) -> &mut Self
    where
        F: FnOnce(&mut Self),
    {
        self.try_namespace(name, |ns| {
            build(ns);
            Ok(())
        })
        .expect("host namespace should be valid")
    }

    /// Adds a nested namespace and returns validation errors instead of panicking.
    pub fn try_namespace<F>(
        &mut self,
        name: impl Into<String>,
        build: F,
    ) -> StdResult<&mut Self, HostApiError>
    where
        F: FnOnce(&mut Self) -> StdResult<(), HostApiError>,
    {
        let name = name.into();
        check_luau_identifier("host namespace", &name)?;
        let mut child = Self::default();
        build(&mut child)?;
        self.entries.push(Entry::Namespace { name, ns: child });
        Ok(self)
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

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

fn check_luau_identifier(kind: &'static str, name: &str) -> StdResult<(), HostApiError> {
    if is_luau_identifier(name) {
        Ok(())
    } else {
        Err(HostApiError::InvalidIdentifier {
            kind,
            name: name.to_owned(),
        })
    }
}

fn is_luau_identifier(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !(first == b'_' || first.is_ascii_alphabetic()) {
        return false;
    }
    bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric()) && !is_luau_keyword(name)
}

fn is_luau_keyword(name: &str) -> bool {
    matches!(
        name,
        "and"
            | "break"
            | "do"
            | "else"
            | "elseif"
            | "end"
            | "false"
            | "for"
            | "function"
            | "if"
            | "in"
            | "local"
            | "nil"
            | "not"
            | "or"
            | "repeat"
            | "return"
            | "then"
            | "true"
            | "until"
            | "while"
            | "continue"
            | "export"
            | "type"
    )
}

fn check_function_signature(signature: &str) -> StdResult<(), HostApiError> {
    let signature = signature.trim();
    if signature.starts_with('(') && signature.contains("->") && !signature.contains('\0') {
        Ok(())
    } else {
        Err(HostApiError::InvalidFunctionSignature {
            signature: signature.to_owned(),
        })
    }
}
