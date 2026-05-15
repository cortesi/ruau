//! Host API registration with paired analyzer definitions.

use std::{
    collections::BTreeSet,
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
    /// Concatenated manual and generated `.d.luau` definitions.
    definitions: String,
    /// Manually supplied `.d.luau` definitions.
    manual_definitions: String,
    /// Generated host-function declarations and installers.
    generated: HostEntries,
    /// Runtime installation callbacks.
    installers: Vec<Installer>,
    /// Top-level globals declared by definitions whose ownership is known.
    declared_globals: BTreeSet<String>,
    /// Top-level globals installed by runtime callbacks whose ownership is known.
    installed_globals: BTreeSet<String>,
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
        self.declared_globals.insert(global.into());
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
        self.installed_globals.insert(global.into());
        self.installers.push(Box::new(installer));
        self
    }

    /// Registers a host function by path and generates its analyzer declaration.
    ///
    /// Dotted paths create read-only namespace tables. For example, registering
    /// `term.echo` with the signature `"(msg: string) -> string"` installs
    /// `term.echo` at runtime and generates `declare term: { echo: (msg: string) -> string }`.
    ///
    /// # Panics
    ///
    /// Panics if any path segment is not a Luau identifier or if `signature` is not shaped like a
    /// Luau function type.
    #[must_use]
    #[track_caller]
    pub fn function<F, A, R>(
        self,
        path: impl Into<String>,
        func: F,
        signature: impl Into<String>,
    ) -> Self
    where
        F: Fn(&Luau, A) -> Result<R> + 'static,
        A: FromLuauMulti + 'static,
        R: IntoLuauMulti + 'static,
    {
        self.try_function(path, func, signature)
            .expect("host function should be valid")
    }

    /// Registers a host function by path and returns validation errors instead of panicking.
    pub fn try_function<F, A, R>(
        mut self,
        path: impl Into<String>,
        func: F,
        signature: impl Into<String>,
    ) -> StdResult<Self, HostApiError>
    where
        F: Fn(&Luau, A) -> Result<R> + 'static,
        A: FromLuauMulti + 'static,
        R: IntoLuauMulti + 'static,
    {
        let path = HostPath::parse("host function", path.into())?;
        let signature = signature.into();
        check_function_signature(&signature)?;
        let func = Rc::new(func);
        self.generated.insert_function(
            &path,
            signature,
            Box::new(move |lua| {
                let func = Rc::clone(&func);
                lua.create_function(move |lua, args| func(lua, args))
            }),
        );
        self.record_generated_global(path.global());
        Ok(self)
    }

    /// Registers an async host function by path and generates its analyzer declaration.
    ///
    /// See [`HostApi::function`] for path and signature rules.
    #[must_use]
    #[track_caller]
    pub fn async_function<F, A, R>(
        self,
        path: impl Into<String>,
        func: F,
        signature: impl Into<String>,
    ) -> Self
    where
        F: AsyncFn(&Luau, A) -> Result<R> + 'static,
        A: FromLuauMulti + 'static,
        R: IntoLuauMulti + 'static,
    {
        self.try_async_function(path, func, signature)
            .expect("host async function should be valid")
    }

    /// Registers an async host function by path and returns validation errors instead of panicking.
    pub fn try_async_function<F, A, R>(
        mut self,
        path: impl Into<String>,
        func: F,
        signature: impl Into<String>,
    ) -> StdResult<Self, HostApiError>
    where
        F: AsyncFn(&Luau, A) -> Result<R> + 'static,
        A: FromLuauMulti + 'static,
        R: IntoLuauMulti + 'static,
    {
        let path = HostPath::parse("host async function", path.into())?;
        let signature = signature.into();
        check_function_signature(&signature)?;
        let func = Rc::new(func);
        self.generated.insert_function(
            &path,
            signature,
            Box::new(move |lua| {
                let func = Rc::clone(&func);
                lua.create_async_function(async move |lua, args| func(lua, args).await)
            }),
        );
        self.record_generated_global(path.global());
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
        self.generated.install_globals(lua)?;
        Ok(())
    }

    /// Appends one normalized definition block.
    fn push_definition(&mut self, definition: &str) {
        let definition = definition.trim();
        if !definition.is_empty() {
            self.manual_definitions.push_str(definition);
            self.manual_definitions.push('\n');
            self.refresh_definitions();
        }
    }

    fn record_generated_global(&mut self, name: &str) {
        self.declared_globals.insert(name.to_owned());
        self.installed_globals.insert(name.to_owned());
        self.refresh_definitions();
    }

    fn refresh_definitions(&mut self) {
        self.definitions.clear();
        self.definitions.push_str(&self.manual_definitions);
        self.generated.write_declarations(&mut self.definitions);
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

#[derive(Default)]
struct HostEntries {
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
        entries: HostEntries,
    },
}

impl HostEntries {
    fn insert_function(&mut self, path: &HostPath, signature: String, factory: FunctionFactory) {
        self.insert_function_at(path.segments(), signature, factory);
    }

    fn insert_function_at(&mut self, path: &[String], signature: String, factory: FunctionFactory) {
        let Some((name, rest)) = path.split_first() else {
            unreachable!("host paths always contain at least one segment");
        };
        if rest.is_empty() {
            self.entries.push(Entry::Function {
                name: name.clone(),
                signature,
                factory,
            });
            return;
        }

        let child = self.namespace_entries_mut(name);
        child.insert_function_at(rest, signature, factory);
    }

    fn namespace_entries_mut(&mut self, name: &str) -> &mut Self {
        if let Some(index) = self.entries.iter().position(|entry| match entry {
            Entry::Namespace { name: existing, .. } => existing == name,
            Entry::Function { .. } => false,
        }) {
            match &mut self.entries[index] {
                Entry::Namespace { entries, .. } => return entries,
                Entry::Function { .. } => unreachable!(),
            }
        }

        self.entries.push(Entry::Namespace {
            name: name.to_owned(),
            entries: Self::default(),
        });
        match self.entries.last_mut().expect("just pushed namespace") {
            Entry::Namespace { entries, .. } => entries,
            Entry::Function { .. } => unreachable!(),
        }
    }

    fn write_declarations(&self, out: &mut String) {
        for entry in &self.entries {
            match entry {
                Entry::Function {
                    name, signature, ..
                } => {
                    writeln!(out, "declare {name}: {signature}").expect("write to String");
                }
                Entry::Namespace { name, entries } => {
                    write!(out, "declare {name}: ").expect("write to String");
                    entries.write_table_type(out);
                    out.push('\n');
                }
            }
        }
    }

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
                Entry::Namespace { name, entries } => {
                    write!(out, "{name}: ").expect("write to String");
                    entries.write_table_type(out);
                }
            }
        }
        out.push_str(" }");
    }

    fn install_globals(&self, lua: &Luau) -> Result<()> {
        for entry in &self.entries {
            match entry {
                Entry::Function { name, factory, .. } => {
                    lua.globals().set(name.as_str(), factory(lua)?)?;
                }
                Entry::Namespace { name, entries } => {
                    lua.globals()
                        .set(name.as_str(), entries.build_table(lua)?)?;
                }
            }
        }
        Ok(())
    }

    fn build_table(&self, lua: &Luau) -> Result<crate::Table> {
        let table = lua.create_table()?;
        for entry in &self.entries {
            match entry {
                Entry::Function { name, factory, .. } => {
                    let function = factory(lua)?;
                    table.set(name.as_str(), function)?;
                }
                Entry::Namespace { name, entries } => {
                    let child = entries.build_table(lua)?;
                    table.set(name.as_str(), child)?;
                }
            }
        }
        table.set_readonly(true);
        Ok(table)
    }
}

struct HostPath {
    segments: Vec<String>,
}

impl HostPath {
    fn parse(kind: &'static str, path: String) -> StdResult<Self, HostApiError> {
        let segments: Vec<String> = path.split('.').map(str::to_owned).collect();
        if segments.is_empty() {
            return Err(HostApiError::InvalidIdentifier { kind, name: path });
        }
        for segment in &segments {
            check_luau_identifier(kind, segment)?;
        }
        Ok(Self { segments })
    }

    fn global(&self) -> &str {
        &self.segments[0]
    }

    fn segments(&self) -> &[String] {
        &self.segments
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
