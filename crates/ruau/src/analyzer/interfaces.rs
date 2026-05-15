//! Host module interface catalog support.

use std::collections::BTreeMap;

use super::{
    AnalysisError, CheckOptions, Checker, Diagnostic, ModuleSchema, VirtualModule,
    extract_module_schema, schema::checker_source_for_interface,
};
use crate::resolver::ModuleSource;

/// How an interface source contributes to type checking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModuleInterfaceKind {
    /// `.d.luau` declaration source; `schema` is populated.
    #[default]
    Declaration,
    /// `.luau` implementation source; the analyzer infers the exported type.
    Implementation,
}

/// Parsed, reusable interface entry for one require specifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleInterface {
    /// Require specifier used by scripts, for example `rust/cargo`.
    specifier: String,
    /// Original `.d.luau` source.
    source: String,
    /// Diagnostics produced while validating this interface.
    diagnostics: Vec<Diagnostic>,
    /// Rich schema extracted from the declaration source.
    schema: ModuleSchema,
    /// Generated normal Luau module source used by the checker.
    checker_source: String,
    /// Whether this interface can be required as a value.
    requireable: bool,
    /// Whether the interface is declaration or implementation source.
    kind: ModuleInterfaceKind,
}

impl ModuleInterface {
    /// Builds a declaration-backed interface from source.
    fn declaration(
        specifier: String,
        source: String,
        diagnostics: Vec<Diagnostic>,
    ) -> Result<Self, AnalysisError> {
        let schema = extract_module_schema(&source)?;
        let checker_source = checker_source_for_interface(&schema, &source)?;
        let requireable = schema.root.is_some();
        Ok(Self {
            specifier,
            source,
            diagnostics,
            schema,
            checker_source,
            requireable,
            kind: ModuleInterfaceKind::Declaration,
        })
    }

    /// Builds an implementation-backed interface from source.
    fn implementation(specifier: String, source: String) -> Self {
        Self {
            specifier,
            checker_source: source.clone(),
            source,
            diagnostics: Vec::new(),
            schema: ModuleSchema::default(),
            requireable: true,
            kind: ModuleInterfaceKind::Implementation,
        }
    }

    /// Returns the require specifier used by scripts, for example `rust/cargo`.
    #[must_use]
    pub fn specifier(&self) -> &str {
        &self.specifier
    }

    /// Returns the original source used to create this interface.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Returns diagnostics produced while validating this interface.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Returns the schema extracted from declaration source.
    #[must_use]
    pub fn schema(&self) -> &ModuleSchema {
        &self.schema
    }

    /// Returns the checker-facing Luau module source generated for this interface.
    #[must_use]
    pub fn checker_source(&self) -> &str {
        &self.checker_source
    }

    /// Returns whether this interface can be required as a value.
    #[must_use]
    pub const fn is_requireable(&self) -> bool {
        self.requireable
    }

    /// Returns whether the interface is declaration or implementation source.
    #[must_use]
    pub const fn kind(&self) -> ModuleInterfaceKind {
        self.kind
    }

    /// Returns this interface as a checker virtual module.
    #[must_use]
    fn virtual_module(&self) -> VirtualModule<'_> {
        VirtualModule {
            name: &self.specifier,
            source: &self.checker_source,
        }
    }
}

/// Named collection of typed module interfaces.
///
/// The set is plain owned data: checks borrow it, build virtual modules from its current
/// contents, and do not mutate it. Embedders can keep one set for a session and clone or replace
/// it when their catalog changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleInterfaceSet {
    interfaces: BTreeMap<String, ModuleInterface>,
}

impl ModuleInterfaceSet {
    /// Creates an empty interface set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts one interface source and returns the previous entry, if any.
    pub fn insert(
        &mut self,
        specifier: impl Into<String>,
        source: impl Into<String>,
    ) -> Result<Option<ModuleInterface>, AnalysisError> {
        let specifier = specifier.into();
        let source = source.into();
        let interface = ModuleInterface::declaration(specifier.clone(), source, Vec::new())?;
        Ok(self.interfaces.insert(specifier, interface))
    }

    /// Inserts implementation source as a requireable virtual module.
    ///
    /// Implementation interfaces have no declaration schema. The checker infers their exported
    /// shape from the source each time the containing interface set is used.
    pub fn insert_implementation(
        &mut self,
        specifier: impl Into<String>,
        source: impl Into<String>,
    ) -> Option<ModuleInterface> {
        let specifier = specifier.into();
        let source = source.into();
        let interface = ModuleInterface::implementation(specifier.clone(), source);
        self.interfaces.insert(specifier, interface)
    }

    /// Inserts a pre-resolved interface and returns the previous entry, if any.
    pub fn insert_interface(&mut self, interface: ModuleInterface) -> Option<ModuleInterface> {
        self.interfaces
            .insert(interface.specifier.clone(), interface)
    }

    /// Inserts a pre-resolved interface source.
    pub fn insert_source(
        &mut self,
        source: &ModuleSource,
    ) -> Result<Option<ModuleInterface>, AnalysisError> {
        self.insert(source.id().as_str().to_owned(), source.source().to_owned())
    }

    /// Inserts and validates one interface source with the supplied checker.
    pub async fn insert_checked(
        &mut self,
        checker: &mut Checker,
        specifier: impl Into<String>,
        source: impl Into<String>,
    ) -> Result<Option<ModuleInterface>, AnalysisError> {
        let specifier = specifier.into();
        let source = source.into();
        let mut interface = ModuleInterface::declaration(specifier.clone(), source, Vec::new())?;
        let result = checker
            .check_with_options(
                interface.checker_source(),
                CheckOptions {
                    module_name: Some(specifier.as_str()),
                    ..CheckOptions::default()
                },
            )
            .await?;
        interface.diagnostics = result.diagnostics;
        Ok(self.interfaces.insert(specifier, interface))
    }

    /// Returns the interface for one specifier.
    #[must_use]
    pub fn get(&self, specifier: &str) -> Option<&ModuleInterface> {
        self.interfaces.get(specifier)
    }

    /// Iterates interfaces in stable specifier order.
    pub fn interfaces(&self) -> impl Iterator<Item = &ModuleInterface> {
        self.interfaces.values()
    }

    /// Returns checker virtual modules for requireable interfaces.
    #[must_use]
    pub fn virtual_modules(&self) -> Vec<VirtualModule<'_>> {
        self.interfaces
            .values()
            .filter(|interface| interface.requireable)
            .map(ModuleInterface::virtual_module)
            .collect()
    }
}
