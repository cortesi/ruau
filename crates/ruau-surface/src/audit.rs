//! Host-module shape audit: validates that each native module's declaration
//! matches the bindings it registers at runtime, that no module collides with
//! surface builtins or other modules, and that declaration-only globals parse
//! and type-check.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use ruau_syntax::{
    Stat, TableProp, Type,
    parse::{Config, parse_with_config},
};
use ruau_typecheck::{
    Checker,
    builtins::{DefinitionModule, Environment, TypeScope},
    types::{Arena, TypeId},
    views::TypeView,
};
use ruau_vm::{
    HostFunction, HostType, ModuleBinding, ModuleExport, NativeModule, RuntimeCapabilities,
    module::{Callable, HostType as InstallHostType, Installer, Value},
};

use crate::{
    ConfigError, DeclarationGlobalSpec, builtin_environment_for,
    builtin_environment_for_with_definition_modules,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HostBindingKind {
    Function,
    /// Value produced by trusted source whose runtime shape is not available to the audit.
    Source,
    Value,
    Table,
}

#[derive(Clone, Debug)]
struct SourcePrivateInput {
    module: String,
    source_value: String,
    input_index: usize,
    key: String,
}

#[derive(Debug, Default)]
struct HostModuleShape {
    globals: BTreeMap<String, HostBindingKind>,
    libraries: BTreeMap<String, BTreeMap<String, HostBindingKind>>,
    library_roots: BTreeSet<String>,
    /// Script-visible module tables returned by native `require` exports.
    /// `Require` modules populate this without also installing a global;
    /// `Both` modules populate it alongside their ordinary bindings.
    module_exports: BTreeMap<String, BTreeMap<String, HostBindingKind>>,
    /// Globals registered with `ModuleBinding::GlobalOverride` — the explicit
    /// builtin-replacement opt-in. A subset of `globals` keys.
    overrides: BTreeSet<String>,
    /// Host-only (`ModuleBinding::Hidden`) tables and their members. These are
    /// never script-visible, so they take no part in the declared-shape match:
    /// the declaration must not declare a global for them, and contributes
    /// only types (aliases/classes) on their behalf.
    hidden: BTreeMap<String, BTreeMap<String, HostBindingKind>>,
    host_types: BTreeSet<String>,
    support_chunks: BTreeSet<String>,
    source_private_inputs: Vec<SourcePrivateInput>,
}

impl HostModuleShape {
    fn insert_global(
        &mut self,
        module: &str,
        name: &str,
        kind: HostBindingKind,
    ) -> Result<(), String> {
        match self.globals.insert(name.to_owned(), kind) {
            None => Ok(()),
            Some(previous)
                if previous == HostBindingKind::Table
                    && kind == HostBindingKind::Table
                    && self.library_roots.contains(name) =>
            {
                self.globals.insert(name.to_owned(), previous);
                Ok(())
            }
            Some(previous) => {
                self.globals.insert(name.to_owned(), previous);
                Err(format!("module {module} declares duplicate global {name}"))
            }
        }
    }

    fn ensure_library_root(&mut self, module: &str, library: &str) -> Result<(), String> {
        match self.globals.get(library).copied() {
            None => {
                self.globals
                    .insert(library.to_owned(), HostBindingKind::Table);
                self.library_roots.insert(library.to_owned());
                Ok(())
            }
            Some(HostBindingKind::Table) if self.library_roots.contains(library) => Ok(()),
            Some(HostBindingKind::Table) => Err(format!(
                "module {module} binds library root {library} over a table global"
            )),
            Some(_) => Err(format!(
                "module {module} binds library root {library} over a non-table global"
            )),
        }
    }

    fn insert_library_member(
        &mut self,
        module: &str,
        library: &str,
        name: &str,
        kind: HostBindingKind,
    ) -> Result<(), String> {
        let members = self.libraries.entry(library.to_owned()).or_default();
        match members.insert(name.to_owned(), kind) {
            None => Ok(()),
            Some(previous) => {
                members.insert(name.to_owned(), previous);
                Err(format!(
                    "module {module} declares duplicate library binding {library}.{name}"
                ))
            }
        }
    }

    fn insert_module_export_member(
        &mut self,
        module: &str,
        name: &str,
        kind: HostBindingKind,
    ) -> Result<(), String> {
        let members = self.module_exports.entry(module.to_owned()).or_default();
        match members.insert(name.to_owned(), kind) {
            None => Ok(()),
            Some(previous) => {
                members.insert(name.to_owned(), previous);
                Err(format!(
                    "module {module} declares duplicate require export {module}.{name}"
                ))
            }
        }
    }

    fn insert_hidden_member(
        &mut self,
        module: &str,
        table: &str,
        name: &str,
        kind: HostBindingKind,
    ) -> Result<(), String> {
        if self.support_chunks.contains(table) {
            return Err(format!(
                "module {module} binds hidden table {table} over a support chunk"
            ));
        }
        let members = self.hidden.entry(table.to_owned()).or_default();
        match members.insert(name.to_owned(), kind) {
            None => Ok(()),
            Some(previous) => {
                members.insert(name.to_owned(), previous);
                Err(format!(
                    "module {module} registers duplicate hidden binding {table}.{name}"
                ))
            }
        }
    }

    fn insert_support_chunk(&mut self, module: &str, key: &str) -> Result<(), String> {
        if self.hidden.contains_key(key) {
            return Err(format!(
                "module {module} binds support chunk {key} over a hidden table"
            ));
        }
        if self.support_chunks.insert(key.to_owned()) {
            Ok(())
        } else {
            Err(format!(
                "module {module} registers duplicate support chunk {key}"
            ))
        }
    }

    fn insert_host_type(&mut self, module: &str, name: &str) -> Result<(), String> {
        if self.host_types.insert(name.to_owned()) {
            Ok(())
        } else {
            Err(format!(
                "module {module} registers duplicate host type {name}"
            ))
        }
    }

    fn collect_module_value_shape(
        &mut self,
        walk: ShapeWalk<'_>,
        prefix: &str,
        value: &Value,
    ) -> Result<(), String> {
        let Value::Table(table) = value else {
            return Ok(());
        };
        for entry in &table.entries {
            let path = walk.member_path(prefix, entry.name.as_ref());
            let kind = module_value_kind(&entry.value);
            self.insert_library_member(walk.module, walk.root, &path, kind)?;
            self.collect_module_value_shape(walk, &path, &entry.value)?;
        }
        Ok(())
    }

    fn collect_module_export_value_shape(
        &mut self,
        walk: ShapeWalk<'_>,
        prefix: &str,
        value: &Value,
    ) -> Result<(), String> {
        let Value::Table(table) = value else {
            return Ok(());
        };
        for entry in &table.entries {
            let path = walk.member_path(prefix, entry.name.as_ref());
            let kind = module_value_kind(&entry.value);
            self.insert_module_export_member(walk.module, &path, kind)?;
            self.collect_module_export_value_shape(walk, &path, &entry.value)?;
        }
        Ok(())
    }

    fn collect_declared_table_shape(
        &mut self,
        walk: ShapeWalk<'_>,
        prefix: &str,
        props: &[TableProp],
    ) -> Result<(), String> {
        for prop in props {
            let path = walk.member_path(prefix, prop.name.as_str());
            let kind = type_binding_kind(&prop.prop_type);
            self.insert_library_member(walk.module, walk.root, &path, kind)?;
            if let Some(props) = table_props(&prop.prop_type) {
                self.collect_declared_table_shape(walk, &path, props)?;
            }
        }
        Ok(())
    }

    fn merge_from(&mut self, module: &str, shape: &Self) -> Result<(), String> {
        for (global, kind) in &shape.globals {
            if self.globals.get(global) == Some(kind)
                && *kind == HostBindingKind::Table
                && self.library_roots.contains(global)
                && shape.library_roots.contains(global)
            {
                continue;
            }
            self.insert_global(module, global, *kind)?;
            if shape.library_roots.contains(global) {
                self.library_roots.insert(global.clone());
            }
        }
        for (library, members) in &shape.libraries {
            for (member, kind) in members {
                self.insert_library_member(module, library, member, *kind)?;
            }
        }
        for (export, members) in &shape.module_exports {
            if self.module_exports.contains_key(export) {
                return Err(format!("duplicate native require export {export}"));
            }
            for (member, kind) in members {
                self.insert_module_export_member(module, member, *kind)?;
            }
        }
        for (table, members) in &shape.hidden {
            if self.support_chunks.contains(table) {
                return Err(format!(
                    "hidden table {table} collides with a support chunk"
                ));
            }
            for (member, kind) in members {
                self.insert_hidden_member(module, table, member, *kind)?;
            }
        }
        for key in &shape.support_chunks {
            self.insert_support_chunk(module, key)?;
        }
        for host_type in &shape.host_types {
            self.insert_host_type(module, host_type)?;
        }
        self.source_private_inputs
            .extend(shape.source_private_inputs.iter().cloned());
        Ok(())
    }

    fn validate_source_private_inputs(&self) -> Result<(), (String, String)> {
        for input in &self.source_private_inputs {
            if self.support_chunks.contains(&input.key) {
                return Err((
                    input.module.clone(),
                    format!(
                        "source value {} private input {} names support chunk {} instead of a hidden module table",
                        input.source_value,
                        input.input_index + 1,
                        input.key
                    ),
                ));
            }
            let Some(members) = self.hidden.get(&input.key) else {
                return Err((
                    input.module.clone(),
                    format!(
                        "source value {} private input {} names missing hidden module table {}",
                        input.source_value,
                        input.input_index + 1,
                        input.key
                    ),
                ));
            };
            if members
                .values()
                .any(|kind| *kind == HostBindingKind::Source)
            {
                return Err((
                    input.module.clone(),
                    format!(
                        "source value {} private input {} names hidden table {} that is also populated by a source value",
                        input.source_value,
                        input.input_index + 1,
                        input.key
                    ),
                ));
            }
        }
        Ok(())
    }
}

struct HostModuleAuditBuilder {
    module: String,
    export: ModuleExport,
    shape: HostModuleShape,
    errors: Vec<String>,
}

impl HostModuleAuditBuilder {
    fn new(module: &str, export: ModuleExport) -> Self {
        Self {
            module: module.to_owned(),
            export,
            shape: HostModuleShape::default(),
            errors: Vec::new(),
        }
    }

    fn finish(self) -> Result<HostModuleShape, String> {
        if self.errors.is_empty() {
            Ok(self.shape)
        } else {
            Err(self.errors.join("; "))
        }
    }

    fn record_function(&mut self, name: &str, binding: &ModuleBinding) {
        if self.record_module_export(name, binding, HostBindingKind::Function) {
            return;
        }
        let result = match binding {
            ModuleBinding::Global => {
                self.shape
                    .insert_global(&self.module, name, HostBindingKind::Function)
            }
            ModuleBinding::GlobalOverride => self
                .shape
                .insert_global(&self.module, name, HostBindingKind::Function)
                .map(|()| {
                    self.shape.overrides.insert(name.to_owned());
                }),
            ModuleBinding::Library(library) => self
                .shape
                .ensure_library_root(&self.module, library)
                .and_then(|()| {
                    self.shape.insert_library_member(
                        &self.module,
                        library,
                        name,
                        HostBindingKind::Function,
                    )
                }),
            ModuleBinding::Hidden(table) => self.shape.insert_hidden_member(
                &self.module,
                table,
                name,
                HostBindingKind::Function,
            ),
        };
        if let Err(error) = result {
            self.errors.push(error);
        }
    }

    fn record_module_export(
        &mut self,
        name: &str,
        binding: &ModuleBinding,
        kind: HostBindingKind,
    ) -> bool {
        if matches!(binding, ModuleBinding::Hidden(_))
            || matches!(self.export, ModuleExport::Globals)
        {
            return false;
        }
        if let Err(error) = self
            .shape
            .insert_module_export_member(&self.module, name, kind)
        {
            self.errors.push(error);
        }
        self.export == ModuleExport::Require
    }

    fn record_source_value(&mut self, name: &str, binding: &ModuleBinding) {
        if self.record_module_export(name, binding, HostBindingKind::Source) {
            return;
        }
        let result = match binding {
            ModuleBinding::Global | ModuleBinding::GlobalOverride => {
                let result = self
                    .shape
                    .insert_global(&self.module, name, HostBindingKind::Source);
                if result.is_ok() && matches!(binding, ModuleBinding::GlobalOverride) {
                    self.shape.overrides.insert(name.to_owned());
                }
                result
            }
            ModuleBinding::Library(library) => self
                .shape
                .ensure_library_root(&self.module, library)
                .and_then(|()| {
                    self.shape.insert_library_member(
                        &self.module,
                        library,
                        name,
                        HostBindingKind::Source,
                    )
                }),
            ModuleBinding::Hidden(table) => {
                self.shape
                    .insert_hidden_member(&self.module, table, name, HostBindingKind::Source)
            }
        };
        if let Err(error) = result {
            self.errors.push(error);
        }
    }
}

impl Installer for HostModuleAuditBuilder {
    fn function(&mut self, name: &str, binding: ModuleBinding, _f: Box<dyn HostFunction>) {
        self.record_function(name, &binding);
    }

    fn host_callable(&mut self, name: &str, binding: ModuleBinding, _f: Callable) {
        self.record_function(name, &binding);
    }

    fn constant(&mut self, name: &str, binding: ModuleBinding, value: Value) {
        let kind = module_value_kind(&value);
        if !matches!(binding, ModuleBinding::Hidden(_))
            && !matches!(self.export, ModuleExport::Globals)
            && let Err(error) = self.shape.collect_module_export_value_shape(
                ShapeWalk {
                    module: &self.module,
                    root: &self.module,
                },
                name,
                &value,
            )
        {
            self.errors.push(error);
            return;
        }
        if self.record_module_export(name, &binding, kind) {
            return;
        }
        let result = match &binding {
            ModuleBinding::Global | ModuleBinding::GlobalOverride => {
                let overrides = matches!(binding, ModuleBinding::GlobalOverride);
                self.shape
                    .insert_global(&self.module, name, kind)
                    .and_then(|()| {
                        if overrides {
                            self.shape.overrides.insert(name.to_owned());
                        }
                        self.shape.collect_module_value_shape(
                            ShapeWalk {
                                module: &self.module,
                                root: name,
                            },
                            "",
                            &value,
                        )
                    })
            }
            ModuleBinding::Library(library) => self
                .shape
                .ensure_library_root(&self.module, library)
                .and_then(|()| {
                    self.shape
                        .insert_library_member(&self.module, library, name, kind)
                })
                .and_then(|()| {
                    self.shape.collect_module_value_shape(
                        ShapeWalk {
                            module: &self.module,
                            root: library.as_ref(),
                        },
                        name,
                        &value,
                    )
                }),
            // Hidden constants are host-facing only: record the member for
            // duplicate detection, but walk no nested shape — hidden bindings
            // carry no declaration obligation.
            ModuleBinding::Hidden(table) => {
                self.shape
                    .insert_hidden_member(&self.module, table, name, kind)
            }
        };
        if let Err(error) = result {
            self.errors.push(error);
        }
    }

    fn source_value(&mut self, name: &str, binding: ModuleBinding, _source: &[u8]) {
        self.record_source_value(name, &binding);
    }

    fn source_value_with(
        &mut self,
        name: &str,
        binding: ModuleBinding,
        _source: &[u8],
        private_inputs: &[&str],
    ) {
        self.record_source_value(name, &binding);
        for (input_index, key) in private_inputs.iter().enumerate() {
            if key.is_empty() {
                self.errors.push(format!(
                    "module {} source value {name} has an empty private input at position {}",
                    self.module,
                    input_index + 1
                ));
                continue;
            }
            if let Some(first_index) = private_inputs[..input_index]
                .iter()
                .position(|previous| previous == key)
            {
                self.errors.push(format!(
                    "module {} source value {name} repeats private input {key} at position {} (first used at position {})",
                    self.module,
                    input_index + 1,
                    first_index + 1
                ));
                continue;
            }
            self.shape.source_private_inputs.push(SourcePrivateInput {
                module: self.module.clone(),
                source_value: name.to_owned(),
                input_index,
                key: (*key).to_owned(),
            });
        }
    }

    fn host_type(&mut self, ty: InstallHostType) {
        let payload = ty.into_engine();
        let name = match payload.downcast::<HostType>() {
            Ok(ty) => ty.name().to_owned(),
            Err(payload) => match payload.downcast::<Arc<HostType>>() {
                Ok(ty) => ty.name().to_owned(),
                Err(_) => {
                    self.errors
                        .push("host_type payload was not an ruau-vm HostType".to_owned());
                    return;
                }
            },
        };
        if let Err(error) = self.shape.insert_host_type(&self.module, &name) {
            self.errors.push(error);
        }
    }

    fn support_chunk(&mut self, registry_key: &str, _source: &[u8]) {
        if let Err(error) = self.shape.insert_support_chunk(&self.module, registry_key) {
            self.errors.push(error);
        }
    }
}

fn module_value_kind(value: &Value) -> HostBindingKind {
    match value {
        Value::Array(_) | Value::Table(_) => HostBindingKind::Table,
        Value::Nil
        | Value::Boolean(_)
        | Value::Number(_)
        | Value::Integer(_)
        | Value::LightUserdata { .. }
        | Value::Bytes(_) => HostBindingKind::Value,
    }
}

/// The fixed coordinates of one shape-collection walk: the host module
/// being audited and the library root the walk descends from. The growing
/// member path travels as its own parameter so the three same-typed strings
/// cannot be swapped at a recursive call.
#[derive(Clone, Copy)]
struct ShapeWalk<'a> {
    module: &'a str,
    root: &'a str,
}

impl ShapeWalk<'_> {
    fn member_path(self, prefix: &str, name: &str) -> String {
        if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}.{name}")
        }
    }
}

pub struct HostModuleAudit {
    declarations: Vec<DefinitionModule>,
    shapes: Vec<(String, HostModuleShape)>,
    declared_names: Vec<(String, Vec<String>, Vec<String>)>,
}

impl HostModuleAudit {
    pub(crate) fn declarations(&self) -> &[DefinitionModule] {
        &self.declarations
    }
}

pub fn validate_host_modules(
    capabilities: &RuntimeCapabilities,
    modules: &[Arc<dyn NativeModule>],
) -> Result<HostModuleAudit, ConfigError> {
    let mut declarations = Vec::with_capacity(modules.len());
    let mut shapes = Vec::with_capacity(modules.len());
    let mut declared_names = Vec::with_capacity(modules.len());
    let mut all_bindings = HostModuleShape::default();
    let builtin_globals = runtime_capability_builtin_global_names(capabilities);
    for module in modules {
        let declaration = module.declaration().render();
        let declared = declared_module_audit(module.name(), &declaration).map_err(|reason| {
            ConfigError::InvalidHostModuleDeclaration {
                module: module.name().to_owned(),
                reason,
            }
        })?;
        let mut builder = HostModuleAuditBuilder::new(module.name(), module.export());
        module.install(&mut builder);
        let runtime =
            builder
                .finish()
                .map_err(|reason| ConfigError::InvalidHostModuleDeclaration {
                    module: module.name().to_owned(),
                    reason,
                })?;
        let expected = declared_runtime_shape(module.name(), module.export(), &declared.shape)
            .map_err(|reason| ConfigError::InvalidHostModuleDeclaration {
                module: module.name().to_owned(),
                reason,
            })?;
        let mismatch = host_module_shape_mismatch(&expected, &runtime);
        if !mismatch.is_empty() {
            return Err(ConfigError::InvalidHostModuleDeclaration {
                module: module.name().to_owned(),
                reason: mismatch,
            });
        }
        reject_surface_omitted_host_bindings(capabilities, module.name(), &runtime)?;
        reject_unflagged_builtin_collisions(&builtin_globals, module.name(), &runtime)?;
        all_bindings
            .merge_from(module.name(), &runtime)
            .map_err(|reason| ConfigError::InvalidHostModuleDeclaration {
                module: module.name().to_owned(),
                reason,
            })?;
        declarations.push(DefinitionModule::new(
            module.name().to_owned(),
            declaration.into_owned(),
        ));
        declared_names.push((
            module.name().to_owned(),
            declared.shape.globals.keys().cloned().collect(),
            declared.type_names,
        ));
        shapes.push((module.name().to_owned(), expected));
    }
    all_bindings
        .validate_source_private_inputs()
        .map_err(|(module, reason)| ConfigError::InvalidHostModuleDeclaration { module, reason })?;
    Ok(HostModuleAudit {
        declarations,
        shapes,
        declared_names,
    })
}

pub fn validate_declaration_modules(
    capabilities: &RuntimeCapabilities,
    declarations: &[DefinitionModule],
    host_modules: &HostModuleAudit,
) -> Result<(Arena, Environment), ConfigError> {
    let mut expected_globals = Vec::new();
    let mut expected_types = Vec::new();
    for ((module, globals, types), declaration) in
        host_modules.declared_names.iter().zip(declarations)
    {
        expected_globals.extend(globals.iter().map(|name| (module.clone(), name.clone())));
        // A module-scoped declaration keeps its type names private, so the
        // shared environment is not expected to install them.
        if declaration.type_scope == TypeScope::Ambient {
            expected_types.extend(types.iter().map(|name| (module.clone(), name.clone())));
        }
    }
    for declaration in declarations.iter().skip(host_modules.declarations.len()) {
        let module = declaration.name.as_ref();
        let declared =
            declared_module_audit(module, declaration.source.as_ref()).map_err(|reason| {
                ConfigError::InvalidDeclarationModule {
                    module: module.to_owned(),
                    reason,
                }
            })?;
        expected_globals.extend(
            declared
                .shape
                .globals
                .keys()
                .map(|name| (module.to_owned(), name.clone())),
        );
        // A module-scoped declaration keeps its type names private, so the
        // shared environment is not expected to install them.
        if declaration.type_scope == TypeScope::Ambient {
            expected_types.extend(
                declared
                    .type_names
                    .into_iter()
                    .map(|name| (module.to_owned(), name)),
            );
        }
    }

    let mut arena = Arena::new();
    let builtins =
        builtin_environment_for_with_definition_modules(capabilities, &mut arena, declarations)?;
    validate_host_module_declaration_types(host_modules, &arena, &builtins)?;
    for (module, global) in expected_globals {
        if builtins.global(&global).is_none() {
            return Err(ConfigError::InvalidDeclarationModule {
                module,
                reason: format!(
                    "declaration did not install global {global}; check its type annotations"
                ),
            });
        }
    }
    for (module, ty) in expected_types {
        if builtins.ty(&ty).is_none() {
            return Err(ConfigError::InvalidDeclarationModule {
                module,
                reason: format!(
                    "declaration did not install type {ty}; check its type annotations"
                ),
            });
        }
    }
    Ok((arena, builtins))
}

fn declared_runtime_shape(
    module: &str,
    export: ModuleExport,
    declared: &HostModuleShape,
) -> Result<HostModuleShape, String> {
    let mut shape = HostModuleShape {
        globals: declared.globals.clone(),
        libraries: declared.libraries.clone(),
        library_roots: declared.library_roots.clone(),
        module_exports: BTreeMap::new(),
        overrides: BTreeSet::new(),
        hidden: BTreeMap::new(),
        host_types: BTreeSet::new(),
        support_chunks: BTreeSet::new(),
        source_private_inputs: Vec::new(),
    };
    if export == ModuleExport::Globals {
        return Ok(shape);
    }
    let Some(kind) = shape.globals.get(module).copied() else {
        return Err(format!(
            "module export mode {export:?} requires declaration table `{module}`"
        ));
    };
    if kind != HostBindingKind::Table {
        return Err(format!(
            "module export mode {export:?} requires `{module}` to be declared as a table"
        ));
    }
    let members = shape.libraries.get(module).cloned().unwrap_or_default();
    shape.module_exports.insert(module.to_owned(), members);
    if export == ModuleExport::Require {
        shape.globals.remove(module);
        shape.libraries.remove(module);
        shape.library_roots.remove(module);
    }
    Ok(shape)
}

/// The builtin global names the checker environment defines for these runtime
/// capabilities before any host-module declaration is merged.
fn runtime_capability_builtin_global_names(capabilities: &RuntimeCapabilities) -> BTreeSet<String> {
    let mut arena = Arena::new();
    builtin_environment_for(capabilities, &mut arena)
        .globals()
        .map(|global| global.name.clone())
        .collect()
}

/// Global bindings are fail-closed about the surface's builtin set: a
/// plain `Global` colliding with a builtin requires the explicit
/// `GlobalOverride` opt-in, and an override must have a builtin to replace.
fn reject_unflagged_builtin_collisions(
    builtin_globals: &BTreeSet<String>,
    module: &str,
    shape: &HostModuleShape,
) -> Result<(), ConfigError> {
    for global in shape.globals.keys() {
        // A library root shared with a surface library (a module extending
        // `string`, say) is the documented library-extension path, not a
        // global replacement.
        if shape.library_roots.contains(global) {
            continue;
        }
        let collides = builtin_globals.contains(global);
        let overrides = shape.overrides.contains(global);
        if collides && !overrides {
            return Err(ConfigError::InvalidHostModuleDeclaration {
                module: module.to_owned(),
                reason: format!(
                    "global {global} collides with a surface builtin; replacing it \
                     requires the explicit ModuleBinding::GlobalOverride opt-in"
                ),
            });
        }
        if overrides && !collides {
            return Err(ConfigError::InvalidHostModuleDeclaration {
                module: module.to_owned(),
                reason: format!(
                    "global {global} is bound as an override, but the surface \
                     installs no builtin of that name to replace"
                ),
            });
        }
    }
    Ok(())
}

fn reject_surface_omitted_host_bindings(
    capabilities: &RuntimeCapabilities,
    module: &str,
    shape: &HostModuleShape,
) -> Result<(), ConfigError> {
    for library in capabilities.omitted_libraries() {
        let name = library.global_name();
        if shape.globals.contains_key(name) || shape.libraries.contains_key(name) {
            return Err(ConfigError::InvalidHostModuleDeclaration {
                module: module.to_owned(),
                reason: format!("binds omitted surface library {name}"),
            });
        }
    }
    Ok(())
}

pub fn host_module_manifest_version(
    modules: &[Arc<dyn NativeModule>],
    declarations: &[DefinitionModule],
) -> u64 {
    let mut hash = FNV1A64_OFFSET;
    for (index, declaration) in declarations.iter().enumerate() {
        fnv1a64_update(&mut hash, declaration.name.as_bytes());
        fnv1a64_update(&mut hash, b"\0");
        let export = modules
            .get(index)
            .map_or("DeclarationOnly", |module| match module.export() {
                ModuleExport::Globals => "Globals",
                ModuleExport::Require => "Require",
                ModuleExport::Both => "Both",
            });
        fnv1a64_update(&mut hash, export.as_bytes());
        fnv1a64_update(&mut hash, b"\0");
        fnv1a64_update(&mut hash, declaration.source.as_bytes());
        fnv1a64_update(&mut hash, b"\0");
    }
    hash
}

const FNV1A64_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV1A64_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a64_update(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(FNV1A64_PRIME);
    }
}

fn validate_host_module_declaration_types(
    audit: &HostModuleAudit,
    arena: &Arena,
    builtins: &Environment,
) -> Result<(), ConfigError> {
    for (module, shape) in &audit.shapes {
        for (global, kind) in &shape.globals {
            let Some(global_ty) = builtins.global(global).map(|global| global.ty) else {
                return Err(ConfigError::InvalidHostModuleDeclaration {
                    module: module.clone(),
                    reason: format!(
                        "declaration did not install global {global}; check its type annotations"
                    ),
                });
            };
            validate_declared_binding_type(
                module,
                arena,
                global_ty,
                *kind,
                &format!("global {global}"),
            )?;
        }
        for (library, members) in &shape.libraries {
            let Some(library_ty) = builtins.global(library).map(|global| global.ty) else {
                return Err(ConfigError::InvalidHostModuleDeclaration {
                    module: module.clone(),
                    reason: format!(
                        "declaration did not install library {library}; check its type annotations"
                    ),
                });
            };
            for (member, kind) in members {
                let Some(member_ty) = table_property_path_type(arena, library_ty, member) else {
                    return Err(ConfigError::InvalidHostModuleDeclaration {
                        module: module.clone(),
                        reason: format!(
                            "declaration did not install library binding {library}.{member}; check its type annotations"
                        ),
                    });
                };
                validate_declared_binding_type(
                    module,
                    arena,
                    member_ty,
                    *kind,
                    &format!("library binding {library}.{member}"),
                )?;
            }
        }
        for (export, members) in &shape.module_exports {
            let Some(export_ty) = builtins.global(export).map(|global| global.ty) else {
                return Err(ConfigError::InvalidHostModuleDeclaration {
                    module: module.clone(),
                    reason: format!(
                        "declaration did not install module export {export}; check its type annotations"
                    ),
                });
            };
            validate_declared_binding_type(
                module,
                arena,
                export_ty,
                HostBindingKind::Table,
                &format!("module export {export}"),
            )?;
            for (member, kind) in members {
                let Some(member_ty) = table_property_path_type(arena, export_ty, member) else {
                    return Err(ConfigError::InvalidHostModuleDeclaration {
                        module: module.clone(),
                        reason: format!(
                            "declaration did not install module export binding {export}.{member}; check its type annotations"
                        ),
                    });
                };
                validate_declared_binding_type(
                    module,
                    arena,
                    member_ty,
                    *kind,
                    &format!("module export binding {export}.{member}"),
                )?;
            }
        }
    }
    Ok(())
}

pub fn validate_declaration_globals(
    capabilities: &RuntimeCapabilities,
    module_declarations: &[DefinitionModule],
    globals: &[DeclarationGlobalSpec],
) -> Result<(), ConfigError> {
    if globals.is_empty() {
        return Ok(());
    }

    let mut seen = BTreeSet::new();
    let mut declarations = module_declarations.to_vec();
    for global in globals {
        if !seen.insert(global.name.clone()) {
            return Err(ConfigError::InvalidDeclarationGlobal {
                name: global.name.clone(),
                reason: "duplicate declaration-only global".to_owned(),
            });
        }
        let shape =
            declared_host_module_shape(&global.name, &global.source()).map_err(|reason| {
                ConfigError::InvalidDeclarationGlobal {
                    name: global.name.clone(),
                    reason,
                }
            })?;
        if !shape.globals.contains_key(&global.name) {
            return Err(ConfigError::InvalidDeclarationGlobal {
                name: global.name.clone(),
                reason: "generated declaration did not define the requested global".to_owned(),
            });
        }
        declarations.push(global.definition_module());
    }

    let mut arena = Arena::new();
    let builtins =
        builtin_environment_for_with_definition_modules(capabilities, &mut arena, &declarations)?;
    let mut checker = Checker::with_builtins(arena, builtins);
    for global in globals {
        checker
            .require_global(&global.name, &global.type_text)
            .map_err(|diagnostics| ConfigError::InvalidDeclarationGlobal {
                name: global.name.clone(),
                reason: diagnostics.render("<declaration-global>"),
            })?;
    }
    Ok(())
}

fn validate_declared_binding_type(
    module: &str,
    arena: &Arena,
    ty: TypeId,
    kind: HostBindingKind,
    label: &str,
) -> Result<(), ConfigError> {
    let valid = match kind {
        HostBindingKind::Function => is_callable_type(arena, ty),
        HostBindingKind::Source | HostBindingKind::Value => true,
        HostBindingKind::Table => is_table_type(arena, ty),
    };
    if valid {
        return Ok(());
    }
    let expected = match kind {
        HostBindingKind::Function => "function",
        HostBindingKind::Source => "source value",
        HostBindingKind::Value => "value",
        HostBindingKind::Table => "table",
    };
    Err(ConfigError::InvalidHostModuleDeclaration {
        module: module.to_owned(),
        reason: format!("declaration for {label} is not a {expected} type"),
    })
}

fn is_callable_type(arena: &Arena, ty: TypeId) -> bool {
    TypeView::new(arena, ty).is_callable()
}

fn is_table_type(arena: &Arena, ty: TypeId) -> bool {
    TypeView::new(arena, ty).is_table_like()
}

fn table_property_path_type(arena: &Arena, ty: TypeId, path: &str) -> Option<TypeId> {
    TypeView::new(arena, ty)
        .property_path(path)
        .map(|view| view.id())
}

fn declared_host_module_shape(module: &str, source: &str) -> Result<HostModuleShape, String> {
    declared_module_audit(module, source).map(|declared| declared.shape)
}

struct DeclaredModuleAudit {
    shape: HostModuleShape,
    type_names: Vec<String>,
}

fn declared_module_audit(module: &str, source: &str) -> Result<DeclaredModuleAudit, String> {
    let parsed = parse_with_config(
        source,
        &Config {
            allow_declaration_syntax: true,
            capture_comments: true,
            ..Config::default()
        },
    );
    if !parsed.errors.is_empty() {
        let errors = parsed
            .errors
            .iter()
            .map(|error| format!("{error:?}"))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!("declaration parse failed: {errors}"));
    }
    let mut shape = HostModuleShape::default();
    let aliases = local_type_aliases(&parsed.root);
    collect_declared_host_bindings(module, &parsed.root, &aliases, &mut shape)?;
    let mut type_names = Vec::new();
    collect_declared_type_names(&parsed.root, &mut type_names);
    Ok(DeclaredModuleAudit { shape, type_names })
}

fn collect_declared_type_names(stat: &Stat, names: &mut Vec<String>) {
    match stat {
        Stat::Block { body, .. } => {
            for stat in body {
                collect_declared_type_names(stat, names);
            }
        }
        Stat::DeclareClass { name, .. } => {
            names.push(name.as_str().to_owned());
        }
        Stat::TypeAlias {
            name,
            exported,
            generics,
            generic_packs,
            ..
        } if !exported && generics.is_empty() && generic_packs.is_empty() => {
            names.push(name.as_str().to_owned());
        }
        _ => {}
    }
}

fn collect_declared_host_bindings<'a>(
    module: &str,
    stat: &'a Stat,
    aliases: &BTreeMap<&'a str, &'a Type>,
    shape: &mut HostModuleShape,
) -> Result<(), String> {
    match stat {
        Stat::Block { body, .. } => {
            for stat in body {
                collect_declared_host_bindings(module, stat, aliases, shape)?;
            }
        }
        Stat::DeclareFunction { name, .. } => {
            shape.insert_global(module, name.as_str(), HostBindingKind::Function)?;
        }
        Stat::DeclareGlobal {
            name,
            declared_type,
            ..
        } => {
            let declared_type = resolve_declared_type(declared_type, aliases);
            let kind = type_binding_kind(declared_type);
            shape.insert_global(module, name.as_str(), kind)?;
            if let Some(props) = table_props(declared_type) {
                shape.collect_declared_table_shape(
                    ShapeWalk {
                        module,
                        root: name.as_str(),
                    },
                    "",
                    props,
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Maps a module's local non-generic type aliases by name.
fn local_type_aliases(stat: &Stat) -> BTreeMap<&str, &Type> {
    fn walk<'a>(stat: &'a Stat, aliases: &mut BTreeMap<&'a str, &'a Type>) {
        match stat {
            Stat::Block { body, .. } => {
                for stat in body {
                    walk(stat, aliases);
                }
            }
            Stat::TypeAlias {
                name,
                generics,
                generic_packs,
                value,
                ..
            } if generics.is_empty() && generic_packs.is_empty() => {
                aliases.insert(name.as_str(), value);
            }
            _ => {}
        }
    }
    let mut aliases = BTreeMap::new();
    walk(stat, &mut aliases);
    aliases
}

/// Follows groups and local alias references to a declared global's shape,
/// so `declare json: Module` audits with the alias table's shape.
fn resolve_declared_type<'a>(ty: &'a Type, aliases: &BTreeMap<&'a str, &'a Type>) -> &'a Type {
    let mut current = ty;
    for _ in 0..8 {
        match current {
            Type::Group { inner, .. } => current = inner,
            Type::Reference {
                prefix: None,
                name,
                parameters,
                ..
            } if parameters.is_empty() => match aliases.get(name.as_str()) {
                Some(next) => current = next,
                None => break,
            },
            _ => break,
        }
    }
    current
}

fn type_binding_kind(ty: &Type) -> HostBindingKind {
    match ty {
        Type::Function { .. } => HostBindingKind::Function,
        Type::Table { .. } => HostBindingKind::Table,
        Type::Group { inner, .. } => type_binding_kind(inner),
        _ => HostBindingKind::Value,
    }
}

fn table_props(ty: &Type) -> Option<&[TableProp]> {
    match ty {
        Type::Table { props, .. } => Some(props.as_slice()),
        Type::Group { inner, .. } => table_props(inner),
        _ => None,
    }
}

fn host_module_shape_mismatch(declared: &HostModuleShape, runtime: &HostModuleShape) -> String {
    let mut parts = Vec::new();
    add_binding_delta(
        &mut parts,
        "declares globals not registered at runtime",
        &declared.globals,
        &runtime.globals,
        &runtime.globals,
        true,
    );
    add_binding_delta(
        &mut parts,
        "registers globals missing from declaration",
        &runtime.globals,
        &declared.globals,
        &runtime.globals,
        false,
    );
    for (library, declared_members) in &declared.libraries {
        if runtime.globals.get(library) == Some(&HostBindingKind::Source) {
            continue;
        }
        let Some(runtime_members) = runtime.libraries.get(library) else {
            parts.push(format!(
                "declares library {library} but registers no runtime bindings for it"
            ));
            continue;
        };
        add_binding_delta(
            &mut parts,
            &format!("declares {library} members not registered at runtime"),
            declared_members,
            runtime_members,
            runtime_members,
            true,
        );
        add_binding_delta(
            &mut parts,
            &format!("registers {library} members missing from declaration"),
            runtime_members,
            declared_members,
            runtime_members,
            false,
        );
    }
    for library in runtime.libraries.keys() {
        if !declared.libraries.contains_key(library) {
            parts.push(format!(
                "registers library {library} but declaration has no table for it"
            ));
        }
    }
    for (export, declared_members) in &declared.module_exports {
        let Some(runtime_members) = runtime.module_exports.get(export) else {
            parts.push(format!(
                "declares module export {export} but registers no runtime bindings for it"
            ));
            continue;
        };
        add_binding_delta(
            &mut parts,
            &format!("declares {export} exports not registered at runtime"),
            declared_members,
            runtime_members,
            runtime_members,
            true,
        );
        add_binding_delta(
            &mut parts,
            &format!("registers {export} exports missing from declaration"),
            runtime_members,
            declared_members,
            runtime_members,
            false,
        );
    }
    for export in runtime.module_exports.keys() {
        if !declared.module_exports.contains_key(export) {
            parts.push(format!(
                "registers module export {export} but declaration has no table for it"
            ));
        }
    }
    parts.join("; ")
}

fn add_binding_delta(
    parts: &mut Vec<String>,
    label: &str,
    left: &BTreeMap<String, HostBindingKind>,
    right: &BTreeMap<String, HostBindingKind>,
    runtime: &BTreeMap<String, HostBindingKind>,
    source_covers_missing: bool,
) {
    let missing = left
        .keys()
        .filter(|name| {
            !(right.contains_key(*name) || source_covers_missing && source_covers(name, runtime))
        })
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        parts.push(format!("{label}: {}", missing.join(", ")));
    }
    let mismatched = left
        .iter()
        .filter_map(|(name, kind)| {
            let other = right.get(name)?;
            (kind != other && !source_covers(name, runtime)).then(|| {
                format!(
                    "{name} ({} vs {})",
                    binding_kind_label(*kind),
                    binding_kind_label(*other)
                )
            })
        })
        .collect::<Vec<_>>();
    if !mismatched.is_empty() {
        parts.push(format!(
            "{label} with different kind: {}",
            mismatched.join(", ")
        ));
    }
}

/// Return whether an opaque source binding supplies this binding or one of its descendants.
fn source_covers(name: &str, runtime: &BTreeMap<String, HostBindingKind>) -> bool {
    runtime.iter().any(|(source, kind)| {
        *kind == HostBindingKind::Source
            && (name == source
                || name
                    .strip_prefix(source)
                    .is_some_and(|suffix| suffix.starts_with('.')))
    })
}

fn binding_kind_label(kind: HostBindingKind) -> &'static str {
    match kind {
        HostBindingKind::Function => "function",
        HostBindingKind::Source => "source value",
        HostBindingKind::Value => "value",
        HostBindingKind::Table => "table",
    }
}
