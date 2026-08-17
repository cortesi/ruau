use std::sync::{Arc, TryLockError};

use crate::{CallOptions, ModuleDomainId, RuntimeCompiler, Vm};

pub const CALL_OPTIONS_ACTIVE: &str = "call options are already active on another VM entry";

/// Restores one indivisible call overlay when a VM entry ends.
pub struct CallContextGuard<'vm, 'options> {
    vm: &'vm mut Vm,
    options: &'options CallOptions,
    previous_app_data: Option<crate::scope::AppData>,
    previous_print_sink: Option<Option<crate::PrintSink>>,
    previous_runtime_compiler: Option<Arc<dyn RuntimeCompiler>>,
}

impl<'vm, 'options> CallContextGuard<'vm, 'options> {
    #[inline(always)]
    pub(crate) fn new(
        vm: &'vm mut Vm,
        options: &'options CallOptions,
    ) -> Result<Self, CallOptionsActive> {
        let mut lease = match options.overlay.try_lock() {
            Ok(lease) => lease,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => return Err(CallOptionsActive),
        };
        if lease.active {
            return Err(CallOptionsActive);
        }
        lease.active = true;

        let previous_app_data = lease
            .overlay
            .app_data
            .take()
            .map(|app_data| vm.app_data.replace(app_data));
        let previous_print_sink = lease
            .overlay
            .print_sink
            .take()
            .map(|sink| vm.heap.replace_print_sink(Some(sink)));
        let previous_runtime_compiler = lease
            .overlay
            .runtime_compiler
            .take()
            .map(|compiler| vm.heap.replace_runtime_compiler(compiler));
        drop(lease);
        Ok(Self {
            vm,
            options,
            previous_app_data,
            previous_print_sink,
            previous_runtime_compiler,
        })
    }

    #[inline(always)]
    pub(crate) fn vm_mut(&mut self) -> &mut Vm {
        self.vm
    }
}

impl Drop for CallContextGuard<'_, '_> {
    #[inline(always)]
    fn drop(&mut self) {
        let mut lease = self
            .options
            .overlay
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        debug_assert!(lease.active, "active call overlay lease was released");
        if let Some(previous) = self.previous_app_data.take() {
            lease.overlay.app_data = Some(self.vm.app_data.replace(previous));
        }
        if let Some(previous) = self.previous_print_sink.take() {
            lease.overlay.print_sink = self.vm.heap.replace_print_sink(previous);
        }
        if let Some(previous) = self.previous_runtime_compiler.take() {
            lease.overlay.runtime_compiler = Some(self.vm.heap.replace_runtime_compiler(previous));
        }
        lease.active = false;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CallOptionsActive;

/// Restores the previous module-cache domain when a VM entry ends.
pub struct ModuleDomainGuard<'vm> {
    vm: &'vm mut Vm,
    previous: ModuleDomainId,
}

impl<'vm> ModuleDomainGuard<'vm> {
    #[inline(always)]
    pub(crate) fn new(vm: &'vm mut Vm, domain: ModuleDomainId) -> Self {
        let previous = vm.heap.replace_module_domain(domain);
        Self { vm, previous }
    }

    #[inline(always)]
    pub(crate) fn vm_mut(&mut self) -> &mut Vm {
        self.vm
    }
}

impl Drop for ModuleDomainGuard<'_> {
    #[inline(always)]
    fn drop(&mut self) {
        self.vm.heap.replace_module_domain(self.previous);
    }
}

/// Restores the previous runtime compiler when a synchronous operation ends.
pub struct RuntimeCompilerGuard<'vm> {
    vm: &'vm mut Vm,
    previous: Option<Arc<dyn RuntimeCompiler>>,
}

impl<'vm> RuntimeCompilerGuard<'vm> {
    #[inline(always)]
    pub(crate) fn new(vm: &'vm mut Vm, compiler: Arc<dyn RuntimeCompiler>) -> Self {
        let previous = vm.heap.replace_runtime_compiler(compiler);
        Self {
            vm,
            previous: Some(previous),
        }
    }

    #[inline(always)]
    pub(crate) fn vm_mut(&mut self) -> &mut Vm {
        self.vm
    }
}

impl Drop for RuntimeCompilerGuard<'_> {
    #[inline(always)]
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            self.vm.heap.replace_runtime_compiler(previous);
        }
    }
}
