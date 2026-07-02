//! Module registry and per-VM environment provisioning.
//!
//! A [`ModuleRegistry`] is built once and shared (`Arc`) across requests; the
//! selected modules are installed into a VM's global environment at build. A
//! [`NativeModule`] registers its [`HostFunction`](ruau_vm_api::HostFunction)s
//! through a [`ModuleBuilder`], and the engine's installer binds each as a base
//! global, a member of a named library table, or a member of a host-only
//! hidden table on top of the engine's fixed builtin surface. Base-global
//! bindings are fail-closed about that surface: a fresh `Global` colliding
//! with an installed global is a build error, and replacing a builtin takes
//! the explicit `GlobalOverride` opt-in.

use std::{borrow::Cow, sync::Arc};

use ruau_vm_api::{
    EngineCallable, EngineHostType, HostFunction, ModuleArray, ModuleBinding, ModuleBuilder,
    ModuleExport, ModuleTable, ModuleValue, NativeModule, RawGc, RawValue, marker,
};

use crate::{ModuleId, heap::Heap, host::ModuleHostCallable, host_type::HostType, table::LuaTable};

/// A built-once, shared set of native modules. Deeply immutable (modules are
/// `Send + Sync`) so it can be shared by `Arc` across tenant VMs.
#[derive(Default, Clone)]
pub struct ModuleRegistry {
    modules: Vec<Arc<dyn NativeModule>>,
}

impl ModuleRegistry {
    /// Adds a native module to the registry.
    pub fn register(&mut self, module: Arc<dyn NativeModule>) {
        self.modules.push(module);
    }

    /// The registered modules, in registration order.
    #[must_use]
    pub(crate) fn modules(&self) -> &[Arc<dyn NativeModule>] {
        &self.modules
    }
}

/// A provisioned environment: the module set selected for one VM. Today it is
/// the registry verbatim; runtime-capability subsetting can narrow it later.
#[derive(Default, Clone)]
pub struct Environment {
    registry: ModuleRegistry,
}

/// One build-time named-registry binding. The VM keeps this list so
/// `clear_named_registry` can re-register the host surface after wiping
/// per-run named state.
#[derive(Clone, Debug)]
pub struct NamedBinding {
    pub(crate) name: Vec<u8>,
    pub(crate) value: RawValue,
}

/// One trusted Lua support chunk registered by a native module.
#[derive(Clone, Debug)]
pub struct SupportChunk {
    pub(crate) module: String,
    pub(crate) key: Vec<u8>,
    pub(crate) source: Vec<u8>,
}

/// The build-time VM pieces installed from native modules.
pub struct InstalledModules {
    pub named_bindings: Vec<NamedBinding>,
    pub host_types: Vec<Arc<HostType>>,
    pub support_chunks: Vec<SupportChunk>,
}

impl Environment {
    /// Adds a module to this environment's selected set.
    pub fn register(&mut self, module: Arc<dyn NativeModule>) {
        self.registry.register(module);
    }

    #[must_use]
    pub(crate) fn has_require_exports(&self) -> bool {
        self.registry
            .modules()
            .iter()
            .any(|module| module.export() != ModuleExport::Globals)
    }

    /// Installs every provisioned module's bindings into `globals` (and hidden
    /// tables into the heap's named registry), returning the installed hidden
    /// bindings.
    ///
    /// # Errors
    /// Returns the first module/member installation error. The build fails
    /// rather than running against a partially installed host surface.
    pub(crate) fn install(
        &self,
        heap: &mut Heap,
        globals: RawGc<marker::Table>,
    ) -> Result<InstalledModules, ModuleInstallError> {
        let mut named_bindings = Vec::new();
        let mut host_types = Vec::new();
        let mut support_chunks = Vec::new();
        for module in self.registry.modules() {
            let export = module.export();
            let export_table = match export {
                ModuleExport::Globals => None,
                ModuleExport::Require | ModuleExport::Both => {
                    Some(heap.alloc_table(LuaTable::new()).ok_or_else(|| {
                        ModuleInstallError::new(
                            module.name(),
                            "<module>",
                            ModuleBinding::library(module.name().to_owned()),
                            ModuleInstallErrorKind::Allocation,
                        )
                    })?)
                }
            };
            let mut installer = ModuleInstaller {
                heap,
                globals,
                module: module.name(),
                export,
                export_table,
                named_bindings: &mut named_bindings,
                host_types: &mut host_types,
                support_chunks: &mut support_chunks,
                error: None,
            };
            module.build(&mut installer);
            if let Some(error) = installer.error.take() {
                return Err(error);
            }
            if let Some(table) = export_table {
                let id = ModuleId::canonicalized(module.name());
                if installer.heap.native_module_export_get(&id).is_some() {
                    return Err(ModuleInstallError::new(
                        module.name(),
                        "<module>",
                        ModuleBinding::library(module.name().to_owned()),
                        ModuleInstallErrorKind::NativeExportCollision,
                    ));
                }
                installer
                    .heap
                    .native_module_export_set(id, RawValue::Table(table))
                    .ok_or_else(|| {
                        ModuleInstallError::new(
                            module.name(),
                            "<module>",
                            ModuleBinding::library(module.name().to_owned()),
                            ModuleInstallErrorKind::Allocation,
                        )
                    })?;
            }
        }
        Ok(InstalledModules {
            named_bindings,
            host_types,
            support_chunks,
        })
    }
}

/// Why installing a native module's bindings into a VM failed; carried by
/// `VmBuildError::ModuleInstall`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleInstallError {
    module: String,
    member: String,
    binding: ModuleBinding,
    kind: ModuleInstallErrorKind,
}

impl ModuleInstallError {
    fn new(
        module: &str,
        member: &str,
        binding: ModuleBinding,
        kind: ModuleInstallErrorKind,
    ) -> Self {
        Self {
            module: module.to_owned(),
            member: member.to_owned(),
            binding,
            kind,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ModuleInstallErrorKind {
    Allocation,
    BadHostCallablePayload,
    BadHostTypePayload,
    GlobalCollision,
    NativeExportCollision,
    OverrideTargetMissing,
}

impl std::fmt::Display for ModuleInstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let binding = match &self.binding {
            ModuleBinding::Global => Cow::Borrowed("global"),
            ModuleBinding::GlobalOverride => Cow::Borrowed("global override"),
            ModuleBinding::Library(library) => Cow::Owned(format!("library `{library}`")),
            ModuleBinding::Hidden(table) => Cow::Owned(format!("hidden table `{table}`")),
        };
        let reason = match self.kind {
            ModuleInstallErrorKind::Allocation => "allocation failed",
            ModuleInstallErrorKind::BadHostCallablePayload => {
                "host_callable payload was not an ruau-vm ModuleHostCallable"
            }
            ModuleInstallErrorKind::BadHostTypePayload => {
                "host_type payload was not an ruau-vm HostType"
            }
            ModuleInstallErrorKind::GlobalCollision => {
                "the global is already installed; replacing a builtin requires \
                 the explicit ModuleBinding::GlobalOverride opt-in"
            }
            ModuleInstallErrorKind::NativeExportCollision => {
                "the native module require export id is already installed"
            }
            ModuleInstallErrorKind::OverrideTargetMissing => {
                "no global of this name exists to override"
            }
        };
        write!(
            f,
            "native module `{}` member `{}` ({binding}) failed to install: {reason}",
            self.module, self.member
        )
    }
}

impl std::error::Error for ModuleInstallError {}

/// The engine's [`ModuleBuilder`]: it allocates a closure for each registered host
/// function and binds it as a base global or a member of a named library table. A
/// failed allocation flips `ok` (the trait method cannot return an error); the
/// caller checks it after the install pass.
struct ModuleInstaller<'a> {
    heap: &'a mut Heap,
    globals: RawGc<marker::Table>,
    module: &'a str,
    export: ModuleExport,
    export_table: Option<RawGc<marker::Table>>,
    named_bindings: &'a mut Vec<NamedBinding>,
    host_types: &'a mut Vec<Arc<HostType>>,
    support_chunks: &'a mut Vec<SupportChunk>,
    error: Option<ModuleInstallError>,
}

impl ModuleInstaller<'_> {
    fn fail(&mut self, member: &str, binding: ModuleBinding, kind: ModuleInstallErrorKind) {
        self.error
            .get_or_insert_with(|| ModuleInstallError::new(self.module, member, binding, kind));
    }

    /// Finds the existing library table named `name` under globals, or creates and
    /// installs a fresh one — so several modules can share a library table and a
    /// module can extend an engine library (e.g. add to `string`).
    fn library_table(&mut self, name: &str) -> Option<RawGc<marker::Table>> {
        let key = RawValue::String(self.heap.intern_str(name.as_bytes())?);
        if let Some(RawValue::Table(existing)) = self.heap.table(self.globals).map(|g| g.get(key)) {
            return Some(existing);
        }
        let lib = self.heap.alloc_table(LuaTable::new())?;
        self.heap
            .table_mut(self.globals)?
            .set(key, RawValue::Table(lib));
        Some(lib)
    }

    /// Finds the existing hidden table registered under `name`, or creates one
    /// and registers it in the heap's named registry — so several modules can
    /// share a hidden table, like library tables.
    fn hidden_table(&mut self, name: &str) -> Option<RawGc<marker::Table>> {
        if let Some(RawValue::Table(existing)) = self.heap.named_get(name.as_bytes()) {
            return Some(existing);
        }
        let table = self.heap.alloc_table(LuaTable::new())?;
        self.heap
            .named_set(name.as_bytes(), RawValue::Table(table))?;
        self.named_bindings.push(NamedBinding {
            name: name.as_bytes().to_vec(),
            value: RawValue::Table(table),
        });
        Some(table)
    }

    fn binding_target(&mut self, binding: &ModuleBinding) -> Option<RawGc<marker::Table>> {
        match binding {
            ModuleBinding::Global | ModuleBinding::GlobalOverride => Some(self.globals),
            ModuleBinding::Library(lib) => self.library_table(lib.as_ref()),
            ModuleBinding::Hidden(name) => self.hidden_table(name.as_ref()),
        }
    }

    fn materialize_module_value(&mut self, value: ModuleValue) -> Option<RawValue> {
        match value {
            ModuleValue::Nil => Some(RawValue::Nil),
            ModuleValue::Boolean(value) => Some(RawValue::Boolean(value)),
            ModuleValue::Number(value) => Some(RawValue::Number(value)),
            ModuleValue::Integer(value) => Some(RawValue::Integer(value)),
            ModuleValue::LightUserdata { handle, tag } => {
                Some(RawValue::LightUserdata { handle, tag })
            }
            ModuleValue::Bytes(bytes) => self.heap.intern_str(&bytes).map(RawValue::String),
            ModuleValue::Array(array) => self.materialize_module_array(array).map(RawValue::Table),
            ModuleValue::Table(table) => self.materialize_module_table(table).map(RawValue::Table),
        }
    }

    fn materialize_module_array(&mut self, array: ModuleArray) -> Option<RawGc<marker::Table>> {
        let raw = self.heap.alloc_table(LuaTable::new())?;
        for (index, value) in array.values.into_iter().enumerate() {
            let key = RawValue::Number((index + 1) as f64);
            let value = self.materialize_module_value(value)?;
            if !self.heap.table_mut(raw)?.set(key, value) {
                return None;
            }
        }
        Some(raw)
    }

    fn materialize_module_table(&mut self, table: ModuleTable) -> Option<RawGc<marker::Table>> {
        let raw = self.heap.alloc_table(LuaTable::new())?;
        for entry in table.entries {
            let key = RawValue::String(self.heap.intern_str(entry.name.as_bytes())?);
            let value = self.materialize_module_value(entry.value)?;
            if !self.heap.table_mut(raw)?.set(key, value) {
                return None;
            }
        }
        Some(raw)
    }

    fn set_binding(&mut self, name: &str, binding: ModuleBinding, value: RawValue) {
        if self.error.is_some() {
            return;
        }
        if let Some(table) = self.export_table
            && !matches!(binding, ModuleBinding::Hidden(_))
            && self.set_module_export_member(table, name, value).is_none()
        {
            self.fail(name, binding, ModuleInstallErrorKind::Allocation);
            return;
        }
        if self.export == ModuleExport::Require && !matches!(binding, ModuleBinding::Hidden(_)) {
            return;
        }
        let mut collision = None;
        let installed = (|| {
            let key = RawValue::String(self.heap.intern_str(name.as_bytes())?);
            let target = self.binding_target(&binding)?;
            // Base-global bindings are fail-closed about the existing surface:
            // a fresh `Global` must not collide with an installed global, and
            // a `GlobalOverride` must have a global to replace.
            let occupied = !matches!(self.heap.table(target)?.get(key), RawValue::Nil);
            match binding {
                ModuleBinding::Global if occupied => {
                    collision = Some(ModuleInstallErrorKind::GlobalCollision);
                    return Some(());
                }
                ModuleBinding::GlobalOverride if !occupied => {
                    collision = Some(ModuleInstallErrorKind::OverrideTargetMissing);
                    return Some(());
                }
                _ => {}
            }
            self.heap.table_mut(target)?.set(key, value);
            Some(())
        })();
        if let Some(kind) = collision {
            self.fail(name, binding, kind);
        } else if installed.is_none() {
            self.fail(name, binding, ModuleInstallErrorKind::Allocation);
        }
    }

    fn set_module_export_member(
        &mut self,
        table: RawGc<marker::Table>,
        name: &str,
        value: RawValue,
    ) -> Option<()> {
        let key = RawValue::String(self.heap.intern_str(name.as_bytes())?);
        self.heap.table_mut(table)?.set(key, value);
        Some(())
    }
}

impl ModuleBuilder for ModuleInstaller<'_> {
    fn function(&mut self, name: &str, binding: ModuleBinding, f: Box<dyn HostFunction>) {
        if self.error.is_some() {
            return;
        }
        let installed = (|| {
            let closure = self.heap.alloc_host(f)?;
            Some(RawValue::Function(closure))
        })();
        if let Some(value) = installed {
            self.set_binding(name, binding, value);
        } else {
            self.fail(name, binding, ModuleInstallErrorKind::Allocation);
        }
    }

    fn host_callable(&mut self, name: &str, binding: ModuleBinding, f: EngineCallable) {
        if self.error.is_some() {
            return;
        }
        let Ok(callable) = f.into_engine().downcast::<ModuleHostCallable>() else {
            self.fail(
                name,
                binding,
                ModuleInstallErrorKind::BadHostCallablePayload,
            );
            return;
        };
        let installed = (|| {
            let closure = match *callable {
                ModuleHostCallable::Scoped(f) => self.heap.alloc_scoped_host(f)?,
                ModuleHostCallable::Async(f) => self.heap.alloc_async_host(f)?,
            };
            Some(RawValue::Function(closure))
        })();
        if let Some(value) = installed {
            self.set_binding(name, binding, value);
        } else {
            self.fail(name, binding, ModuleInstallErrorKind::Allocation);
        }
    }

    fn constant(&mut self, name: &str, binding: ModuleBinding, value: ModuleValue) {
        if self.error.is_some() {
            return;
        }
        if let Some(value) = self.materialize_module_value(value) {
            self.set_binding(name, binding, value);
        } else {
            self.fail(name, binding, ModuleInstallErrorKind::Allocation);
        }
    }

    fn host_type(&mut self, ty: EngineHostType) {
        if self.error.is_some() {
            return;
        }
        let Ok(ty) = ty.into_engine().downcast::<HostType>() else {
            self.fail(
                "<host_type>",
                ModuleBinding::Hidden(Cow::Borrowed("<host_type>")),
                ModuleInstallErrorKind::BadHostTypePayload,
            );
            return;
        };
        self.host_types.push(Arc::new(*ty));
    }

    fn support_chunk(&mut self, registry_key: &str, source: &[u8]) {
        if self.error.is_some() {
            return;
        }
        self.support_chunks.push(SupportChunk {
            module: self.module.to_owned(),
            key: registry_key.as_bytes().to_vec(),
            source: source.to_vec(),
        });
    }
}
