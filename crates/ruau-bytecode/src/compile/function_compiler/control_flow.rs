use super::*;

impl FunctionCompiler {
    pub(super) fn compile_if_statement(
        &mut self,
        condition: &Expr,
        then_body: &Stat,
        else_body: Option<&Stat>,
        location: Option<Location>,
        is_tail: bool,
    ) -> Result<(), CompileError> {
        match self.optimized_condition_truthiness_expr(condition)? {
            Some(true) => {
                self.compile_stat_tail(then_body, is_tail)?;
                return Ok(());
            }
            Some(false) => {
                if let Some(else_body) = else_body {
                    self.compile_stat_tail(else_body, is_tail)?;
                }
                return Ok(());
            }
            None => {}
        }

        if let Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
            ..
        } = condition
            && self.optimized_condition_truthiness_expr(right)? == Some(false)
        {
            self.compile_expr_side(left)?;
            if let Some(else_body) = else_body {
                self.compile_stat_tail(else_body, is_tail)?;
            }
            return Ok(());
        }

        if self.compile_elided_loop_control_if(condition, then_body, else_body)? {
            return Ok(());
        }
        if self.compile_elided_break_if(condition, then_body, else_body)? {
            return Ok(());
        }

        let jumps = self.emit_condition_jumps(condition, false)?;

        self.compile_stat_tail(then_body, is_tail)?;
        let then_exit = if else_body.is_some() && !self.current_code_returns() {
            if is_tail
                && !else_body_is_empty(else_body)
                && !tail_if_branch_uses_shared_return(then_body, self.context.optimization_level())
            {
                if let Some(line) = stat_last_line(then_body) {
                    self.builder.set_debug_line(line);
                    self.builder.set_implicit_return_line_base(line);
                }
                self.emit_return(0, 1);
                None
            } else {
                if is_tail
                    && tail_if_branch_needs_shared_exit(
                        then_body,
                        self.context.optimization_level(),
                    )
                    && let Some(line) = stat_line(trailing_statement(then_body))
                {
                    self.builder.set_debug_line(line);
                }
                Some(self.emit_jump_placeholder(Opcode::Jump))
            }
        } else {
            None
        };

        let redundant_fallthrough_close =
            if else_body.is_none() && is_tail && stat_guarantees_return(then_body) {
                self.close_upvals_before_trailing_return()
            } else {
                None
            };
        if let Some(close) = redundant_fallthrough_close {
            self.builder.emit(close);
        }

        self.patch_jumps_to_current(jumps)?;
        if let Some(else_body) = else_body {
            let else_start = self.builder.current_code().len();
            self.compile_stat_tail(else_body, is_tail)?;
            let else_emitted = self.builder.current_code().len() > else_start;
            let else_returns = self.current_code_returns();
            let needs_tail_return = is_tail
                && !else_body_is_empty(Some(else_body))
                && (!else_emitted
                    || !else_returns
                    || (then_exit.is_some() && stat_guarantees_return(else_body)));
            if needs_tail_return {
                if let Some(line) = location.map(|location| {
                    location.end.line
                        + 1
                        + u32::from(
                            location.end.line > location.begin.line
                                || self.current_function_depth != 0,
                        )
                }) {
                    self.builder.set_debug_line(line);
                    self.builder.set_implicit_return_line_base(line);
                }
                if let Some(then_exit) = then_exit {
                    self.patch_jump_to_current(&then_exit)?;
                }
                self.emit_return(0, 1);
            } else if let Some(then_exit) = then_exit {
                self.patch_jump_to_current(&then_exit)?;
            }
        } else if is_tail
            && !tail_if_branch_needs_shared_exit(then_body, self.context.optimization_level())
        {
            if let Some(line) = location
                .map(|location| {
                    location.end.line
                        + 1
                        + u32::from(
                            self.current_function_depth != 0
                                || (!stat_guarantees_return(then_body)
                                    && location.end.line > location.begin.line),
                        )
                })
                .or_else(|| stat_last_line(then_body).map(|line| line + 1))
            {
                self.builder.set_debug_line(line);
                self.builder.set_implicit_return_line_base(line);
            }
            self.builder.emit(Instruction::abc(Opcode::Return, 0, 1, 0));
        }
        Ok(())
    }

    pub(super) fn compile_elided_continue_if_before_break(
        &mut self,
        stat: &Stat,
        next_stat: Option<&Stat>,
    ) -> Result<bool, CompileError> {
        if self.context.optimization_level() == 0 {
            return Ok(false);
        }
        let Stat::If {
            condition,
            then_body,
            else_body,
            ..
        } = stat
        else {
            return Ok(false);
        };
        if self.loop_stack.is_empty()
            || self
                .loop_stack
                .last()
                .is_some_and(|context| context.continue_exits_loop)
            || self
                .loop_stack
                .last()
                .is_some_and(|context| self.locals_captured_from(context.local_offset))
            || loop_control_branch_kind(then_body) != Some(LoopControlBranchKind::Continue)
            || !else_body
                .as_deref()
                .is_some_and(|body| else_body_is_empty(Some(body)))
            || next_stat.and_then(loop_control_branch_kind) != Some(LoopControlBranchKind::Break)
        {
            return Ok(false);
        }

        if let Some(line) = stat_line(stat) {
            self.builder.set_debug_line(line);
        }
        self.emit_coverage();
        let break_jumps = self.emit_condition_jumps(condition, false)?;
        self.append_break_jumps(break_jumps)?;
        self.compile_continue_statement()?;
        Ok(true)
    }

    pub(super) fn elide_tail_repeat_continue_condition_local(
        &mut self,
        stat: &Stat,
        next_stat: Option<&Stat>,
    ) -> Result<bool, CompileError> {
        if self.context.optimization_level() == 0 {
            return Ok(false);
        }
        let Stat::Local { vars, values, .. } = stat else {
            return Ok(false);
        };
        let [var] = vars.as_slice() else {
            return Ok(false);
        };
        let Some(Stat::Repeat {
            condition, body, ..
        }) = next_stat
        else {
            return Ok(false);
        };
        let value = match values.as_slice() {
            [] => ConstantValue::Nil,
            [value] => {
                let Some(value) = self.constant_value_expr(value)? else {
                    return Ok(false);
                };
                value
            }
            _ => return Ok(false),
        };
        let local_id = var.id.index();
        if loop_control_branch_kind(body) != Some(LoopControlBranchKind::Continue)
            || condition_truthiness_with_local_constant(condition, local_id, &value) != Some(true)
        {
            return Ok(false);
        }

        self.local_values.set_constant(local_id, Some(value));
        Ok(true)
    }

    pub(super) fn compile_elided_loop_control_if(
        &mut self,
        condition: &Expr,
        then_body: &Stat,
        else_body: Option<&Stat>,
    ) -> Result<bool, CompileError> {
        let Some(then_kind) = loop_control_branch_kind(then_body) else {
            return Ok(false);
        };
        if self.loop_stack.is_empty() {
            return Ok(false);
        }

        match self.optimized_condition_truthiness_expr(condition)? {
            Some(true) => {
                match then_kind {
                    LoopControlBranchKind::Break => self.compile_break_statement()?,
                    LoopControlBranchKind::Continue => self.compile_continue_statement()?,
                }
                return Ok(true);
            }
            Some(false) => {
                if let Some(else_body) = else_body {
                    self.compile_stat_tail(else_body, false)?;
                }
                return Ok(true);
            }
            None => {}
        }

        match then_kind {
            LoopControlBranchKind::Break if else_body.is_none() => {
                if self
                    .loop_stack
                    .last()
                    .is_some_and(|context| self.locals_captured_from(context.local_offset))
                {
                    return Ok(false);
                }
                let break_jumps = self.emit_condition_jumps(condition, true)?;
                self.append_break_jumps(break_jumps)?;
            }
            LoopControlBranchKind::Break if else_body_is_empty(else_body) => {
                let false_jumps = self.emit_condition_jumps(condition, false)?;
                self.compile_break_statement()?;
                self.patch_jumps_to_current(false_jumps)?;
            }
            LoopControlBranchKind::Continue
                if else_body.is_some() && else_body_is_empty(else_body) =>
            {
                let false_jumps = self.emit_condition_jumps(condition, false)?;
                self.compile_continue_statement()?;
                self.patch_jumps_to_current(false_jumps)?;
            }
            LoopControlBranchKind::Continue
                if else_body
                    .and_then(loop_control_branch_kind)
                    .is_some_and(|kind| kind == LoopControlBranchKind::Break) =>
            {
                let false_jumps = self.emit_condition_jumps(condition, false)?;
                if let Some(line) = stat_line(leading_statement(then_body)) {
                    self.builder.set_debug_line(line);
                }
                self.compile_continue_statement()?;
                self.patch_jumps_to_current(false_jumps)?;
                if let Some(else_body) = else_body
                    && let Some(line) = stat_line(leading_statement(else_body))
                {
                    self.builder.set_debug_line(line);
                }
                self.compile_break_statement()?;
            }
            LoopControlBranchKind::Continue if else_body.is_none() => {
                if self
                    .loop_stack
                    .last()
                    .is_some_and(|context| self.locals_captured_from(context.local_offset_continue))
                {
                    return Ok(false);
                }
                if self
                    .loop_stack
                    .last()
                    .is_some_and(|context| context.continue_exits_loop)
                {
                    if self
                        .loop_stack
                        .last()
                        .is_some_and(|context| context.return_on_break)
                    {
                        let false_jumps = self.emit_condition_jumps(condition, false)?;
                        self.compile_continue_statement()?;
                        self.patch_jumps_to_current(false_jumps)?;
                    } else {
                        let break_jumps = self.emit_condition_jumps(condition, true)?;
                        self.append_break_jumps(break_jumps)?;
                    }
                } else {
                    let continue_jumps = self.emit_condition_jumps(condition, true)?;
                    self.append_continue_jumps(continue_jumps)?;
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    pub(super) fn compile_elided_break_if(
        &mut self,
        condition: &Expr,
        then_body: &Stat,
        else_body: Option<&Stat>,
    ) -> Result<bool, CompileError> {
        let Some(then_kind) = break_branch_kind(then_body) else {
            return Ok(false);
        };
        let Some((else_condition, else_kind)) = else_if_break_branch(else_body) else {
            return Ok(false);
        };
        if self.loop_stack.is_empty() {
            return Ok(false);
        }
        if self
            .loop_stack
            .last()
            .is_some_and(|context| self.locals_captured_from(context.local_offset))
        {
            return Ok(false);
        }

        match (then_kind, else_kind) {
            (BreakBranchKind::Break, BreakBranchKind::Break) => {
                let false_jumps = self.emit_condition_jumps(condition, false)?;
                self.compile_break_statement()?;
                self.patch_jumps_to_current(false_jumps)?;
                if let Some(line) = expr_line(else_condition) {
                    self.builder.set_debug_line(line);
                }
                let break_jumps = self.emit_condition_jumps(else_condition, true)?;
                self.append_break_jumps(break_jumps)?;
            }
            (BreakBranchKind::WhileTrueBreak, BreakBranchKind::WhileTrueBreak) => {
                if self.context.optimization_level() == 0 {
                    return Ok(false);
                }
                let mut after_if_jumps = Vec::new();
                let false_jumps = self.emit_condition_jumps(condition, false)?;
                after_if_jumps.extend(self.emit_while_true_break_to_current_target());
                after_if_jumps.push(self.emit_jump_placeholder(Opcode::Jump));
                self.patch_jumps_to_current(false_jumps)?;

                if let Some(line) = expr_line(else_condition) {
                    self.builder.set_debug_line(line);
                }
                after_if_jumps.extend(self.emit_condition_jumps(else_condition, false)?);
                after_if_jumps.extend(self.emit_while_true_break_to_current_target());
                self.patch_jumps_to_current(after_if_jumps)?;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    pub(super) fn compile_break_statement(&mut self) -> Result<(), CompileError> {
        let Some(context) = self.loop_stack.last() else {
            return Err(CompileError::new("break outside loop"));
        };
        let local_offset = context.local_offset;
        let return_on_break = context.return_on_break;
        self.close_locals_from(local_offset);
        self.clear_dead_locals_from(local_offset);
        if return_on_break {
            self.emit_return(0, 1);
        } else {
            let jump = self.emit_jump_placeholder(Opcode::Jump);
            self.loop_stack
                .last_mut()
                .expect("loop context still present")
                .break_jumps
                .push(jump);
        }
        Ok(())
    }

    pub(super) fn compile_continue_statement(&mut self) -> Result<(), CompileError> {
        let Some(context) = self.loop_stack.last() else {
            return Err(CompileError::new("continue outside loop"));
        };
        let local_offset_continue = context.local_offset_continue;
        let continue_exits_loop = context.continue_exits_loop;
        let return_on_break = context.return_on_break;
        let continue_target = context.continue_target;
        self.loop_stack
            .last_mut()
            .expect("loop context still present")
            .continue_used = true;
        self.close_locals_from(local_offset_continue);
        if continue_exits_loop {
            if return_on_break {
                self.emit_return(0, 1);
            } else {
                let jump = self.emit_jump_placeholder(Opcode::Jump);
                self.loop_stack
                    .last_mut()
                    .expect("loop context still present")
                    .break_jumps
                    .push(jump);
            }
        } else if let Some(target) = continue_target {
            self.emit_jump_to_word(Opcode::JumpBack, target)?;
        } else {
            let jump = self.emit_jump_placeholder(Opcode::Jump);
            self.loop_stack
                .last_mut()
                .expect("loop context still present")
                .continue_jumps
                .push(jump);
        }
        Ok(())
    }

    pub(super) fn compile_while_statement(
        &mut self,
        condition: &Expr,
        body: &Stat,
        location: Option<Location>,
        is_tail: bool,
    ) -> Result<(), CompileError> {
        let condition_truthiness = self.optimized_condition_truthiness_expr(condition)?;
        if condition_truthiness == Some(false) {
            self.update_after_statement_location(location);
            return Ok(());
        }
        self.builder.set_proto_flags(0);
        if condition_truthiness == Some(true)
            && break_branch_kind(body) == Some(BreakBranchKind::Break)
        {
            if let Some(line) = stat_line(leading_statement(body)) {
                self.builder.set_debug_line(line);
            }
            let break_jump = self.emit_jump_placeholder(Opcode::Jump);
            if let Some(location) = location {
                self.builder.set_debug_line(location.begin.line + 1);
            }
            let loop_jump = self.emit_jump_placeholder(Opcode::JumpBack);
            self.patch_jumps_to_current(vec![break_jump, loop_jump])?;
            self.update_after_statement_location(location);
            return Ok(());
        }

        let loop_start = self.builder.current_word_offset();
        let exit_jumps = if condition_truthiness == Some(true) {
            Vec::new()
        } else {
            self.emit_condition_jumps(condition, false)?
        };
        self.loop_stack.push(LoopContext {
            break_jumps: Vec::new(),
            continue_jumps: Vec::new(),
            local_offset: self.active_locals.len(),
            local_offset_continue: self.active_locals.len(),
            continue_used: false,
            continue_target: Some(loop_start),
            continue_exits_loop: false,
            return_on_break: is_tail && self.context.optimization_level() > 0,
        });
        self.compile_stat_tail(body, false)?;
        let context = self.loop_stack.pop().expect("while loop context exists");
        if let Some(location) = location {
            self.builder.set_debug_line(location.begin.line + 1);
        }
        self.patch_jumps_to_current(context.continue_jumps)?;
        self.emit_jump_to_word(Opcode::JumpBack, loop_start)?;
        self.patch_jumps_to_current(exit_jumps)?;
        self.patch_jumps_to_current(context.break_jumps)?;
        self.update_after_statement_location(location);
        Ok(())
    }

    pub(super) fn compile_repeat_statement(
        &mut self,
        condition: &Expr,
        body: &Stat,
        location: Option<Location>,
        is_tail: bool,
    ) -> Result<(), CompileError> {
        self.builder.set_proto_flags(0);
        let loop_start = self.builder.current_word_offset();
        let condition_truthiness = self.optimized_condition_truthiness_expr(condition)?;
        self.loop_stack.push(LoopContext {
            break_jumps: Vec::new(),
            continue_jumps: Vec::new(),
            local_offset: self.active_locals.len(),
            local_offset_continue: self.active_locals.len(),
            continue_used: false,
            continue_target: None,
            continue_exits_loop: condition_truthiness == Some(true),
            return_on_break: is_tail && self.context.optimization_level() > 0,
        });
        let body_scope = self.start_local_scope();
        let condition_local_start = match body {
            Stat::Block { body, .. } => self.compile_repeat_block_statements(body)?,
            body => {
                self.compile_stat_tail(body, false)?;
                let mut condition_local_start = None;
                self.note_repeat_body_statement(&mut condition_local_start)?;
                condition_local_start
            }
        };
        let context = self.loop_stack.pop().expect("repeat loop context exists");

        if let Some(condition_local_start) = condition_local_start {
            if let Some(line) = stat_last_line(body) {
                self.builder.set_debug_line(line);
                self.builder.set_implicit_return_line_base(line);
            }
            let next_register = if self.context.optimization_level() == 0
                && self.context.options().debug_level >= 2
            {
                self.next_register
            } else {
                self.active_locals
                    .get(condition_local_start)
                    .map(|local| local.register)
                    .unwrap_or(self.next_register)
            };
            self.close_locals_from(condition_local_start);
            self.pop_locals_from(condition_local_start);
            self.next_register = next_register;
        }

        if condition_truthiness != Some(true) {
            self.patch_jumps_to_current(context.continue_jumps)?;
        }
        if let Some(line) = expr_line(condition) {
            self.builder.set_debug_line(line);
        }

        if condition_truthiness == Some(true) {
            self.close_locals_from(body_scope.active_local_start);
            self.patch_jumps_to_current(context.break_jumps)?;
            self.pop_local_scope(body_scope);
            self.update_after_statement_location(location);
            if context.return_on_break
                && loop_control_branch_kind(body) == Some(LoopControlBranchKind::Continue)
            {
                self.builder.emit_implicit_return();
            }
            return Ok(());
        }
        if condition_truthiness == Some(false) {
            self.close_locals_from(body_scope.active_local_start);
            self.emit_jump_to_word(Opcode::JumpBack, loop_start)?;
            self.patch_jumps_to_current(context.break_jumps)?;
            self.pop_local_scope(body_scope);
            self.update_after_statement_location(location);
            return Ok(());
        }
        let exit_jumps = self.emit_condition_jumps(condition, true)?;
        self.close_locals_from(body_scope.active_local_start);
        self.emit_jump_to_word(Opcode::JumpBack, loop_start)?;
        self.patch_jumps_to_current(exit_jumps)?;
        self.close_locals_from(body_scope.active_local_start);
        self.patch_jumps_to_current(context.break_jumps)?;
        self.pop_local_scope(body_scope);
        self.update_after_statement_location(location);
        Ok(())
    }

    pub(super) fn compile_for_statement(
        &mut self,
        var: &ruau_ast::syntax::Local,
        from: &Expr,
        to: &Expr,
        step: Option<&Expr>,
        body: &Stat,
        location: Option<Location>,
    ) -> Result<(), CompileError> {
        if self.context.optimization_level() >= 2
            && let Some(plan) = self.loop_unroll_plan(var, from, to, step, body)?
            && !self.local_is_written(var.id.index())
        {
            self.compile_unrolled_for(var, body, location, plan)?;
            return Ok(());
        }

        self.builder.set_proto_flags(0);
        let base = self.next_register;
        let index_register = register_add(base, 2)?;
        let mutable_loop_var = stat_assigns_local(body, var.id.index());
        let value_register = if mutable_loop_var {
            register_add(base, 3)?
        } else {
            index_register
        };
        let loop_var_type_start = self.builder.current_type_info_pc();
        self.compile_expr_to(from, index_register)?;
        if matches!(
            to,
            Expr::Call { .. } | Expr::Group { .. } | Expr::TypeAssertion { .. }
        ) {
            self.compile_expr_to(to, register_add(base, 3)?)?;
            self.builder.emit(Instruction::abc(
                Opcode::Move,
                base,
                register_add(base, 3)?,
                0,
            ));
        } else {
            self.compile_expr_to(to, base)?;
        }
        if let Some(step) = step {
            let step_register = register_add(base, 1)?;
            if self.context.optimization_level() == 0
                && let Expr::Unary {
                    op: UnaryOp::Minus,
                    expr,
                    ..
                } = step
            {
                self.compile_dynamic_minus_with_source(
                    expr,
                    step_register,
                    register_add(base, 3)?,
                )?;
            } else {
                self.compile_expr_to(step, step_register)?;
            }
        } else {
            self.builder
                .emit(Instruction::ad(Opcode::LoadN, register_add(base, 1)?, 1));
        }

        let prep_index = self
            .builder
            .emit(Instruction::ad(Opcode::ForNPrep, base, 0));
        let mut frame = self.start_register_frame();
        self.bind_frame_local_with_debug(
            &mut frame,
            var.id.index(),
            value_register,
            var.name.as_str(),
            Some(self.builder.current_type_info_pc()),
        );
        self.start_typed_local_tag(
            TypeTag::Number as u16 as u8,
            value_register,
            loop_var_type_start,
        );
        self.next_register = register_add(value_register, 1)?;
        // Reserve the loop's control registers (`base`=limit … `value_register`=loop var)
        // for the body: a frame builder that would clobber one of these anonymous registers
        // must relocate above the watermark. `value_register` is also an active local, so
        // it is doubly covered; `base`/`base+1` (limit/step) are covered only here.
        self.reserved_ranges
            .push((base, register_add(value_register, 1)?));
        let body_start_word = self.builder.current_word_offset();
        if mutable_loop_var {
            self.builder.emit(Instruction::abc(
                Opcode::Move,
                value_register,
                index_register,
                0,
            ));
        }
        self.loop_stack.push(LoopContext {
            break_jumps: Vec::new(),
            continue_jumps: Vec::new(),
            local_offset: frame.active_local_start,
            local_offset_continue: frame.active_local_start,
            continue_used: false,
            continue_target: None,
            continue_exits_loop: false,
            return_on_break: false,
        });
        self.compile_stat_tail(body, false)?;
        self.reserved_ranges.pop();
        let context = self.loop_stack.pop().expect("numeric for context exists");
        self.close_locals_from(frame.active_local_start);
        if let Some(location) = location {
            self.builder.set_debug_line(location.begin.line + 1);
        }
        self.patch_jumps_to_current(context.continue_jumps)?;
        self.close_typed_locals_from(frame.active_typed_local_start);
        self.close_debug_locals_from(frame.active_local_start);
        let loop_word = self.builder.current_word_offset();
        let loop_offset = loop_offset(body_start_word, loop_word)?;
        self.builder
            .emit(Instruction::ad(Opcode::ForNLoop, base, loop_offset));
        let prep_word = self.builder.instruction_word_offset(prep_index);
        let prep_offset = numeric_for_prep_offset(prep_word, loop_word)?;
        self.builder
            .patch_ad(prep_index, Opcode::ForNPrep, base, prep_offset);
        self.patch_jumps_to_current(context.break_jumps)?;
        self.clear_dead_locals_from(frame.active_local_start);
        self.restore_register_frame(frame);
        self.builder.set_max_stack_size(register_add(base, 3)?);
        self.update_after_statement_location(location);
        Ok(())
    }

    pub(super) fn compile_unrolled_for(
        &mut self,
        var: &ruau_ast::syntax::Local,
        body: &Stat,
        location: Option<Location>,
        plan: LoopUnrollPlan,
    ) -> Result<(), CompileError> {
        let local_id = var.id.index();
        let previous_constant = self.local_values.constant(local_id);
        self.loop_stack.push(LoopContext {
            break_jumps: Vec::new(),
            continue_jumps: Vec::new(),
            local_offset: self.active_locals.len(),
            local_offset_continue: self.active_locals.len(),
            continue_used: false,
            continue_target: None,
            continue_exits_loop: false,
            return_on_break: false,
        });

        for index in 0..plan.trip_count {
            self.local_values.set_constant(
                local_id,
                Some(ConstantValue::Number(
                    plan.from + f64::from(index) * plan.step,
                )),
            );
            let continue_start = self
                .loop_stack
                .last()
                .expect("unrolled loop context exists")
                .continue_jumps
                .len();
            self.compile_stat_tail(body, false)?;
            let continue_jumps = self
                .loop_stack
                .last_mut()
                .expect("unrolled loop context exists")
                .continue_jumps
                .split_off(continue_start);
            self.patch_jumps_to_current(continue_jumps)?;
        }

        let context = self.loop_stack.pop().expect("unrolled loop context exists");
        self.patch_jumps_to_current(context.break_jumps)?;
        self.local_values.set_constant(local_id, previous_constant);
        self.update_after_statement_location(location);
        Ok(())
    }

    pub(super) fn compile_for_in_statement(
        &mut self,
        vars: &[ruau_ast::syntax::Local],
        values: &[Expr],
        body: &Stat,
        location: Option<Location>,
    ) -> Result<(), CompileError> {
        self.builder.set_proto_flags(0);
        let base = self.next_register;
        let prep_opcode = self.generic_for_prep_opcode(values);
        self.compile_for_in_values(values, base)?;

        let prep_index = self.builder.emit(Instruction::ad(prep_opcode, base, 0));
        let mut frame = self.start_register_frame();
        for (index, var) in vars.iter().enumerate() {
            self.bind_frame_local_with_debug(
                &mut frame,
                var.id.index(),
                register_at(register_add(base, 3)?, index, "generic for variable index")?,
                var.name.as_str(),
                Some(self.builder.current_type_info_pc()),
            );
        }
        // The iterator frame is `base`=generator, `base+1`=state, `base+2`=control, then the
        // loop variables at `base+3`… `FORGLOOP` writes at least two variable slots even for a
        // single-variable loop, so the watermark sits at `base + 3 + max(vars, 2)`. Hard-coding
        // `base + 5` exposed the third variable of a 3+-variable loop at the watermark, where a
        // relocated frame would land on it.
        let var_count = bytecode_u8_count("generic for variable", vars.len())?;
        let var_slots = var_count.max(2);
        self.next_register = register_add(register_add(base, 3)?, var_slots)?;
        self.builder
            .set_max_stack_size(register_add(register_add(base, 3)?, var_slots)?);
        // Reserve the iterator's control registers (`base`=generator, `base+1`=state,
        // `base+2`=control) and loop variables for the body, like the numeric `for`: a frame
        // builder must relocate rather than clobber them.
        self.reserved_ranges.push((base, self.next_register));
        let body_start_word = self.builder.current_word_offset();
        self.loop_stack.push(LoopContext {
            break_jumps: Vec::new(),
            continue_jumps: Vec::new(),
            local_offset: frame.active_local_start,
            local_offset_continue: frame.active_local_start,
            continue_used: false,
            continue_target: None,
            continue_exits_loop: false,
            return_on_break: false,
        });
        self.compile_stat_tail(body, false)?;
        self.reserved_ranges.pop();
        let context = self.loop_stack.pop().expect("generic for context exists");
        self.close_locals_from(frame.active_local_start);
        if let Some(location) = location {
            self.builder.set_debug_line(location.begin.line + 1);
        }
        self.patch_jumps_to_current(context.continue_jumps)?;
        self.close_typed_locals_from(frame.active_typed_local_start);
        self.close_debug_locals_from(frame.active_local_start);
        let loop_word = self.builder.current_word_offset();
        let loop_offset = loop_offset(body_start_word, loop_word)?;
        self.builder.emit(ad_with_aux(
            Opcode::ForGLoop,
            base,
            loop_offset,
            Some(forgloop_aux(prep_opcode, vars.len())?),
        ));
        let prep_word = self.builder.instruction_word_offset(prep_index);
        let prep_offset = generic_for_prep_offset(prep_word, loop_word)?;
        self.builder
            .patch_ad(prep_index, prep_opcode, base, prep_offset);
        self.patch_jumps_to_current(context.break_jumps)?;
        self.clear_dead_locals_from(frame.active_local_start);
        self.restore_register_frame(frame);
        self.update_after_statement_location(location);
        Ok(())
    }

    pub(super) fn compile_for_in_values(
        &mut self,
        values: &[Expr],
        base: u8,
    ) -> Result<(), CompileError> {
        if let [
            Expr::Call {
                func,
                args,
                is_self: false,
                ..
            },
        ] = values
        {
            self.compile_iterator_call_to(func, args, base)?;
            return Ok(());
        }

        // The generic-for iterator state occupies exactly three registers
        // (generator, state, control). Compile the first three values into
        // them, padding with nil when fewer are given. Lua adjusts the `in`
        // list to that triple, so any surplus values are still evaluated for
        // their side effects but their results are discarded — evaluate them
        // into the scratch slot above the state (`base + 3`, free until the
        // loop variables bind), which never clobbers the iterator triple.
        let kept = values.len().min(3);
        for (index, value) in values.iter().take(kept).enumerate() {
            self.compile_expr_to(value, register_at(base, index, "generic for value index")?)?;
        }
        for index in values.len()..3 {
            self.builder.emit(Instruction::abc(
                Opcode::LoadNil,
                register_at(base, index, "generic for value index")?,
                0,
                0,
            ));
        }
        if values.len() > 3 {
            let scratch = register_add(base, 3)?;
            for value in &values[3..] {
                self.compile_expr_to(value, scratch)?;
            }
            self.builder.set_max_stack_size(register_add(base, 4)?);
        } else {
            self.builder.set_max_stack_size(register_add(base, 3)?);
        }
        Ok(())
    }

    pub(super) fn compile_iterator_call_to(
        &mut self,
        func: &Expr,
        args: &[Expr],
        register: u8,
    ) -> Result<(), CompileError> {
        if let Some(path) = self.direct_import_path(func) {
            self.compile_import_path(&path, register)?;
        } else {
            self.compile_expr_to(func, register)?;
        }
        let multret_tail = self.compile_call_args(args, register_add(register, 1)?)?;
        let arg_operand = if multret_tail {
            0
        } else {
            bytecode_count_operand("generic for iterator argument", args.len())?
        };
        self.builder.set_max_stack_size(register_add(
            register,
            bytecode_count_operand("generic for iterator argument", args.len())?,
        )?);
        self.emit_call_instruction(register, arg_operand, CallResults::Fixed(3), multret_tail);
        Ok(())
    }

    pub(super) fn generic_for_prep_opcode(&self, values: &[Expr]) -> Opcode {
        match values {
            [Expr::Call { func, .. }] => {
                if matches!(self.import_path(func).as_deref(), Some([name]) if name == "ipairs") {
                    Opcode::ForGPrepInext
                } else if matches!(
                    self.direct_import_path(func).as_deref(),
                    Some([name]) if name == "pairs"
                ) {
                    Opcode::ForGPrepNext
                } else {
                    Opcode::ForGPrep
                }
            }
            [func, ..] if matches!(self.direct_import_path(func).as_deref(), Some([name]) if name == "next") => {
                Opcode::ForGPrepNext
            }
            _ => Opcode::ForGPrep,
        }
    }

    pub(super) fn emit_while_true_break_to_current_target(&mut self) -> Vec<PendingJump> {
        vec![
            self.emit_jump_placeholder(Opcode::Jump),
            self.emit_jump_placeholder(Opcode::JumpBack),
        ]
    }

    pub(super) fn emit_jump_placeholder(&mut self, opcode: Opcode) -> PendingJump {
        let index = self.builder.emit(Instruction::ad(opcode, 0, 0));
        PendingJump::Ad {
            index,
            opcode,
            register: 0,
        }
    }

    pub(super) fn emit_jump_to_word(
        &mut self,
        opcode: Opcode,
        target: u32,
    ) -> Result<(), CompileError> {
        let source = self.builder.current_word_offset();
        let offset = i32::try_from(target).expect("bytecode word offset fits i32")
            - i32::try_from(source).expect("bytecode word offset fits i32")
            - 1;
        let offset = i16::try_from(offset)
            .map_err(|_| CompileError::new(format!("jump offset {offset} overflows i16")))?;
        self.builder.emit(Instruction::ad(opcode, 0, offset));
        Ok(())
    }

    pub(super) fn append_break_jumps(
        &mut self,
        jumps: Vec<PendingJump>,
    ) -> Result<(), CompileError> {
        let Some(context) = self.loop_stack.last_mut() else {
            return Err(CompileError::new("break target outside loop"));
        };
        context.break_jumps.extend(jumps);
        Ok(())
    }

    pub(super) fn append_continue_jumps(
        &mut self,
        jumps: Vec<PendingJump>,
    ) -> Result<(), CompileError> {
        let Some(context) = self.loop_stack.last_mut() else {
            return Err(CompileError::new("continue target outside loop"));
        };
        context.continue_used = true;
        context.continue_jumps.extend(jumps);
        Ok(())
    }

    pub(super) fn update_after_statement_location(&mut self, location: Option<Location>) {
        if let Some(location) = location {
            self.builder.set_debug_line(location.end.line + 1);
            self.builder
                .set_implicit_return_line_base(location.end.line + 1);
        }
    }

    pub(super) fn emit_condition_jumps(
        &mut self,
        condition: &Expr,
        jump_when_truthy: bool,
    ) -> Result<Vec<PendingJump>, CompileError> {
        self.emit_condition_jumps_at(condition, jump_when_truthy, self.next_register)
    }

    /// Emits a pending JumpIf/JumpIfNot on `register` for one branch sense.
    pub(super) fn emit_truthiness_jump(
        &mut self,
        register: u8,
        jump_when_truthy: bool,
    ) -> PendingJump {
        let opcode = if jump_when_truthy {
            Opcode::JumpIf
        } else {
            Opcode::JumpIfNot
        };
        let index = self.builder.emit(Instruction::ad(opcode, register, 0));
        PendingJump::Ad {
            index,
            opcode,
            register,
        }
    }

    pub(super) fn emit_condition_jumps_at(
        &mut self,
        condition: &Expr,
        jump_when_truthy: bool,
        scratch_register: u8,
    ) -> Result<Vec<PendingJump>, CompileError> {
        if self.context.optimization_level() > 0
            && let Some(value) = self.constant_value_expr(condition)?
        {
            return if constant_truthiness(&value) == jump_when_truthy {
                Ok(vec![self.emit_jump_placeholder(Opcode::Jump)])
            } else {
                Ok(Vec::new())
            };
        }

        match condition {
            Expr::Unary {
                op: UnaryOp::Not,
                expr,
                ..
            } => self.emit_condition_jumps_at(expr, !jump_when_truthy, scratch_register),
            Expr::Group { expr, .. }
            | Expr::TypeAssertion { expr, .. }
            | Expr::Instantiate { expr, .. } => {
                self.emit_condition_jumps_at(expr, jump_when_truthy, scratch_register)
            }
            Expr::Binary {
                op: BinaryOp::And,
                left,
                right,
                ..
            } => {
                if let Some(right_truthy) = self.optimized_condition_truthiness_expr(right)? {
                    return if jump_when_truthy {
                        if right_truthy {
                            self.emit_condition_jumps_at(left, true, scratch_register)
                        } else {
                            let left_false_jumps =
                                self.emit_condition_jumps_at(left, false, scratch_register)?;
                            self.patch_jumps_to_current(left_false_jumps)?;
                            Ok(Vec::new())
                        }
                    } else if right_truthy {
                        self.emit_condition_jumps_at(left, false, scratch_register)
                    } else {
                        let mut jumps =
                            self.emit_condition_jumps_at(left, false, scratch_register)?;
                        jumps.push(self.emit_jump_placeholder(Opcode::Jump));
                        Ok(jumps)
                    };
                }
                if jump_when_truthy {
                    let left_false_jumps =
                        self.emit_condition_jumps_at(left, false, scratch_register)?;
                    let truthy_jumps =
                        self.emit_condition_jumps_at(right, true, scratch_register)?;
                    self.patch_jumps_to_current(left_false_jumps)?;
                    Ok(truthy_jumps)
                } else {
                    let mut jumps = self.emit_condition_jumps_at(left, false, scratch_register)?;
                    jumps.extend(self.emit_condition_jumps_at(right, false, scratch_register)?);
                    Ok(jumps)
                }
            }
            Expr::Binary {
                op: BinaryOp::Or,
                left,
                right,
                ..
            } => {
                if let Some(right_truthy) = self.optimized_condition_truthiness_expr(right)? {
                    return if jump_when_truthy {
                        if right_truthy {
                            let mut jumps =
                                self.emit_condition_jumps_at(left, true, scratch_register)?;
                            jumps.push(self.emit_jump_placeholder(Opcode::Jump));
                            Ok(jumps)
                        } else {
                            self.emit_condition_jumps_at(left, true, scratch_register)
                        }
                    } else if right_truthy {
                        let left_truthy_jumps =
                            self.emit_condition_jumps_at(left, true, scratch_register)?;
                        self.patch_jumps_to_current(left_truthy_jumps)?;
                        Ok(Vec::new())
                    } else {
                        self.emit_condition_jumps_at(left, false, scratch_register)
                    };
                }
                if jump_when_truthy {
                    let mut jumps = self.emit_condition_jumps_at(left, true, scratch_register)?;
                    jumps.extend(self.emit_condition_jumps_at(right, true, scratch_register)?);
                    Ok(jumps)
                } else {
                    let left_truthy_jumps =
                        self.emit_condition_jumps_at(left, true, scratch_register)?;
                    let falsey_jumps =
                        self.emit_condition_jumps_at(right, false, scratch_register)?;
                    self.patch_jumps_to_current(left_truthy_jumps)?;
                    Ok(falsey_jumps)
                }
            }
            Expr::Binary {
                op, left, right, ..
            } if comparison_jump_opcode(*op, jump_when_truthy).is_some() => {
                if let Some(jump) = self.emit_jumpx_constant_compare_at(
                    *op,
                    left,
                    right,
                    scratch_register,
                    jump_when_truthy,
                )? {
                    return Ok(vec![jump]);
                }
                let (opcode, left, right) = self.comparison_jump_operands(
                    *op,
                    left,
                    right,
                    jump_when_truthy,
                    scratch_register,
                )?;
                let index = self.builder.emit(Instruction::abc_with_aux(
                    opcode,
                    left,
                    0,
                    0,
                    Some(u32::from(right)),
                ));
                Ok(vec![PendingJump::Compare {
                    index,
                    opcode,
                    left,
                    right,
                }])
            }
            Expr::Local { local, .. } => {
                let register = self.local_source_register(local, scratch_register)?;
                Ok(vec![self.emit_truthiness_jump(register, jump_when_truthy)])
            }
            Expr::Global { name, .. } => {
                let register = scratch_register;
                self.compile_global_load(name.as_str(), register);
                self.builder.set_max_stack_size(register_add(register, 1)?);
                Ok(vec![self.emit_truthiness_jump(register, jump_when_truthy)])
            }
            _ => {
                let register = scratch_register;
                self.compile_expr_to(condition, register)?;
                Ok(vec![self.emit_truthiness_jump(register, jump_when_truthy)])
            }
        }
    }

    pub(super) fn compile_condition_value(
        &mut self,
        condition: &Expr,
        target: Option<u8>,
        scratch_register: u8,
        skip_jumps: &mut Vec<PendingJump>,
        only_truth: bool,
    ) -> Result<(), CompileError> {
        if self.context.optimization_level() > 0
            && let Some(value) = self.constant_value_expr(condition)?
        {
            if constant_truthiness(&value) == only_truth {
                if let Some(target) = target {
                    self.compile_expr_to(condition, target)?;
                }
                skip_jumps.push(self.emit_jump_placeholder(Opcode::Jump));
            }
            return Ok(());
        }

        match condition {
            Expr::Binary {
                op: op @ (BinaryOp::And | BinaryOp::Or),
                left,
                right,
                ..
            } => {
                let same_truth_branch = only_truth == matches!(op, BinaryOp::And);
                if same_truth_branch {
                    let mut else_jumps = Vec::new();
                    let nested_scratch = target.map_or(scratch_register, |target| {
                        scratch_register.max(target.saturating_add(1))
                    });
                    self.compile_condition_value(
                        left,
                        None,
                        nested_scratch,
                        &mut else_jumps,
                        !only_truth,
                    )?;
                    self.compile_condition_value(
                        right,
                        target,
                        scratch_register,
                        skip_jumps,
                        only_truth,
                    )?;
                    self.patch_jumps_to_current(else_jumps)?;
                } else {
                    self.compile_condition_value(
                        left,
                        target,
                        scratch_register,
                        skip_jumps,
                        only_truth,
                    )?;
                    self.compile_condition_value(
                        right,
                        target,
                        scratch_register,
                        skip_jumps,
                        only_truth,
                    )?;
                }
                Ok(())
            }
            Expr::Binary {
                op, left, right, ..
            } if comparison_jump_opcode(*op, only_truth).is_some() => {
                if let Some(target) = target {
                    self.builder.emit(Instruction::abc(
                        Opcode::LoadB,
                        target,
                        u8::from(only_truth),
                        0,
                    ));
                }
                let scratch_register = target.map_or(scratch_register, |target| {
                    scratch_register.max(target.saturating_add(1))
                });
                if let Some(jump) = self.emit_jumpx_constant_compare_at(
                    *op,
                    left,
                    right,
                    scratch_register,
                    only_truth,
                )? {
                    skip_jumps.push(jump);
                    return Ok(());
                }
                let (opcode, left, right) =
                    self.comparison_jump_operands(*op, left, right, only_truth, scratch_register)?;
                let index = self.builder.emit(Instruction::abc_with_aux(
                    opcode,
                    left,
                    0,
                    0,
                    Some(u32::from(right)),
                ));
                skip_jumps.push(PendingJump::Compare {
                    index,
                    opcode,
                    left,
                    right,
                });
                Ok(())
            }
            Expr::Unary {
                op: UnaryOp::Not,
                expr,
                ..
            } if target.is_none() => {
                self.compile_condition_value(expr, None, scratch_register, skip_jumps, !only_truth)
            }
            Expr::Group { expr, .. }
            | Expr::TypeAssertion { expr, .. }
            | Expr::Instantiate { expr, .. } => {
                self.compile_condition_value(expr, target, scratch_register, skip_jumps, only_truth)
            }
            _ => {
                let register = if let Some(target) = target {
                    self.compile_expr_to(condition, target)?;
                    target
                } else {
                    self.condition_value_register(condition, scratch_register)?
                };
                let opcode = if only_truth {
                    Opcode::JumpIf
                } else {
                    Opcode::JumpIfNot
                };
                let index = self.builder.emit(Instruction::ad(opcode, register, 0));
                skip_jumps.push(PendingJump::Ad {
                    index,
                    opcode,
                    register,
                });
                Ok(())
            }
        }
    }

    pub(super) fn condition_value_register(
        &mut self,
        expr: &Expr,
        fallback_register: u8,
    ) -> Result<u8, CompileError> {
        if let Some(register) = self.local_expr_register(expr)? {
            return Ok(register);
        }
        self.compile_expr_to(expr, fallback_register)?;
        Ok(fallback_register)
    }

    pub(super) fn comparison_jump_operands(
        &mut self,
        op: BinaryOp,
        left: &Expr,
        right: &Expr,
        jump_when_truthy: bool,
        scratch_register: u8,
    ) -> Result<(Opcode, u8, u8), CompileError> {
        let opcode = comparison_jump_opcode(op, jump_when_truthy).expect("comparison opcode");
        let left = self.condition_operand_register(left, scratch_register)?;
        let right = self.condition_operand_register(right, scratch_register.max(left + 1))?;
        let (left, right) = match op {
            BinaryOp::CompareGt | BinaryOp::CompareGe => (right, left),
            _ => (left, right),
        };
        Ok((opcode, left, right))
    }

    pub(super) fn condition_operand_register(
        &mut self,
        expr: &Expr,
        fallback_register: u8,
    ) -> Result<u8, CompileError> {
        match expr {
            Expr::Local { local, .. } => self.local_source_register(local, fallback_register),
            Expr::Group { expr, .. }
            | Expr::TypeAssertion { expr, .. }
            | Expr::Instantiate { expr, .. } => {
                self.condition_operand_register(expr, fallback_register)
            }
            _ => {
                self.compile_expr_to(expr, fallback_register)?;
                Ok(fallback_register)
            }
        }
    }

    pub(super) fn patch_jumps_to_current(
        &mut self,
        jumps: Vec<PendingJump>,
    ) -> Result<(), CompileError> {
        for jump in jumps {
            self.patch_jump_to_current(&jump)?;
        }
        Ok(())
    }

    pub(super) fn patch_jump_to_current(&mut self, jump: &PendingJump) -> Result<(), CompileError> {
        let index = jump.index();
        let source = self.builder.instruction_word_offset(index);
        let target = self.builder.current_word_offset();
        let offset = i32::try_from(target).expect("bytecode word offset fits i32")
            - i32::try_from(source).expect("bytecode word offset fits i32")
            - 1;
        let offset = i16::try_from(offset)
            .map_err(|_| CompileError::new(format!("jump offset {offset} overflows i16")))?;
        match *jump {
            PendingJump::Ad {
                index,
                opcode,
                register,
            } => self.builder.patch_ad(index, opcode, register, offset),
            PendingJump::AdWithAux {
                index,
                opcode,
                register,
                aux,
            } => self
                .builder
                .patch_ad_with_aux(index, opcode, register, offset, aux),
            PendingJump::Compare {
                index,
                opcode,
                left,
                right,
            } => {
                self.builder
                    .patch_ad_with_aux(index, opcode, left, offset, Some(u32::from(right)))
            }
        }
        Ok(())
    }
}
