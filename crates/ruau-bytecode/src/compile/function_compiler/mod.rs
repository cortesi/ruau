//! Function-level AST lowering and bytecode emission.

// Region modules keep `FunctionCompiler` as the only mutable state owner while
// reducing the review surface for each lowering family.
#![allow(clippy::multiple_inherent_impl)]

use std::{
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

use ruau_ast::{
    Location,
    syntax::{
        BinaryOp, CompoundAssignOp, Expr, Local, LocalId, LocalRef, Number, Stat, TableItemKind,
        Type, UnaryOp,
    },
};

use super::{
    CONSTANT_STRING_FOLD_LIMIT, CompileError,
    analysis::{
        ConstantValue, ExprId, FunctionId, FunctionProtoInfo, LocalValueFacts, TableSizePrediction,
        builtin_args_are_eligible, builtin_function_id,
    },
    builtin_folding::fold_builtin_constant,
    context::CompileContext,
    helpers::*,
    options::KnownMember,
};
use crate::{
    BytecodeChunk, ClassShape, FeedbackSlot, Instruction, TableEntry,
    builder::{ChunkBuilder, ProtoMetadata},
    opcodes::{CaptureType, FeedbackType, Opcode, ProtoFlag, TypeTag},
};

mod calls;
mod control_flow;
mod expressions;
mod registers;

use registers::{ActiveLocal, ActiveTypedLocal, LocalScope, RegisterFrame};

/// Direction of one lvalue transfer: read the lvalue into a register, or
/// store a register into the lvalue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LvalueAccess {
    Get,
    Set,
}

/// Lowering owner for one chunk: compiles every registered nested
/// function and the main body, carrying the register, constant, and
/// upvalue state for the function currently being lowered.
pub(super) struct FunctionCompiler {
    pub(super) context: CompileContext,
    builder: ChunkBuilder,
    local_registers: BTreeMap<u32, u8>,
    local_values: LocalValueFacts,
    elided_local_initializers: BTreeSet<u32>,
    active_locals: Vec<ActiveLocal>,
    active_typed_locals: Vec<ActiveTypedLocal>,
    loop_stack: Vec<LoopContext>,
    upvalues: Vec<FunctionUpvalue>,
    compiled_upvalues: BTreeMap<FunctionId, Vec<FunctionUpvalue>>,
    function_stack: Vec<FunctionId>,
    current_function_id: Option<FunctionId>,
    inline_stack: Vec<FunctionId>,
    inline_function_args: BTreeMap<u32, FunctionId>,
    current_function_depth: usize,
    next_register: u8,
    /// Half-open register ranges reserved by an enclosing numeric/generic `for` for its
    /// anonymous control registers (limit/step/index, iterator state) — registers that must
    /// survive the loop body but are not tracked as locals. A frame builder that would lay
    /// a call/table frame across one of these must relocate above the watermark. Per
    /// function (a nested closure has its own register space), so saved/restored with the
    /// rest of the function state.
    reserved_ranges: Vec<(u8, u8)>,
}

impl FunctionCompiler {
    pub(super) fn new(context: CompileContext, implicit_return_line_delta: u8) -> Self {
        let bytecode_version = context.bytecode_version();
        let mut builder = ChunkBuilder::new();
        builder.set_bytecode_version(bytecode_version);
        builder.set_implicit_return_line_delta(implicit_return_line_delta);

        Self {
            context,
            builder,
            local_registers: BTreeMap::new(),
            local_values: LocalValueFacts::default(),
            elided_local_initializers: BTreeSet::new(),
            active_locals: Vec::new(),
            active_typed_locals: Vec::new(),
            loop_stack: Vec::new(),
            upvalues: Vec::new(),
            compiled_upvalues: BTreeMap::new(),
            function_stack: Vec::new(),
            current_function_id: None,
            inline_stack: Vec::new(),
            inline_function_args: BTreeMap::new(),
            current_function_depth: 0,
            next_register: 0,
            reserved_ranges: Vec::new(),
        }
    }

    pub(super) fn finish(self) -> BytecodeChunk {
        self.builder.finish(
            self.context.options().debug_level,
            self.context.optimization_level() > 0,
        )
    }

    pub(super) fn compile_registered_functions(&mut self) -> Result<(), CompileError> {
        let ids = self.context.functions.ordered_ids().to_vec();
        for id in ids {
            self.context.check_cancelled()?;
            if self
                .context
                .functions
                .get(id)
                .and_then(|info| info.proto())
                .is_some()
            {
                continue;
            }
            let expr = self.context.functions.expr(id).cloned().ok_or_else(|| {
                CompileError::new(format!(
                    "function registry did not contain expression for {:?}",
                    id.syntax_id()
                ))
            })?;
            self.ensure_function_proto("", &expr)?;
        }
        Ok(())
    }

    pub(super) fn compile_stat(&mut self, stat: &Stat) -> Result<(), CompileError> {
        self.compile_stat_tail(stat, true)
    }

    pub(super) fn compile_root(&mut self, root: &Stat) -> Result<(), CompileError> {
        self.context.check_cancelled()?;
        let Stat::Block { body, location, .. } = root else {
            return self.compile_stat(root);
        };

        let scope = self.start_local_scope();
        self.compile_block_statements(body, true)?;
        if !self.current_code_returns() {
            if let Some(location) = location {
                self.builder.set_debug_line(location.end.line + 1);
                self.builder
                    .set_implicit_return_line_base(location.end.line + 1);
            }
            self.emit_return(0, 1);
        }
        self.pop_local_scope(scope);
        Ok(())
    }

    fn compile_block_statements(
        &mut self,
        body: &[Stat],
        is_tail: bool,
    ) -> Result<(), CompileError> {
        let last_index = body.len().saturating_sub(1);
        for (index, stat) in body.iter().enumerate() {
            self.context.check_cancelled()?;
            if self.context.optimization_level() > 0
                && is_tail
                && index + 2 == body.len()
                && self.elide_tail_repeat_continue_condition_local(stat, body.get(index + 1))?
            {
                continue;
            }
            if self.compile_elided_continue_if_before_break(stat, body.get(index + 1))? {
                continue;
            }
            if self.context.optimization_level() > 0
                && let Some(constants) =
                    constant_table_key_local_elision(stat, body.get(index + 1))?
            {
                self.local_values.extend_constants(constants);
                continue;
            }
            if self.context.optimization_level() > 0
                && let Some(local_id) = elided_constant_local_initializer(stat, body.get(index + 1))
            {
                self.elided_local_initializers.insert(local_id);
            }
            self.compile_stat_tail(stat, is_tail && index == last_index)?;
            if self.context.always_terminates(stat) {
                break;
            }
        }
        Ok(())
    }

    fn compile_repeat_block_statements(
        &mut self,
        body: &[Stat],
    ) -> Result<Option<usize>, CompileError> {
        let mut condition_local_start = None;

        for (index, stat) in body.iter().enumerate() {
            self.context.check_cancelled()?;
            if self.compile_elided_continue_if_before_break(stat, body.get(index + 1))? {
                self.note_repeat_body_statement(&mut condition_local_start)?;
                continue;
            }
            if self.context.optimization_level() > 0
                && let Some(constants) =
                    constant_table_key_local_elision(stat, body.get(index + 1))?
            {
                self.local_values.extend_constants(constants);
                self.note_repeat_body_statement(&mut condition_local_start)?;
                continue;
            }
            if self.context.optimization_level() > 0
                && let Some(local_id) = elided_constant_local_initializer(stat, body.get(index + 1))
            {
                self.elided_local_initializers.insert(local_id);
            }
            self.compile_stat_tail(stat, false)?;
            self.note_repeat_body_statement(&mut condition_local_start)?;
        }

        Ok(condition_local_start)
    }

    fn note_repeat_body_statement(
        &mut self,
        condition_local_start: &mut Option<usize>,
    ) -> Result<(), CompileError> {
        let active_local_len = self.active_locals.len();
        let Some(context) = self.loop_stack.last_mut() else {
            return Err(CompileError::new("repeat body outside loop context"));
        };

        context.local_offset_continue = active_local_len;
        if context.continue_used && condition_local_start.is_none() {
            *condition_local_start = Some(active_local_len);
        }

        Ok(())
    }

    fn finish_block_scope(&mut self, scope: LocalScope) {
        if !self.current_code_returns() {
            self.close_locals_from(scope.active_local_start);
            self.clear_dead_locals_from(scope.active_local_start);
        }
        self.pop_local_scope(scope);
    }

    fn pop_local_scope(&mut self, scope: LocalScope) {
        self.close_typed_locals_from(scope.active_typed_local_start);
        self.pop_locals_from(scope.active_local_start);
        self.next_register = scope.next_register;
    }

    fn set_expr_debug_line(&mut self, expr: &Expr) {
        if let Some(line) = expr_line(expr) {
            self.builder.set_debug_line(line);
        }
    }

    fn set_expr_end_debug_line(&mut self, expr: &Expr) {
        if let Some(line) = expr_end_line(expr) {
            self.builder.set_debug_line(line);
        }
    }

    fn set_namecall_debug_line(&mut self, func: &Expr) {
        if let Expr::IndexName {
            index_location: Some(location),
            ..
        } = func
        {
            self.builder.set_debug_line(location.begin.line + 1);
        } else {
            self.set_expr_end_debug_line(func);
        }
    }

    fn compile_stat_tail(&mut self, stat: &Stat, is_tail: bool) -> Result<(), CompileError> {
        self.context.check_cancelled()?;
        if let Some(line) = stat_line(stat) {
            self.builder.set_debug_line(line);
        }

        match stat {
            Stat::Block { body, .. } => {
                let scope = self.start_local_scope();
                self.compile_block_statements(body, is_tail)?;
                self.finish_block_scope(scope);
                Ok(())
            }
            Stat::Return { list, .. } => {
                self.emit_coverage();
                self.compile_return(list)
            }
            Stat::Local {
                location,
                vars,
                values,
            } => {
                self.emit_coverage();
                if self.try_elide_redundant_locals(vars, values)? {
                    if let Some(location) = location {
                        self.builder.set_debug_line(location.end.line + 1);
                        self.builder
                            .set_implicit_return_line_base(location.end.line + 1);
                    }
                    return Ok(());
                }
                if self.try_elide_local_aliases(vars, values)? {
                    if let Some(location) = location {
                        self.builder.set_debug_line(location.end.line + 1);
                        self.builder
                            .set_implicit_return_line_base(location.end.line + 1);
                    }
                    return Ok(());
                }
                let first_register = self.next_register;
                let local_end = register_span_end(first_register, vars.len(), "local variable")?;
                let active_local_start = self.active_locals.len();
                for (index, var) in vars.iter().enumerate() {
                    let register = register_at(first_register, index, "local variable index")?;
                    let type_tag = values
                        .get(index)
                        .and_then(|value| self.local_declaration_type_tag(var, Some(value)));
                    self.declare_local_with_debug_start_and_type(var, register, None, type_tag);
                }
                if is_tail
                    && vars.len() == 1
                    && let [
                        Expr::IfElse {
                            condition,
                            true_expr,
                            false_expr,
                            ..
                        },
                    ] = values.as_slice()
                    && self
                        .if_else_logical_rewrite(condition, true_expr, false_expr)
                        .is_none()
                {
                    self.set_local_value_facts(vars[0].id.index(), None, None);
                    self.compile_if_else_void_tail(
                        condition,
                        true_expr,
                        false_expr,
                        first_register,
                    )?;
                    self.start_debug_locals_from(active_local_start);
                    let first_register_end = register_add(first_register, 1)?;
                    self.builder.set_max_stack_size(first_register_end);
                    self.next_register = self.next_register.max(first_register_end);
                    if let Some(location) = location {
                        self.builder.set_debug_line(location.end.line + 1);
                        self.builder
                            .set_implicit_return_line_base(location.end.line + 1);
                    }
                    return Ok(());
                }
                let has_multret_tail = !values.is_empty()
                    && values.len() <= vars.len()
                    && values.last().is_some_and(call_uses_multret);
                if has_multret_tail {
                    let tail_index = values.len() - 1;
                    for (index, value) in values.iter().take(tail_index).enumerate() {
                        if !self
                            .elided_local_initializers
                            .contains(&vars[index].id.index())
                        {
                            self.compile_local_initializer_to(
                                value,
                                register_at(first_register, index, "local initializer index")?,
                                local_end,
                            )?;
                        }
                    }
                    let tail_register =
                        register_at(first_register, tail_index, "local multret tail index")?;
                    let tail_count =
                        bytecode_fixed_count("local multret target", vars.len() - tail_index)?;
                    self.compile_expr_temp_n(&values[tail_index], tail_register, tail_count)?;
                } else {
                    for (index, value) in values.iter().enumerate().take(vars.len()) {
                        if !self
                            .elided_local_initializers
                            .contains(&vars[index].id.index())
                        {
                            self.compile_local_initializer_to(
                                value,
                                register_at(first_register, index, "local initializer index")?,
                                local_end,
                            )?;
                        }
                    }
                    for index in values.len()..vars.len() {
                        self.builder.emit(Instruction::abc(
                            Opcode::LoadNil,
                            register_at(first_register, index, "local nil index")?,
                            0,
                            0,
                        ));
                    }
                    self.next_register = self.next_register.max(local_end);
                    for value in values.iter().skip(vars.len()) {
                        let frame = self.start_register_frame();
                        self.compile_expr_side(value)?;
                        self.restore_register_frame(frame);
                    }
                }
                self.start_debug_locals_from(active_local_start);
                for (index, var) in vars.iter().enumerate() {
                    let filled_by_multret_tail = has_multret_tail && index >= values.len() - 1;
                    let constant = if filled_by_multret_tail {
                        None
                    } else {
                        values
                            .get(index)
                            .map(|value| self.constant_value_expr(value))
                            .transpose()?
                            .flatten()
                            .or_else(|| (index >= values.len()).then_some(ConstantValue::Nil))
                    };
                    let import_path = if filled_by_multret_tail {
                        None
                    } else {
                        values
                            .get(index)
                            .and_then(|value| self.local_import_path_initializer(value))
                    };
                    self.set_local_value_facts(var.id.index(), constant, import_path);
                }
                self.builder.set_max_stack_size(local_end);
                self.next_register = self.next_register.max(local_end);
                if let Some(location) = location {
                    self.builder.set_debug_line(location.end.line + 1);
                    self.builder
                        .set_implicit_return_line_base(location.end.line + 1);
                }
                Ok(())
            }
            Stat::LocalFunction {
                location,
                name,
                func,
                ..
            } => {
                self.emit_coverage();
                self.compile_local_function(name, func)?;
                self.update_after_statement_location(*location);
                Ok(())
            }
            Stat::Function {
                location,
                name,
                func,
                ..
            } => {
                self.emit_coverage();
                self.compile_function_statement(name, func)?;
                self.update_after_statement_location(*location);
                Ok(())
            }
            Stat::Assign {
                location,
                vars,
                values,
            } => {
                self.emit_coverage();
                if !self.compile_multi_assignment(vars, values)? {
                    for (var, value) in vars.iter().zip(values.iter()) {
                        self.compile_assignment(var, value)?;
                    }
                }
                if let Some(location) = location {
                    self.builder.set_debug_line(location.end.line + 1);
                    self.builder
                        .set_implicit_return_line_base(location.end.line + 1);
                }
                Ok(())
            }
            Stat::CompoundAssign {
                location,
                op,
                var,
                value,
            } => {
                self.emit_coverage();
                self.compile_compound_assignment(*op, var, value)?;
                if let Some(location) = location {
                    self.builder.set_debug_line(location.end.line + 1);
                    self.builder
                        .set_implicit_return_line_base(location.end.line + 1);
                }
                Ok(())
            }
            Stat::Expr { expr, .. } => {
                self.emit_coverage();
                self.compile_expr_statement(expr)
            }
            Stat::If {
                location,
                condition,
                then_body,
                else_body,
                ..
            } => {
                self.emit_coverage();
                let folded_condition = self
                    .optimized_condition_truthiness_expr(condition)?
                    .is_some();
                self.compile_if_statement(
                    condition,
                    then_body,
                    else_body.as_deref(),
                    *location,
                    is_tail,
                )?;
                if !folded_condition
                    && loop_control_branch_kind(then_body).is_none()
                    && let Some(line) = else_body
                        .as_deref()
                        .or(Some(then_body.as_ref()))
                        .and_then(stat_last_line)
                {
                    self.builder.set_debug_line(line + 1);
                    self.builder.set_implicit_return_line_base(line + 1);
                }
                Ok(())
            }
            Stat::Break { .. } => {
                self.emit_coverage();
                self.compile_break_statement()
            }
            Stat::Continue { .. } => {
                self.emit_coverage();
                self.compile_continue_statement()
            }
            Stat::While {
                location,
                condition,
                body,
                ..
            } => {
                self.emit_coverage();
                self.compile_while_statement(condition, body, *location, is_tail)
            }
            Stat::Repeat {
                location,
                condition,
                body,
            } => {
                self.emit_coverage();
                self.compile_repeat_statement(condition, body, *location, is_tail)
            }
            Stat::For {
                location,
                var,
                from,
                to,
                step,
                body,
                ..
            } => {
                self.emit_coverage();
                self.compile_for_statement(var, from, to, step.as_deref(), body, *location)
            }
            Stat::ForIn {
                location,
                vars,
                values,
                body,
                ..
            } => {
                self.emit_coverage();
                self.compile_for_in_statement(vars, values, body, *location)
            }
            Stat::Class {
                location,
                class_local,
                members,
                ..
            } => {
                self.emit_coverage();
                self.compile_class_statement(class_local.as_ref(), members)?;
                self.update_after_statement_location(*location);
                Ok(())
            }
            Stat::TypeAlias { .. } => Ok(()),
            Stat::TypeFunction { .. } => {
                self.emit_coverage();
                Ok(())
            }
            _ => Err(CompileError::new(format!(
                "minimal bytecode compiler does not support {stat:?}"
            ))),
        }
    }

    fn emit_coverage(&mut self) {
        for _ in 0..self.context.coverage_level() {
            self.emit_one_coverage();
        }
    }

    fn emit_one_coverage(&mut self) {
        if self.context.coverage_level() > 0 {
            self.builder
                .emit(Instruction::abc(Opcode::Coverage, 0, 0, 0));
        }
    }

    fn current_code_returns(&self) -> bool {
        self.builder
            .current_code()
            .last()
            .is_some_and(|instruction| instruction.opcode == Opcode::Return)
    }

    fn trailing_close_upvals(&self) -> Option<Instruction> {
        self.builder
            .current_code()
            .last()
            .filter(|instruction| instruction.opcode == Opcode::CloseUpvals)
            .cloned()
    }

    fn close_upvals_before_trailing_return(&self) -> Option<Instruction> {
        let code = self.builder.current_code();
        let [.., close, ret] = code else {
            return None;
        };
        (ret.opcode == Opcode::Return && close.opcode == Opcode::CloseUpvals).then_some(*close)
    }

    fn emit_return(&mut self, register: u8, count: u8) {
        self.close_locals_from(0);
        self.builder
            .emit(Instruction::abc(Opcode::Return, register, count, 0));
    }

    fn compile_return(&mut self, values: &[Expr]) -> Result<(), CompileError> {
        if let [
            Expr::IfElse {
                condition,
                true_expr,
                false_expr,
                ..
            },
        ] = values
            && self
                .if_else_logical_rewrite(condition, true_expr, false_expr)
                .is_none()
        {
            let register = self.next_register;
            self.compile_if_else_return(condition, true_expr, false_expr, register)?;
            return Ok(());
        }

        if let [
            value @ Expr::Call {
                func,
                args,
                is_self,
                ..
            },
        ] = values
        {
            let register = self.next_register;
            if self.context.optimization_level() > 1
                && let Some(constant) = self.analysis_constant_value_expr(value)
            {
                let constant = constant.clone();
                self.builder.set_max_stack_size(register_add(register, 1)?);
                self.compile_constant_value(constant, register)?;
                self.emit_return(register, 2);
                return Ok(());
            }
            if self.context.optimization_level() > 1
                && !*is_self
                && self.inline_function_has_value_return(func)
                && self.try_compile_inlined_call(
                    func,
                    args,
                    register,
                    CallResults::Fixed(1),
                    InlineCallMode::Return,
                )?
            {
                return Ok(());
            }
            let results = self.return_call_results(func, *is_self);
            self.compile_call_to(value, register, results)?;
            self.emit_return(register, results.return_operand());
            return Ok(());
        }

        if let [value @ Expr::Function { .. }] = values {
            let register = self.next_register;
            let function = self.compile_function_proto("", value)?;
            self.emit_function_closure(register, &function)?;
            self.emit_return(register, 2);
            return Ok(());
        }

        if let [value @ Expr::InterpString { .. }] = values {
            let register = self.next_register;
            self.compile_interp_string_return(value, register)?;
            self.emit_return(register, 2);
            return Ok(());
        }

        if let [value] = values
            && let Some(register) = self.short_circuit_return_register(value)?
        {
            self.builder.set_max_stack_size(register_add(register, 1)?);
            self.emit_return(register, 2);
            return Ok(());
        }

        if let Some(start) = contiguous_local_return_start(&self.local_registers, values) {
            self.builder.set_max_stack_size(register_span_end(
                start,
                values.len(),
                "return value",
            )?);
            self.emit_return(start, bytecode_count_operand("return value", values.len())?);
            return Ok(());
        }

        if let Some((last, prefix)) = values.split_last()
            && self.return_tail_uses_multret(last)?
        {
            let register = self.next_register;
            for (index, value) in prefix.iter().enumerate() {
                self.compile_expr_to(value, register_at(register, index, "return prefix index")?)?;
            }
            let tail_register = register_at(register, prefix.len(), "return multret tail index")?;
            self.compile_multret_arg_to(last, tail_register)?;
            self.builder
                .set_max_stack_size(register_add(tail_register, 1)?);
            self.emit_return(register, CallResults::Multret.return_operand());
            return Ok(());
        }

        for (index, value) in values.iter().enumerate() {
            self.compile_expr_to(
                value,
                register_at(self.next_register, index, "return value index")?,
            )?;
        }
        self.builder.set_max_stack_size(register_span_end(
            self.next_register,
            values.len(),
            "return value",
        )?);
        self.emit_return(
            self.next_register,
            bytecode_count_operand("return value", values.len())?,
        );
        Ok(())
    }

    fn return_tail_uses_multret(&self, value: &Expr) -> Result<bool, CompileError> {
        if !call_uses_multret(value) || self.analysis_constant_value_expr(value).is_some() {
            return Ok(false);
        }
        if self.context.optimization_level() < 2 {
            return Ok(true);
        }

        let mut visited = BTreeSet::new();
        Ok(!self.inline_multret_value_can_be_fixed(value, &mut visited)?)
    }

    fn short_circuit_return_register(&self, expr: &Expr) -> Result<Option<u8>, CompileError> {
        if self.context.optimization_level() == 0 {
            return Ok(None);
        }
        Ok(match expr {
            Expr::Binary {
                op: BinaryOp::And,
                left,
                ..
            } if self
                .short_circuit_constant_value(left)?
                .is_some_and(|value| !constant_truthiness(&value)) =>
            {
                self.local_expr_register(left)?
            }
            Expr::Binary {
                op: BinaryOp::Or,
                left,
                ..
            } if self
                .short_circuit_constant_value(left)?
                .is_some_and(|value| constant_truthiness(&value)) =>
            {
                self.local_expr_register(left)?
            }
            Expr::Group { expr, .. }
            | Expr::TypeAssertion { expr, .. }
            | Expr::Instantiate { expr, .. } => self.short_circuit_return_register(expr)?,
            _ => None,
        })
    }

    fn compile_local_function(
        &mut self,
        name: &ruau_ast::syntax::Local,
        func: &Expr,
    ) -> Result<(), CompileError> {
        let register = self.reserve_register()?;
        let active_local_start = self.active_locals.len();
        self.declare_local_pending_debug(name, register);

        let function = self.compile_function_proto(name.name.as_str(), func)?;
        self.emit_function_closure(register, &function)?;
        self.start_debug_locals_from(active_local_start);
        Ok(())
    }

    fn compile_function_statement(&mut self, name: &Expr, func: &Expr) -> Result<(), CompileError> {
        let frame = self.start_register_frame();
        let register = self.reserve_register()?;

        let function = self.compile_function_proto(function_statement_debug_name(name), func)?;
        self.emit_function_closure(register, &function)?;

        let lvalue = self.compile_lvalue(name)?;
        self.compile_lvalue_use(&lvalue, register, LvalueAccess::Set)?;
        self.restore_register_frame(frame);
        Ok(())
    }

    fn compile_class_statement(
        &mut self,
        class_local: Option<&Local>,
        members: &[Stat],
    ) -> Result<(), CompileError> {
        let Some(class_local) = class_local else {
            return Ok(());
        };

        let register = self.reserve_register()?;
        let active_local_start = self.active_locals.len();
        self.declare_local_pending_debug(class_local, register);
        self.set_local_value_facts(class_local.id.index(), None, None);

        let class_name = self.builder.add_string_constant(class_local.name.as_str());
        let mut property_names = Vec::new();
        let mut method_plans = Vec::new();
        for member in members {
            match member {
                Stat::ClassProperty { name, .. } => {
                    property_names.push(self.builder.add_string_constant(name.as_str()));
                }
                Stat::TypeFunction { name, func, .. } => {
                    let function = self.compile_function_proto(name.as_str(), func)?;
                    let closure = if function.shareable && function.captures.is_empty() {
                        let constant = self.builder.add_closure(function.proto);
                        let child_proto = self.builder.add_child_proto(function.proto);
                        ClassClosurePlan::Shareable {
                            constant,
                            child_proto,
                        }
                    } else {
                        ClassClosurePlan::NewClosure {
                            child_proto: self.builder.add_child_proto(function.proto),
                        }
                    };
                    let method_name = self.builder.add_string_constant(name.as_str());
                    method_plans.push(ClassMethodPlan {
                        name: method_name,
                        closure,
                        captures: function.captures,
                    });
                }
                _ => {}
            }
        }

        let class_shape = self.builder.add_class_shape(ClassShape {
            class_name,
            property_names,
            method_names: method_plans.iter().map(|method| method.name).collect(),
        });
        self.builder.emit(Instruction::abc_with_aux(
            Opcode::LoadKx,
            register,
            0,
            0,
            Some(class_shape),
        ));
        self.start_debug_locals_from(active_local_start);

        let method_register = register_add(register, 1)?;
        self.builder
            .set_max_stack_size(register_add(method_register, 1)?);
        for method in method_plans {
            match method.closure {
                ClassClosurePlan::Shareable {
                    constant,
                    child_proto,
                } => {
                    self.emit_closure_instruction(method_register, constant, child_proto);
                }
                ClassClosurePlan::NewClosure { child_proto } => {
                    self.builder.emit(Instruction::ad(
                        Opcode::NewClosure,
                        method_register,
                        child_proto,
                    ));
                }
            }
            self.emit_captures(&method.captures);
            self.builder.emit(Instruction::abc_with_aux(
                Opcode::NewClassMember,
                register,
                0,
                method_register,
                Some(method.name),
            ));
        }

        self.next_register = self.next_register.max(method_register);
        Ok(())
    }

    fn emit_function_closure(
        &mut self,
        register: u8,
        function: &CompiledFunction,
    ) -> Result<(), CompileError> {
        self.builder.set_max_stack_size(register_add(register, 1)?);
        if !function.shareable {
            let child_proto = self.builder.add_child_proto(function.proto);
            self.builder
                .emit(Instruction::ad(Opcode::NewClosure, register, child_proto));
        } else {
            let closure = self.builder.add_closure(function.proto);
            let child_proto = self.builder.add_child_proto(function.proto);
            self.emit_closure_instruction(register, closure, child_proto);
        }
        self.emit_captures(&function.captures);
        Ok(())
    }

    fn emit_closure_instruction(&mut self, register: u8, constant: u32, child_proto: i16) {
        if let Some(constant) = constant_ad_operand(constant) {
            self.builder
                .emit(Instruction::ad(Opcode::DupClosure, register, constant));
        } else {
            self.builder
                .emit(Instruction::ad(Opcode::NewClosure, register, child_proto));
        }
    }

    fn resolve_function_captures(
        &mut self,
        upvalues: &[FunctionUpvalue],
    ) -> Result<Vec<FunctionCapture>, CompileError> {
        let mut captures = Vec::with_capacity(upvalues.len());
        for upvalue in upvalues {
            if let Some(source) = self.local_registers.get(&upvalue.local_id).copied() {
                let kind = if self
                    .context
                    .variable(LocalId::new(upvalue.local_id))
                    .is_some_and(|variable| variable.is_written())
                {
                    self.mark_local_captured(upvalue.local_id);
                    CaptureType::Ref
                } else {
                    CaptureType::Val
                };
                captures.push(FunctionCapture { kind, source });
                continue;
            }

            if let Some(constant) = self.local_constant(upvalue.local_id) {
                let source = self.reserve_register()?;
                self.compile_constant_value(constant, source)?;
                captures.push(FunctionCapture {
                    kind: CaptureType::Val,
                    source,
                });
                continue;
            }

            if self
                .context
                .variable(LocalId::new(upvalue.local_id))
                .is_some()
            {
                let source = self.ensure_upvalue(upvalue.local_id)?;
                captures.push(FunctionCapture {
                    kind: CaptureType::Upval,
                    source,
                });
                continue;
            };

            return Err(CompileError::new(format!(
                "minimal bytecode compiler could not resolve parent local {} for closure capture",
                upvalue.local_id
            )));
        }
        Ok(captures)
    }

    fn ensure_upvalue(&mut self, local_id: u32) -> Result<u8, CompileError> {
        if let Some(index) = self
            .upvalues
            .iter()
            .position(|upvalue| upvalue.local_id == local_id)
        {
            return bytecode_u8_count("upvalue", index);
        }
        let index = bytecode_u8_count("upvalue", self.upvalues.len())?;
        self.upvalues.push(FunctionUpvalue { local_id });
        Ok(index)
    }

    fn local_source_register(
        &mut self,
        local: &LocalRef,
        fallback_register: u8,
    ) -> Result<u8, CompileError> {
        let local_id = local.id.index();
        if let Some(register) = self.local_registers.get(&local_id).copied() {
            return Ok(register);
        }
        if let Some(constant) = self
            .local_constant(local_id)
            .or_else(|| self.context.local_constant(local.id).cloned())
        {
            self.compile_constant_value(constant, fallback_register)?;
            return Ok(fallback_register);
        }
        if local.function_depth < self.current_function_depth {
            let upvalue = self.ensure_upvalue(local_id)?;
            self.builder.emit(Instruction::abc(
                Opcode::GetUpval,
                fallback_register,
                upvalue,
                0,
            ));
            self.builder
                .set_max_stack_size(register_add(fallback_register, 1)?);
            return Ok(fallback_register);
        }
        Err(CompileError::new(format!(
            "unknown local id {} in local_source_register",
            local_id
        )))
    }

    fn compile_expr_statement(&mut self, expr: &Expr) -> Result<(), CompileError> {
        if !matches!(expr, Expr::Call { .. }) {
            return Err(CompileError::new(format!(
                "minimal bytecode compiler only supports call expression statements: {expr:?}"
            )));
        }
        self.compile_call_to(expr, self.next_register, CallResults::None)
    }

    fn compile_global_import(&mut self, name: &str, register: u8) {
        let string_constant = self.builder.add_string_constant(name);
        let import_id = single_name_import_id(string_constant);
        let import = self.builder.add_import(import_id);
        self.builder.emit(Instruction::abc_with_aux(
            Opcode::GetImport,
            register,
            import as u8,
            (import >> 8) as u8,
            Some(import_id),
        ));
    }

    fn compile_import_path(&mut self, names: &[String], register: u8) -> Result<(), CompileError> {
        self.builder.set_max_stack_size(register_add(register, 1)?);
        let constants = names
            .iter()
            .map(|name| self.builder.add_string_constant(name))
            .collect::<Vec<_>>();
        let import_id = import_path_id(&constants)?;
        let import = self.builder.add_import(import_id);
        self.builder.emit(Instruction::abc_with_aux(
            Opcode::GetImport,
            register,
            import as u8,
            (import >> 8) as u8,
            Some(import_id),
        ));
        Ok(())
    }

    fn compile_global_load(&mut self, name: &str, register: u8) {
        if self.global_load_needs_table_lookup(name) {
            let constant = self.builder.add_string_constant(name);
            self.builder.emit(Instruction::abc_with_aux(
                Opcode::GetGlobal,
                register,
                0,
                string_hash(name),
                Some(constant),
            ));
        } else {
            self.compile_global_import(name, register);
        }
    }

    fn global_load_needs_table_lookup(&self, name: &str) -> bool {
        self.context.optimization_level() == 0
            || self.context.preserve_fenv_semantics()
            || (self.context.assigned_globals().contains(name) && name != "_G")
    }

    fn compile_local_initializer_to(
        &mut self,
        expr: &Expr,
        register: u8,
        temp_register: u8,
    ) -> Result<(), CompileError> {
        if matches!(expr, Expr::Call { .. })
            && register < temp_register
            && self.constant_value_expr(expr)?.is_none()
        {
            self.compile_call_to(expr, temp_register, CallResults::Fixed(1))?;
            self.builder
                .emit(Instruction::abc(Opcode::Move, register, temp_register, 0));
            Ok(())
        } else {
            self.compile_expr_to(expr, register)
        }
    }

    fn import_path(&self, expr: &Expr) -> Option<Vec<String>> {
        if self.context.optimization_level() == 0 || self.context.preserve_fenv_semantics() {
            return None;
        }
        match expr {
            Expr::Global { name, .. }
                if !self.context.assigned_globals().contains(name.as_str()) =>
            {
                Some(vec![name.as_str().to_owned()])
            }
            Expr::Local { local, .. } => self.local_values.import_path(local.id.index()),
            Expr::IndexName { expr, index, .. } => {
                let mut path = self.import_path(expr)?;
                path.push(index.as_str().to_owned());
                Some(path)
            }
            Expr::Group { expr, .. }
            | Expr::TypeAssertion { expr, .. }
            | Expr::Instantiate { expr, .. } => self.import_path(expr),
            _ => None,
        }
    }

    fn direct_import_path(&self, expr: &Expr) -> Option<Vec<String>> {
        if self.context.optimization_level() == 0 || self.context.preserve_fenv_semantics() {
            return None;
        }
        match expr {
            Expr::Global { name, .. }
                if !self.context.assigned_globals().contains(name.as_str()) =>
            {
                Some(vec![name.as_str().to_owned()])
            }
            Expr::IndexName { expr, index, .. } => {
                let mut path = self.direct_import_path(expr)?;
                path.push(index.as_str().to_owned());
                Some(path)
            }
            Expr::Group { expr, .. }
            | Expr::TypeAssertion { expr, .. }
            | Expr::Instantiate { expr, .. } => self.direct_import_path(expr),
            _ => None,
        }
    }

    fn local_import_path_initializer(&self, expr: &Expr) -> Option<Vec<String>> {
        match expr {
            Expr::Binary {
                op: BinaryOp::Or,
                left,
                ..
            } => self.direct_import_path(left),
            Expr::Group { expr, .. }
            | Expr::TypeAssertion { expr, .. }
            | Expr::Instantiate { expr, .. } => self.local_import_path_initializer(expr),
            _ => self.direct_import_path(expr),
        }
    }

    fn return_call_results(&self, func: &Expr, is_self: bool) -> CallResults {
        if is_self || self.context.optimization_level() <= 1 {
            return CallResults::Multret;
        }
        let Some(path) = self.import_path(func) else {
            return CallResults::Multret;
        };
        if fastcall_fixed_return(&path) || self.vector_constructor_returns_fixed(&path) {
            CallResults::Fixed(1)
        } else {
            CallResults::Multret
        }
    }

    fn vector_constructor_returns_fixed(&self, path: &[String]) -> bool {
        let Some(vector_ctor) = self.context.vector_ctor() else {
            return matches!(path, [lib, name] if lib == "vector" && name == "create");
        };

        match path {
            [name] => self.context.vector_lib().is_none() && name == vector_ctor,
            [lib, name] => {
                let vector_lib = self.context.vector_lib().unwrap_or("vector");
                lib == vector_lib && name == vector_ctor
            }
            _ => false,
        }
    }

    fn compile_function_proto(
        &mut self,
        fallback_debug_name: &str,
        func: &Expr,
    ) -> Result<CompiledFunction, CompileError> {
        let function_id = self.ensure_function_proto(fallback_debug_name, func)?;
        self.compiled_function(function_id)
    }

    fn compiled_function(
        &mut self,
        function_id: FunctionId,
    ) -> Result<CompiledFunction, CompileError> {
        let (proto, upvalues) = {
            let info = self.context.functions.get(function_id).ok_or_else(|| {
                CompileError::new(format!(
                    "function registry did not contain compiled function {:?}",
                    function_id.syntax_id()
                ))
            })?;
            let proto = info.proto().ok_or_else(|| {
                CompileError::new(format!(
                    "function {:?} was emitted before its proto was compiled",
                    function_id.syntax_id()
                ))
            })?;
            let upvalues = self
                .compiled_upvalues
                .get(&function_id)
                .cloned()
                .unwrap_or_else(|| {
                    info.upvalues()
                        .iter()
                        .filter(|upvalue| {
                            self.context
                                .local_constant(LocalId::new(upvalue.local_id()))
                                .is_none()
                        })
                        .map(|upvalue| FunctionUpvalue {
                            local_id: upvalue.local_id(),
                        })
                        .collect::<Vec<_>>()
                });
            (proto.proto_id(), upvalues)
        };
        let captures = self.resolve_function_captures(&upvalues)?;
        let shareable = self.should_share_closure(function_id);
        Ok(CompiledFunction {
            proto,
            captures,
            shareable,
        })
    }

    fn ensure_function_proto(
        &mut self,
        fallback_debug_name: &str,
        func: &Expr,
    ) -> Result<FunctionId, CompileError> {
        let Expr::Function {
            args,
            self_arg,
            generics,
            vararg,
            body,
            location,
            debug_name,
            syntax_id,
            function_depth,
            ..
        } = func
        else {
            return Err(CompileError::new(format!(
                "local function did not contain a function expression: {func:?}"
            )));
        };

        let function_id = FunctionId::new(*syntax_id);
        if self
            .context
            .functions
            .get(function_id)
            .and_then(|info| info.proto())
            .is_some()
        {
            return Ok(function_id);
        }

        let line_defined = location
            .map(|location| location.begin.line + 1)
            .unwrap_or(1);
        let debug_name = if debug_name.is_empty() {
            fallback_debug_name
        } else {
            debug_name.as_str()
        };
        let inherited_import_paths = self
            .context
            .functions
            .get(function_id)
            .map(|info| {
                info.upvalues()
                    .iter()
                    .filter_map(|upvalue| {
                        let local_id = upvalue.local_id();
                        self.local_values
                            .import_path(local_id)
                            .or_else(|| {
                                self.context
                                    .local_import_path(LocalId::new(local_id))
                                    .map(<[String]>::to_vec)
                            })
                            .map(|path| (upvalue.local_id(), path))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let inherited_function_stack = self.function_stack.clone();
        let inherited_inline_stack = self.inline_stack.clone();
        let parent_proto = self.builder.begin_proto();
        let parent_state = self.take_function_state();
        self.function_stack = inherited_function_stack;
        self.inline_stack = inherited_inline_stack;
        self.next_register = 0;
        self.current_function_depth = *function_depth;
        self.builder.set_proto_flags(0);
        self.builder.set_debug_line(line_defined);

        let parameter_count = args.len() + usize::from(self_arg.is_some());
        let parameter_count_u8 = bytecode_u8_count("function parameter", parameter_count)?;
        let compile_result = (|| -> Result<(), CompileError> {
            self.function_stack.push(function_id);
            self.current_function_id = Some(function_id);
            if *vararg {
                self.builder.emit(Instruction::abc(
                    Opcode::PrepVarargs,
                    parameter_count_u8,
                    0,
                    0,
                ));
            }
            if parameter_count > 0 {
                self.reserve_registers(parameter_count_u8)?;
            }

            let mut next_argument_register = 0u8;
            if let Some(self_arg) = self_arg {
                self.declare_local(self_arg, next_argument_register);
                next_argument_register = next_argument_register.saturating_add(1);
            }
            for (index, arg) in args.iter().enumerate() {
                let register =
                    register_at(next_argument_register, index, "function parameter index")?;
                self.declare_local(arg, register);
            }
            for (local_id, path) in &inherited_import_paths {
                self.local_values
                    .set_import_path(*local_id, Some(path.clone()));
            }

            self.compile_function_body(body)?;
            self.function_stack.pop();
            self.close_typed_locals_from(0);
            let needs_return = self
                .builder
                .current_code()
                .last()
                .is_none_or(|instruction| instruction.opcode != Opcode::Return);
            if needs_return {
                self.builder.set_debug_line(
                    location
                        .map(|location| location.end.line + 1)
                        .unwrap_or(line_defined),
                );
                self.emit_return(0, 1);
            }
            Ok(())
        })();
        if self.function_stack.last().copied() == Some(function_id) {
            self.function_stack.pop();
        }
        let upvalues = if self.context.options().debug_level >= 2 {
            self.context
                .functions
                .get(function_id)
                .map(|info| {
                    info.upvalues()
                        .iter()
                        .map(|upvalue| FunctionUpvalue {
                            local_id: upvalue.local_id(),
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|| self.upvalues.clone())
        } else {
            self.upvalues.clone()
        };
        let max_stack_size = self.builder.max_stack_size();
        let mut flags = self.builder.proto_flags();
        let debug_name = if debug_name.is_empty() {
            0
        } else {
            self.builder.add_string(debug_name)
        };
        self.set_function_type_info(
            *location,
            self_arg.as_ref(),
            generics,
            args,
            parameter_count_u8,
        );
        self.push_upvalue_type_info(function_id, &upvalues);
        self.push_debug_upvalues(function_id, &upvalues);
        self.close_debug_locals_from(0);
        let mut function_proto = self.builder.end_proto(parent_proto);
        self.restore_function_state(parent_state);
        compile_result?;
        let upvalue_count = bytecode_u8_count("upvalue", upvalues.len())?;
        let has_upvalues = !upvalues.is_empty();
        self.compiled_upvalues.insert(function_id, upvalues);

        if self.context.bytecode_version() >= 11
            && !*vararg
            && !has_upvalues
            && !self.context.getfenv_used()
            && !self.context.setfenv_used()
        {
            flags |= ProtoFlag::INLINABLE;
        }
        if self.context.optimization_level() > 0 {
            function_proto.fold_jumps();
        }
        let proto = function_proto.into_proto(&ProtoMetadata {
            num_params: parameter_count_u8,
            num_upvalues: upvalue_count,
            is_vararg: *vararg,
            flags,
            line_defined,
            debug_name,
            debug_level: self.context.options().debug_level,
        });
        let proto_id = self.builder.add_proto(proto);
        self.context
            .functions
            .record_compiled_proto(
                function_id,
                FunctionProtoInfo::new(proto_id, max_stack_size, upvalue_count, flags),
            )
            .ok_or_else(|| {
                CompileError::new(format!(
                    "function registry did not contain compiled function {:?}",
                    syntax_id
                ))
            })?;
        Ok(function_id)
    }

    fn compile_function_body(&mut self, body: &Stat) -> Result<(), CompileError> {
        match body {
            Stat::Block { body, .. } => self.compile_block_statements(body, true),
            body => self.compile_stat_tail(body, true),
        }
    }

    fn current_function_captures_function(&self, callee_id: FunctionId) -> bool {
        let Some(current_id) = self.current_function_id else {
            return false;
        };
        let Some(current) = self.context.functions.get(current_id) else {
            return false;
        };
        let Some(callee) = self.context.functions.get(callee_id) else {
            return false;
        };
        if current.function_depth() <= callee.function_depth() {
            return false;
        }
        current.upvalues().iter().any(|upvalue| {
            self.context
                .variable(LocalId::new(upvalue.local_id()))
                .and_then(|variable| variable.initial_expr())
                .is_some_and(|initial_expr| FunctionId::new(initial_expr) == callee_id)
        })
    }

    fn should_share_closure(&self, id: FunctionId) -> bool {
        if self.context.optimization_level() < 1 || self.context.setfenv_used() {
            return false;
        }
        self.should_share_closure_inner(id, &mut BTreeSet::new())
    }

    fn should_share_closure_inner(&self, id: FunctionId, seen: &mut BTreeSet<FunctionId>) -> bool {
        if !seen.insert(id) {
            return true;
        }
        let Some(info) = self.context.functions.get(id) else {
            return false;
        };
        for upvalue in info.upvalues() {
            let Some(variable) = self.context.variable(LocalId::new(upvalue.local_id())) else {
                return false;
            };
            if variable.is_written() {
                return false;
            }
            if upvalue.function_depth() != 0 || upvalue.loop_depth() != 0 {
                let Some(initial_expr) = variable.initial_expr() else {
                    return false;
                };
                let initial_function = FunctionId::new(initial_expr);
                if initial_function != id {
                    if self.function_captures_local(initial_function, upvalue.local_id()) {
                        return false;
                    }
                    if !self.should_share_closure_inner(initial_function, seen) {
                        return false;
                    }
                }
            }
        }
        true
    }

    fn function_captures_local(&self, function_id: FunctionId, local_id: u32) -> bool {
        self.context.functions.get(function_id).is_some_and(|info| {
            info.upvalues()
                .iter()
                .any(|upvalue| upvalue.local_id() == local_id)
        })
    }

    fn local_captured_by_any_function(&self, local_id: u32) -> bool {
        self.context
            .functions
            .ordered_ids()
            .iter()
            .any(|function_id| self.function_captures_local(*function_id, local_id))
    }

    fn set_function_type_info(
        &mut self,
        function_location: Option<Location>,
        self_arg: Option<&ruau_ast::syntax::Local>,
        generics: &[ruau_ast::syntax::GenericType],
        args: &[ruau_ast::syntax::Local],
        parameter_count: u8,
    ) {
        let has_arg_annotations = self_arg
            .and_then(|self_arg| self_arg.annotation.as_deref())
            .is_some()
            || args.iter().any(|arg| arg.annotation.is_some());
        if self.context.type_info_level() == 0
            && (!has_arg_annotations || self.context.optimization_level() < 2)
        {
            return;
        }
        let aliases = self.type_aliases_for_location(function_location);
        let generic_names = generics
            .iter()
            .map(|generic| generic.name.as_str().to_owned())
            .collect::<BTreeSet<_>>();
        let mut bytes = Vec::with_capacity(args.len() + usize::from(self_arg.is_some()) + 2);
        bytes.push(TypeTag::Function as u16 as u8);
        bytes.push(parameter_count);
        let mut has_non_any = false;
        if self_arg.is_some() {
            bytes.push(TypeTag::Table as u16 as u8);
            has_non_any = true;
        }
        for arg in args {
            let tag = arg
                .annotation
                .as_deref()
                .map(|luau_type| self.bytecode_type_tag(luau_type, &aliases, &generic_names))
                .unwrap_or(TypeTag::Any as u16 as u8);
            has_non_any |= tag != TypeTag::Any as u16 as u8;
            bytes.push(tag);
        }
        if has_non_any {
            self.builder.set_function_type_info(bytes);
        }
    }

    fn push_upvalue_type_info(&mut self, function_id: FunctionId, upvalues: &[FunctionUpvalue]) {
        if self.context.type_info_level() == 0 {
            return;
        }
        let types = self
            .context
            .functions
            .get(function_id)
            .map(|info| {
                info.upvalues()
                    .iter()
                    .map(|upvalue| {
                        (
                            upvalue.local_id(),
                            upvalue.luau_type().and_then(type_info_tag),
                        )
                    })
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();
        for upvalue in upvalues {
            self.builder.push_upvalue_type_info(
                types
                    .get(&upvalue.local_id)
                    .and_then(|tag| *tag)
                    .unwrap_or(TypeTag::Any as u16 as u8),
            );
        }
    }

    fn type_aliases_for_location(
        &self,
        location: Option<Location>,
    ) -> BTreeMap<String, TypeAliasInfo> {
        let mut aliases = BTreeMap::new();
        collect_type_aliases_for_location(self.context.root(), location, &mut aliases);
        aliases
    }

    fn bytecode_type_tag(
        &self,
        luau_type: &Type,
        aliases: &BTreeMap<String, TypeAliasInfo>,
        generics: &BTreeSet<String>,
    ) -> u8 {
        self.bytecode_type_tag_inner(
            luau_type,
            aliases,
            generics,
            &BTreeMap::new(),
            &mut BTreeSet::new(),
        )
    }

    fn bytecode_type_tag_inner(
        &self,
        luau_type: &Type,
        aliases: &BTreeMap<String, TypeAliasInfo>,
        generics: &BTreeSet<String>,
        substitutions: &BTreeMap<String, Type>,
        seen_aliases: &mut BTreeSet<String>,
    ) -> u8 {
        let tag = match luau_type {
            Type::Reference {
                prefix,
                name,
                parameters,
                ..
            } if prefix.is_none() && parameters.is_empty() => {
                let name = name.as_str();
                if let Some(substitution) = substitutions.get(name) {
                    return self.bytecode_type_tag_inner(
                        substitution,
                        aliases,
                        generics,
                        substitutions,
                        seen_aliases,
                    );
                }
                if generics.contains(name) {
                    return TypeTag::Any as u16 as u8;
                }
                if let Some(alias) = aliases.get(name) {
                    return self.resolve_type_alias(
                        name,
                        alias,
                        aliases,
                        generics,
                        substitutions,
                        seen_aliases,
                    );
                }
                if self.context.options().vector_type.as_deref() == Some(name) {
                    TypeTag::Vector
                } else {
                    primitive_or_userdata_type_tag(name)
                }
            }
            Type::Reference {
                prefix,
                name,
                parameters,
                ..
            } if prefix.is_none() => {
                let name = name.as_str();
                if generics.contains(name) {
                    return TypeTag::Any as u16 as u8;
                }
                if let Some(alias) = aliases.get(name) {
                    return self.resolve_type_alias(
                        name,
                        alias,
                        aliases,
                        generics,
                        substitutions,
                        seen_aliases,
                    );
                }
                if self.context.options().vector_type.as_deref() == Some(name) {
                    TypeTag::Vector
                } else {
                    primitive_or_userdata_type_tag(name)
                }
            }
            Type::Optional { .. } => TypeTag::Nil,
            Type::Table { .. } => TypeTag::Table,
            Type::Function { .. } => TypeTag::Function,
            Type::Group { inner, .. } => {
                return self.bytecode_type_tag_inner(
                    inner,
                    aliases,
                    generics,
                    substitutions,
                    seen_aliases,
                );
            }
            Type::SingletonBool { .. } => TypeTag::Boolean,
            Type::SingletonString { .. } => TypeTag::String,
            Type::Union { types, .. } => {
                let mut optional = false;
                let mut tag = None;
                for ty in types {
                    let current = self.bytecode_type_tag_inner(
                        ty,
                        aliases,
                        generics,
                        substitutions,
                        seen_aliases,
                    );
                    if current == TypeTag::Nil as u16 as u8 {
                        optional = true;
                        continue;
                    }
                    match tag {
                        None => tag = Some(current),
                        Some(tag) if tag == current => {}
                        Some(_) => return TypeTag::Any as u16 as u8,
                    }
                }
                let Some(tag) = tag else {
                    return TypeTag::Any as u16 as u8;
                };
                if optional && tag != TypeTag::Any as u16 as u8 {
                    return tag | TypeTag::OptionalBit as u16 as u8;
                }
                return tag;
            }
            _ => TypeTag::Any,
        };
        tag as u16 as u8
    }

    fn resolve_type_alias(
        &self,
        name: &str,
        alias: &TypeAliasInfo,
        aliases: &BTreeMap<String, TypeAliasInfo>,
        generics: &BTreeSet<String>,
        substitutions: &BTreeMap<String, Type>,
        seen_aliases: &mut BTreeSet<String>,
    ) -> u8 {
        if !seen_aliases.insert(name.to_owned()) {
            return TypeTag::Any as u16 as u8;
        }
        if !alias.generics.is_empty() {
            seen_aliases.remove(name);
            return TypeTag::Any as u16 as u8;
        }
        let tag = self.bytecode_type_tag_inner(
            &alias.value,
            aliases,
            generics,
            substitutions,
            seen_aliases,
        );
        seen_aliases.remove(name);
        tag
    }

    fn push_debug_upvalues(&mut self, function_id: FunctionId, upvalues: &[FunctionUpvalue]) {
        if self.context.options().debug_level < 2 {
            return;
        }
        let names = self
            .context
            .functions
            .get(function_id)
            .map(|info| {
                info.upvalues()
                    .iter()
                    .map(|upvalue| (upvalue.local_id(), upvalue.name().to_owned()))
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();
        for upvalue in upvalues {
            let name = names
                .get(&upvalue.local_id)
                .map_or(0, |name| self.builder.add_string(name));
            self.builder.push_debug_upvalue(name);
        }
    }

    fn emit_captures(&mut self, captures: &[FunctionCapture]) {
        for capture in captures {
            self.builder.emit(Instruction::abc(
                Opcode::Capture,
                capture.kind as u8,
                capture.source,
                0,
            ));
        }
    }

    fn compile_function_expr_to(
        &mut self,
        func: &Expr,
        register: u8,
        fallback_debug_name: &str,
    ) -> Result<(), CompileError> {
        let function = self.compile_function_proto(fallback_debug_name, func)?;
        self.emit_function_closure(register, &function)?;
        Ok(())
    }
}

struct CompiledFunction {
    proto: u32,
    captures: Vec<FunctionCapture>,
    shareable: bool,
}

struct ClassMethodPlan {
    name: u32,
    closure: ClassClosurePlan,
    captures: Vec<FunctionCapture>,
}

enum ClassClosurePlan {
    Shareable { constant: u32, child_proto: i16 },
    NewClosure { child_proto: i16 },
}

pub(super) fn constant_ad_operand(constant: u32) -> Option<i16> {
    i16::try_from(constant).ok()
}

#[derive(Clone)]
pub(super) struct TypeAliasInfo {
    pub(super) generics: Vec<String>,
    pub(super) value: Type,
}

struct LoopContext {
    break_jumps: Vec<PendingJump>,
    continue_jumps: Vec<PendingJump>,
    local_offset: usize,
    local_offset_continue: usize,
    continue_used: bool,
    continue_target: Option<u32>,
    continue_exits_loop: bool,
    return_on_break: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CallResults {
    None,
    Fixed(u8),
    Multret,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InlineCallMode {
    Value,
    Return,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct LoopUnrollPlan {
    pub(super) trip_count: i32,
    pub(super) from: f64,
    pub(super) step: f64,
}

impl CallResults {
    fn operand(self) -> u8 {
        match self {
            Self::None => 1,
            Self::Fixed(count) => count + 1,
            Self::Multret => 0,
        }
    }

    fn return_operand(self) -> u8 {
        match self {
            Self::None => 1,
            Self::Fixed(count) => count + 1,
            Self::Multret => 0,
        }
    }
}

enum PendingJump {
    Ad {
        index: usize,
        opcode: Opcode,
        register: u8,
    },
    AdWithAux {
        index: usize,
        opcode: Opcode,
        register: u8,
        aux: Option<u32>,
    },
    Compare {
        index: usize,
        opcode: Opcode,
        left: u8,
        right: u8,
    },
}

struct Assignment {
    lvalue: LValue,
    conflict_register: Option<u8>,
    value_register: Option<u8>,
}

#[derive(Clone, Debug)]
enum LValue {
    Local {
        local_id: u32,
        register: u8,
        location: Option<Location>,
    },
    Upvalue {
        upvalue: u8,
        location: Option<Location>,
    },
    Global {
        name: String,
        location: Option<Location>,
    },
    IndexName {
        table: u8,
        name: String,
        location: Option<Location>,
    },
    IndexNumber {
        table: u8,
        index: u8,
        location: Option<Location>,
    },
    IndexExpr {
        table: u8,
        index: u8,
        location: Option<Location>,
    },
}

impl LValue {
    fn is_local(&self) -> bool {
        matches!(self, Self::Local { .. })
    }

    fn local_register(&self) -> Option<u8> {
        match self {
            Self::Local { register, .. } => Some(*register),
            Self::Upvalue { .. }
            | Self::Global { .. }
            | Self::IndexName { .. }
            | Self::IndexNumber { .. }
            | Self::IndexExpr { .. } => None,
        }
    }

    fn location(&self) -> Option<Location> {
        match self {
            Self::Local { location, .. }
            | Self::Upvalue { location, .. }
            | Self::Global { location, .. }
            | Self::IndexName { location, .. }
            | Self::IndexNumber { location, .. }
            | Self::IndexExpr { location, .. } => *location,
        }
    }

    fn mark_local_register_conflicts(
        &self,
        assigned_locals: &BTreeSet<u32>,
        local_registers: &BTreeMap<u32, u8>,
        conflicts: &mut BTreeSet<u32>,
    ) {
        match self {
            Self::IndexName { table, .. } | Self::IndexNumber { table, .. } => {
                mark_register_local_conflict(*table, assigned_locals, local_registers, conflicts);
            }
            Self::IndexExpr { table, index, .. } => {
                mark_register_local_conflict(*table, assigned_locals, local_registers, conflicts);
                mark_register_local_conflict(*index, assigned_locals, local_registers, conflicts);
            }
            Self::Local { .. } | Self::Upvalue { .. } | Self::Global { .. } => {}
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BreakBranchKind {
    Break,
    WhileTrueBreak,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LoopControlBranchKind {
    Break,
    Continue,
}

impl PendingJump {
    fn index(&self) -> usize {
        match self {
            Self::Ad { index, .. }
            | Self::AdWithAux { index, .. }
            | Self::Compare { index, .. } => *index,
        }
    }
}

fn tail_if_branch_uses_shared_return(stat: &Stat, optimization_level: u8) -> bool {
    matches!(single_statement(stat), Stat::Expr { .. })
        || tail_if_branch_needs_shared_exit(stat, optimization_level)
}

fn tail_if_branch_needs_shared_exit(stat: &Stat, optimization_level: u8) -> bool {
    optimization_level < 2 && matches!(trailing_statement(stat), Stat::For { .. })
}

fn trailing_statement(stat: &Stat) -> &Stat {
    match stat {
        Stat::Block { body, .. } if !body.is_empty() => {
            trailing_statement(body.last().expect("checked non-empty body"))
        }
        _ => stat,
    }
}

#[derive(Clone, Debug)]
struct FunctionCapture {
    kind: CaptureType,
    source: u8,
}

#[derive(Clone, Debug)]
struct FunctionUpvalue {
    local_id: u32,
}
