//! Declaration-coupled native-module authoring.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    future::Future,
    sync::Arc,
};

use ruau_declaration::{Alias, Builder as DeclarationBuilder, Class, Field, Type};
use ruau_vm::{
    AsyncHostContext, AsyncHostFunction, FromHostArgs, FromLuaMulti, HostArgCursor, HostCall,
    HostContext, HostFunction, HostFuture, HostReturn, HostType, IntoHostReturn, IntoLuaMulti,
    ModuleBinding, ModuleExport, MultiValue, NativeModule, RuntimeError, Scope, ScopedHostFunction,
    module::{Installer, InstallerExt, Table, Value},
};

/// Opt-in ready-made JSON module.
#[cfg(feature = "json-module")]
pub mod json;

/// One runtime binding coupled to its declaration visibility.
///
/// Public constructors require a declaration type. [`Self::hidden`] is the
/// only runtime-only form, so declaration-only or runtime-only public bindings
/// cannot be represented.
#[derive(Clone, Debug)]
pub struct Binding {
    runtime: ModuleBinding,
    declaration: BindingDeclaration,
    documentation: Option<String>,
}

#[derive(Clone, Debug)]
enum BindingDeclaration {
    Generated(Type),
    Existing,
    Hidden,
}

impl Binding {
    /// Installs a runtime binding whose declaration already exists in the
    /// builder's declaration source.
    #[must_use]
    pub fn declared(runtime: ModuleBinding) -> Self {
        let declaration = match runtime {
            ModuleBinding::Hidden(_) => BindingDeclaration::Hidden,
            ModuleBinding::Global | ModuleBinding::GlobalOverride | ModuleBinding::Library(_) => {
                BindingDeclaration::Existing
            }
        };
        Self {
            runtime,
            declaration,
            documentation: None,
        }
    }

    /// Declares and installs a fresh global.
    #[must_use]
    pub fn global(ty: Type) -> Self {
        Self {
            runtime: ModuleBinding::Global,
            declaration: BindingDeclaration::Generated(ty),
            documentation: None,
        }
    }

    /// Declares and explicitly overrides an existing global.
    #[must_use]
    pub fn global_override(ty: Type) -> Self {
        Self {
            runtime: ModuleBinding::GlobalOverride,
            declaration: BindingDeclaration::Generated(ty),
            documentation: None,
        }
    }

    /// Declares and installs one field in a named library table.
    #[must_use]
    pub fn library(name: impl Into<String>, ty: Type) -> Self {
        Self {
            runtime: ModuleBinding::library(name.into()),
            declaration: BindingDeclaration::Generated(ty),
            documentation: None,
        }
    }

    /// Installs a binding in a host-only named table with no public declaration.
    #[must_use]
    pub fn hidden(table: impl Into<String>) -> Self {
        Self {
            runtime: ModuleBinding::hidden(table.into()),
            declaration: BindingDeclaration::Hidden,
            documentation: None,
        }
    }

    /// Installs a global declared by the builder's existing declaration source.
    #[must_use]
    pub fn declared_global() -> Self {
        Self::declared(ModuleBinding::Global)
    }

    /// Overrides a global declared by the builder's existing declaration source.
    #[must_use]
    pub fn declared_global_override() -> Self {
        Self::declared(ModuleBinding::GlobalOverride)
    }

    /// Installs a library field declared by the builder's existing declaration source.
    #[must_use]
    pub fn declared_library(name: impl Into<String>) -> Self {
        Self::declared(ModuleBinding::library(name.into()))
    }

    /// Attaches documentation to the public declaration generated for this binding.
    ///
    /// Hidden bindings reject documentation when the module is built because they
    /// have no public declaration to carry it.
    #[must_use]
    pub fn doc(mut self, documentation: impl Into<String>) -> Self {
        self.documentation = Some(documentation.into());
        self
    }

    /// Returns the low-level runtime binding.
    #[must_use]
    pub const fn runtime(&self) -> &ModuleBinding {
        &self.runtime
    }

    /// Returns the public declaration type, or `None` for a hidden binding.
    #[must_use]
    pub const fn declaration(&self) -> Option<&Type> {
        match &self.declaration {
            BindingDeclaration::Generated(ty) => Some(ty),
            BindingDeclaration::Existing | BindingDeclaration::Hidden => None,
        }
    }
}

type Registration = Arc<dyn Fn(&mut dyn Installer) + Send + Sync>;

struct BindingEntry {
    name: String,
    binding: Binding,
    registration: Registration,
    private_inputs: Option<Arc<[String]>>,
}

struct HostTypeEntry {
    class: Option<Class>,
    host_type: Arc<HostType>,
}

struct SupportChunk {
    key: String,
    source: Vec<u8>,
}

/// Builds a native module whose declarations and runtime bindings are
/// registered together.
pub struct Builder {
    name: String,
    export: ModuleExport,
    existing_declaration: Option<String>,
    aliases: Vec<Alias>,
    classes: Vec<Class>,
    extern_types: Vec<String>,
    bindings: Vec<BindingEntry>,
    host_types: Vec<HostTypeEntry>,
    support_chunks: Vec<SupportChunk>,
}

impl fmt::Debug for Builder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Builder")
            .field("name", &self.name)
            .field("export", &self.export)
            .field(
                "has_existing_declaration",
                &self.existing_declaration.is_some(),
            )
            .field("aliases", &self.aliases.len())
            .field("classes", &self.classes.len())
            .field("extern_types", &self.extern_types.len())
            .field("bindings", &self.bindings.len())
            .field("host_types", &self.host_types.len())
            .field("support_chunks", &self.support_chunks.len())
            .finish()
    }
}

impl Builder {
    /// Starts a declaration-coupled module with global-only export behavior.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            export: ModuleExport::Globals,
            existing_declaration: None,
            aliases: Vec::new(),
            classes: Vec::new(),
            extern_types: Vec::new(),
            bindings: Vec::new(),
            host_types: Vec::new(),
            support_chunks: Vec::new(),
        }
    }

    /// Starts a declaration-coupled module from an existing hand-authored declaration.
    ///
    /// Registrations use the `declared_*` [`Binding`] constructors. The declaration remains
    /// byte-for-byte authoritative, while the normal [`Surface`](ruau_surface::Surface) module
    /// audit verifies its runtime parity when the generated module is installed.
    #[must_use]
    pub fn from_declaration(
        name: impl Into<String>,
        declaration: ruau_declaration::DeclarationSource<'_>,
    ) -> Self {
        let mut builder = Self::new(name);
        builder.existing_declaration = Some(declaration.render().into_owned());
        builder
    }

    /// Selects how the module table is exported.
    pub fn export(&mut self, export: ModuleExport) -> &mut Self {
        self.export = export;
        self
    }

    /// Adds a declaration-only type alias.
    pub fn alias(&mut self, alias: Alias) -> &mut Self {
        self.aliases.push(alias);
        self
    }

    /// Adds a declaration-only class.
    pub fn class(&mut self, class: Class) -> &mut Self {
        self.classes.push(class);
        self
    }

    /// Declares a type name supplied by another module or preamble.
    pub fn extern_ty(&mut self, name: impl Into<String>) -> &mut Self {
        self.extern_types.push(name.into());
        self
    }

    /// Registers a low-level host function with its declaration binding.
    pub fn function(
        &mut self,
        name: impl Into<String>,
        binding: Binding,
        function: Arc<dyn HostFunction>,
    ) -> &mut Self {
        let name = name.into();
        let registered_name = name.clone();
        let runtime = binding.runtime.clone();
        self.push_binding(
            name,
            binding,
            Arc::new(move |builder| {
                builder.function(
                    &registered_name,
                    runtime.clone(),
                    Box::new(SharedHostFunction(Arc::clone(&function))),
                );
            }),
        )
    }

    /// Registers a synchronous heap-free leaf function.
    pub fn leaf_function<F, A, R>(
        &mut self,
        name: impl Into<String>,
        binding: Binding,
        function: F,
    ) -> &mut Self
    where
        F: Fn(A) -> R + Send + Sync + 'static,
        A: FromHostArgs + 'static,
        R: IntoHostReturn + 'static,
    {
        let name = name.into();
        let registered_name = name.clone();
        let runtime = binding.runtime.clone();
        let function = Arc::new(function);
        self.push_binding(
            name,
            binding,
            Arc::new(move |builder| {
                let function = Arc::clone(&function);
                builder.leaf_function(&registered_name, runtime.clone(), move |args: A| {
                    function(args)
                });
            }),
        )
    }

    /// Registers a shared scoped host function.
    pub fn scoped_function(
        &mut self,
        name: impl Into<String>,
        binding: Binding,
        function: Arc<dyn ScopedHostFunction>,
    ) -> &mut Self {
        let name = name.into();
        let registered_name = name.clone();
        let runtime = binding.runtime.clone();
        self.push_binding(
            name,
            binding,
            Arc::new(move |builder| {
                builder.scoped_function(
                    &registered_name,
                    runtime.clone(),
                    Box::new(SharedScopedFunction(Arc::clone(&function))),
                );
            }),
        )
    }

    /// Registers a typed scoped function over owned argument and return shapes.
    pub fn scoped_function_fn<F, A, R>(
        &mut self,
        name: impl Into<String>,
        binding: Binding,
        function: F,
    ) -> &mut Self
    where
        F: for<'s> Fn(&Scope<'s>, A) -> Result<R, RuntimeError> + Send + Sync + 'static,
        A: for<'s> FromLuaMulti<'s> + 'static,
        R: for<'s> IntoLuaMulti<'s> + 'static,
    {
        let name = name.into();
        let registered_name = name.clone();
        let runtime = binding.runtime.clone();
        let function = Arc::new(function);
        self.push_binding(
            name,
            binding,
            Arc::new(move |builder| {
                let function = Arc::clone(&function);
                builder.scoped_function_fn(
                    &registered_name,
                    runtime.clone(),
                    move |scope: &Scope<'_>, args: A| function(scope, args),
                );
            }),
        )
    }

    /// Registers a lifetime-generic borrowed `MultiValue` closure.
    pub fn borrowed_function<F>(
        &mut self,
        name: impl Into<String>,
        binding: Binding,
        function: F,
    ) -> &mut Self
    where
        F: for<'s> Fn(&Scope<'s>, MultiValue<'s>) -> Result<MultiValue<'s>, RuntimeError>
            + Send
            + Sync
            + 'static,
    {
        let function: Arc<dyn ScopedHostFunction> = Arc::new(function);
        self.scoped_function(name, binding, function)
    }

    /// Registers a scoped function with named, cursor-based argument decoding.
    pub fn cursor_function<F>(
        &mut self,
        name: impl Into<String>,
        binding: Binding,
        function: F,
    ) -> &mut Self
    where
        F: for<'scope, 's> Fn(HostArgCursor<'scope, 's>) -> Result<MultiValue<'s>, RuntimeError>
            + Send
            + Sync
            + 'static,
    {
        let name = name.into();
        let registered_name = name.clone();
        let runtime = binding.runtime.clone();
        let function = Arc::new(function);
        self.push_binding(
            name,
            binding,
            Arc::new(move |builder| {
                let function = Arc::clone(&function);
                builder.cursor_function(&registered_name, runtime.clone(), move |args| {
                    function(args)
                });
            }),
        )
    }

    /// Registers a shared asynchronous scoped host function.
    pub fn async_function(
        &mut self,
        name: impl Into<String>,
        binding: Binding,
        function: Arc<dyn AsyncHostFunction>,
    ) -> &mut Self {
        let name = name.into();
        let registered_name = name.clone();
        let runtime = binding.runtime.clone();
        self.push_binding(
            name,
            binding,
            Arc::new(move |builder| {
                builder.async_function(
                    &registered_name,
                    runtime.clone(),
                    Box::new(SharedAsyncFunction(Arc::clone(&function))),
                );
            }),
        )
    }

    /// Registers an asynchronous typed closure.
    pub fn async_function_fn<F, A, Fut>(
        &mut self,
        name: impl Into<String>,
        binding: Binding,
        function: F,
    ) -> &mut Self
    where
        F: Fn(AsyncHostContext, A) -> Fut + Send + Sync + 'static,
        A: for<'s> FromLuaMulti<'s> + Send + 'static,
        Fut: Future<Output = Result<HostReturn, RuntimeError>> + Send + 'static,
    {
        let name = name.into();
        let registered_name = name.clone();
        let runtime = binding.runtime.clone();
        let function = Arc::new(function);
        self.push_binding(
            name,
            binding,
            Arc::new(move |builder| {
                let function = Arc::clone(&function);
                builder.async_function_fn(
                    &registered_name,
                    runtime.clone(),
                    move |context, args: A| function(context, args),
                );
            }),
        )
    }

    /// Registers a constant with its declaration binding.
    pub fn constant(
        &mut self,
        name: impl Into<String>,
        binding: Binding,
        value: impl Into<Value>,
    ) -> &mut Self {
        let name = name.into();
        let registered_name = name.clone();
        let runtime = binding.runtime.clone();
        let value = value.into();
        self.push_binding(
            name,
            binding,
            Arc::new(move |builder| {
                builder.constant(&registered_name, runtime.clone(), value.clone());
            }),
        )
    }

    /// Registers a string-keyed constant table with its declaration binding.
    pub fn table(&mut self, name: impl Into<String>, binding: Binding, table: Table) -> &mut Self {
        self.constant(name, binding, Value::Table(table))
    }

    /// Registers a value produced by trusted Luau source before sandboxing.
    pub fn source_value(
        &mut self,
        name: impl Into<String>,
        binding: Binding,
        source: impl Into<Vec<u8>>,
    ) -> &mut Self {
        let name = name.into();
        let registered_name = name.clone();
        let runtime = binding.runtime.clone();
        let source = Arc::<[u8]>::from(source.into());
        self.push_binding(
            name,
            binding,
            Arc::new(move |builder| {
                builder.source_value(&registered_name, runtime.clone(), &source);
            }),
        )
    }

    /// Registers a trusted Luau source value with fixed hidden-table inputs.
    ///
    /// The VM resolves every input key before it runs any source value and
    /// passes the original hidden tables as ordered positional arguments. An
    /// empty input list is equivalent to [`Self::source_value`].
    pub fn source_value_with<I, S>(
        &mut self,
        name: impl Into<String>,
        binding: Binding,
        source: impl Into<Vec<u8>>,
        private_inputs: I,
    ) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let name = name.into();
        let registered_name = name.clone();
        let runtime = binding.runtime.clone();
        let source = Arc::<[u8]>::from(source.into());
        let private_inputs = Arc::<[String]>::from(
            private_inputs
                .into_iter()
                .map(Into::into)
                .collect::<Vec<_>>(),
        );
        let registered_inputs = Arc::clone(&private_inputs);
        self.push_binding_with_private_inputs(
            name,
            binding,
            Arc::new(move |builder| {
                let private_inputs = registered_inputs
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>();
                builder.source_value_with(
                    &registered_name,
                    runtime.clone(),
                    &source,
                    &private_inputs,
                );
            }),
            Some(private_inputs),
        )
    }

    /// Registers a host userdata type and its declaration class together.
    pub fn host_type(&mut self, class: Class, host_type: Arc<HostType>) -> &mut Self {
        self.host_types.push(HostTypeEntry {
            class: Some(class),
            host_type,
        });
        self
    }

    /// Registers a host userdata type declared by the existing declaration source.
    pub fn declared_host_type(&mut self, host_type: Arc<HostType>) -> &mut Self {
        self.host_types.push(HostTypeEntry {
            class: None,
            host_type,
        });
        self
    }

    /// Registers a trusted hidden support chunk.
    pub fn support_chunk(
        &mut self,
        registry_key: impl Into<String>,
        source: impl Into<Vec<u8>>,
    ) -> &mut Self {
        self.support_chunks.push(SupportChunk {
            key: registry_key.into(),
            source: source.into(),
        });
        self
    }

    /// Validates and builds an ordinary native module trait object.
    ///
    /// Registration order is normalized by binding path. Public bindings
    /// always generate their declaration from the same entry that installs the
    /// runtime value.
    ///
    /// # Errors
    /// Returns [`BuildError`] for duplicate paths, invalid
    /// declaration models, incompatible require exports, duplicate support
    /// keys, empty or repeated source-value private inputs, or mismatched host
    /// type declarations.
    pub fn build(mut self) -> Result<Arc<dyn NativeModule>, BuildError> {
        if self.name.trim().is_empty() {
            return Err(BuildError::EmptyModuleName);
        }
        self.validate_declaration_mode()?;
        self.bindings.sort_by_key(binding_sort_key);
        reject_duplicate_bindings(&self.bindings)?;
        reject_invalid_private_inputs(&self.bindings)?;
        self.aliases
            .sort_by(|left, right| left.name.cmp(&right.name));
        self.classes
            .sort_by(|left, right| left.name.cmp(&right.name));
        self.extern_types.sort();
        self.extern_types.dedup();
        self.host_types
            .sort_by(|left, right| left.host_type.name().cmp(right.host_type.name()));
        self.support_chunks
            .sort_by(|left, right| left.key.cmp(&right.key));
        reject_duplicate_support_chunks(&self.support_chunks)?;

        let declaration = if let Some(declaration) = self.existing_declaration.take() {
            declaration
        } else {
            self.build_generated_declaration()?
        };

        let host_types = self
            .host_types
            .into_iter()
            .map(|entry| entry.host_type)
            .collect();

        Ok(Arc::new(GeneratedNativeModule {
            name: self.name,
            export: self.export,
            declaration,
            registrations: self
                .bindings
                .into_iter()
                .map(|entry| entry.registration)
                .collect(),
            host_types,
            support_chunks: self.support_chunks,
        }))
    }

    fn validate_declaration_mode(&self) -> Result<(), BuildError> {
        let existing = self.existing_declaration.is_some();
        if existing
            && (!self.aliases.is_empty()
                || !self.classes.is_empty()
                || !self.extern_types.is_empty())
        {
            return Err(BuildError::MixedDeclarationModes);
        }
        for entry in &self.bindings {
            match (&entry.binding.declaration, existing) {
                (BindingDeclaration::Generated(_), true)
                | (BindingDeclaration::Existing, false) => {
                    return Err(BuildError::MixedDeclarationModes);
                }
                (BindingDeclaration::Existing | BindingDeclaration::Hidden, true)
                | (BindingDeclaration::Generated(_) | BindingDeclaration::Hidden, false) => {}
            }
            if entry.binding.documentation.is_some() {
                match entry.binding.declaration {
                    BindingDeclaration::Generated(_) => {}
                    BindingDeclaration::Existing => {
                        return Err(BuildError::ExistingBindingHasDocumentation {
                            binding: binding_display(entry),
                        });
                    }
                    BindingDeclaration::Hidden => {
                        return Err(BuildError::HiddenBindingHasDocumentation {
                            binding: binding_display(entry),
                        });
                    }
                }
            }
        }
        if self
            .host_types
            .iter()
            .any(|entry| entry.class.is_some() == existing)
        {
            return Err(BuildError::MixedDeclarationModes);
        }
        Ok(())
    }

    fn build_generated_declaration(&self) -> Result<String, BuildError> {
        let mut declaration = DeclarationBuilder::new();
        for alias in &self.aliases {
            declaration.add_alias(alias.clone());
        }
        for class in &self.classes {
            declaration.add_class(class.clone());
        }
        for name in &self.extern_types {
            declaration.add_external_type(name.clone());
        }
        for entry in &self.host_types {
            let class = entry
                .class
                .as_ref()
                .expect("generated declaration mode has modeled host classes");
            if class.name.as_ref() != entry.host_type.name() {
                return Err(BuildError::HostTypeNameMismatch {
                    class: class.name.to_string(),
                    runtime: entry.host_type.name().to_owned(),
                });
            }
            declaration.add_class(class.clone());
        }

        let mut globals = BTreeMap::new();
        let mut libraries: BTreeMap<String, BTreeMap<String, DeclarationEntry>> = BTreeMap::new();
        let mut module_export_fields = BTreeMap::new();
        for entry in &self.bindings {
            let ty = match &entry.binding.declaration {
                BindingDeclaration::Generated(ty) => ty.clone(),
                BindingDeclaration::Existing => unreachable!("declaration mode validated"),
                BindingDeclaration::Hidden => continue,
            };
            let declaration_entry = DeclarationEntry {
                ty,
                documentation: entry.binding.documentation.clone(),
            };
            if !matches!(self.export, ModuleExport::Globals)
                && module_export_fields
                    .insert(entry.name.clone(), declaration_entry.clone())
                    .is_some()
            {
                return Err(BuildError::InvalidRequireExport {
                    reason: format!(
                        "more than one public binding exports member `{}`",
                        entry.name
                    ),
                });
            }
            match &entry.binding.runtime {
                ModuleBinding::Global | ModuleBinding::GlobalOverride => {
                    globals.insert(entry.name.clone(), declaration_entry);
                }
                ModuleBinding::Library(library) => {
                    libraries
                        .entry(library.to_string())
                        .or_default()
                        .insert(entry.name.clone(), declaration_entry);
                }
                ModuleBinding::Hidden(_) => {
                    return Err(BuildError::HiddenBindingHasDeclaration {
                        binding: binding_display(entry),
                    });
                }
            }
        }
        validate_export_shape(&self.name, self.export, &globals, &libraries)?;
        if !matches!(self.export, ModuleExport::Globals) {
            libraries.insert(self.name.clone(), module_export_fields);
        }
        for (name, entry) in globals {
            let mut global = ruau_declaration::Global::new(name, entry.ty);
            if let Some(documentation) = entry.documentation {
                global = global.doc(documentation);
            }
            declaration.add_global(global);
        }
        for (library, fields) in libraries {
            declaration.add_global(ruau_declaration::Global::new(
                library,
                Type::table(fields.into_iter().map(|(name, entry)| {
                    let mut field = Field::new(name, entry.ty);
                    if let Some(documentation) = entry.documentation {
                        field = field.doc(documentation);
                    }
                    field
                })),
            ));
        }
        Ok(declaration
            .build()
            .map_err(|error| BuildError::InvalidDeclaration(error.to_string()))?
            .render())
    }

    fn push_binding(
        &mut self,
        name: String,
        binding: Binding,
        registration: Registration,
    ) -> &mut Self {
        self.push_binding_with_private_inputs(name, binding, registration, None)
    }

    fn push_binding_with_private_inputs(
        &mut self,
        name: String,
        binding: Binding,
        registration: Registration,
        private_inputs: Option<Arc<[String]>>,
    ) -> &mut Self {
        self.bindings.push(BindingEntry {
            name,
            binding,
            registration,
            private_inputs,
        });
        self
    }
}

#[derive(Clone)]
struct DeclarationEntry {
    ty: Type,
    documentation: Option<String>,
}

struct GeneratedNativeModule {
    name: String,
    export: ModuleExport,
    declaration: String,
    registrations: Vec<Registration>,
    host_types: Vec<Arc<HostType>>,
    support_chunks: Vec<SupportChunk>,
}

impl NativeModule for GeneratedNativeModule {
    fn name(&self) -> &str {
        &self.name
    }

    fn declaration(&self) -> ruau_declaration::DeclarationSource<'_> {
        ruau_declaration::DeclarationSource::Text(&self.declaration)
    }

    fn export(&self) -> ModuleExport {
        self.export
    }

    fn install(&self, builder: &mut dyn Installer) {
        for registration in &self.registrations {
            registration(builder);
        }
        for host_type in &self.host_types {
            builder.shared_host_type(Arc::clone(host_type));
        }
        for support in &self.support_chunks {
            builder.support_chunk(&support.key, &support.source);
        }
    }
}

/// Validation failure while building a declaration-coupled native module.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BuildError {
    /// The module name was empty or whitespace-only.
    EmptyModuleName,
    /// Two entries registered the same runtime path.
    DuplicateBinding {
        /// Canonical runtime path.
        binding: String,
    },
    /// A hidden runtime binding unexpectedly carried a public declaration.
    HiddenBindingHasDeclaration {
        /// Canonical runtime path.
        binding: String,
    },
    /// A hidden runtime binding unexpectedly carried public documentation.
    HiddenBindingHasDocumentation {
        /// Canonical runtime path.
        binding: String,
    },
    /// An existing declaration binding tried to replace source-authored documentation.
    ExistingBindingHasDocumentation {
        /// Canonical runtime path.
        binding: String,
    },
    /// Generated and existing declaration inputs were combined in one module.
    MixedDeclarationModes,
    /// A require export did not define exactly the required module table shape.
    InvalidRequireExport {
        /// Human-readable reason.
        reason: String,
    },
    /// Two support chunks used the same registry key.
    DuplicateSupportChunk {
        /// Repeated registry key.
        key: String,
    },
    /// A source value named an empty private-input key.
    EmptyPrivateInput {
        /// Canonical runtime path of the source value.
        binding: String,
        /// Zero-based position of the invalid input.
        input_index: usize,
    },
    /// A source value repeated a private-input key.
    DuplicatePrivateInput {
        /// Canonical runtime path of the source value.
        binding: String,
        /// Repeated registry key.
        key: String,
        /// Zero-based position of the first occurrence.
        first_index: usize,
        /// Zero-based position of the repeated occurrence.
        input_index: usize,
    },
    /// The declaration class and runtime host type names differed.
    HostTypeNameMismatch {
        /// Declaration class name.
        class: String,
        /// Runtime host type name.
        runtime: String,
    },
    /// The generated typed declaration failed validation.
    InvalidDeclaration(String),
}

impl fmt::Display for BuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyModuleName => formatter.write_str("native module name cannot be empty"),
            Self::DuplicateBinding { binding } => {
                write!(formatter, "duplicate native module binding `{binding}`")
            }
            Self::HiddenBindingHasDeclaration { binding } => {
                write!(
                    formatter,
                    "hidden native module binding `{binding}` cannot have a public declaration"
                )
            }
            Self::HiddenBindingHasDocumentation { binding } => {
                write!(
                    formatter,
                    "hidden native module binding `{binding}` cannot have public documentation"
                )
            }
            Self::ExistingBindingHasDocumentation { binding } => write!(
                formatter,
                "existing native module binding `{binding}` keeps documentation in its declaration source"
            ),
            Self::MixedDeclarationModes => formatter.write_str(
                "native module builder cannot mix generated and existing declaration inputs",
            ),
            Self::InvalidRequireExport { reason } => {
                write!(formatter, "invalid native require export: {reason}")
            }
            Self::DuplicateSupportChunk { key } => {
                write!(formatter, "duplicate native module support chunk `{key}`")
            }
            Self::EmptyPrivateInput {
                binding,
                input_index,
            } => write!(
                formatter,
                "native module source value `{binding}` has an empty private input at position {}",
                input_index + 1
            ),
            Self::DuplicatePrivateInput {
                binding,
                key,
                first_index,
                input_index,
            } => write!(
                formatter,
                "native module source value `{binding}` repeats private input `{key}` at position {} (first used at position {})",
                input_index + 1,
                first_index + 1
            ),
            Self::HostTypeNameMismatch { class, runtime } => {
                write!(
                    formatter,
                    "host type declaration `{class}` does not match runtime type `{runtime}`"
                )
            }
            Self::InvalidDeclaration(error) => {
                write!(
                    formatter,
                    "invalid generated native module declaration: {error}"
                )
            }
        }
    }
}

impl Error for BuildError {}

fn binding_sort_key(entry: &BindingEntry) -> String {
    binding_display(entry)
}

fn binding_display(entry: &BindingEntry) -> String {
    match &entry.binding.runtime {
        ModuleBinding::Global => format!("global:{}", entry.name),
        ModuleBinding::GlobalOverride => format!("global-override:{}", entry.name),
        ModuleBinding::Library(library) => format!("library:{library}.{}", entry.name),
        ModuleBinding::Hidden(table) => format!("hidden:{table}.{}", entry.name),
    }
}

fn reject_duplicate_bindings(bindings: &[BindingEntry]) -> Result<(), BuildError> {
    let mut seen = BTreeSet::new();
    for entry in bindings {
        let path = match &entry.binding.runtime {
            ModuleBinding::Global | ModuleBinding::GlobalOverride => {
                format!("global:{}", entry.name)
            }
            ModuleBinding::Library(library) => format!("library:{library}.{}", entry.name),
            ModuleBinding::Hidden(table) => format!("hidden:{table}.{}", entry.name),
        };
        if !seen.insert(path.clone()) {
            return Err(BuildError::DuplicateBinding { binding: path });
        }
    }
    Ok(())
}

fn reject_invalid_private_inputs(bindings: &[BindingEntry]) -> Result<(), BuildError> {
    for entry in bindings {
        let Some(private_inputs) = &entry.private_inputs else {
            continue;
        };
        let mut seen = BTreeMap::new();
        for (input_index, key) in private_inputs.iter().enumerate() {
            if key.is_empty() {
                return Err(BuildError::EmptyPrivateInput {
                    binding: binding_display(entry),
                    input_index,
                });
            }
            if let Some(first_index) = seen.insert(key.as_str(), input_index) {
                return Err(BuildError::DuplicatePrivateInput {
                    binding: binding_display(entry),
                    key: key.clone(),
                    first_index,
                    input_index,
                });
            }
        }
    }
    Ok(())
}

fn reject_duplicate_support_chunks(support_chunks: &[SupportChunk]) -> Result<(), BuildError> {
    for pair in support_chunks.windows(2) {
        if pair[0].key == pair[1].key {
            return Err(BuildError::DuplicateSupportChunk {
                key: pair[0].key.clone(),
            });
        }
    }
    Ok(())
}

fn validate_export_shape<T>(
    module: &str,
    export: ModuleExport,
    globals: &BTreeMap<String, T>,
    libraries: &BTreeMap<String, BTreeMap<String, T>>,
) -> Result<(), BuildError> {
    match export {
        ModuleExport::Globals => Ok(()),
        ModuleExport::Require => {
            if globals.is_empty() && libraries.len() == 1 && libraries.contains_key(module) {
                Ok(())
            } else {
                Err(BuildError::InvalidRequireExport {
                    reason: format!(
                        "require-only module `{module}` must declare only library `{module}`"
                    ),
                })
            }
        }
        ModuleExport::Both => {
            if globals.is_empty() && libraries.len() == 1 && libraries.contains_key(module) {
                Ok(())
            } else {
                Err(BuildError::InvalidRequireExport {
                    reason: format!(
                        "module `{module}` with both exports must declare only library `{module}`"
                    ),
                })
            }
        }
    }
}

struct SharedHostFunction(Arc<dyn HostFunction>);

impl HostFunction for SharedHostFunction {
    fn call(&self, context: &mut dyn HostContext) -> HostCall {
        self.0.call(context)
    }
}

struct SharedScopedFunction(Arc<dyn ScopedHostFunction>);

impl ScopedHostFunction for SharedScopedFunction {
    fn call<'s>(
        &self,
        scope: &Scope<'s>,
        args: MultiValue<'s>,
    ) -> Result<MultiValue<'s>, RuntimeError> {
        self.0.call(scope, args)
    }
}

struct SharedAsyncFunction(Arc<dyn AsyncHostFunction>);

impl AsyncHostFunction for SharedAsyncFunction {
    fn call<'s>(
        &self,
        context: AsyncHostContext,
        scope: &Scope<'s>,
        args: MultiValue<'s>,
    ) -> Result<HostFuture, RuntimeError> {
        self.0.call(context, scope, args)
    }
}
