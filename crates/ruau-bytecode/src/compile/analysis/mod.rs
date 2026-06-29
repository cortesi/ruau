use std::{
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

use ruau_ast::{
    json::{JsonBinaryOp, JsonUnaryOp},
    syntax::{Expr, Local, LocalId, Stat, SyntaxId, Type},
    visit::{NodePath, Visitor, WalkControl, walk_stat},
};

use super::{
    builtin_folding::{fold_builtin_constant, math_member_constant},
    helpers::luau_fold_mod,
    options::{CompilerOptions, KnownMemberValue},
};
use crate::opcodes::BuiltinFunction;

pub type ExprId = SyntaxId;
pub type TypeId = SyntaxId;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FunctionId(SyntaxId);

impl FunctionId {
    pub(crate) const fn new(syntax_id: SyntaxId) -> Self {
        Self(syntax_id)
    }

    pub(crate) const fn syntax_id(self) -> SyntaxId {
        self.0
    }
}

#[derive(Clone, Debug, Default)]
pub struct ModuleAnalysis {
    expressions: BTreeMap<ExprId, ExpressionIdentity>,
    types: BTreeMap<TypeId, TypeIdentity>,
    locals: BTreeMap<LocalId, LocalIdentity>,
    variables: BTreeMap<LocalId, VariableFact>,
    builtins: BTreeMap<ExprId, BuiltinCall>,
    constants: BTreeMap<ExprId, ConstantValue>,
    local_constants: BTreeMap<LocalId, ConstantValue>,
    table_props: BTreeMap<LocalId, BTreeMap<String, ConstantValue>>,
    table_shapes: BTreeMap<ExprId, TableSizePrediction>,
    globals: BTreeMap<String, GlobalState>,
    getfenv_used: bool,
    setfenv_used: bool,
}

impl ModuleAnalysis {
    fn variable_mut(&mut self, id: LocalId) -> &mut VariableFact {
        self.variables.entry(id).or_default()
    }

    fn mark_global_written(&mut self, name: &str) {
        self.globals.insert(name.to_owned(), GlobalState::Written);
    }

    fn mark_global_mutable(&mut self, name: &str) {
        self.globals
            .entry(name.to_owned())
            .or_insert(GlobalState::Mutable);
    }

    fn record_global_read(&mut self, name: &str) {
        match name {
            "getfenv" => self.getfenv_used = true,
            "setfenv" => self.setfenv_used = true,
            _ => {}
        }
    }

    #[cfg(test)]
    pub(crate) fn expression_count(&self) -> usize {
        self.expressions.len()
    }

    #[cfg(test)]
    pub(crate) fn type_count(&self) -> usize {
        self.types.len()
    }

    #[cfg(test)]
    pub(crate) fn local_count(&self) -> usize {
        self.locals.len()
    }

    #[cfg(test)]
    pub(crate) fn contains_expression(&self, id: ExprId) -> bool {
        self.expressions.contains_key(&id)
    }

    #[cfg(test)]
    pub(crate) fn contains_type(&self, id: TypeId) -> bool {
        self.types.contains_key(&id)
    }

    pub(crate) fn variable(&self, id: LocalId) -> Option<&VariableFact> {
        self.variables.get(&id)
    }

    pub(crate) fn builtin_call(&self, id: ExprId) -> Option<&BuiltinCall> {
        self.builtins.get(&id)
    }

    pub(crate) fn constant_expr(&self, id: ExprId) -> Option<&ConstantValue> {
        self.constants.get(&id)
    }

    pub(crate) fn local_constant(&self, id: LocalId) -> Option<&ConstantValue> {
        self.local_constants.get(&id)
    }

    pub(crate) fn table_prop(&self, id: LocalId, key: &str) -> Option<&ConstantValue> {
        self.table_props.get(&id)?.get(key)
    }

    pub(crate) fn always_terminates(&self, stat: &Stat) -> bool {
        always_terminates(self, stat)
    }

    pub(crate) fn table_shape(&self, id: ExprId) -> TableSizePrediction {
        self.table_shapes.get(&id).copied().unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn mark_local_constant(&mut self, id: LocalId, constant: bool) {
        self.variable_mut(id).constant = constant;
    }

    pub(crate) fn globals_blocking_imports(&self) -> BTreeSet<String> {
        self.globals
            .iter()
            .filter(|(_, state)| state.blocks_imports())
            .map(|(name, _)| name.clone())
            .collect()
    }

    pub(crate) fn global_state(&self, name: &str) -> GlobalState {
        self.globals.get(name).copied().unwrap_or_default()
    }

    pub(crate) fn getfenv_used(&self) -> bool {
        self.getfenv_used
    }

    pub(crate) fn setfenv_used(&self) -> bool {
        self.setfenv_used
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuiltinCall {
    path: Vec<String>,
    function_id: u8,
}

impl BuiltinCall {
    #[cfg(test)]
    pub(crate) fn path(&self) -> &[String] {
        &self.path
    }

    pub(crate) fn function_id(&self) -> u8 {
        self.function_id
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum GlobalState {
    #[default]
    Default,
    Mutable,
    Written,
}

impl GlobalState {
    pub(crate) const fn blocks_imports(self) -> bool {
        !matches!(self, Self::Default)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VariableFact {
    initial_expr: Option<ExprId>,
    import_path: Option<Vec<String>>,
    loop_depth: usize,
    written: bool,
    constant: bool,
}

impl VariableFact {
    pub(crate) fn initial_expr(&self) -> Option<ExprId> {
        self.initial_expr
    }

    pub(crate) fn import_path(&self) -> Option<&[String]> {
        self.import_path.as_deref()
    }

    pub(crate) const fn loop_depth(&self) -> usize {
        self.loop_depth
    }

    pub(crate) fn is_written(&self) -> bool {
        self.written
    }

    pub(crate) fn is_constant(&self) -> bool {
        self.constant
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConstantValue {
    Nil,
    Bool(bool),
    Number(f64),
    Integer(i64),
    String(String),
    Vector { bits: [u32; 4] },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TableSizePrediction {
    pub(crate) hash_size: u8,
    pub(crate) array_size: u32,
}

#[derive(Clone, Debug, Default)]
pub struct LocalValueFacts {
    constants: BTreeMap<u32, ConstantValue>,
    import_paths: BTreeMap<u32, Vec<String>>,
}

impl LocalValueFacts {
    pub(crate) fn constant(&self, local_id: u32) -> Option<ConstantValue> {
        self.constants.get(&local_id).cloned()
    }

    pub(crate) fn set_constant(&mut self, local_id: u32, value: Option<ConstantValue>) {
        if let Some(value) = value {
            self.constants.insert(local_id, value);
        } else {
            self.constants.remove(&local_id);
        }
    }

    pub(crate) fn extend_constants(
        &mut self,
        values: impl IntoIterator<Item = (u32, ConstantValue)>,
    ) {
        self.constants.extend(values);
    }

    pub(crate) fn import_path(&self, local_id: u32) -> Option<Vec<String>> {
        self.import_paths.get(&local_id).cloned()
    }

    pub(crate) fn set_import_path(&mut self, local_id: u32, path: Option<Vec<String>>) {
        if let Some(path) = path {
            self.import_paths.insert(local_id, path);
        } else {
            self.import_paths.remove(&local_id);
        }
    }

    pub(crate) fn invalidate_local(&mut self, local_id: u32) {
        self.constants.remove(&local_id);
        self.import_paths.remove(&local_id);
    }
}

#[derive(Clone, Debug, Default)]
pub struct FunctionRegistry {
    functions: BTreeMap<FunctionId, FunctionInfo>,
    /// `Rc`, not owned: a registered function subtree is cloned out of the
    /// module AST exactly once, at registration (the AST stores `Expr` inline,
    /// so this one copy is unavoidable without an AST ownership change); every
    /// later lookup — the registered-function compile pass, inline-candidate
    /// resolution — shares it instead of re-cloning the subtree.
    exprs: BTreeMap<FunctionId, Rc<Expr>>,
    order: Vec<FunctionId>,
}

impl FunctionRegistry {
    fn insert(&mut self, id: FunctionId, expr: &Expr, info: FunctionInfo) {
        if self.functions.contains_key(&id) {
            return;
        }
        self.order.push(id);
        self.exprs.insert(id, Rc::new(expr.clone()));
        self.functions.insert(id, info);
    }

    pub(crate) fn get(&self, id: FunctionId) -> Option<&FunctionInfo> {
        self.functions.get(&id)
    }

    pub(crate) fn record_compiled_proto(
        &mut self,
        id: FunctionId,
        proto: FunctionProtoInfo,
    ) -> Option<()> {
        self.functions.get_mut(&id)?.record_compiled_proto(proto);
        Some(())
    }

    pub(crate) fn ordered_ids(&self) -> &[FunctionId] {
        &self.order
    }

    pub(crate) fn expr(&self, id: FunctionId) -> Option<&Rc<Expr>> {
        self.exprs.get(&id)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.functions.len()
    }
}

#[derive(Clone, Debug)]
struct ExpressionIdentity;

#[derive(Clone, Debug)]
struct TypeIdentity;

#[derive(Clone, Debug)]
struct LocalIdentity;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FunctionInfo {
    compile_order: usize,
    arg_count: usize,
    has_self_arg: bool,
    vararg: bool,
    function_depth: usize,
    debug_name: String,
    upvalues: Vec<FunctionUpvalueInfo>,
    proto: Option<FunctionProtoInfo>,
    has_type_annotations: bool,
    syntactic_inline_candidate: bool,
    returns_one: bool,
}

impl FunctionInfo {
    fn from_expr(
        compile_order: usize,
        expr: &Expr,
        analysis: &ModuleAnalysis,
        upvalues: Vec<FunctionUpvalueInfo>,
    ) -> Self {
        let Expr::Function {
            args,
            self_arg,
            vararg,
            vararg_annotation,
            return_annotation,
            body,
            function_depth,
            debug_name,
            ..
        } = expr
        else {
            unreachable!("FunctionInfo::from_expr only accepts function expressions")
        };

        let has_type_annotations = args.iter().any(|arg| arg.luau_type.is_some())
            || self_arg
                .as_ref()
                .is_some_and(|self_arg| self_arg.luau_type.is_some())
            || vararg_annotation.is_some()
            || return_annotation.is_some();

        Self {
            compile_order,
            arg_count: args.len() + usize::from(self_arg.is_some()),
            has_self_arg: self_arg.is_some(),
            vararg: *vararg,
            function_depth: *function_depth,
            debug_name: debug_name.clone(),
            upvalues,
            proto: None,
            has_type_annotations,
            syntactic_inline_candidate: !*vararg && self_arg.is_none(),
            returns_one: function_returns_one(analysis, body),
        }
    }

    #[cfg(test)]
    pub(crate) const fn compile_order(&self) -> usize {
        self.compile_order
    }

    #[cfg(test)]
    pub(crate) const fn arg_count(&self) -> usize {
        self.arg_count
    }

    #[cfg(test)]
    pub(crate) const fn vararg(&self) -> bool {
        self.vararg
    }

    pub(crate) const fn function_depth(&self) -> usize {
        self.function_depth
    }

    #[cfg(test)]
    pub(crate) fn debug_name(&self) -> &str {
        &self.debug_name
    }

    pub(crate) fn upvalues(&self) -> &[FunctionUpvalueInfo] {
        &self.upvalues
    }

    pub(crate) const fn proto(&self) -> Option<FunctionProtoInfo> {
        self.proto
    }

    fn record_compiled_proto(&mut self, proto: FunctionProtoInfo) {
        self.proto = Some(proto);
    }

    #[cfg(test)]
    pub(crate) const fn has_type_annotations(&self) -> bool {
        self.has_type_annotations
    }

    pub(crate) const fn syntactic_inline_candidate(&self) -> bool {
        self.syntactic_inline_candidate
    }

    #[cfg(test)]
    pub(crate) const fn returns_one(&self) -> bool {
        self.returns_one
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FunctionProtoInfo {
    proto_id: u32,
    stack_size: u8,
    upvalue_count: u8,
    flags: u8,
}

impl FunctionProtoInfo {
    pub(crate) const fn new(proto_id: u32, stack_size: u8, upvalue_count: u8, flags: u8) -> Self {
        Self {
            proto_id,
            stack_size,
            upvalue_count,
            flags,
        }
    }

    pub(crate) const fn proto_id(self) -> u32 {
        self.proto_id
    }

    pub(crate) const fn stack_size(self) -> u8 {
        self.stack_size
    }

    pub(crate) const fn upvalue_count(self) -> u8 {
        self.upvalue_count
    }

    #[cfg(test)]
    pub(crate) const fn flags(self) -> u8 {
        self.flags
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FunctionUpvalueInfo {
    local_id: u32,
    name: String,
    luau_type: Option<Box<Type>>,
    function_depth: usize,
    loop_depth: usize,
    written: bool,
}

impl FunctionUpvalueInfo {
    pub(crate) const fn local_id(&self) -> u32 {
        self.local_id
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn luau_type(&self) -> Option<&Type> {
        self.luau_type.as_deref()
    }

    pub(crate) const fn function_depth(&self) -> usize {
        self.function_depth
    }

    pub(crate) const fn loop_depth(&self) -> usize {
        self.loop_depth
    }

    #[cfg(test)]
    pub(crate) const fn is_written(&self) -> bool {
        self.written
    }
}

pub fn collect_module_identities(
    root: &Stat,
    options: &CompilerOptions,
) -> (ModuleAnalysis, FunctionRegistry) {
    let mut analysis = ModuleAnalysis::default();
    let mut functions = FunctionRegistry::default();
    walk_stat(
        root,
        &mut IdentityCollector {
            analysis: &mut analysis,
        },
    );

    track_values(root, &options.mutable_globals, &mut analysis);
    analyze_local_import_paths(root, &mut analysis);
    analyze_builtin_calls(root, options, &mut analysis);
    analyze_constants(root, options, &mut analysis);
    analyze_table_shapes(root, &mut analysis);
    collect_functions(root, &analysis, &mut functions);

    (analysis, functions)
}

struct IdentityCollector<'a> {
    analysis: &'a mut ModuleAnalysis,
}

impl<'ast> Visitor<'ast> for IdentityCollector<'_> {
    fn visit_local(&mut self, _path: &NodePath, local: &'ast Local) -> WalkControl {
        let id = local.id;
        self.analysis.locals.insert(id, LocalIdentity);
        self.analysis.variables.entry(id).or_default();
        WalkControl::Continue
    }

    fn visit_expr(&mut self, _path: &NodePath, expr: &'ast Expr) -> WalkControl {
        let id = expr.syntax_id();
        self.analysis.expressions.insert(id, ExpressionIdentity);
        if let Expr::Global { name, .. } = expr {
            self.analysis.record_global_read(name.as_str());
        }
        WalkControl::Continue
    }

    fn visit_type(&mut self, _path: &NodePath, luau_type: &'ast Type) -> WalkControl {
        let id = luau_type.syntax_id();
        self.analysis.types.insert(id, TypeIdentity);
        WalkControl::Continue
    }
}

fn collect_functions(root: &Stat, analysis: &ModuleAnalysis, functions: &mut FunctionRegistry) {
    walk_stat(
        root,
        &mut FunctionCollector {
            analysis,
            functions,
        },
    );
}

struct FunctionCollector<'a> {
    analysis: &'a ModuleAnalysis,
    functions: &'a mut FunctionRegistry,
}

impl<'ast> Visitor<'ast> for FunctionCollector<'_> {
    fn visit_stat(&mut self, _path: &NodePath, stat: &'ast Stat) -> WalkControl {
        match stat {
            Stat::Class { members, .. } => {
                for member in members {
                    if let Stat::TypeFunction { func, .. } = member {
                        ruau_ast::visit::walk_expr(func, self);
                    }
                }
                WalkControl::SkipChildren
            }
            Stat::TypeFunction { .. } => WalkControl::SkipChildren,
            _ => WalkControl::Continue,
        }
    }

    fn visit_expr(&mut self, _path: &NodePath, expr: &'ast Expr) -> WalkControl {
        if let Expr::Function {
            syntax_id, body, ..
        } = expr
        {
            walk_stat(body, self);
            let upvalues = collect_function_upvalues(self.analysis, self.functions, expr);
            let id = FunctionId::new(*syntax_id);
            let info =
                FunctionInfo::from_expr(self.functions.order.len(), expr, self.analysis, upvalues);
            self.functions.insert(id, expr, info);
            WalkControl::SkipChildren
        } else {
            WalkControl::Continue
        }
    }
}

fn collect_function_upvalues(
    analysis: &ModuleAnalysis,
    functions: &FunctionRegistry,
    expr: &Expr,
) -> Vec<FunctionUpvalueInfo> {
    let Expr::Function {
        body,
        function_depth,
        ..
    } = expr
    else {
        unreachable!("collect_function_upvalues only accepts function expressions")
    };

    let mut collector = FunctionUpvalueCollector {
        analysis,
        functions,
        function_depth: *function_depth,
        seen: BTreeSet::new(),
        upvalues: Vec::new(),
    };
    walk_stat(body, &mut collector);
    collector.upvalues
}

struct FunctionUpvalueCollector<'a> {
    analysis: &'a ModuleAnalysis,
    functions: &'a FunctionRegistry,
    function_depth: usize,
    seen: BTreeSet<u32>,
    upvalues: Vec<FunctionUpvalueInfo>,
}

impl FunctionUpvalueCollector<'_> {
    fn record_local_ref(&mut self, local: &ruau_ast::syntax::LocalRef) {
        if local.function_depth >= self.function_depth {
            return;
        }
        if !self.seen.insert(local.id.index()) {
            return;
        }
        self.upvalues.push(FunctionUpvalueInfo {
            local_id: local.id.index(),
            name: local.name.as_str().to_owned(),
            luau_type: local.luau_type.clone(),
            function_depth: local.function_depth,
            loop_depth: self
                .analysis
                .variable(local.id)
                .map_or(0, VariableFact::loop_depth),
            written: self
                .analysis
                .variable(local.id)
                .is_some_and(VariableFact::is_written),
        });
    }

    fn record_forwarded_child_upvalue(&mut self, upvalue: &FunctionUpvalueInfo) {
        if upvalue.function_depth >= self.function_depth {
            return;
        }
        if !self.seen.insert(upvalue.local_id) {
            return;
        }
        self.upvalues.push(upvalue.clone());
    }
}

impl<'ast> Visitor<'ast> for FunctionUpvalueCollector<'_> {
    fn visit_expr(&mut self, _path: &NodePath, expr: &'ast Expr) -> WalkControl {
        match expr {
            Expr::Local { local, .. } => {
                self.record_local_ref(local);
                WalkControl::Continue
            }
            Expr::Function { syntax_id, .. } => {
                if let Some(info) = self.functions.get(FunctionId::new(*syntax_id)) {
                    for upvalue in info.upvalues() {
                        self.record_forwarded_child_upvalue(upvalue);
                    }
                }
                WalkControl::SkipChildren
            }
            _ => WalkControl::Continue,
        }
    }
}

fn function_returns_one(analysis: &ModuleAnalysis, body: &Stat) -> bool {
    if !always_terminates(analysis, body) {
        return false;
    }

    let mut visitor = FunctionReturnVisitor { returns_one: true };
    walk_stat(body, &mut visitor);
    visitor.returns_one
}

struct FunctionReturnVisitor {
    returns_one: bool,
}

impl<'ast> Visitor<'ast> for FunctionReturnVisitor {
    fn visit_stat(&mut self, _path: &NodePath, stat: &'ast Stat) -> WalkControl {
        if let Stat::Return { list, .. } = stat {
            self.returns_one &= list.len() == 1 && !return_expr_may_multret(&list[0]);
            WalkControl::SkipChildren
        } else {
            WalkControl::Continue
        }
    }

    fn visit_expr(&mut self, _path: &NodePath, _expr: &'ast Expr) -> WalkControl {
        WalkControl::SkipChildren
    }
}

fn return_expr_may_multret(expr: &Expr) -> bool {
    match expr {
        Expr::Call { .. } | Expr::Varargs { .. } => true,
        Expr::Group { expr, .. } | Expr::TypeAssertion { expr, .. } => {
            return_expr_may_multret(expr)
        }
        _ => false,
    }
}

fn track_values(root: &Stat, mutable_globals: &[String], analysis: &mut ModuleAnalysis) {
    analysis.mark_global_mutable("_G");
    for name in mutable_globals {
        analysis.mark_global_mutable(name);
    }
    track_values_stat(root, analysis, 0);
}

fn analyze_local_import_paths(root: &Stat, analysis: &mut ModuleAnalysis) {
    match root {
        Stat::Block { body, .. } => {
            for stat in body {
                analyze_local_import_paths(stat, analysis);
            }
        }
        Stat::Local { vars, values, .. } => {
            for (index, local) in vars.iter().enumerate() {
                analysis.variable_mut(local.id).import_path = values
                    .get(index)
                    .and_then(|value| local_import_path_initializer(value, analysis));
            }
            for value in values {
                analyze_local_import_paths_expr(value, analysis);
            }
        }
        Stat::Assign { values, .. } => {
            for value in values {
                analyze_local_import_paths_expr(value, analysis);
            }
        }
        Stat::CompoundAssign { value, .. } => analyze_local_import_paths_expr(value, analysis),
        Stat::Return { list, .. } => {
            for expr in list {
                analyze_local_import_paths_expr(expr, analysis);
            }
        }
        Stat::Expr { expr, .. } => analyze_local_import_paths_expr(expr, analysis),
        Stat::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            analyze_local_import_paths_expr(condition, analysis);
            analyze_local_import_paths(then_body, analysis);
            if let Some(else_body) = else_body.as_deref() {
                analyze_local_import_paths(else_body, analysis);
            }
        }
        Stat::While {
            condition, body, ..
        } => {
            analyze_local_import_paths_expr(condition, analysis);
            analyze_local_import_paths(body, analysis);
        }
        Stat::Repeat {
            condition, body, ..
        } => {
            analyze_local_import_paths(body, analysis);
            analyze_local_import_paths_expr(condition, analysis);
        }
        Stat::For {
            from,
            to,
            step,
            body,
            ..
        } => {
            analyze_local_import_paths_expr(from, analysis);
            analyze_local_import_paths_expr(to, analysis);
            if let Some(step) = step {
                analyze_local_import_paths_expr(step, analysis);
            }
            analyze_local_import_paths(body, analysis);
        }
        Stat::ForIn { values, body, .. } => {
            for value in values {
                analyze_local_import_paths_expr(value, analysis);
            }
            analyze_local_import_paths(body, analysis);
        }
        Stat::Function { func, .. } | Stat::LocalFunction { func, .. } => {
            analyze_local_import_paths_expr(func, analysis);
        }
        Stat::TypeFunction { func, .. } => analyze_local_import_paths_expr(func, analysis),
        Stat::Class { members, .. } => {
            for member in members {
                analyze_local_import_paths(member, analysis);
            }
        }
        Stat::Error {
            expressions,
            statements,
            ..
        } => {
            for expr in expressions {
                analyze_local_import_paths_expr(expr, analysis);
            }
            for stat in statements {
                analyze_local_import_paths(stat, analysis);
            }
        }
        Stat::Break { .. }
        | Stat::Continue { .. }
        | Stat::DeclareGlobal { .. }
        | Stat::DeclareFunction { .. }
        | Stat::DeclareClass { .. }
        | Stat::TypeAlias { .. }
        | Stat::ClassProperty { .. } => {}
    }
}

fn analyze_local_import_paths_expr(expr: &Expr, analysis: &mut ModuleAnalysis) {
    match expr {
        Expr::Function { body, .. } => analyze_local_import_paths(body, analysis),
        Expr::Call { func, args, .. } => {
            analyze_local_import_paths_expr(func, analysis);
            for arg in args {
                analyze_local_import_paths_expr(arg, analysis);
            }
        }
        Expr::IndexName { expr, .. }
        | Expr::Group { expr, .. }
        | Expr::TypeAssertion { expr, .. }
        | Expr::Unary { expr, .. } => analyze_local_import_paths_expr(expr, analysis),
        Expr::InterpString { expressions, .. } | Expr::Error { expressions, .. } => {
            for expr in expressions {
                analyze_local_import_paths_expr(expr, analysis);
            }
        }
        Expr::IndexExpr { expr, index, .. } => {
            analyze_local_import_paths_expr(expr, analysis);
            analyze_local_import_paths_expr(index, analysis);
        }
        Expr::Binary { left, right, .. } => {
            analyze_local_import_paths_expr(left, analysis);
            analyze_local_import_paths_expr(right, analysis);
        }
        Expr::IfElse {
            condition,
            true_expr,
            false_expr,
            ..
        } => {
            analyze_local_import_paths_expr(condition, analysis);
            analyze_local_import_paths_expr(true_expr, analysis);
            analyze_local_import_paths_expr(false_expr, analysis);
        }
        Expr::Table { items, .. } => {
            for item in items {
                if let Some(key) = &item.key {
                    analyze_local_import_paths_expr(key, analysis);
                }
                analyze_local_import_paths_expr(&item.value, analysis);
            }
        }
        Expr::Instantiate { expr, .. } => analyze_local_import_paths_expr(expr, analysis),
        Expr::Local { .. }
        | Expr::Global { .. }
        | Expr::Nil { .. }
        | Expr::Bool { .. }
        | Expr::Integer { .. }
        | Expr::Number { .. }
        | Expr::String { .. }
        | Expr::Varargs { .. } => {}
    }
}

fn local_import_path_initializer(expr: &Expr, analysis: &ModuleAnalysis) -> Option<Vec<String>> {
    match expr {
        Expr::Binary {
            op: JsonBinaryOp::Or,
            left,
            ..
        } => direct_import_path_expr(left, analysis),
        Expr::Group { expr, .. } | Expr::TypeAssertion { expr, .. } => {
            local_import_path_initializer(expr, analysis)
        }
        _ => direct_import_path_expr(expr, analysis),
    }
}

fn direct_import_path_expr(expr: &Expr, analysis: &ModuleAnalysis) -> Option<Vec<String>> {
    match expr {
        Expr::Global { name, .. }
            if analysis.global_state(name.as_str()) == GlobalState::Default =>
        {
            Some(vec![name.as_str().to_owned()])
        }
        Expr::Local { local, .. } => analysis
            .variable(local.id)
            .filter(|variable| !variable.is_written())
            .and_then(|variable| variable.import_path().map(<[String]>::to_vec)),
        Expr::IndexName { expr, index, .. } => {
            let mut path = direct_import_path_expr(expr, analysis)?;
            path.push(index.as_str().to_owned());
            Some(path)
        }
        Expr::Group { expr, .. } | Expr::TypeAssertion { expr, .. } => {
            direct_import_path_expr(expr, analysis)
        }
        _ => None,
    }
}

fn track_values_stat(stat: &Stat, analysis: &mut ModuleAnalysis, loop_depth: usize) {
    match stat {
        Stat::Block { body, .. } => {
            for stat in body {
                track_values_stat(stat, analysis, loop_depth);
            }
        }
        Stat::Return { list, .. } => {
            for expr in list {
                track_values_expr(expr, analysis);
            }
        }
        Stat::Expr { expr, .. } => track_values_expr(expr, analysis),
        Stat::Local { vars, values, .. } => {
            for (index, local) in vars.iter().enumerate() {
                let variable = analysis.variable_mut(local.id);
                variable.initial_expr = values.get(index).map(Expr::syntax_id);
                variable.loop_depth = loop_depth;
            }
            for value in values {
                track_values_expr(value, analysis);
            }
        }
        Stat::Assign { vars, values, .. } => {
            for var in vars {
                assign_target(var, analysis);
            }
            for value in values {
                track_values_expr(value, analysis);
            }
        }
        Stat::CompoundAssign { var, value, .. } => {
            assign_target(var, analysis);
            track_values_expr(value, analysis);
        }
        Stat::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            track_values_expr(condition, analysis);
            track_values_stat(then_body, analysis, loop_depth);
            if let Some(else_body) = else_body.as_deref() {
                track_values_stat(else_body, analysis, loop_depth);
            }
        }
        Stat::While {
            condition, body, ..
        } => {
            track_values_expr(condition, analysis);
            track_values_stat(body, analysis, loop_depth + 1);
        }
        Stat::Repeat {
            condition, body, ..
        } => {
            track_values_stat(body, analysis, loop_depth + 1);
            track_values_expr(condition, analysis);
        }
        Stat::For {
            var,
            from,
            to,
            step,
            body,
            ..
        } => {
            analysis.variable_mut(var.id).loop_depth = loop_depth + 1;
            track_values_expr(from, analysis);
            track_values_expr(to, analysis);
            if let Some(step) = step {
                track_values_expr(step, analysis);
            }
            track_values_stat(body, analysis, loop_depth + 1);
        }
        Stat::ForIn {
            vars, values, body, ..
        } => {
            for var in vars {
                analysis.variable_mut(var.id).loop_depth = loop_depth + 1;
            }
            for value in values {
                track_values_expr(value, analysis);
            }
            track_values_stat(body, analysis, loop_depth + 1);
        }
        Stat::Function { name, func, .. } => {
            assign_target(name, analysis);
            track_values_expr(func, analysis);
        }
        Stat::LocalFunction { name, func, .. } => {
            let variable = analysis.variable_mut(name.id);
            variable.initial_expr = Some(func.syntax_id());
            variable.loop_depth = loop_depth;
            track_values_expr(func, analysis);
        }
        Stat::TypeFunction { func, .. } => track_values_expr(func, analysis),
        Stat::Class { members, .. } => {
            for member in members {
                track_values_stat(member, analysis, loop_depth);
            }
        }
        Stat::Error {
            expressions,
            statements,
            ..
        } => {
            for expr in expressions {
                track_values_expr(expr, analysis);
            }
            for stat in statements {
                track_values_stat(stat, analysis, loop_depth);
            }
        }
        Stat::DeclareGlobal { .. }
        | Stat::DeclareFunction { .. }
        | Stat::DeclareClass { .. }
        | Stat::TypeAlias { .. }
        | Stat::ClassProperty { .. }
        | Stat::Break { .. }
        | Stat::Continue { .. } => {}
    }
}

fn assign_target(expr: &Expr, analysis: &mut ModuleAnalysis) {
    match expr {
        Expr::Local { local, .. } => {
            analysis.variable_mut(local.id).written = true;
        }
        Expr::Global { name, .. } => {
            analysis.mark_global_written(name.as_str());
        }
        Expr::Group { expr, .. } | Expr::TypeAssertion { expr, .. } => {
            assign_target(expr, analysis);
        }
        _ => track_values_expr(expr, analysis),
    }
}

fn track_values_expr(expr: &Expr, analysis: &mut ModuleAnalysis) {
    match expr {
        Expr::Global { name, .. } => analysis.record_global_read(name.as_str()),
        Expr::Call { func, args, .. } => {
            track_values_expr(func, analysis);
            for arg in args {
                track_values_expr(arg, analysis);
            }
        }
        Expr::Binary { left, right, .. } => {
            track_values_expr(left, analysis);
            track_values_expr(right, analysis);
        }
        Expr::Unary { expr, .. }
        | Expr::TypeAssertion { expr, .. }
        | Expr::IndexName { expr, .. }
        | Expr::Group { expr, .. }
        | Expr::Instantiate { expr, .. } => {
            track_values_expr(expr, analysis);
        }
        Expr::IfElse {
            condition,
            true_expr,
            false_expr,
            ..
        } => {
            track_values_expr(condition, analysis);
            track_values_expr(true_expr, analysis);
            track_values_expr(false_expr, analysis);
        }
        Expr::IndexExpr { expr, index, .. } => {
            track_values_expr(expr, analysis);
            track_values_expr(index, analysis);
        }
        Expr::Table { items, .. } => {
            for item in items {
                if let Some(key) = &item.key {
                    track_values_expr(key, analysis);
                }
                track_values_expr(&item.value, analysis);
            }
        }
        Expr::InterpString { expressions, .. } | Expr::Error { expressions, .. } => {
            for expr in expressions {
                track_values_expr(expr, analysis);
            }
        }
        Expr::Function {
            args,
            self_arg,
            body,
            ..
        } => {
            for arg in args {
                analysis.variable_mut(arg.id).initial_expr = None;
            }
            if let Some(self_arg) = self_arg {
                analysis.variable_mut(self_arg.id).initial_expr = None;
            }
            track_values_stat(body, analysis, 0);
        }
        Expr::Nil { .. }
        | Expr::Bool { .. }
        | Expr::Number { .. }
        | Expr::Integer { .. }
        | Expr::String { .. }
        | Expr::Local { .. }
        | Expr::Varargs { .. } => {}
    }
}

fn analyze_builtin_calls(root: &Stat, options: &CompilerOptions, analysis: &mut ModuleAnalysis) {
    let initializers = collect_local_initializer_exprs(root);

    let disabled = disabled_builtin_ids(options, analysis);
    let mut builtins = BTreeMap::new();
    walk_stat(
        root,
        &mut BuiltinCollector {
            analysis,
            options,
            initializers: &initializers,
            disabled: &disabled,
            builtins: &mut builtins,
        },
    );
    analysis.builtins = builtins;
}

struct BuiltinCollector<'a, 'ast> {
    analysis: &'a ModuleAnalysis,
    options: &'a CompilerOptions,
    initializers: &'a BTreeMap<LocalId, &'ast Expr>,
    disabled: &'a BTreeSet<u8>,
    builtins: &'a mut BTreeMap<ExprId, BuiltinCall>,
}

impl<'ast> Visitor<'ast> for BuiltinCollector<'_, 'ast> {
    fn visit_expr(&mut self, _path: &NodePath, expr: &'ast Expr) -> WalkControl {
        let Expr::Call {
            syntax_id,
            func,
            args,
            is_self,
            ..
        } = expr
        else {
            return WalkControl::Continue;
        };
        if *is_self {
            return WalkControl::Continue;
        }

        let Some(path) = builtin_path_for_expr(func, self.analysis, self.initializers) else {
            return WalkControl::Continue;
        };
        let Some(function_id) = builtin_function_id(&path, self.options) else {
            return WalkControl::Continue;
        };
        if self.disabled.contains(&function_id) || !builtin_args_are_eligible(function_id, args) {
            return WalkControl::Continue;
        }

        self.builtins.insert(
            *syntax_id,
            BuiltinCall {
                path: path.iter().map(|part| (*part).to_owned()).collect(),
                function_id,
            },
        );
        WalkControl::Continue
    }
}

fn collect_local_initializer_exprs(root: &Stat) -> BTreeMap<LocalId, &Expr> {
    let mut collector = LocalInitializerCollector::default();
    walk_stat(root, &mut collector);
    collector.initializers
}

#[derive(Default)]
struct LocalInitializerCollector<'ast> {
    initializers: BTreeMap<LocalId, &'ast Expr>,
}

impl<'ast> Visitor<'ast> for LocalInitializerCollector<'ast> {
    fn visit_stat(&mut self, _path: &NodePath, stat: &'ast Stat) -> WalkControl {
        match stat {
            Stat::Local { vars, values, .. } => {
                for (index, local) in vars.iter().enumerate() {
                    if let Some(value) = values.get(index) {
                        self.initializers.insert(local.id, value);
                    }
                }
            }
            Stat::LocalFunction { name, func, .. } => {
                self.initializers.insert(name.id, func);
            }
            _ => {}
        }
        WalkControl::Continue
    }
}

fn builtin_path_for_expr<'a>(
    expr: &'a Expr,
    analysis: &ModuleAnalysis,
    initializers: &BTreeMap<LocalId, &'a Expr>,
) -> Option<Vec<&'a str>> {
    match expr {
        Expr::Local { local, .. } => {
            let fact = analysis.variable(local.id)?;
            if fact.is_written() {
                return None;
            }
            builtin_path_for_expr(initializers.get(&local.id)?, analysis, initializers)
        }
        Expr::IndexName { expr, index, .. } => {
            let object = builtin_object_name(expr, analysis, initializers)?;
            (analysis.global_state(object) == GlobalState::Default)
                .then(|| vec![object, index.as_str()])
        }
        Expr::Global { name, .. } => (analysis.global_state(name.as_str()) == GlobalState::Default)
            .then(|| vec![name.as_str()]),
        _ => None,
    }
}

fn builtin_object_name<'a>(
    expr: &'a Expr,
    analysis: &ModuleAnalysis,
    initializers: &BTreeMap<LocalId, &'a Expr>,
) -> Option<&'a str> {
    match expr {
        Expr::Local { local, .. } => {
            let fact = analysis.variable(local.id)?;
            if fact.is_written() {
                return None;
            }
            alias_initializer_object_name(initializers.get(&local.id)?)
        }
        Expr::Global { name, .. } => Some(name.as_str()),
        _ => None,
    }
}

fn alias_initializer_object_name(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Global { name, .. } => Some(name.as_str()),
        Expr::Binary {
            op: JsonBinaryOp::Or,
            left,
            ..
        } => match left.as_ref() {
            Expr::Global { name, .. } => Some(name.as_str()),
            _ => None,
        },
        _ => None,
    }
}

fn disabled_builtin_ids(options: &CompilerOptions, analysis: &ModuleAnalysis) -> BTreeSet<u8> {
    options
        .disabled_builtins
        .iter()
        .filter_map(|disabled| {
            let path = disabled.split('.').collect::<Vec<&str>>();
            let global_name = path.last()?;
            if path.is_empty()
                || path.len() > 2
                || path.iter().any(|part| part.is_empty())
                || analysis.global_state(global_name) != GlobalState::Default
            {
                return None;
            }
            builtin_function_id(&path, options)
        })
        .collect()
}

pub fn builtin_args_are_eligible(function_id: u8, args: &[Expr]) -> bool {
    function_id != BuiltinFunction::SELECT_VARARG || matches!(args, [_, Expr::Varargs { .. }])
}

pub fn builtin_function_id(path: &[&str], options: &CompilerOptions) -> Option<u8> {
    match path {
        [name] => global_builtin_function_id(name, options),
        [lib, name] if *lib == "math" => math_builtin_function_id(name),
        [lib, name] if *lib == "bit32" => bit32_builtin_function_id(name),
        [lib, name] if *lib == "string" => string_builtin_function_id(name),
        [lib, name] if *lib == "table" => table_builtin_function_id(name),
        [lib, name] if *lib == "buffer" => buffer_builtin_function_id(name, options),
        [lib, name] if *lib == "vector" => vector_builtin_function_id(name),
        [lib, name] if *lib == "integer" && options.fast_flag("LuauIntegerFastcalls") => {
            integer_builtin_function_id(name)
        }
        [lib, name]
            if options.vector_ctor.as_deref() == Some(*name)
                && options.vector_lib.as_deref() == Some(*lib) =>
        {
            Some(BuiltinFunction::VECTOR)
        }
        _ => None,
    }
}

fn global_builtin_function_id(name: &str, options: &CompilerOptions) -> Option<u8> {
    match name {
        "assert" => Some(BuiltinFunction::ASSERT),
        "type" => Some(BuiltinFunction::TYPE),
        "typeof" => Some(BuiltinFunction::TYPEOF),
        "rawset" => Some(BuiltinFunction::RAWSET),
        "rawget" => Some(BuiltinFunction::RAWGET),
        "rawequal" => Some(BuiltinFunction::RAWEQUAL),
        "unpack" => Some(BuiltinFunction::TABLE_UNPACK),
        "select" => Some(BuiltinFunction::SELECT_VARARG),
        "rawlen" => Some(BuiltinFunction::RAWLEN),
        "getmetatable" => Some(BuiltinFunction::GETMETATABLE),
        "setmetatable" => Some(BuiltinFunction::SETMETATABLE),
        "tonumber" => Some(BuiltinFunction::TONUMBER),
        "tostring" => Some(BuiltinFunction::TOSTRING),
        _ if options.vector_ctor.as_deref() == Some(name) && options.vector_lib.is_none() => {
            Some(BuiltinFunction::VECTOR)
        }
        _ => None,
    }
}

fn math_builtin_function_id(name: &str) -> Option<u8> {
    match name {
        "abs" => Some(BuiltinFunction::MATH_ABS),
        "acos" => Some(BuiltinFunction::MATH_ACOS),
        "asin" => Some(BuiltinFunction::MATH_ASIN),
        "atan2" => Some(BuiltinFunction::MATH_ATAN2),
        "atan" => Some(BuiltinFunction::MATH_ATAN),
        "ceil" => Some(BuiltinFunction::MATH_CEIL),
        "cosh" => Some(BuiltinFunction::MATH_COSH),
        "cos" => Some(BuiltinFunction::MATH_COS),
        "deg" => Some(BuiltinFunction::MATH_DEG),
        "exp" => Some(BuiltinFunction::MATH_EXP),
        "floor" => Some(BuiltinFunction::MATH_FLOOR),
        "fmod" => Some(BuiltinFunction::MATH_FMOD),
        "frexp" => Some(BuiltinFunction::MATH_FREXP),
        "ldexp" => Some(BuiltinFunction::MATH_LDEXP),
        "log10" => Some(BuiltinFunction::MATH_LOG10),
        "log" => Some(BuiltinFunction::MATH_LOG),
        "max" => Some(BuiltinFunction::MATH_MAX),
        "min" => Some(BuiltinFunction::MATH_MIN),
        "modf" => Some(BuiltinFunction::MATH_MODF),
        "pow" => Some(BuiltinFunction::MATH_POW),
        "rad" => Some(BuiltinFunction::MATH_RAD),
        "sinh" => Some(BuiltinFunction::MATH_SINH),
        "sin" => Some(BuiltinFunction::MATH_SIN),
        "sqrt" => Some(BuiltinFunction::MATH_SQRT),
        "tanh" => Some(BuiltinFunction::MATH_TANH),
        "tan" => Some(BuiltinFunction::MATH_TAN),
        "clamp" => Some(BuiltinFunction::MATH_CLAMP),
        "sign" => Some(BuiltinFunction::MATH_SIGN),
        "round" => Some(BuiltinFunction::MATH_ROUND),
        "lerp" => Some(BuiltinFunction::MATH_LERP),
        "isnan" => Some(BuiltinFunction::MATH_ISNAN),
        "isinf" => Some(BuiltinFunction::MATH_ISINF),
        "isfinite" => Some(BuiltinFunction::MATH_ISFINITE),
        _ => None,
    }
}

fn bit32_builtin_function_id(name: &str) -> Option<u8> {
    match name {
        "arshift" => Some(BuiltinFunction::BIT32_ARSHIFT),
        "band" => Some(BuiltinFunction::BIT32_BAND),
        "bnot" => Some(BuiltinFunction::BIT32_BNOT),
        "bor" => Some(BuiltinFunction::BIT32_BOR),
        "bxor" => Some(BuiltinFunction::BIT32_BXOR),
        "btest" => Some(BuiltinFunction::BIT32_BTEST),
        "extract" => Some(BuiltinFunction::BIT32_EXTRACT),
        "lrotate" => Some(BuiltinFunction::BIT32_LROTATE),
        "lshift" => Some(BuiltinFunction::BIT32_LSHIFT),
        "replace" => Some(BuiltinFunction::BIT32_REPLACE),
        "rrotate" => Some(BuiltinFunction::BIT32_RROTATE),
        "rshift" => Some(BuiltinFunction::BIT32_RSHIFT),
        "countlz" => Some(BuiltinFunction::BIT32_COUNTLZ),
        "countrz" => Some(BuiltinFunction::BIT32_COUNTRZ),
        "byteswap" => Some(BuiltinFunction::BIT32_BYTESWAP),
        _ => None,
    }
}

fn string_builtin_function_id(name: &str) -> Option<u8> {
    match name {
        "byte" => Some(BuiltinFunction::STRING_BYTE),
        "char" => Some(BuiltinFunction::STRING_CHAR),
        "len" => Some(BuiltinFunction::STRING_LEN),
        "sub" => Some(BuiltinFunction::STRING_SUB),
        _ => None,
    }
}

fn table_builtin_function_id(name: &str) -> Option<u8> {
    match name {
        "insert" => Some(BuiltinFunction::TABLE_INSERT),
        "unpack" => Some(BuiltinFunction::TABLE_UNPACK),
        _ => None,
    }
}

fn buffer_builtin_function_id(name: &str, options: &CompilerOptions) -> Option<u8> {
    match name {
        "readi8" => Some(BuiltinFunction::BUFFER_READI8),
        "readu8" => Some(BuiltinFunction::BUFFER_READU8),
        "writei8" | "writeu8" => Some(BuiltinFunction::BUFFER_WRITEU8),
        "readi16" => Some(BuiltinFunction::BUFFER_READI16),
        "readu16" => Some(BuiltinFunction::BUFFER_READU16),
        "writei16" | "writeu16" => Some(BuiltinFunction::BUFFER_WRITEU16),
        "readi32" => Some(BuiltinFunction::BUFFER_READI32),
        "readu32" => Some(BuiltinFunction::BUFFER_READU32),
        "writei32" | "writeu32" => Some(BuiltinFunction::BUFFER_WRITEU32),
        "readf32" => Some(BuiltinFunction::BUFFER_READF32),
        "writef32" => Some(BuiltinFunction::BUFFER_WRITEF32),
        "readf64" => Some(BuiltinFunction::BUFFER_READF64),
        "writef64" => Some(BuiltinFunction::BUFFER_WRITEF64),
        "readinteger"
            if options.fast_flag("LuauIntegerFastcalls")
                && options.fast_flag("LuauIntegerBufferFastcalls") =>
        {
            Some(BuiltinFunction::BUFFER_READINTEGER)
        }
        "writeinteger"
            if options.fast_flag("LuauIntegerFastcalls")
                && options.fast_flag("LuauIntegerBufferFastcalls") =>
        {
            Some(BuiltinFunction::BUFFER_WRITEINTEGER)
        }
        _ => None,
    }
}

fn vector_builtin_function_id(name: &str) -> Option<u8> {
    match name {
        "create" => Some(BuiltinFunction::VECTOR),
        "magnitude" => Some(BuiltinFunction::VECTOR_MAGNITUDE),
        "normalize" => Some(BuiltinFunction::VECTOR_NORMALIZE),
        "cross" => Some(BuiltinFunction::VECTOR_CROSS),
        "dot" => Some(BuiltinFunction::VECTOR_DOT),
        "floor" => Some(BuiltinFunction::VECTOR_FLOOR),
        "ceil" => Some(BuiltinFunction::VECTOR_CEIL),
        "abs" => Some(BuiltinFunction::VECTOR_ABS),
        "sign" => Some(BuiltinFunction::VECTOR_SIGN),
        "clamp" => Some(BuiltinFunction::VECTOR_CLAMP),
        "min" => Some(BuiltinFunction::VECTOR_MIN),
        "max" => Some(BuiltinFunction::VECTOR_MAX),
        "lerp" => Some(BuiltinFunction::VECTOR_LERP),
        _ => None,
    }
}

fn integer_builtin_function_id(name: &str) -> Option<u8> {
    match name {
        "create" => Some(BuiltinFunction::INTEGER_CREATE),
        "tonumber" => Some(BuiltinFunction::INTEGER_TONUMBER),
        "neg" => Some(BuiltinFunction::INTEGER_NEG),
        "add" => Some(BuiltinFunction::INTEGER_ADD),
        "sub" => Some(BuiltinFunction::INTEGER_SUB),
        "mul" => Some(BuiltinFunction::INTEGER_MUL),
        "div" => Some(BuiltinFunction::INTEGER_DIV),
        "min" => Some(BuiltinFunction::INTEGER_MIN),
        "max" => Some(BuiltinFunction::INTEGER_MAX),
        "rem" => Some(BuiltinFunction::INTEGER_REM),
        "idiv" => Some(BuiltinFunction::INTEGER_IDIV),
        "udiv" => Some(BuiltinFunction::INTEGER_UDIV),
        "urem" => Some(BuiltinFunction::INTEGER_UREM),
        "mod" => Some(BuiltinFunction::INTEGER_MOD),
        "clamp" => Some(BuiltinFunction::INTEGER_CLAMP),
        "band" => Some(BuiltinFunction::INTEGER_BAND),
        "bor" => Some(BuiltinFunction::INTEGER_BOR),
        "bnot" => Some(BuiltinFunction::INTEGER_BNOT),
        "bxor" => Some(BuiltinFunction::INTEGER_BXOR),
        "lt" => Some(BuiltinFunction::INTEGER_LT),
        "le" => Some(BuiltinFunction::INTEGER_LE),
        "ult" => Some(BuiltinFunction::INTEGER_ULT),
        "ule" => Some(BuiltinFunction::INTEGER_ULE),
        "gt" => Some(BuiltinFunction::INTEGER_GT),
        "ge" => Some(BuiltinFunction::INTEGER_GE),
        "ugt" => Some(BuiltinFunction::INTEGER_UGT),
        "uge" => Some(BuiltinFunction::INTEGER_UGE),
        "lshift" => Some(BuiltinFunction::INTEGER_LSHIFT),
        "rshift" => Some(BuiltinFunction::INTEGER_RSHIFT),
        "arshift" => Some(BuiltinFunction::INTEGER_ARSHIFT),
        "lrotate" => Some(BuiltinFunction::INTEGER_LROTATE),
        "rrotate" => Some(BuiltinFunction::INTEGER_RROTATE),
        "extract" => Some(BuiltinFunction::INTEGER_EXTRACT),
        "btest" => Some(BuiltinFunction::INTEGER_BTEST),
        "countrz" => Some(BuiltinFunction::INTEGER_COUNTRZ),
        "countlz" => Some(BuiltinFunction::INTEGER_COUNTLZ),
        "bswap" => Some(BuiltinFunction::INTEGER_BSWAP),
        _ => None,
    }
}

fn analyze_constants(root: &Stat, options: &CompilerOptions, analysis: &mut ModuleAnalysis) {
    if options.optimization_level == 0 {
        return;
    }

    let constant_table_locals = analyze_constant_table_locals(root, &analysis.variables);
    let fold_library_constants =
        options.optimization_level >= 2 && !analysis.getfenv_used && !analysis.setfenv_used;
    let mut analyzer = ConstantAnalyzer {
        options,
        builtins: &analysis.builtins,
        variables: &analysis.variables,
        constant_table_locals: &constant_table_locals,
        globals: &analysis.globals,
        fold_library_constants,
        expr_constants: BTreeMap::new(),
        local_constants: BTreeMap::new(),
        table_props: BTreeMap::new(),
        table_expr_props: BTreeMap::new(),
        constant_locals: BTreeSet::new(),
    };

    analyzer.analyze_stat(root);

    analysis.constants = analyzer.expr_constants;
    analysis.local_constants = analyzer.local_constants;
    analysis.table_props = analyzer.table_props;
    for local_id in analyzer.constant_locals {
        analysis.variable_mut(local_id).constant = true;
    }
}

struct ConstantAnalyzer<'a> {
    options: &'a CompilerOptions,
    builtins: &'a BTreeMap<ExprId, BuiltinCall>,
    variables: &'a BTreeMap<LocalId, VariableFact>,
    constant_table_locals: &'a BTreeMap<LocalId, TableConstantKind>,
    globals: &'a BTreeMap<String, GlobalState>,
    fold_library_constants: bool,
    expr_constants: BTreeMap<ExprId, ConstantValue>,
    local_constants: BTreeMap<LocalId, ConstantValue>,
    table_props: BTreeMap<LocalId, BTreeMap<String, ConstantValue>>,
    table_expr_props: BTreeMap<ExprId, BTreeMap<String, ConstantValue>>,
    constant_locals: BTreeSet<LocalId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TableConstantKind {
    ConstantTable,
    ConstantOther,
    NotConstant,
}

fn analyze_constant_table_locals(
    root: &Stat,
    variables: &BTreeMap<LocalId, VariableFact>,
) -> BTreeMap<LocalId, TableConstantKind> {
    let mut tracker = TableConstantTracker {
        variables,
        locals: BTreeMap::new(),
    };
    tracker.analyze_stat(root);
    tracker.locals
}

struct TableConstantTracker<'a> {
    variables: &'a BTreeMap<LocalId, VariableFact>,
    locals: BTreeMap<LocalId, TableConstantKind>,
}

impl TableConstantTracker<'_> {
    // Mirrors upstream's TableMutationTracker: only locals initialized from
    // constant table literals, and never potentially mutated, can feed
    // property constants.
    fn analyze_stat(&mut self, stat: &Stat) {
        match stat {
            Stat::Block { body, .. } => {
                for stat in body {
                    self.analyze_stat(stat);
                }
            }
            Stat::Return { list, .. } => {
                for expr in list {
                    self.observe_mutations(expr, self.could_be_table_reference(expr));
                }
            }
            Stat::Expr { expr, .. } => self.observe_mutations(expr, false),
            Stat::Local { vars, values, .. } => {
                for (local, rhs) in vars.iter().zip(values.iter()) {
                    let variable = self.variables.get(&local.id);
                    if !variable.is_some_and(VariableFact::is_written) {
                        if self.is_constant_table_literal(rhs) {
                            self.locals
                                .insert(local.id, TableConstantKind::ConstantTable);
                        } else if self.is_non_table_constant(rhs) {
                            self.locals
                                .insert(local.id, TableConstantKind::ConstantOther);
                        }
                    }

                    if !self.locals.contains_key(&local.id) {
                        self.observe_mutations(rhs, self.could_be_table_reference(rhs));
                    }
                }

                for value in values.iter().skip(vars.len()) {
                    self.observe_mutations(value, false);
                }
            }
            Stat::Assign { vars, values, .. } => {
                for rhs in values.iter().take(vars.len()) {
                    self.observe_mutations(rhs, self.could_be_table_reference(rhs));
                }
                for value in values.iter().skip(vars.len()) {
                    self.observe_mutations(value, false);
                }
                for lhs in vars {
                    self.observe_mutations(lhs, true);
                }
            }
            Stat::CompoundAssign { var, value, .. } => {
                self.observe_mutations(value, self.could_be_table_reference(value));
                self.observe_mutations(var, true);
            }
            Stat::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                self.observe_mutations(condition, false);
                self.analyze_stat(then_body);
                if let Some(else_body) = else_body.as_deref() {
                    self.analyze_stat(else_body);
                }
            }
            Stat::While {
                condition, body, ..
            } => {
                self.observe_mutations(condition, false);
                self.analyze_stat(body);
            }
            Stat::Repeat {
                condition, body, ..
            } => {
                self.analyze_stat(body);
                self.observe_mutations(condition, false);
            }
            Stat::For {
                from,
                to,
                step,
                body,
                ..
            } => {
                self.observe_mutations(from, false);
                self.observe_mutations(to, false);
                if let Some(step) = step {
                    self.observe_mutations(step, false);
                }
                self.analyze_stat(body);
            }
            Stat::ForIn { values, body, .. } => {
                for value in values {
                    self.observe_mutations(value, true);
                }
                self.analyze_stat(body);
            }
            Stat::Function { name, func, .. } => {
                self.observe_mutations(func, false);
                self.observe_mutations(name, true);
            }
            Stat::LocalFunction { func, .. } | Stat::TypeFunction { func, .. } => {
                self.observe_mutations(func, false);
            }
            Stat::Class { members, .. } => {
                for member in members {
                    self.analyze_stat(member);
                }
            }
            Stat::Error {
                expressions,
                statements,
                ..
            } => {
                for expr in expressions {
                    self.observe_mutations(expr, false);
                }
                for stat in statements {
                    self.analyze_stat(stat);
                }
            }
            Stat::DeclareGlobal { .. }
            | Stat::DeclareFunction { .. }
            | Stat::DeclareClass { .. }
            | Stat::TypeAlias { .. }
            | Stat::ClassProperty { .. }
            | Stat::Break { .. }
            | Stat::Continue { .. } => {}
        }
    }

    fn is_non_table_constant(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Nil { .. }
            | Expr::Bool { .. }
            | Expr::Number { .. }
            | Expr::Integer { .. }
            | Expr::String { .. } => true,
            Expr::Local { local, .. } => {
                self.locals.get(&local.id) == Some(&TableConstantKind::ConstantOther)
            }
            Expr::IndexName { expr, .. } => self.direct_local_id(expr).is_some_and(|local_id| {
                self.locals.get(&local_id) == Some(&TableConstantKind::ConstantTable)
            }),
            Expr::IndexExpr { expr, index, .. } => {
                self.direct_local_id(expr).is_some_and(|local_id| {
                    self.locals.get(&local_id) == Some(&TableConstantKind::ConstantTable)
                }) && self.is_non_table_constant(index)
            }
            Expr::Unary { expr, .. }
            | Expr::TypeAssertion { expr, .. }
            | Expr::Group { expr, .. }
            | Expr::Instantiate { expr, .. } => self.is_non_table_constant(expr),
            Expr::Binary { left, right, .. } => {
                self.is_non_table_constant(left) && self.is_non_table_constant(right)
            }
            Expr::IfElse {
                condition,
                true_expr,
                false_expr,
                ..
            } => {
                self.is_non_table_constant(condition)
                    && self.is_non_table_constant(true_expr)
                    && self.is_non_table_constant(false_expr)
            }
            Expr::InterpString { expressions, .. } | Expr::Error { expressions, .. } => expressions
                .iter()
                .all(|expr| self.is_non_table_constant(expr)),
            Expr::Global { .. }
            | Expr::Varargs { .. }
            | Expr::Call { .. }
            | Expr::Table { .. }
            | Expr::Function { .. } => false,
        }
    }

    fn is_constant_table_literal(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Table { items, .. } => items.iter().all(|item| {
                item.key
                    .as_ref()
                    .is_none_or(|key| self.is_non_table_constant(key))
                    && self.is_non_table_constant(&item.value)
            }),
            Expr::TypeAssertion { expr, .. }
            | Expr::Group { expr, .. }
            | Expr::Instantiate { expr, .. } => self.is_constant_table_literal(expr),
            _ => false,
        }
    }

    fn could_be_table_reference(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Local { .. } => true,
            Expr::IfElse {
                true_expr,
                false_expr,
                ..
            } => {
                self.could_be_table_reference(true_expr)
                    || self.could_be_table_reference(false_expr)
            }
            Expr::Binary {
                op: JsonBinaryOp::And | JsonBinaryOp::Or,
                left,
                right,
                ..
            } => self.could_be_table_reference(left) || self.could_be_table_reference(right),
            Expr::TypeAssertion { expr, .. }
            | Expr::Group { expr, .. }
            | Expr::Instantiate { expr, .. } => self.could_be_table_reference(expr),
            _ => false,
        }
    }

    fn observe_mutations(&mut self, expr: &Expr, could_mutate_table: bool) {
        match expr {
            Expr::Local { local, .. } => {
                if could_mutate_table && self.locals.contains_key(&local.id) {
                    self.locals.insert(local.id, TableConstantKind::NotConstant);
                }
            }
            Expr::Call { func, args, .. } => {
                self.observe_mutations(func, true);
                for arg in args {
                    self.observe_mutations(arg, self.could_be_table_reference(arg));
                }
            }
            Expr::IndexName { expr, .. } => self.observe_mutations(expr, could_mutate_table),
            Expr::IndexExpr { expr, index, .. } => {
                self.observe_mutations(index, false);
                self.observe_mutations(expr, could_mutate_table);
            }
            Expr::Function { body, .. } => self.analyze_stat(body),
            Expr::Table { items, .. } => {
                for item in items {
                    if let Some(key) = &item.key {
                        self.observe_mutations(key, false);
                    }
                    self.observe_mutations(&item.value, self.could_be_table_reference(&item.value));
                }
            }
            Expr::Unary { expr, .. } => self.observe_mutations(expr, false),
            Expr::Binary {
                op, left, right, ..
            } => {
                let short_circuiting = matches!(op, JsonBinaryOp::And | JsonBinaryOp::Or);
                self.observe_mutations(left, short_circuiting);
                self.observe_mutations(right, short_circuiting);
            }
            Expr::IfElse {
                condition,
                true_expr,
                false_expr,
                ..
            } => {
                self.observe_mutations(condition, false);
                self.observe_mutations(true_expr, could_mutate_table);
                self.observe_mutations(false_expr, could_mutate_table);
            }
            Expr::InterpString { expressions, .. } | Expr::Error { expressions, .. } => {
                for expr in expressions {
                    self.observe_mutations(expr, false);
                }
            }
            Expr::TypeAssertion { expr, .. }
            | Expr::Group { expr, .. }
            | Expr::Instantiate { expr, .. } => self.observe_mutations(expr, could_mutate_table),
            Expr::Nil { .. }
            | Expr::Bool { .. }
            | Expr::Number { .. }
            | Expr::Integer { .. }
            | Expr::String { .. }
            | Expr::Global { .. }
            | Expr::Varargs { .. } => {}
        }
    }

    fn direct_local_id(&self, expr: &Expr) -> Option<LocalId> {
        match expr {
            Expr::Local { local, .. } => Some(local.id),
            Expr::TypeAssertion { expr, .. } | Expr::Group { expr, .. } => {
                self.direct_local_id(expr)
            }
            _ => None,
        }
    }
}

impl ConstantAnalyzer<'_> {
    fn analyze_stat(&mut self, stat: &Stat) {
        match stat {
            Stat::Block { body, .. } => {
                for stat in body {
                    self.analyze_stat(stat);
                }
            }
            Stat::Return { list, .. } => {
                for expr in list {
                    self.analyze_expr(expr);
                }
            }
            Stat::Expr { expr, .. } => {
                self.analyze_expr(expr);
            }
            Stat::Local { vars, values, .. } => {
                for (index, value) in values.iter().enumerate() {
                    let constant = self.analyze_expr(value);
                    if let Some(local) = vars.get(index) {
                        self.record_local_table_props(local.id, value);
                        self.record_local_value(local.id, constant);
                    }
                }

                if vars.len() > values.len() {
                    let last_is_multret = values.last().is_some_and(|value| {
                        matches!(value, Expr::Call { .. } | Expr::Varargs { .. })
                    });
                    if !last_is_multret {
                        for local in vars.iter().skip(values.len()) {
                            self.record_local_value(local.id, Some(ConstantValue::Nil));
                        }
                    }
                }
            }
            Stat::Assign { vars, values, .. } => {
                for var in vars {
                    self.analyze_expr(var);
                }
                for value in values {
                    self.analyze_expr(value);
                }
            }
            Stat::CompoundAssign { var, value, .. } => {
                self.analyze_expr(var);
                self.analyze_expr(value);
            }
            Stat::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                self.analyze_expr(condition);
                self.analyze_stat(then_body);
                if let Some(else_body) = else_body.as_deref() {
                    self.analyze_stat(else_body);
                }
            }
            Stat::While {
                condition, body, ..
            } => {
                self.analyze_expr(condition);
                self.analyze_stat(body);
            }
            Stat::Repeat {
                condition, body, ..
            } => {
                self.analyze_stat(body);
                self.analyze_expr(condition);
            }
            Stat::For {
                from,
                to,
                step,
                body,
                ..
            } => {
                self.analyze_expr(from);
                self.analyze_expr(to);
                if let Some(step) = step {
                    self.analyze_expr(step);
                }
                self.analyze_stat(body);
            }
            Stat::ForIn { values, body, .. } => {
                for value in values {
                    self.analyze_expr(value);
                }
                self.analyze_stat(body);
            }
            Stat::Function { name, func, .. } => {
                self.analyze_expr(name);
                self.analyze_expr(func);
            }
            Stat::LocalFunction { name, func, .. } => {
                let constant = self.analyze_expr(func);
                self.record_local_value(name.id, constant);
            }
            Stat::TypeFunction { func, .. } => {
                self.analyze_expr(func);
            }
            Stat::Class { members, .. } => {
                for member in members {
                    self.analyze_stat(member);
                }
            }
            Stat::Error {
                expressions,
                statements,
                ..
            } => {
                for expr in expressions {
                    self.analyze_expr(expr);
                }
                for stat in statements {
                    self.analyze_stat(stat);
                }
            }
            Stat::DeclareGlobal { .. }
            | Stat::DeclareFunction { .. }
            | Stat::DeclareClass { .. }
            | Stat::TypeAlias { .. }
            | Stat::ClassProperty { .. }
            | Stat::Break { .. }
            | Stat::Continue { .. } => {}
        }
    }

    fn analyze_expr(&mut self, expr: &Expr) -> Option<ConstantValue> {
        let constant = match expr {
            Expr::Nil { .. } => Some(ConstantValue::Nil),
            Expr::Bool { value, .. } => Some(ConstantValue::Bool(*value)),
            Expr::Number { value, .. } => value.as_f64().map(ConstantValue::Number),
            Expr::Integer { .. } => None,
            Expr::String { value, .. } => Some(ConstantValue::String(value.clone())),
            Expr::Global { .. } | Expr::Varargs { .. } => None,
            Expr::Local { local, .. } => self.local_constants.get(&local.id).cloned(),
            Expr::Call {
                syntax_id,
                func,
                args,
                ..
            } => {
                self.analyze_expr(func);
                let args = args
                    .iter()
                    .map(|arg| self.analyze_expr(arg))
                    .collect::<Vec<_>>();
                self.fold_builtin_call(*syntax_id, &args)
            }
            Expr::Binary {
                op, left, right, ..
            } => {
                let left = self.analyze_expr(left);
                let right = self.analyze_expr(right);
                fold_binary_constant(*op, left.as_ref(), right.as_ref())
            }
            Expr::Unary { op, expr, .. } => {
                let arg = self.analyze_expr(expr);
                fold_unary_constant(*op, arg.as_ref())
            }
            Expr::TypeAssertion { expr, .. }
            | Expr::Group { expr, .. }
            | Expr::Instantiate { expr, .. } => self.analyze_expr(expr),
            Expr::IfElse {
                condition,
                true_expr,
                false_expr,
                ..
            } => {
                let condition = self.analyze_expr(condition);
                let true_constant = self.analyze_expr(true_expr);
                let false_constant = self.analyze_expr(false_expr);
                condition.and_then(|condition| {
                    if constant_truthiness(&condition) {
                        true_constant
                    } else {
                        false_constant
                    }
                })
            }
            Expr::IndexName { expr, index, .. } => {
                let value = self.analyze_expr(expr);
                if let Some(value) = self.table_prop_value(expr, index.as_str()) {
                    Some(value)
                } else if let Some(value) = value
                    && let Some(component) = vector_component_constant(&value, index.as_str())
                {
                    Some(component)
                } else {
                    self.known_member_constant(expr, index.as_str())
                }
            }
            Expr::IndexExpr { expr, index, .. } => {
                let index = self.analyze_expr(index);
                self.analyze_expr(expr);
                if let Some(ConstantValue::String(key)) = index.as_ref()
                    && !key.is_empty()
                {
                    self.table_prop_value(expr, key.as_str())
                } else {
                    None
                }
            }
            Expr::Table { items, .. } => {
                for item in items {
                    if let Some(key) = &item.key {
                        self.analyze_expr(key);
                    }
                    self.analyze_expr(&item.value);
                }
                if let Some(props) = self.table_props_from_items(items) {
                    self.table_expr_props.insert(expr.syntax_id(), props);
                }
                None
            }
            Expr::InterpString {
                strings,
                expressions,
                ..
            } => self.fold_interp_string(strings, expressions),
            Expr::Function { body, .. } => {
                self.analyze_stat(body);
                None
            }
            Expr::Error { expressions, .. } => {
                for expr in expressions {
                    self.analyze_expr(expr);
                }
                None
            }
        };

        if let Some(value) = &constant {
            self.expr_constants.insert(expr.syntax_id(), value.clone());
        }
        constant
    }

    fn record_local_value(&mut self, local_id: LocalId, value: Option<ConstantValue>) {
        let Some(variable) = self.variables.get(&local_id) else {
            return;
        };
        if variable.is_written() {
            return;
        }
        if let Some(value) = value {
            self.local_constants.insert(local_id, value);
            self.constant_locals.insert(local_id);
        }
    }

    fn record_local_table_props(&mut self, local_id: LocalId, value: &Expr) {
        if self.constant_table_locals.get(&local_id) != Some(&TableConstantKind::ConstantTable) {
            return;
        }
        if let Some(props) = self.table_props_for_expr(value) {
            self.table_props.insert(local_id, props);
        }
    }

    fn table_prop_value(&self, expr: &Expr, key: &str) -> Option<ConstantValue> {
        let Expr::Local { local, .. } = unwrapped_expr(expr) else {
            return None;
        };
        self.table_props.get(&local.id)?.get(key).cloned()
    }

    fn table_props_for_expr(&self, expr: &Expr) -> Option<BTreeMap<String, ConstantValue>> {
        match expr {
            Expr::Table { syntax_id, .. } => self.table_expr_props.get(syntax_id).cloned(),
            Expr::TypeAssertion { expr, .. }
            | Expr::Group { expr, .. }
            | Expr::Instantiate { expr, .. } => self.table_props_for_expr(expr),
            _ => None,
        }
    }

    fn table_props_from_items(
        &self,
        items: &[ruau_ast::syntax::TableItem],
    ) -> Option<BTreeMap<String, ConstantValue>> {
        let mut props = BTreeMap::new();
        for item in items {
            let key = item.key.as_ref()?;
            let Some(ConstantValue::String(key)) = self.expr_constants.get(&key.syntax_id()) else {
                return None;
            };
            if key.is_empty() {
                return None;
            }
            let value = self.expr_constants.get(&item.value.syntax_id())?.clone();
            props.insert(key.clone(), value);
        }

        (props.len() == items.len()).then_some(props)
    }

    fn fold_builtin_call(
        &self,
        call_id: ExprId,
        args: &[Option<ConstantValue>],
    ) -> Option<ConstantValue> {
        if !self.fold_library_constants {
            return None;
        }
        let builtin = self.builtins.get(&call_id)?;
        fold_builtin_constant(builtin.function_id(), args)
    }

    fn fold_interp_string(
        &mut self,
        strings: &[String],
        expressions: &[Expr],
    ) -> Option<ConstantValue> {
        let expression_constants = expressions
            .iter()
            .map(|expr| self.analyze_expr(expr))
            .collect::<Vec<_>>();
        if expression_constants
            .iter()
            .any(|constant| !matches!(constant, Some(ConstantValue::String(_))))
        {
            return None;
        }

        let mut result = String::new();
        for (index, string) in strings.iter().enumerate() {
            result.push_str(string);
            if let Some(Some(ConstantValue::String(value))) = expression_constants.get(index) {
                result.push_str(value);
            }
        }
        (result.len() <= super::CONSTANT_STRING_FOLD_LIMIT).then_some(ConstantValue::String(result))
    }

    fn known_member_constant(&self, expr: &Expr, member: &str) -> Option<ConstantValue> {
        if !self.fold_library_constants {
            return None;
        }
        let Expr::Global { name, .. } = expr else {
            return None;
        };
        let library = name.as_str();
        if self.globals.get(library).copied().unwrap_or_default() != GlobalState::Default {
            return None;
        }
        if library == "math"
            && let Some(value) = math_member_constant(member)
        {
            return Some(value);
        }
        self.options
            .known_members
            .iter()
            .find(|known| known.library == library && known.member == member)
            .map(|known| known_member_value_to_constant(&known.value))
    }
}

fn fold_unary_constant(op: JsonUnaryOp, arg: Option<&ConstantValue>) -> Option<ConstantValue> {
    let arg = arg?;
    match (op, arg) {
        (JsonUnaryOp::Not, value) => Some(ConstantValue::Bool(!constant_truthiness(value))),
        (JsonUnaryOp::Minus, ConstantValue::Number(value)) => Some(ConstantValue::Number(-value)),
        (JsonUnaryOp::Minus, ConstantValue::Vector { bits }) => Some(ConstantValue::Vector {
            bits: bits.map(|bits| (-f32::from_bits(bits)).to_bits()),
        }),
        // A byte string carries the `U+FFFF` byte-preservation marker, so its char/byte length
        // is not the decoded Luau byte length; defer `#` on it to a runtime op.
        (JsonUnaryOp::Len, ConstantValue::String(value)) if !value.contains('\u{ffff}') => {
            Some(ConstantValue::Number(value.len() as f64))
        }
        _ => None,
    }
}

fn fold_binary_constant(
    op: JsonBinaryOp,
    left: Option<&ConstantValue>,
    right: Option<&ConstantValue>,
) -> Option<ConstantValue> {
    let left = left?;
    match op {
        JsonBinaryOp::And => {
            if constant_truthiness(left) {
                right.cloned()
            } else {
                Some(left.clone())
            }
        }
        JsonBinaryOp::Or => {
            if constant_truthiness(left) {
                Some(left.clone())
            } else {
                right.cloned()
            }
        }
        _ => {
            let right = right?;
            match op {
                JsonBinaryOp::Add => numeric_binary(left, right, |left, right| left + right)
                    .or_else(|| vector_binary(left, right, VectorBinaryOp::Add)),
                JsonBinaryOp::Sub => numeric_binary(left, right, |left, right| left - right)
                    .or_else(|| vector_binary(left, right, VectorBinaryOp::Sub)),
                JsonBinaryOp::Mul => numeric_binary(left, right, |left, right| left * right)
                    .or_else(|| vector_binary(left, right, VectorBinaryOp::Mul)),
                JsonBinaryOp::Div => numeric_binary(left, right, |left, right| left / right)
                    .or_else(|| vector_binary(left, right, VectorBinaryOp::Div)),
                JsonBinaryOp::FloorDiv => {
                    numeric_binary(left, right, |left, right| (left / right).floor())
                        .or_else(|| vector_binary(left, right, VectorBinaryOp::FloorDiv))
                }
                JsonBinaryOp::Mod => numeric_binary(left, right, luau_fold_mod),
                JsonBinaryOp::Pow => numeric_binary(left, right, f64::powf),
                JsonBinaryOp::Concat => match (left, right) {
                    (ConstantValue::String(left), ConstantValue::String(right))
                        if left.len() + right.len() <= super::CONSTANT_STRING_FOLD_LIMIT =>
                    {
                        Some(ConstantValue::String(format!("{left}{right}")))
                    }
                    _ => None,
                },
                JsonBinaryOp::CompareEq => Some(ConstantValue::Bool(left == right)),
                JsonBinaryOp::CompareNe => Some(ConstantValue::Bool(left != right)),
                JsonBinaryOp::CompareLt => numeric_compare(left, right, |left, right| left < right),
                JsonBinaryOp::CompareLe => {
                    numeric_compare(left, right, |left, right| left <= right)
                }
                JsonBinaryOp::CompareGt => numeric_compare(left, right, |left, right| left > right),
                JsonBinaryOp::CompareGe => {
                    numeric_compare(left, right, |left, right| left >= right)
                }
                JsonBinaryOp::And | JsonBinaryOp::Or => unreachable!("handled above"),
            }
        }
    }
}

#[derive(Clone, Copy)]
enum VectorBinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    FloorDiv,
}

fn vector_binary(
    left: &ConstantValue,
    right: &ConstantValue,
    op: VectorBinaryOp,
) -> Option<ConstantValue> {
    match (left, right) {
        (ConstantValue::Vector { bits: left }, ConstantValue::Vector { bits: right }) => {
            vector_vector_binary(*left, *right, op)
        }
        (ConstantValue::Number(left), ConstantValue::Vector { bits: right }) => {
            vector_scalar_binary(*left as f32, *right, op, ScalarSide::Left)
        }
        (ConstantValue::Vector { bits: left }, ConstantValue::Number(right)) => {
            vector_scalar_binary(*right as f32, *left, op, ScalarSide::Right)
        }
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum ScalarSide {
    Left,
    Right,
}

fn vector_vector_binary(
    left: [u32; 4],
    right: [u32; 4],
    op: VectorBinaryOp,
) -> Option<ConstantValue> {
    match op {
        VectorBinaryOp::Add => Some(vector_binary_result(left, right, |left, right| {
            left + right
        })),
        VectorBinaryOp::Sub => Some(vector_binary_result(left, right, |left, right| {
            left - right
        })),
        VectorBinaryOp::Mul => {
            vector_binary_result_with_guard(left, right, |left, right| left * right)
        }
        VectorBinaryOp::Div => {
            vector_binary_result_with_guard(left, right, |left, right| left / right)
        }
        VectorBinaryOp::FloorDiv => {
            vector_binary_result_with_guard(left, right, |left, right| (left / right).floor())
        }
    }
}

fn vector_scalar_binary(
    scalar: f32,
    vector: [u32; 4],
    op: VectorBinaryOp,
    side: ScalarSide,
) -> Option<ConstantValue> {
    match op {
        VectorBinaryOp::Mul => {
            vector_scalar_result_with_guard(scalar, vector, side, |left, right| left * right)
        }
        VectorBinaryOp::Div => {
            vector_scalar_result_with_guard(scalar, vector, side, |left, right| left / right)
        }
        VectorBinaryOp::FloorDiv => {
            vector_scalar_result_with_guard(scalar, vector, side, |left, right| {
                (left / right).floor()
            })
        }
        VectorBinaryOp::Add | VectorBinaryOp::Sub => None,
    }
}

fn vector_binary_result(
    left: [u32; 4],
    right: [u32; 4],
    op: impl Fn(f32, f32) -> f32,
) -> ConstantValue {
    ConstantValue::Vector {
        bits: std::array::from_fn(|index| {
            op(f32::from_bits(left[index]), f32::from_bits(right[index])).to_bits()
        }),
    }
}

fn vector_binary_result_with_guard(
    left: [u32; 4],
    right: [u32; 4],
    op: impl Fn(f32, f32) -> f32,
) -> Option<ConstantValue> {
    let had_w = f32::from_bits(left[3]) != 0.0 || f32::from_bits(right[3]) != 0.0;
    let result_w = op(f32::from_bits(left[3]), f32::from_bits(right[3]));
    if result_w != 0.0 && !had_w {
        return None;
    }

    Some(vector_binary_result(left, right, op))
}

fn vector_scalar_result_with_guard(
    scalar: f32,
    vector: [u32; 4],
    side: ScalarSide,
    op: impl Fn(f32, f32) -> f32,
) -> Option<ConstantValue> {
    let vector_w = f32::from_bits(vector[3]);
    let result_w = match side {
        ScalarSide::Left => op(scalar, vector_w),
        ScalarSide::Right => op(vector_w, scalar),
    };
    if result_w != 0.0 && vector_w == 0.0 {
        return None;
    }

    Some(ConstantValue::Vector {
        bits: std::array::from_fn(|index| {
            let value = f32::from_bits(vector[index]);
            match side {
                ScalarSide::Left => op(scalar, value),
                ScalarSide::Right => op(value, scalar),
            }
            .to_bits()
        }),
    })
}

fn numeric_binary(
    left: &ConstantValue,
    right: &ConstantValue,
    op: impl FnOnce(f64, f64) -> f64,
) -> Option<ConstantValue> {
    match (left, right) {
        (ConstantValue::Number(left), ConstantValue::Number(right)) => {
            Some(ConstantValue::Number(op(*left, *right)))
        }
        _ => None,
    }
}

fn numeric_compare(
    left: &ConstantValue,
    right: &ConstantValue,
    op: impl FnOnce(f64, f64) -> bool,
) -> Option<ConstantValue> {
    match (left, right) {
        (ConstantValue::Number(left), ConstantValue::Number(right)) => {
            Some(ConstantValue::Bool(op(*left, *right)))
        }
        _ => None,
    }
}

fn vector_component_constant(value: &ConstantValue, member: &str) -> Option<ConstantValue> {
    let ConstantValue::Vector { bits } = value else {
        return None;
    };
    let index = match member {
        "x" | "X" => 0,
        "y" | "Y" => 1,
        "z" | "Z" => 2,
        _ => return None,
    };
    Some(ConstantValue::Number(f64::from(f32::from_bits(
        bits[index],
    ))))
}

fn unwrapped_expr(expr: &Expr) -> &Expr {
    match expr {
        Expr::TypeAssertion { expr, .. }
        | Expr::Group { expr, .. }
        | Expr::Instantiate { expr, .. } => unwrapped_expr(expr),
        _ => expr,
    }
}

fn known_member_value_to_constant(value: &KnownMemberValue) -> ConstantValue {
    match value {
        KnownMemberValue::Nil => ConstantValue::Nil,
        KnownMemberValue::Boolean { value } => ConstantValue::Bool(*value),
        KnownMemberValue::Number { value } => ConstantValue::Number(*value),
        KnownMemberValue::Integer { value } => ConstantValue::Integer(*value),
        KnownMemberValue::Vector { value } => ConstantValue::Vector {
            bits: value.map(f32::to_bits),
        },
        KnownMemberValue::String { value } => ConstantValue::String(value.clone()),
    }
}

fn constant_truthiness(value: &ConstantValue) -> bool {
    !matches!(value, ConstantValue::Nil | ConstantValue::Bool(false))
}

fn always_terminates(analysis: &ModuleAnalysis, stat: &Stat) -> bool {
    match stat {
        Stat::Block { body, .. } => body.iter().any(|stat| always_terminates(analysis, stat)),
        Stat::Return { .. } | Stat::Break { .. } | Stat::Continue { .. } => true,
        Stat::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            if constant_truthiness_expr(analysis, condition) == Some(true) {
                return always_terminates(analysis, then_body);
            }
            if constant_truthiness_expr(analysis, condition) == Some(false)
                && let Some(else_body) = else_body
            {
                return always_terminates(analysis, else_body);
            }
            else_body.as_deref().is_some_and(|else_body| {
                always_terminates(analysis, then_body) && always_terminates(analysis, else_body)
            })
        }
        Stat::Expr { .. }
        | Stat::Local { .. }
        | Stat::Assign { .. }
        | Stat::CompoundAssign { .. }
        | Stat::While { .. }
        | Stat::Repeat { .. }
        | Stat::For { .. }
        | Stat::ForIn { .. }
        | Stat::Function { .. }
        | Stat::LocalFunction { .. }
        | Stat::DeclareGlobal { .. }
        | Stat::DeclareFunction { .. }
        | Stat::DeclareClass { .. }
        | Stat::TypeAlias { .. }
        | Stat::TypeFunction { .. }
        | Stat::Class { .. }
        | Stat::ClassProperty { .. }
        | Stat::Error { .. } => false,
    }
}

fn constant_truthiness_expr(analysis: &ModuleAnalysis, expr: &Expr) -> Option<bool> {
    analysis
        .constant_expr(expr.syntax_id())
        .map(constant_truthiness)
}

fn analyze_table_shapes(root: &Stat, analysis: &mut ModuleAnalysis) {
    let mut analyzer = TableShapeAnalyzer {
        table_shapes: BTreeMap::new(),
        tables: BTreeMap::new(),
        fields: BTreeSet::new(),
        loops: BTreeMap::new(),
    };
    analyzer.analyze_stat(root);
    analysis.table_shapes = analyzer.table_shapes;
}

struct TableShapeAnalyzer {
    table_shapes: BTreeMap<ExprId, TableSizePrediction>,
    tables: BTreeMap<LocalId, ExprId>,
    fields: BTreeSet<(ExprId, String)>,
    loops: BTreeMap<LocalId, u32>,
}

impl TableShapeAnalyzer {
    const MAX_LOOP_BOUND: u32 = 16;

    fn analyze_stat(&mut self, stat: &Stat) {
        match stat {
            Stat::Block { body, .. } => {
                for stat in body {
                    self.analyze_stat(stat);
                }
            }
            Stat::Return { list, .. } => {
                for expr in list {
                    self.analyze_expr(expr);
                }
            }
            Stat::Expr { expr, .. } => {
                self.analyze_expr(expr);
            }
            Stat::Local { vars, values, .. } => {
                if let ([local], [value]) = (vars.as_slice(), values.as_slice())
                    && let Some(table_id) = empty_table_hint(value)
                {
                    self.tables.insert(local.id, table_id);
                }
                for value in values {
                    self.analyze_expr(value);
                }
            }
            Stat::Assign { vars, values, .. } => {
                for var in vars {
                    self.assign(var);
                }
                for value in values {
                    self.analyze_expr(value);
                }
            }
            Stat::CompoundAssign { var, value, .. } => {
                self.analyze_expr(var);
                self.analyze_expr(value);
            }
            Stat::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                self.analyze_expr(condition);
                self.analyze_stat(then_body);
                if let Some(else_body) = else_body.as_deref() {
                    self.analyze_stat(else_body);
                }
            }
            Stat::While {
                condition, body, ..
            } => {
                self.analyze_expr(condition);
                self.analyze_stat(body);
            }
            Stat::Repeat {
                condition, body, ..
            } => {
                self.analyze_stat(body);
                self.analyze_expr(condition);
            }
            Stat::For {
                var,
                from,
                to,
                step,
                body,
                ..
            } => {
                if step.is_none()
                    && let (Some(from), Some(to)) =
                        (constant_number_literal(from), constant_number_literal(to))
                    && from == 1.0
                    && (1.0..=f64::from(Self::MAX_LOOP_BOUND)).contains(&to)
                {
                    self.loops.insert(var.id, to as u32);
                }
                self.analyze_expr(from);
                self.analyze_expr(to);
                if let Some(step) = step {
                    self.analyze_expr(step);
                }
                self.analyze_stat(body);
            }
            Stat::ForIn { values, body, .. } => {
                for value in values {
                    self.analyze_expr(value);
                }
                self.analyze_stat(body);
            }
            Stat::Function { name, func, .. } => {
                self.assign(name);
                self.analyze_expr(func);
            }
            Stat::LocalFunction { func, .. } | Stat::TypeFunction { func, .. } => {
                self.analyze_expr(func);
            }
            Stat::Class { members, .. } => {
                for member in members {
                    self.analyze_stat(member);
                }
            }
            Stat::Error {
                expressions,
                statements,
                ..
            } => {
                for expr in expressions {
                    self.analyze_expr(expr);
                }
                for stat in statements {
                    self.analyze_stat(stat);
                }
            }
            Stat::DeclareGlobal { .. }
            | Stat::DeclareFunction { .. }
            | Stat::DeclareClass { .. }
            | Stat::TypeAlias { .. }
            | Stat::ClassProperty { .. }
            | Stat::Break { .. }
            | Stat::Continue { .. } => {}
        }
    }

    fn analyze_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Call { func, args, .. } => {
                self.analyze_expr(func);
                for arg in args {
                    self.analyze_expr(arg);
                }
            }
            Expr::Binary { left, right, .. } => {
                self.analyze_expr(left);
                self.analyze_expr(right);
            }
            Expr::Unary { expr, .. }
            | Expr::TypeAssertion { expr, .. }
            | Expr::IndexName { expr, .. }
            | Expr::Group { expr, .. }
            | Expr::Instantiate { expr, .. } => {
                self.analyze_expr(expr);
            }
            Expr::IfElse {
                condition,
                true_expr,
                false_expr,
                ..
            } => {
                self.analyze_expr(condition);
                self.analyze_expr(true_expr);
                self.analyze_expr(false_expr);
            }
            Expr::IndexExpr { expr, index, .. } => {
                self.analyze_expr(expr);
                self.analyze_expr(index);
            }
            Expr::Table { items, .. } => {
                for item in items {
                    if let Some(key) = &item.key {
                        self.analyze_expr(key);
                    }
                    self.analyze_expr(&item.value);
                }
            }
            Expr::InterpString { expressions, .. } | Expr::Error { expressions, .. } => {
                for expr in expressions {
                    self.analyze_expr(expr);
                }
            }
            Expr::Function { body, .. } => {
                self.analyze_stat(body);
            }
            Expr::Nil { .. }
            | Expr::Bool { .. }
            | Expr::Number { .. }
            | Expr::Integer { .. }
            | Expr::String { .. }
            | Expr::Global { .. }
            | Expr::Local { .. }
            | Expr::Varargs { .. } => {}
        }
    }

    fn assign(&mut self, expr: &Expr) {
        match expr {
            Expr::IndexName { expr, index, .. } => {
                self.assign_named_field(expr, index.as_str());
            }
            Expr::IndexExpr { expr, index, .. } => {
                self.assign_index_field(expr, index);
            }
            _ => {}
        }
    }

    fn assign_named_field(&mut self, expr: &Expr, index: &str) {
        let Expr::Local { local, .. } = expr else {
            return;
        };
        let Some(table_id) = self.tables.get(&local.id).copied() else {
            return;
        };
        if self.fields.insert((table_id, index.to_owned())) {
            let shape = self.shape_mut(table_id);
            shape.hash_size = shape.hash_size.saturating_add(1);
        }
    }

    fn assign_index_field(&mut self, expr: &Expr, index: &Expr) {
        let Expr::Local { local, .. } = expr else {
            return;
        };
        let Some(table_id) = self.tables.get(&local.id).copied() else {
            return;
        };

        if let Some(index) = constant_number_literal(index) {
            let shape = self.shape_mut(table_id);
            if index == f64::from(shape.array_size + 1) {
                shape.array_size += 1;
            }
        } else if let Expr::Local { local, .. } = index
            && let Some(bound) = self.loops.get(&local.id).copied()
        {
            let shape = self.shape_mut(table_id);
            if shape.array_size == 0 {
                shape.array_size = bound;
            }
        }
    }

    fn shape_mut(&mut self, table_id: ExprId) -> &mut TableSizePrediction {
        self.table_shapes.entry(table_id).or_default()
    }
}

fn empty_table_hint(expr: &Expr) -> Option<ExprId> {
    match expr {
        Expr::Table {
            syntax_id, items, ..
        } if items.is_empty() => Some(*syntax_id),
        Expr::Call {
            func,
            args,
            is_self: false,
            ..
        } if args.len() == 2 => {
            let Expr::Global { name, .. } = func.as_ref() else {
                return None;
            };
            if name.as_str() != "setmetatable" {
                return None;
            }
            let Expr::Table {
                syntax_id, items, ..
            } = &args[0]
            else {
                return None;
            };
            items.is_empty().then_some(*syntax_id)
        }
        _ => None,
    }
}

fn constant_number_literal(expr: &Expr) -> Option<f64> {
    match expr {
        Expr::Number { value, .. } => value.as_f64(),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
