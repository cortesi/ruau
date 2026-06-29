use std::{
    collections::BTreeSet,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use ruau_ast::syntax::{LocalId, Stat};

use super::{
    CompileError,
    analysis::{
        BuiltinCall, ConstantValue, ExprId, FunctionRegistry, ModuleAnalysis, TableSizePrediction,
        VariableFact, collect_module_identities,
    },
    options::{CompilerOptions, KnownMember},
};

pub struct CompileContext {
    options: CompilerOptions,
    bytecode_version: u8,
    /// Shared with the caller's compile pass (`Rc`, not a deep clone): the
    /// root is immutable for the whole compilation, so the context and the
    /// `FunctionCompiler` walking the tree reference one AST.
    root: Rc<Stat>,
    assigned_globals: BTreeSet<String>,
    analysis: ModuleAnalysis,
    pub(crate) functions: FunctionRegistry,
    cancel: Option<Arc<AtomicBool>>,
}

impl CompileContext {
    pub(crate) fn with_cancel(
        root: Rc<Stat>,
        options: &CompilerOptions,
        cancel: Option<Arc<AtomicBool>>,
    ) -> Self {
        let (analysis, functions) = collect_module_identities(&root, options);
        let assigned_globals = analysis.globals_blocking_imports();

        Self {
            options: options.clone(),
            bytecode_version: options.bytecode_version(),
            root,
            assigned_globals,
            analysis,
            functions,
            cancel,
        }
    }

    pub(crate) fn check_cancelled(&self) -> Result<(), CompileError> {
        if self
            .cancel
            .as_ref()
            .is_some_and(|cancel| cancel.load(Ordering::Relaxed))
        {
            return Err(CompileError::cancelled());
        }
        Ok(())
    }

    pub(crate) fn options(&self) -> &CompilerOptions {
        &self.options
    }

    pub(crate) fn bytecode_version(&self) -> u8 {
        self.bytecode_version
    }

    pub(crate) fn root(&self) -> &Stat {
        &self.root
    }

    pub(crate) fn optimization_level(&self) -> u8 {
        self.options.optimization_level
    }

    pub(crate) fn type_info_level(&self) -> u8 {
        self.options.type_info_level
    }

    pub(crate) fn coverage_level(&self) -> u8 {
        self.options.coverage_level
    }

    pub(crate) fn known_members(&self) -> &[KnownMember] {
        &self.options.known_members
    }

    pub(crate) fn vector_lib(&self) -> Option<&str> {
        self.options.vector_lib.as_deref()
    }

    pub(crate) fn vector_ctor(&self) -> Option<&str> {
        self.options.vector_ctor.as_deref()
    }

    pub(crate) fn assigned_globals(&self) -> &BTreeSet<String> {
        &self.assigned_globals
    }

    pub(crate) fn variable(&self, id: LocalId) -> Option<&VariableFact> {
        self.analysis.variable(id)
    }

    pub(crate) fn local_import_path(&self, id: LocalId) -> Option<&[String]> {
        self.analysis.variable(id)?.import_path()
    }

    pub(crate) fn builtin_call(&self, id: ExprId) -> Option<&BuiltinCall> {
        self.analysis.builtin_call(id)
    }

    pub(crate) fn constant_expr(&self, id: ExprId) -> Option<&ConstantValue> {
        self.analysis.constant_expr(id)
    }

    pub(crate) fn local_constant(&self, id: LocalId) -> Option<&ConstantValue> {
        self.analysis.local_constant(id)
    }

    pub(crate) fn table_prop(&self, id: LocalId, key: &str) -> Option<&ConstantValue> {
        self.analysis.table_prop(id, key)
    }

    pub(crate) fn always_terminates(&self, stat: &Stat) -> bool {
        self.analysis.always_terminates(stat)
    }

    pub(crate) fn table_shape(&self, id: ExprId) -> TableSizePrediction {
        self.analysis.table_shape(id)
    }

    pub(crate) fn getfenv_used(&self) -> bool {
        self.analysis.getfenv_used()
    }

    pub(crate) fn setfenv_used(&self) -> bool {
        self.analysis.setfenv_used()
    }

    pub(crate) fn preserve_fenv_semantics(&self) -> bool {
        self.options.preserve_fenv_semantics
            && (self.analysis.getfenv_used() || self.analysis.setfenv_used())
    }
}
