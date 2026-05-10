//! High-level checked host composition.
//!
//! This module keeps the lower-level building blocks visible while giving embedders one place to
//! wire host definitions, runtime installers, requireable interfaces, implementation checks, and
//! checked loading.
//!
//! # Binding contract
//!
//! `CheckedHost` treats Luau declarations and Rust runtime installation as two halves of one host
//! contract:
//!
//! - top-level globals declared through [`HostApi`] definitions must also be installed at runtime;
//! - top-level globals installed through [`HostApi`] installers or [`HostPreamble`] exports must
//!   also have declarations when their ownership is tracked;
//! - declaration-file-backed hosts can use [`HostApi::add_definition_for`] plus
//!   [`HostApi::add_installer`] when a hand-written `.d.luau` file is the source of truth;
//! - preambles must return a table whose named fields are copied to globals by
//!   [`CheckedHost::install_runtime`];
//! - requireable declaration modules belong in [`ModuleInterfaceSet`] through
//!   [`CheckedHost::with_interface`], while executable modules still come from a
//!   [`ModuleResolver`] snapshot at runtime.
//!
//! Function signatures in `.d.luau` are the public contract. Rust closures remain responsible for
//! converting values through the ordinary `FromLuau*` and `IntoLuau*` traits; `CheckedHost` does
//! not infer Rust closure types from Luau schema text. Async functions, captured Rust state, handle
//! tables, and `self` methods are therefore modeled by the runtime installer and checked against
//! the declaration only at the Luau boundary. If a host shape cannot be represented as a top-level
//! global, returned preamble export, or requireable module interface, keep that policy in the
//! embedder and expose only the checkable boundary through this module.

use std::{collections::BTreeSet, result::Result as StdResult};

use thiserror::Error;

use crate::{
    Chunk, HostApi, Luau, Result, Table, Value,
    analyzer::{AnalysisError, CheckResult, Checker, ModuleInterface, ModuleInterfaceSet},
    resolver::{ModuleId, ModuleResolveError, ModuleResolver, ResolverSnapshot},
};

/// A host-owned Luau preamble that returns helper globals to install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPreamble {
    /// Name used in diagnostics and chunk labels.
    name: String,
    /// Luau source evaluated at install time.
    source: String,
    /// Returned table fields copied into globals.
    exports: Vec<String>,
}

impl HostPreamble {
    /// Creates a preamble that returns a table of exported helper globals.
    pub fn new(
        name: impl Into<String>,
        source: impl Into<String>,
        exports: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            name: name.into(),
            source: source.into(),
            exports: exports.into_iter().map(Into::into).collect(),
        }
    }

    /// Returns exported global names.
    pub fn exports(&self) -> impl Iterator<Item = &str> {
        self.exports.iter().map(String::as_str)
    }
}

/// Checked host surface assembled from definitions, installers, and interfaces.
#[derive(Default)]
pub struct CheckedHost {
    /// Runtime host declarations and installers.
    host_api: HostApi,
    /// Requireable host interfaces and implementation modules.
    interfaces: ModuleInterfaceSet,
    /// Runtime preambles that install globals from returned helper tables.
    preambles: Vec<HostPreamble>,
}

impl CheckedHost {
    /// Creates an empty checked host.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a checked host from an existing [`HostApi`].
    #[must_use]
    pub fn from_host_api(host_api: HostApi) -> Self {
        Self {
            host_api,
            ..Self::default()
        }
    }

    /// Returns the underlying host API bundle.
    #[must_use]
    pub fn host_api(&self) -> &HostApi {
        &self.host_api
    }

    /// Returns the interface set used for requireable host modules.
    #[must_use]
    pub fn interfaces(&self) -> &ModuleInterfaceSet {
        &self.interfaces
    }

    /// Mutably borrows the interface set for advanced catalog construction.
    pub fn interfaces_mut(&mut self) -> &mut ModuleInterfaceSet {
        &mut self.interfaces
    }

    /// Replaces the host API bundle.
    #[must_use]
    pub fn with_host_api(mut self, host_api: HostApi) -> Self {
        self.host_api = host_api;
        self
    }

    /// Inserts a declaration interface.
    pub fn with_interface(
        mut self,
        specifier: impl Into<String>,
        source: impl Into<String>,
    ) -> StdResult<Self, AnalysisError> {
        self.interfaces.insert(specifier, source)?;
        Ok(self)
    }

    /// Inserts a pre-resolved interface.
    #[must_use]
    pub fn with_interface_value(mut self, interface: ModuleInterface) -> Self {
        self.interfaces.insert_interface(interface);
        self
    }

    /// Inserts implementation source as a requireable virtual module.
    #[must_use]
    pub fn with_implementation(
        mut self,
        specifier: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        self.interfaces.insert_implementation(specifier, source);
        self
    }

    /// Adds a runtime preamble that returns helper globals.
    #[must_use]
    pub fn with_preamble(mut self, preamble: HostPreamble) -> Self {
        self.preambles.push(preamble);
        self
    }

    /// Checks known declaration/runtime bindings for drift.
    pub fn validate_bindings(&self) -> StdResult<(), CheckedHostError> {
        let declared = self
            .host_api
            .declared_globals()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        let mut installed = self
            .host_api
            .installed_globals()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        for preamble in &self.preambles {
            installed.extend(preamble.exports.iter().cloned());
        }

        if let Some(global) = declared.difference(&installed).next() {
            return Err(CheckedHostError::DeclaredButNotInstalled(global.clone()));
        }
        if let Some(global) = installed.difference(&declared).next() {
            return Err(CheckedHostError::InstalledButNotDeclared(global.clone()));
        }
        Ok(())
    }

    /// Installs host definitions into a checker after validating known bindings.
    pub fn install_definitions(&self, checker: &mut Checker) -> StdResult<(), CheckedHostError> {
        self.validate_bindings()?;
        self.host_api.install_definitions(checker)?;
        Ok(())
    }

    /// Installs runtime host globals and preamble exports into a Luau VM.
    pub async fn install_runtime(&self, lua: &Luau) -> Result<()> {
        self.host_api.install(lua)?;
        for preamble in &self.preambles {
            let helpers: Table = lua
                .load(preamble.source.as_str())
                .name(preamble.name.as_str())
                .eval()
                .await?;
            for export in &preamble.exports {
                let value: Value = helpers.get(export.as_str())?;
                if value.is_nil() {
                    return Err(crate::Error::runtime(format!(
                        "host preamble `{}` did not export `{export}`",
                        preamble.name
                    )));
                }
                lua.globals().set(export.as_str(), value)?;
            }
        }
        Ok(())
    }

    /// Type-checks a script against this host's definitions and interfaces.
    pub async fn check_script(
        &self,
        checker: &mut Checker,
        source: &str,
    ) -> StdResult<CheckResult, CheckedHostError> {
        self.install_definitions(checker)?;
        Ok(checker
            .check_with_interfaces(source, &self.interfaces)
            .await?)
    }

    /// Type-checks implementation source against a declaration interface.
    pub async fn check_implementation(
        &self,
        checker: &mut Checker,
        impl_source: &str,
        impl_module_id: &ModuleId,
        declaration_specifier: &str,
    ) -> StdResult<CheckResult, CheckedHostError> {
        self.install_definitions(checker)?;
        Ok(checker
            .check_implementation(
                impl_source,
                impl_module_id,
                &self.interfaces,
                declaration_specifier,
            )
            .await?)
    }

    /// Resolves, type-checks with host interfaces, and loads a root module.
    pub async fn checked_load_resolved<R>(
        &self,
        lua: &Luau,
        checker: &mut Checker,
        resolver: &R,
        root: impl Into<ModuleId>,
    ) -> StdResult<Chunk<'static>, CheckedHostError>
    where
        R: ModuleResolver + ?Sized,
    {
        self.install_definitions(checker)?;
        let snapshot = ResolverSnapshot::resolve(resolver, root).await?;
        Ok(lua
            .checked_load_with_interfaces(checker, snapshot, &self.interfaces)
            .await?)
    }
}

/// Error returned while validating or using a [`CheckedHost`].
#[derive(Debug, Error)]
pub enum CheckedHostError {
    /// A declaration was tracked, but no runtime installer was tracked for it.
    #[error("host global `{0}` is declared but not installed")]
    DeclaredButNotInstalled(String),
    /// A runtime installer was tracked, but no declaration was tracked for it.
    #[error("host global `{0}` is installed but not declared")]
    InstalledButNotDeclared(String),
    /// Analyzer setup or checking failed.
    #[error(transparent)]
    Analysis(#[from] AnalysisError),
    /// Module resolution failed before checked loading.
    #[error(transparent)]
    Resolve(#[from] ModuleResolveError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::InMemoryResolver;

    #[test]
    fn validate_bindings_reports_declared_without_installer() {
        let host = CheckedHost::from_host_api(
            HostApi::new().add_definition_for("progress", "declare function progress(): ()"),
        );

        assert!(matches!(
            host.validate_bindings(),
            Err(CheckedHostError::DeclaredButNotInstalled(global)) if global == "progress"
        ));
    }

    #[tokio::test]
    async fn preamble_exports_count_as_installed_globals() -> Result<()> {
        let host = CheckedHost::from_host_api(
            HostApi::new().add_definition_for("helper", "declare function helper(): string"),
        )
        .with_preamble(HostPreamble::new(
            "test:preamble",
            "return { helper = function() return 'ok' end }",
            ["helper"],
        ));
        host.validate_bindings().expect("bindings");

        let lua = Luau::new();
        host.install_runtime(&lua).await?;
        let value: String = lua.load("return helper()").eval().await?;
        assert_eq!(value, "ok");
        Ok(())
    }

    #[tokio::test]
    async fn checked_host_surfaces_implementation_conformance() {
        let host = CheckedHost::new()
            .with_interface("demo", "export type Module = { value: number }")
            .expect("interface");
        let mut checker = Checker::new().expect("checker");
        let ok = host
            .check_implementation(
                &mut checker,
                "return { value = 1 }",
                &ModuleId::new("demo_impl"),
                "demo",
            )
            .await
            .expect("check");
        assert!(ok.is_ok(), "{ok:#?}");

        let bad = host
            .check_implementation(
                &mut checker,
                "return { value = 'bad' }",
                &ModuleId::new("bad_impl"),
                "demo",
            )
            .await
            .expect("check");
        assert!(bad.has_errors(), "{bad:#?}");
    }

    #[tokio::test]
    async fn checked_host_loads_resolved_graph() -> Result<()> {
        let host = CheckedHost::new();
        let lua = Luau::new();
        let resolver = InMemoryResolver::new()
            .with_module("main", "return require('dep').value")
            .with_module("dep", "return { value = 'ok' }");
        let mut checker = Checker::new().expect("checker");
        let value: String = host
            .checked_load_resolved(&lua, &mut checker, &resolver, "main")
            .await
            .expect("checked load")
            .eval()
            .await?;
        assert_eq!(value, "ok");
        Ok(())
    }
}
