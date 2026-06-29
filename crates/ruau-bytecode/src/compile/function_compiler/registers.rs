use std::collections::{BTreeMap, BTreeSet};

use ruau_ast::syntax::{Expr, LocalId};

use super::{
    CompileError, ConstantValue, FunctionCompiler, FunctionId, FunctionUpvalue, LocalValueFacts,
    LoopContext, Opcode, register_add, type_info_tag,
};
use crate::{Instruction, opcodes::TypeTag};

#[derive(Debug)]
pub(super) struct RegisterFrame {
    pub(super) next_register: u8,
    pub(super) active_local_start: usize,
    pub(super) active_typed_local_start: usize,
    pub(super) local_bindings: Vec<(u32, Option<u8>)>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct LocalScope {
    pub(super) active_local_start: usize,
    pub(super) active_typed_local_start: usize,
    pub(super) next_register: u8,
}

pub(super) struct SavedFunctionState {
    local_registers: BTreeMap<u32, u8>,
    local_values: LocalValueFacts,
    elided_local_initializers: BTreeSet<u32>,
    active_locals: Vec<ActiveLocal>,
    active_typed_locals: Vec<ActiveTypedLocal>,
    loop_stack: Vec<LoopContext>,
    upvalues: Vec<FunctionUpvalue>,
    function_stack: Vec<FunctionId>,
    current_function_id: Option<FunctionId>,
    inline_stack: Vec<FunctionId>,
    inline_function_args: BTreeMap<u32, FunctionId>,
    current_function_depth: usize,
    next_register: u8,
    reserved_ranges: Vec<(u8, u8)>,
}

#[derive(Clone, Debug)]
pub(super) struct ActiveLocal {
    pub(super) local_id: u32,
    pub(super) register: u8,
    pub(super) debug_name: String,
    pub(super) debug_start_pc: Option<u32>,
    pub(super) captured: bool,
}

pub(super) struct ActiveTypedLocal {
    pub(super) type_tag: u8,
    pub(super) reg: u8,
    pub(super) startpc: u32,
}

impl FunctionCompiler {
    pub(super) fn take_function_state(&mut self) -> SavedFunctionState {
        SavedFunctionState {
            local_registers: std::mem::take(&mut self.local_registers),
            local_values: std::mem::take(&mut self.local_values),
            elided_local_initializers: std::mem::take(&mut self.elided_local_initializers),
            active_locals: std::mem::take(&mut self.active_locals),
            active_typed_locals: std::mem::take(&mut self.active_typed_locals),
            loop_stack: std::mem::take(&mut self.loop_stack),
            upvalues: std::mem::take(&mut self.upvalues),
            function_stack: std::mem::take(&mut self.function_stack),
            current_function_id: self.current_function_id,
            inline_stack: std::mem::take(&mut self.inline_stack),
            inline_function_args: std::mem::take(&mut self.inline_function_args),
            current_function_depth: self.current_function_depth,
            next_register: self.next_register,
            reserved_ranges: std::mem::take(&mut self.reserved_ranges),
        }
    }

    pub(super) fn restore_function_state(&mut self, state: SavedFunctionState) {
        self.local_registers = state.local_registers;
        self.local_values = state.local_values;
        self.elided_local_initializers = state.elided_local_initializers;
        self.active_locals = state.active_locals;
        self.active_typed_locals = state.active_typed_locals;
        self.loop_stack = state.loop_stack;
        self.upvalues = state.upvalues;
        self.function_stack = state.function_stack;
        self.current_function_id = state.current_function_id;
        self.inline_stack = state.inline_stack;
        self.inline_function_args = state.inline_function_args;
        self.current_function_depth = state.current_function_depth;
        self.next_register = state.next_register;
        self.reserved_ranges = state.reserved_ranges;
    }

    /// Whether the half-open register span `[lo, hi)` overlaps a register reserved by an
    /// enclosing `for` loop's control state (see `reserved_ranges`).
    pub(super) fn overlaps_reserved(&self, lo: u8, hi: u8) -> bool {
        self.reserved_ranges
            .iter()
            .any(|&(rlo, rhi)| lo < rhi && rlo < hi)
    }

    pub(super) fn start_register_frame(&self) -> RegisterFrame {
        RegisterFrame {
            next_register: self.next_register,
            active_local_start: self.active_locals.len(),
            active_typed_local_start: self.active_typed_locals.len(),
            local_bindings: Vec::new(),
        }
    }

    pub(super) fn start_local_scope(&self) -> LocalScope {
        LocalScope {
            active_local_start: self.active_locals.len(),
            active_typed_local_start: self.active_typed_locals.len(),
            next_register: self.next_register,
        }
    }

    pub(super) fn restore_register_frame(&mut self, frame: RegisterFrame) {
        self.close_typed_locals_from(frame.active_typed_local_start);
        self.pop_locals_from(frame.active_local_start);
        for (local_id, previous) in frame.local_bindings.into_iter().rev() {
            if let Some(register) = previous {
                self.local_registers.insert(local_id, register);
            } else {
                self.local_registers.remove(&local_id);
            }
        }
        self.next_register = frame.next_register;
    }

    pub(super) fn reserve_register(&mut self) -> Result<u8, CompileError> {
        self.reserve_registers(1)
    }

    pub(super) fn reserve_registers(&mut self, count: u8) -> Result<u8, CompileError> {
        let active_next = self
            .active_locals
            .iter()
            .map(|local| local.register.saturating_add(1))
            .max()
            .unwrap_or(0);
        let register = self.next_register.max(active_next);
        self.next_register = register_add(register, count)?;
        self.builder.set_max_stack_size(self.next_register);
        Ok(register)
    }

    pub(super) fn bind_frame_local(
        &mut self,
        frame: &mut RegisterFrame,
        local_id: u32,
        register: u8,
    ) {
        self.bind_frame_local_with_debug(frame, local_id, register, "", None);
    }

    pub(super) fn bind_frame_local_with_debug(
        &mut self,
        frame: &mut RegisterFrame,
        local_id: u32,
        register: u8,
        debug_name: &str,
        debug_start_pc: Option<u32>,
    ) {
        let previous = self.local_registers.insert(local_id, register);
        frame.local_bindings.push((local_id, previous));
        self.active_locals.push(ActiveLocal {
            local_id,
            register,
            debug_name: debug_name.to_owned(),
            debug_start_pc: debug_start_pc.filter(|_| self.context.options().debug_level >= 2),
            captured: false,
        });
    }

    pub(super) fn declare_local(&mut self, local: &ruau_ast::syntax::Local, register: u8) {
        self.declare_local_with_debug_start_and_type(
            local,
            register,
            Some(self.builder.current_type_info_pc()),
            None,
        );
    }

    pub(super) fn declare_local_pending_debug(
        &mut self,
        local: &ruau_ast::syntax::Local,
        register: u8,
    ) {
        self.declare_local_with_debug_start_and_type(local, register, None, None);
    }

    pub(super) fn declare_local_with_debug_start_and_type(
        &mut self,
        local: &ruau_ast::syntax::Local,
        register: u8,
        debug_start_pc: Option<u32>,
        type_tag: Option<u8>,
    ) {
        self.local_registers.insert(local.id.index(), register);
        self.active_locals.push(ActiveLocal {
            local_id: local.id.index(),
            register,
            debug_name: local.name.as_str().to_owned(),
            debug_start_pc: debug_start_pc.filter(|_| self.context.options().debug_level >= 2),
            captured: false,
        });
        if let Some(type_tag) = type_tag {
            self.start_typed_local_tag(type_tag, register, self.builder.current_type_info_pc());
        }
    }

    pub(super) fn start_debug_locals_from(&mut self, start: usize) {
        if self.context.options().debug_level < 2 {
            return;
        }
        let start_pc = self.builder.current_type_info_pc();
        for local in &mut self.active_locals[start..] {
            if local.debug_name.is_empty() {
                continue;
            }
            local.debug_start_pc.get_or_insert(start_pc);
        }
    }

    fn first_captured_local_register_from(&self, start: usize) -> Option<u8> {
        self.active_locals[start..]
            .iter()
            .filter(|local| local.captured)
            .map(|local| local.register)
            .min()
    }

    pub(super) fn locals_captured_from(&self, start: usize) -> bool {
        self.first_captured_local_register_from(start).is_some()
    }

    pub(super) fn close_locals_from(&mut self, start: usize) {
        let Some(first_capture) = self.first_captured_local_register_from(start) else {
            return;
        };
        self.builder
            .emit(Instruction::abc(Opcode::CloseUpvals, first_capture, 0, 0));
    }

    pub(super) fn clear_dead_locals_from(&mut self, start: usize) {
        if !self.context.options().clear_dead_stack_slots {
            return;
        }
        let outer_registers: BTreeSet<u8> = self.active_locals[..start]
            .iter()
            .map(|local| local.register)
            .collect();
        let mut registers: Vec<u8> = self.active_locals[start..]
            .iter()
            .filter(|local| !outer_registers.contains(&local.register))
            .map(|local| local.register)
            .collect();
        registers.sort_unstable();
        registers.dedup();

        for register in registers {
            self.builder
                .emit(Instruction::abc(Opcode::LoadNil, register, 0, 0));
        }
    }

    pub(super) fn clear_scratch_registers(&mut self, start: u8, end: u8) {
        if !self.context.options().clear_dead_stack_slots {
            return;
        }
        if start >= end {
            return;
        }
        let active_registers: BTreeSet<u8> = self
            .active_locals
            .iter()
            .map(|local| local.register)
            .collect();
        for register in start..end {
            if active_registers.contains(&register) {
                continue;
            }
            self.builder
                .emit(Instruction::abc(Opcode::LoadNil, register, 0, 0));
        }
    }

    pub(super) fn pop_locals_from(&mut self, start: usize) {
        self.close_debug_locals_from(start);
        for local in self.active_locals.drain(start..).rev() {
            self.local_registers.remove(&local.local_id);
            self.local_values.invalidate_local(local.local_id);
            self.elided_local_initializers.remove(&local.local_id);
        }
    }

    pub(super) fn mark_local_captured(&mut self, local_id: u32) {
        if let Some(local) = self
            .active_locals
            .iter_mut()
            .rev()
            .find(|local| local.local_id == local_id)
        {
            local.captured = true;
        }
    }

    pub(super) fn local_is_written(&self, local_id: u32) -> bool {
        self.context
            .variable(LocalId::new(local_id))
            .is_some_and(|variable| variable.is_written())
    }

    pub(super) fn register_holds_active_local(&self, register: u8) -> bool {
        self.active_locals
            .iter()
            .any(|local| local.register == register)
    }

    pub(super) fn scratch_register_at_or_after(&self, register: u8) -> Result<u8, CompileError> {
        // A scratch register must be free, which means at or above the reserved-register
        // watermark `next_register` — not merely past the requested hint and any active
        // local. The hint (`register`) can sit below registers that are reserved *without*
        // being an active local — e.g. a numeric `for` loop's limit/step control occupy
        // `base`/`base+1` but are anonymous, so the active-local scan alone would hand one
        // out as scratch and the loop body would clobber the loop bound.
        let mut scratch = register.max(self.next_register);
        while self.register_holds_active_local(scratch) {
            scratch = register_add(scratch, 1)?;
        }
        Ok(scratch)
    }

    pub(super) fn local_constant(&self, local_id: u32) -> Option<ConstantValue> {
        (!self.local_is_written(local_id))
            .then(|| self.local_values.constant(local_id))
            .flatten()
    }

    pub(super) fn set_local_value_facts(
        &mut self,
        local_id: u32,
        constant: Option<ConstantValue>,
        import_path: Option<Vec<String>>,
    ) {
        if self.local_is_written(local_id) {
            self.local_values.set_constant(local_id, None);
            self.local_values.set_import_path(local_id, None);
        } else {
            self.local_values.set_constant(local_id, constant);
            self.local_values.set_import_path(local_id, import_path);
        }
    }

    pub(super) fn start_typed_local_tag(&mut self, type_tag: u8, reg: u8, startpc: u32) {
        if self.context.type_info_level() == 0 {
            return;
        }
        self.active_typed_locals.push(ActiveTypedLocal {
            type_tag,
            reg,
            startpc,
        });
    }

    pub(super) fn close_typed_locals_from(&mut self, start: usize) {
        if self.active_typed_locals.len() <= start {
            return;
        }
        let endpc = self.builder.current_type_info_pc();
        for local in self.active_typed_locals.split_off(start) {
            self.builder
                .push_local_type_info(local.type_tag, local.reg, local.startpc, endpc);
        }
    }

    pub(super) fn close_debug_locals_from(&mut self, start: usize) {
        if self.context.options().debug_level < 2 || self.active_locals.len() <= start {
            return;
        }
        let end_pc = self.builder.current_type_info_pc();
        let mut locals = Vec::new();
        for local in &mut self.active_locals[start..] {
            let Some(start_pc) = local.debug_start_pc.take() else {
                continue;
            };
            if local.debug_name.is_empty() {
                continue;
            }
            locals.push((local.debug_name.clone(), start_pc, end_pc, local.register));
        }
        for (name, start_pc, end_pc, register) in locals {
            let name = self.builder.add_string(&name);
            self.builder
                .push_debug_local(name, start_pc, end_pc, register);
        }
    }

    pub(super) fn local_declaration_type_tag(
        &self,
        local: &ruau_ast::syntax::Local,
        value: Option<&Expr>,
    ) -> Option<u8> {
        if self.context.type_info_level() == 0 {
            return None;
        }
        Some(
            local
                .luau_type
                .as_deref()
                .and_then(type_info_tag)
                .or_else(|| value.and_then(|value| self.expr_type_info_tag(value)))
                .unwrap_or(TypeTag::Any as u16 as u8),
        )
    }

    pub(super) fn expr_type_info_tag(&self, expr: &Expr) -> Option<u8> {
        match expr {
            Expr::Local { local, .. } => local
                .luau_type
                .as_deref()
                .and_then(type_info_tag)
                .or_else(|| self.active_local_type_tag(local.id.index())),
            Expr::Number { .. } => Some(TypeTag::Number as u16 as u8),
            Expr::Integer { .. } => Some(TypeTag::Integer as u16 as u8),
            Expr::Binary {
                op, left, right, ..
            } if super::arithmetic_opcode(*op).is_some() => {
                let left = self.expr_type_info_tag(left);
                let right = self.expr_type_info_tag(right);
                if left == Some(TypeTag::Vector as u16 as u8)
                    && right == Some(TypeTag::Vector as u16 as u8)
                {
                    Some(TypeTag::Vector as u16 as u8)
                } else if left == Some(TypeTag::Number as u16 as u8)
                    && right == Some(TypeTag::Number as u16 as u8)
                {
                    Some(TypeTag::Number as u16 as u8)
                } else {
                    None
                }
            }
            Expr::Group { expr, .. }
            | Expr::TypeAssertion { expr, .. }
            | Expr::Instantiate { expr, .. } => self.expr_type_info_tag(expr),
            _ => None,
        }
    }

    pub(super) fn active_local_type_tag(&self, local_id: u32) -> Option<u8> {
        let register = self
            .active_locals
            .iter()
            .rev()
            .find(|local| local.local_id == local_id)
            .map(|local| local.register)?;
        self.active_typed_locals
            .iter()
            .rev()
            .find(|local| local.reg == register)
            .map(|local| local.type_tag)
    }
}
