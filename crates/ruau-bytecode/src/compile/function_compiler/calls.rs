use super::*;

impl FunctionCompiler {
    pub(super) fn compile_call_to(
        &mut self,
        expr: &Expr,
        register: u8,
        results: CallResults,
    ) -> Result<(), CompileError> {
        let Expr::Call {
            syntax_id,
            func,
            args,
            is_self,
            ..
        } = expr
        else {
            return Err(CompileError::new(format!(
                "minimal bytecode compiler expected call expression: {expr:?}"
            )));
        };

        self.set_expr_debug_line(expr);
        for arg in args {
            self.clear_table_function_escape(arg);
        }

        // A call lays its frame from the result register up: `func@register`, then the
        // arguments (a method call moves `self` first) at `register+1`… If that frame would
        // cross a register reserved by an enclosing `for` loop's control state, it would
        // clobber the loop bound; build the frame above the watermark and move the single
        // result back, the way upstream runs a call in fresh `regTop` registers and `MOVE`s.
        // Gated on a real overlap, so ordinary calls (whose frame sits at the stack top) are
        // never perturbed and the exact-bytecode differential stays clean.
        let arg_span = bytecode_u8_count("call argument", args.len() + usize::from(*is_self))?;
        let frame_lo = register_add(register, 1)?;
        // A multret tail (last argument a call or `...`) makes the frame open-ended: its results
        // grow upward from the final argument slot at runtime, so the syntactic `arg_span`
        // undercounts. Extend the overlap extent to the watermark, which bounds every reserved
        // register the growing frame could reach.
        let multret_tail = args.last().is_some_and(|arg| {
            call_uses_multret(arg) && self.analysis_constant_value_expr(arg).is_none()
        });
        let frame_hi = if multret_tail {
            self.next_register.max(frame_lo.saturating_add(arg_span))
        } else {
            frame_lo.saturating_add(arg_span)
        };
        if matches!(results, CallResults::Fixed(1))
            && arg_span > 0
            && self.overlaps_reserved(frame_lo, frame_hi)
        {
            let frame = self.next_register;
            self.compile_call_to(expr, frame, results)?;
            self.builder
                .emit(Instruction::abc(Opcode::Move, register, frame, 0));
            self.builder
                .set_max_stack_size(register.max(frame).saturating_add(1));
            return Ok(());
        }

        if *is_self {
            return self.compile_namecall_to(func, args, register, results);
        }

        if self.try_compile_inlined_call(func, args, register, results, InlineCallMode::Value)? {
            return Ok(());
        }

        if self.compile_fastcall2k_import(*syntax_id, func, args, register, results)? {
            return Ok(());
        }
        if self.compile_fastcall_import(*syntax_id, func, args, register, results)? {
            return Ok(());
        }

        self.compile_call_func_to(func, register)?;
        let multret_tail = self.compile_call_args(args, register_add(register, 1)?)?;
        let arg_operand = if multret_tail {
            0
        } else {
            bytecode_count_operand("call argument", args.len())?
        };
        self.builder.set_max_stack_size(register_add(
            register,
            bytecode_count_operand("call argument", args.len())?,
        )?);
        self.set_expr_end_debug_line(func);
        self.emit_call_instruction(register, arg_operand, results, multret_tail);
        Ok(())
    }

    pub(super) fn emit_call_instruction(
        &mut self,
        register: u8,
        arg_operand: u8,
        results: CallResults,
        multret_tail: bool,
    ) {
        if self.context.bytecode_version() >= 11
            && self.current_function_depth != 0
            && !multret_tail
            && results != CallResults::Multret
        {
            let pc = self.builder.current_word_offset();
            let slot = self.builder.push_feedback_slot(FeedbackSlot {
                kind: FeedbackType::CallTarget,
                pc,
            });
            self.builder.emit(Instruction::abc_with_aux(
                Opcode::CallFb,
                register,
                arg_operand,
                results.operand(),
                Some(slot),
            ));
        } else {
            self.builder.emit(Instruction::abc(
                Opcode::Call,
                register,
                arg_operand,
                results.operand(),
            ));
        }
    }

    pub(super) fn try_compile_inlined_call(
        &mut self,
        func: &Expr,
        args: &[Expr],
        register: u8,
        results: CallResults,
        mode: InlineCallMode,
    ) -> Result<bool, CompileError> {
        let target_count = match results {
            CallResults::None => 0,
            CallResults::Fixed(target_count) => target_count,
            CallResults::Multret => return Ok(false),
        };
        if self.context.optimization_level() < 2
            || self.context.getfenv_used()
            || self.context.setfenv_used()
        {
            return Ok(false);
        }

        let direct_function_expr = matches!(ungroup_expr(func), Expr::Function { .. });
        let Some((function_id, function)) = self.inlinable_function_expr(func) else {
            return Ok(false);
        };
        if self.inline_stack.contains(&function_id) {
            return Ok(false);
        }
        if self.function_stack.contains(&function_id) {
            return Ok(false);
        }
        if self.current_function_captures_function(function_id) {
            return Ok(false);
        }
        let inline_depth = self
            .context
            .options()
            .fast_int("LuauCompileInlineDepth")
            .max(0);
        if self.inline_stack.len() >= inline_depth as usize {
            return Ok(false);
        }

        let Some(info) = self.context.functions.get(function_id) else {
            return Ok(false);
        };
        let proto = info.proto();
        let has_upvalues = proto.is_some_and(|proto| proto.upvalue_count() > 0)
            || proto.is_none() && !info.upvalues().is_empty();
        let upvalues_available = info.upvalues().iter().all(|upvalue| {
            let local_id = upvalue.local_id();
            self.local_registers.contains_key(&local_id)
                || self.local_constant(local_id).is_some()
                || self
                    .context
                    .local_constant(LocalId::new(local_id))
                    .is_some()
                || upvalue.function_depth() < self.current_function_depth
        });
        if !info.syntactic_inline_candidate()
            || (has_upvalues && !direct_function_expr && !upvalues_available)
            || usize::from(proto.map_or(0, |proto| proto.stack_size())) > 32
        {
            return Ok(false);
        }

        let Expr::Function {
            attributes,
            args: params,
            body,
            ..
        } = &*function
        else {
            return Ok(false);
        };
        if attributes
            .iter()
            .any(|attribute| attribute.name.as_str().ends_with("noinline"))
        {
            return Ok(false);
        }

        let param_constants = self.inline_param_constants(params, args)?;
        if !self.inline_body_supported(body, &param_constants, function_id)? {
            return Ok(false);
        }
        if !self.inline_cost_allows(params, args, body)? {
            return Ok(false);
        }

        let current_upvalues = info
            .upvalues()
            .iter()
            .filter(|upvalue| {
                upvalue.function_depth() == self.current_function_depth
                    && self
                        .context
                        .variable(LocalId::new(upvalue.local_id()))
                        .is_some_and(|variable| variable.is_written())
            })
            .map(|upvalue| upvalue.local_id())
            .collect::<Vec<_>>();
        let saved_local_values = self.local_values.clone();
        let saved_inline_function_args = self.inline_function_args.clone();
        let mut frame = self.start_register_frame();
        self.next_register = self
            .next_register
            .max(register_add(register, target_count)?);

        let compile_result = (|| -> Result<(), CompileError> {
            for local_id in current_upvalues {
                self.mark_local_captured(local_id);
            }
            self.bind_inline_args(&mut frame, params, args, register, target_count)?;
            self.inline_stack.push(function_id);
            self.compile_inlined_body(body, register, target_count, mode)?;
            if mode == InlineCallMode::Value {
                self.close_locals_from(frame.active_local_start);
            }
            self.inline_stack.pop();
            Ok(())
        })();

        if self.inline_stack.last().copied() == Some(function_id) {
            self.inline_stack.pop();
        }
        self.restore_register_frame(frame);
        self.local_values = saved_local_values;
        self.inline_function_args = saved_inline_function_args;
        compile_result?;
        Ok(true)
    }

    pub(super) fn inlinable_function_expr(&self, func: &Expr) -> Option<(FunctionId, Rc<Expr>)> {
        match func {
            Expr::Function { syntax_id, .. } => {
                let id = FunctionId::new(*syntax_id);
                self.context
                    .functions
                    .expr(id)
                    .cloned()
                    .map(|expr| (id, expr))
            }
            Expr::Local { local, .. } => {
                if !self.local_is_written(local.id.index())
                    && let Some(function_id) = self.inline_function_args.get(&local.id.index())
                {
                    return self
                        .context
                        .functions
                        .expr(*function_id)
                        .cloned()
                        .map(|expr| (*function_id, expr));
                }
                let variable = self.context.variable(local.id)?;
                if variable.is_written() {
                    return None;
                }
                let id = FunctionId::new(variable.initial_expr()?);
                self.context
                    .functions
                    .expr(id)
                    .cloned()
                    .map(|expr| (id, expr))
            }
            Expr::IndexName { .. } => {
                let id = self.table_member_function_id(func)?;
                self.context
                    .functions
                    .expr(id)
                    .cloned()
                    .map(|expr| (id, expr))
            }
            Expr::Group { expr, .. }
            | Expr::TypeAssertion { expr, .. }
            | Expr::Instantiate { expr, .. } => self.inlinable_function_expr(expr),
            _ => None,
        }
    }

    pub(super) fn inline_param_constants(
        &self,
        params: &[ruau_ast::syntax::Local],
        args: &[Expr],
    ) -> Result<BTreeMap<u32, ConstantValue>, CompileError> {
        let mut constants = BTreeMap::new();
        for (index, param) in params.iter().enumerate() {
            let local_id = param.id.index();
            if self.local_is_written(local_id) {
                continue;
            }
            let Some(arg) = args.get(index) else {
                constants.insert(local_id, ConstantValue::Nil);
                continue;
            };
            if index + 1 == args.len() && params.len() > args.len() && call_uses_multret(arg) {
                continue;
            }
            if let Some(value) = self.constant_value_expr(arg)? {
                constants.insert(local_id, value);
            }
        }
        Ok(constants)
    }

    pub(super) fn bind_inline_args(
        &mut self,
        frame: &mut RegisterFrame,
        params: &[ruau_ast::syntax::Local],
        args: &[Expr],
        register: u8,
        target_count: u8,
    ) -> Result<(), CompileError> {
        let mut index = 0usize;
        while index < params.len() {
            let param = &params[index];
            let local_id = param.id.index();
            let arg = args.get(index);
            let captured_by_nested_function = self.local_captured_by_any_function(local_id);
            if !self.local_is_written(local_id) {
                let import_path = arg.and_then(|arg| self.import_path(arg));
                self.local_values.set_import_path(local_id, import_path);
                if let Some((function_id, _)) =
                    arg.and_then(|arg| self.inlinable_function_expr(arg))
                {
                    self.inline_function_args.insert(local_id, function_id);
                } else {
                    self.inline_function_args.remove(&local_id);
                }
            }

            if let Some(arg) = arg
                && index + 1 == args.len()
                && params.len() > args.len()
                && call_uses_multret(arg)
                && self.constant_value_expr(arg)?.is_none()
            {
                let tail_count = u8::try_from(params.len() - index)
                    .map_err(|_| CompileError::new("too many inline call arguments"))?;
                let first = self.reserve_registers(tail_count)?;
                self.compile_expr_temp_n(arg, first, tail_count)?;
                for (offset, param) in params[index..].iter().enumerate() {
                    self.bind_frame_local(
                        frame,
                        param.id.index(),
                        register_at(first, offset, "inline call argument index")?,
                    );
                }
                index = params.len();
                break;
            }

            if captured_by_nested_function
                && !self.local_is_written(local_id)
                && let Some(arg) = arg
                && self.constant_value_expr(arg)?.is_none()
                && let Some(source) = self.reusable_inline_arg_register(arg)?
            {
                self.bind_frame_local(frame, local_id, source);
                index += 1;
                continue;
            }

            if !self.local_is_written(local_id) {
                let constant = match arg {
                    Some(arg) => self.constant_value_expr(arg)?,
                    None => Some(ConstantValue::Nil),
                };
                if let Some(constant) = constant {
                    self.local_values.set_constant(local_id, Some(constant));
                    index += 1;
                    continue;
                }
            }

            if !self.local_is_written(local_id)
                && !captured_by_nested_function
                && let Some(arg) = arg
                && let Some(source) = self.reusable_inline_arg_register(arg)?
            {
                self.bind_frame_local(frame, local_id, source);
                index += 1;
                continue;
            }

            let target = self.reserve_register()?;
            if let Some(arg) = arg {
                self.compile_expr_to(arg, target)?;
            } else {
                self.builder
                    .emit(Instruction::abc(Opcode::LoadNil, target, 0, 0));
            }
            self.bind_frame_local(frame, local_id, target);
            index += 1;
        }

        for extra in args.iter().skip(index) {
            self.next_register = self
                .next_register
                .max(register_add(register, target_count)?);
            self.compile_expr_side(extra)?;
        }
        Ok(())
    }

    pub(super) fn reusable_inline_arg_register(
        &self,
        arg: &Expr,
    ) -> Result<Option<u8>, CompileError> {
        match arg {
            Expr::Local { local, .. }
                if self
                    .context
                    .variable(local.id)
                    .is_some_and(|variable| variable.is_written()) =>
            {
                Ok(None)
            }
            Expr::Group { expr, .. }
            | Expr::TypeAssertion { expr, .. }
            | Expr::Instantiate { expr, .. } => self.reusable_inline_arg_register(expr),
            _ => self.local_expr_register(arg),
        }
    }

    pub(super) fn inline_body_supported(
        &self,
        body: &Stat,
        constants: &BTreeMap<u32, ConstantValue>,
        function_id: FunctionId,
    ) -> Result<bool, CompileError> {
        Ok(match body {
            Stat::Block { body, .. } => {
                let mut supported = true;
                for stat in body {
                    supported &= self.inline_body_supported(stat, constants, function_id)?;
                    if !supported || self.inline_stat_terminates_with_constants(stat, constants)? {
                        break;
                    }
                }
                supported
            }
            Stat::Return { list, .. } => self.inline_return_supported(list, function_id)?,
            Stat::Local { .. } => true,
            Stat::Assign { .. } => true,
            Stat::CompoundAssign { .. } => true,
            Stat::Expr { .. } => true,
            Stat::Break { .. } | Stat::Continue { .. } => true,
            Stat::For {
                var,
                from,
                to,
                step,
                body,
                ..
            } => {
                !self.local_is_written(var.id.index())
                    && self
                        .loop_unroll_plan_with_constants(
                            var,
                            from,
                            to,
                            step.as_deref(),
                            body,
                            Some(constants),
                        )?
                        .is_some()
                    && self.inline_body_supported(body, constants, function_id)?
            }
            Stat::While { body, .. } | Stat::Repeat { body, .. } => {
                self.inline_body_supported(body, constants, function_id)?
            }
            Stat::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                if let Some(condition) = self.inline_condition_truthiness(condition, constants)? {
                    if condition {
                        self.inline_body_supported(then_body, constants, function_id)?
                    } else {
                        else_body.as_deref().is_none_or(|else_body| {
                            self.inline_body_supported(else_body, constants, function_id)
                                .unwrap_or(false)
                        })
                    }
                } else {
                    if let Some(else_body) = else_body.as_deref() {
                        self.inline_body_supported(then_body, constants, function_id)?
                            && self.inline_body_supported(else_body, constants, function_id)?
                    } else {
                        self.inline_body_supported(then_body, constants, function_id)?
                    }
                }
            }
            _ => false,
        })
    }

    pub(in crate::compile) fn inline_stat_terminates_with_constants(
        &self,
        stat: &Stat,
        constants: &BTreeMap<u32, ConstantValue>,
    ) -> Result<bool, CompileError> {
        Ok(match stat {
            Stat::Block { body, .. } => {
                let mut terminates = false;
                for stat in body {
                    terminates = self.inline_stat_terminates_with_constants(stat, constants)?;
                    if terminates {
                        break;
                    }
                }
                terminates
            }
            Stat::Return { .. } | Stat::Break { .. } | Stat::Continue { .. } => true,
            Stat::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                if let Some(truthy) = self.inline_condition_truthiness(condition, constants)? {
                    if truthy {
                        self.inline_stat_terminates_with_constants(then_body, constants)?
                    } else if let Some(else_body) = else_body.as_deref() {
                        self.inline_stat_terminates_with_constants(else_body, constants)?
                    } else {
                        false
                    }
                } else if let Some(else_body) = else_body.as_deref() {
                    self.inline_stat_terminates_with_constants(then_body, constants)?
                        && self.inline_stat_terminates_with_constants(else_body, constants)?
                } else {
                    false
                }
            }
            _ => self.context.always_terminates(stat),
        })
    }

    pub(super) fn inline_return_supported(
        &self,
        values: &[Expr],
        function_id: FunctionId,
    ) -> Result<bool, CompileError> {
        let Some(value) = values.last() else {
            return Ok(true);
        };
        if !call_uses_multret(value) {
            return Ok(true);
        }
        let mut visited = BTreeSet::new();
        visited.insert(function_id);
        self.inline_multret_value_can_be_fixed(value, &mut visited)
    }

    pub(super) fn inline_multret_value_can_be_fixed(
        &self,
        value: &Expr,
        visited: &mut BTreeSet<FunctionId>,
    ) -> Result<bool, CompileError> {
        Ok(match value {
            Expr::Varargs { .. } => false,
            Expr::Call { func, is_self, .. } => {
                if self.return_call_results(func, *is_self) == CallResults::Fixed(1) {
                    return Ok(true);
                }
                if *is_self {
                    return Ok(false);
                }
                let Some((callee_id, function)) = self.inlinable_function_expr(func) else {
                    return Ok(false);
                };
                if !visited.insert(callee_id) {
                    return Ok(false);
                }
                let fixed = self.inline_function_returns_fixed(callee_id, &function, visited)?;
                visited.remove(&callee_id);
                fixed
            }
            Expr::Group { expr, .. }
            | Expr::TypeAssertion { expr, .. }
            | Expr::Instantiate { expr, .. } => {
                self.inline_multret_value_can_be_fixed(expr, visited)?
            }
            _ => true,
        })
    }

    pub(super) fn inline_function_returns_fixed(
        &self,
        function_id: FunctionId,
        function: &Expr,
        visited: &mut BTreeSet<FunctionId>,
    ) -> Result<bool, CompileError> {
        let Some(info) = self.context.functions.get(function_id) else {
            return Ok(false);
        };
        let proto = info.proto();
        let has_upvalues = proto.is_some_and(|proto| proto.upvalue_count() > 0)
            || proto.is_none() && !info.upvalues().is_empty();
        if !info.syntactic_inline_candidate()
            || has_upvalues
            || usize::from(proto.map_or(0, |proto| proto.stack_size())) > 32
        {
            return Ok(false);
        }

        let Expr::Function {
            attributes, body, ..
        } = function
        else {
            return Ok(false);
        };
        if attributes
            .iter()
            .any(|attribute| attribute.name.as_str().ends_with("noinline"))
        {
            return Ok(false);
        }

        self.inline_body_returns_fixed(body, visited)
    }

    pub(super) fn inline_body_returns_fixed(
        &self,
        body: &Stat,
        visited: &mut BTreeSet<FunctionId>,
    ) -> Result<bool, CompileError> {
        Ok(match body {
            Stat::Block { body, .. } => body.iter().all(|stat| {
                self.inline_body_returns_fixed(stat, visited)
                    .unwrap_or(false)
            }),
            Stat::Return { list, .. } => {
                let Some(value) = list.last() else {
                    return Ok(true);
                };
                !call_uses_multret(value)
                    || self.inline_multret_value_can_be_fixed(value, visited)?
            }
            Stat::Assign { .. } => true,
            Stat::Expr { .. } => true,
            Stat::If {
                then_body,
                else_body,
                ..
            } => {
                self.inline_body_returns_fixed(then_body, visited)?
                    && else_body.as_deref().is_none_or(|else_body| {
                        self.inline_body_returns_fixed(else_body, visited)
                            .unwrap_or(false)
                    })
            }
            _ => false,
        })
    }

    pub(super) fn inline_function_has_value_return(&self, func: &Expr) -> bool {
        let Some((_, function)) = self.inlinable_function_expr(func) else {
            return false;
        };
        let Expr::Function { body, .. } = &*function else {
            return false;
        };
        inline_body_has_value_return(body)
    }

    pub(super) fn inline_condition_truthiness(
        &self,
        expr: &Expr,
        constants: &BTreeMap<u32, ConstantValue>,
    ) -> Result<Option<bool>, CompileError> {
        Ok(self
            .inline_constant_value_expr(expr, constants)?
            .map(|value| constant_truthiness(&value)))
    }

    pub(in crate::compile) fn inline_constant_value_expr(
        &self,
        expr: &Expr,
        constants: &BTreeMap<u32, ConstantValue>,
    ) -> Result<Option<ConstantValue>, CompileError> {
        Ok(match expr {
            Expr::Local { local, .. } => constants
                .get(&local.id.index())
                .cloned()
                .or_else(|| self.local_constant(local.id.index()))
                .or_else(|| self.context.local_constant(local.id).cloned()),
            Expr::Group { expr, .. }
            | Expr::TypeAssertion { expr, .. }
            | Expr::Instantiate { expr, .. } => self.inline_constant_value_expr(expr, constants)?,
            Expr::Unary {
                op: UnaryOp::Not,
                expr,
                ..
            } => self
                .inline_constant_value_expr(expr, constants)?
                .map(|value| ConstantValue::Bool(!constant_truthiness(&value))),
            Expr::IfElse {
                condition,
                true_expr,
                false_expr,
                ..
            } => {
                let Some(truthy) = self.inline_condition_truthiness(condition, constants)? else {
                    return Ok(None);
                };
                self.inline_constant_value_expr(
                    if truthy { true_expr } else { false_expr },
                    constants,
                )?
            }
            Expr::Binary {
                op, left, right, ..
            } if comparison_jump_opcode(*op, true).is_some() => {
                let Some(left) = self.inline_constant_value_expr(left, constants)? else {
                    return Ok(None);
                };
                let Some(right) = self.inline_constant_value_expr(right, constants)? else {
                    return Ok(None);
                };
                compare_constant_values(*op, left, right)?.map(ConstantValue::Bool)
            }
            Expr::Binary {
                op, left, right, ..
            } if arithmetic_opcode(*op).is_some() => {
                let Some(left) = self.inline_constant_value_expr(left, constants)? else {
                    return Ok(None);
                };
                let Some(right) = self.inline_constant_value_expr(right, constants)? else {
                    return Ok(None);
                };
                constant_arithmetic_value(*op, &left, &right)?
            }
            Expr::Call {
                syntax_id, args, ..
            } => {
                if !self.fold_library_constants() {
                    return Ok(None);
                }
                let Some(builtin) = self
                    .context
                    .builtin_call(*syntax_id)
                    .map(|builtin| builtin.function_id())
                else {
                    return Ok(None);
                };
                let mut values = Vec::with_capacity(args.len());
                for arg in args {
                    values.push(self.inline_constant_value_expr(arg, constants)?);
                }
                fold_builtin_constant(builtin, &values)
            }
            Expr::InterpString {
                strings,
                expressions,
                ..
            } => {
                let mut folded = String::new();
                for (index, prefix) in strings.iter().enumerate() {
                    folded.push_str(prefix);
                    if let Some(expr) = expressions.get(index) {
                        let Some(value) = self.inline_constant_value_expr(expr, constants)? else {
                            return Ok(None);
                        };
                        push_constant_display(&mut folded, &value);
                    }
                }
                Some(ConstantValue::String(folded))
            }
            _ => self.static_constant_value_expr(expr)?,
        })
    }

    pub(super) fn compile_inlined_body(
        &mut self,
        body: &Stat,
        register: u8,
        target_count: u8,
        mode: InlineCallMode,
    ) -> Result<bool, CompileError> {
        self.compile_inlined_body_inner(body, register, target_count, mode, false)
    }

    pub(super) fn compile_inlined_body_inner(
        &mut self,
        body: &Stat,
        register: u8,
        target_count: u8,
        mode: InlineCallMode,
        scoped_block: bool,
    ) -> Result<bool, CompileError> {
        if !matches!(body, Stat::Block { .. })
            && let Some(line) = stat_line(body)
        {
            self.builder.set_debug_line(line);
        }
        match body {
            Stat::Block { body, .. } => {
                let scope = scoped_block.then(|| self.start_local_scope());
                let mut returned = false;
                for stat in body {
                    if self.compile_inlined_body_inner(stat, register, target_count, mode, true)? {
                        returned = true;
                        break;
                    }
                }
                if !returned {
                    self.emit_inline_fallthrough(register, target_count)?;
                    if mode == InlineCallMode::Return {
                        self.emit_return(
                            register,
                            bytecode_count_operand(
                                "inlined return target",
                                usize::from(target_count),
                            )?,
                        );
                    }
                }
                if let Some(scope) = scope {
                    self.finish_block_scope(scope);
                }
                Ok(returned)
            }
            Stat::Return { list, .. } => {
                if let Some(line) = stat_line(body) {
                    self.builder.set_debug_line(line);
                }
                self.emit_coverage();
                self.compile_inlined_return(list, register, target_count)?;
                if mode == InlineCallMode::Return {
                    self.emit_return(
                        register,
                        bytecode_count_operand("inlined return target", usize::from(target_count))?,
                    );
                }
                Ok(true)
            }
            Stat::Local { .. } => {
                self.compile_stat_tail(body, false)?;
                Ok(false)
            }
            Stat::Assign { .. } => {
                self.compile_stat_tail(body, false)?;
                Ok(false)
            }
            Stat::CompoundAssign { .. } => {
                self.compile_stat_tail(body, false)?;
                Ok(false)
            }
            Stat::Expr { .. } => {
                self.compile_stat_tail(body, false)?;
                Ok(false)
            }
            Stat::Break { .. } | Stat::Continue { .. } => {
                self.compile_stat_tail(body, false)?;
                Ok(false)
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
                let Some(plan) = self.loop_unroll_plan(var, from, to, step.as_deref(), body)?
                else {
                    return Err(CompileError::new(
                        "inlined numeric for loop was not unrollable",
                    ));
                };
                self.compile_unrolled_for(var, body, *location, plan)?;
                Ok(false)
            }
            Stat::While { .. } | Stat::Repeat { .. } => {
                self.compile_stat_tail(body, false)?;
                if let Some(line) = stat_line(body) {
                    self.builder.set_debug_line(line);
                }
                Ok(false)
            }
            Stat::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                if let Some(condition) = self.condition_truthiness_expr(condition)? {
                    if condition {
                        self.compile_inlined_body_inner(
                            then_body,
                            register,
                            target_count,
                            mode,
                            true,
                        )
                    } else if let Some(else_body) = else_body {
                        self.compile_inlined_body_inner(
                            else_body,
                            register,
                            target_count,
                            mode,
                            true,
                        )
                    } else {
                        Ok(false)
                    }
                } else if let Some(else_body) = else_body {
                    let false_jumps = self.emit_condition_jumps(condition, false)?;
                    let then_returned = self.compile_inlined_body_inner(
                        then_body,
                        register,
                        target_count,
                        mode,
                        true,
                    )?;
                    let then_exit = if mode == InlineCallMode::Value {
                        Some(self.emit_jump_placeholder(Opcode::Jump))
                    } else {
                        None
                    };
                    self.patch_jumps_to_current(false_jumps)?;
                    let else_returned = self.compile_inlined_body_inner(
                        else_body,
                        register,
                        target_count,
                        mode,
                        true,
                    )?;
                    let else_exit =
                        if mode == InlineCallMode::Value && else_returned && !then_returned {
                            Some(self.emit_jump_placeholder(Opcode::Jump))
                        } else {
                            None
                        };
                    if let Some(then_exit) = then_exit {
                        self.patch_jump_to_current(&then_exit)?;
                    }
                    if let Some(else_exit) = else_exit {
                        self.patch_jump_to_current(&else_exit)?;
                    }
                    Ok(then_returned && else_returned)
                } else {
                    let message = match mode {
                        InlineCallMode::Value => {
                            "dynamic inlined if without else is not value-supported"
                        }
                        InlineCallMode::Return => {
                            "dynamic inlined if without else is not return-supported"
                        }
                    };
                    if !stat_guarantees_return(then_body) {
                        let false_jumps = self.emit_condition_jumps(condition, false)?;
                        let then_returned = self.compile_inlined_body_inner(
                            then_body,
                            register,
                            target_count,
                            mode,
                            true,
                        )?;
                        if then_returned {
                            return Err(CompileError::new(message));
                        }
                        self.patch_jumps_to_current(false_jumps)?;
                        return Ok(false);
                    }

                    let false_jumps = self.emit_condition_jumps(condition, false)?;
                    let then_returned = self.compile_inlined_body_inner(
                        then_body,
                        register,
                        target_count,
                        mode,
                        true,
                    )?;
                    if !then_returned {
                        return Err(CompileError::new(message));
                    }
                    let redundant_fallthrough_close = self.trailing_close_upvals();
                    let then_exit = if mode == InlineCallMode::Value {
                        Some(self.emit_jump_placeholder(Opcode::Jump))
                    } else {
                        None
                    };
                    if let Some(close) = redundant_fallthrough_close {
                        self.builder.emit(close);
                    }
                    self.patch_jumps_to_current(false_jumps)?;
                    self.emit_inline_fallthrough(register, target_count)?;
                    if mode == InlineCallMode::Return {
                        self.emit_return(
                            register,
                            bytecode_count_operand(
                                "inlined return target",
                                usize::from(target_count),
                            )?,
                        );
                    }
                    if let Some(then_exit) = then_exit {
                        self.patch_jump_to_current(&then_exit)?;
                    }
                    Ok(true)
                }
            }
            _ => Err(CompileError::new(format!(
                "unsupported inlined function body: {body:?}"
            ))),
        }
    }

    pub(super) fn compile_inlined_return(
        &mut self,
        values: &[Expr],
        register: u8,
        target_count: u8,
    ) -> Result<(), CompileError> {
        let mut index = 0u8;
        while index < target_count {
            let Some(value) = values.get(index as usize) else {
                self.builder.emit(Instruction::abc(
                    Opcode::LoadNil,
                    register_add(register, index)?,
                    0,
                    0,
                ));
                index += 1;
                continue;
            };

            if index as usize + 1 == values.len()
                && call_uses_multret(value)
                && self.constant_value_expr(value)?.is_none()
            {
                self.compile_expr_temp_n(
                    value,
                    register_add(register, index)?,
                    target_count - index,
                )?;
                return Ok(());
            }

            self.compile_inlined_expr_to(value, register_add(register, index)?)?;
            index += 1;
        }

        for extra in values.iter().skip(target_count as usize) {
            self.compile_expr_side(extra)?;
        }
        Ok(())
    }

    pub(super) fn compile_inlined_expr_to(
        &mut self,
        expr: &Expr,
        register: u8,
    ) -> Result<(), CompileError> {
        self.builder.set_max_stack_size(register_add(register, 1)?);
        if let Some(value) = self.constant_value_expr(expr)? {
            return self.compile_constant_value(value, register);
        }
        self.compile_expr_to(expr, register)
    }

    pub(super) fn emit_inline_fallthrough(
        &mut self,
        register: u8,
        target_count: u8,
    ) -> Result<(), CompileError> {
        for offset in 0..target_count {
            self.builder.emit(Instruction::abc(
                Opcode::LoadNil,
                register_add(register, offset)?,
                0,
                0,
            ));
        }
        Ok(())
    }

    pub(super) fn compile_fastcall2k_import(
        &mut self,
        call_id: ExprId,
        func: &Expr,
        args: &[Expr],
        register: u8,
        results: CallResults,
    ) -> Result<bool, CompileError> {
        let Some(path) = self.import_path(func) else {
            return Ok(false);
        };
        let Some(builtin) = self
            .context
            .builtin_call(call_id)
            .map(|builtin| builtin.function_id())
            .or_else(|| self.dynamic_builtin_function_id(&path, args))
        else {
            return Ok(false);
        };
        if builtin == crate::opcodes::BuiltinFunction::BIT32_EXTRACT
            && self.compile_bit32_extract_k_fastcall(func, args, &path, register, results)?
        {
            return Ok(true);
        }
        let [first, second] = args else {
            return Ok(false);
        };
        let Some(second) = self.constant_value_expr(second)? else {
            return Ok(false);
        };

        let first_register = register_add(register, 1)?;
        let second_register = register_add(register, 2)?;
        let first_source = if let Some(source) = self.local_expr_register(first)? {
            source
        } else {
            self.compile_expr_to(first, first_register)?;
            first_register
        };
        let fallback_move =
            (first_source != first_register).then_some((first_register, first_source));
        let second_constant = self.add_constant_value(&second);
        let fastcall = self.builder.emit(Instruction::abc_with_aux(
            Opcode::FastCall2K,
            builtin,
            first_source,
            0,
            Some(second_constant),
        ));
        if let Some((target, source)) = fallback_move {
            self.builder
                .emit(Instruction::abc(Opcode::Move, target, source, 0));
        }
        self.emit_load_constant_index(second_register, second_constant);
        self.compile_fastcall_fallback_func_to(func, &path, register)?;
        self.patch_fastcall_skip_to_current(fastcall)?;
        self.builder.emit(Instruction::abc(
            Opcode::Call,
            register,
            bytecode_count_operand("fastcall argument", args.len())?,
            results.operand(),
        ));
        self.builder
            .set_max_stack_size(register_add(second_register, 1)?);
        Ok(true)
    }

    pub(super) fn compile_bit32_extract_k_fastcall(
        &mut self,
        func: &Expr,
        args: &[Expr],
        path: &[String],
        register: u8,
        results: CallResults,
    ) -> Result<bool, CompileError> {
        if !matches!(path, [lib, name] if lib == "bit32" && name == "extract") {
            return Ok(false);
        }
        let [first, second, third] = args else {
            return Ok(false);
        };
        let Some(second_value) = self.constant_value_expr(second)? else {
            return Ok(false);
        };
        let Some(third_value) = self.constant_value_expr(third)? else {
            return Ok(false);
        };
        let Some(packed) = bit32_extract_k_value(&second_value, &third_value) else {
            return Ok(false);
        };

        let fallback_first_register = register_add(register, 1)?;
        let first_source = if let Some(source) = self.local_expr_register(first)? {
            source
        } else {
            self.compile_expr_to(first, fallback_first_register)?;
            fallback_first_register
        };
        let fallback_move = (first_source != fallback_first_register)
            .then_some((fallback_first_register, first_source));

        let packed_constant = self.add_constant_value(&ConstantValue::Number(f64::from(packed)));
        let second_constant = self.add_constant_value(&second_value);
        let third_constant = self.add_constant_value(&third_value);
        let fastcall = self.builder.emit(Instruction::abc_with_aux(
            Opcode::FastCall2K,
            59,
            first_source,
            0,
            Some(packed_constant),
        ));
        if let Some((target, source)) = fallback_move {
            self.builder
                .emit(Instruction::abc(Opcode::Move, target, source, 0));
        }
        self.emit_load_constant_index(register_add(register, 2)?, second_constant);
        self.emit_load_constant_index(register_add(register, 3)?, third_constant);
        self.compile_fastcall_fallback_func_to(func, path, register)?;
        self.patch_fastcall_skip_to_current(fastcall)?;
        self.builder.emit(Instruction::abc(
            Opcode::Call,
            register,
            bytecode_count_operand("fastcall argument", args.len())?,
            results.operand(),
        ));
        self.builder.set_max_stack_size(register_add(register, 4)?);
        Ok(true)
    }

    pub(super) fn compile_fastcall_import(
        &mut self,
        call_id: ExprId,
        func: &Expr,
        args: &[Expr],
        register: u8,
        results: CallResults,
    ) -> Result<bool, CompileError> {
        let Some(path) = self.import_path(func) else {
            return Ok(false);
        };
        let Some(builtin) = self
            .context
            .builtin_call(call_id)
            .map(|builtin| builtin.function_id())
            .or_else(|| self.dynamic_builtin_function_id(&path, args))
        else {
            return Ok(false);
        };
        if matches!(path.as_slice(), [name] if name == "select") && results == CallResults::Multret
        {
            return Ok(false);
        }
        let fixed_arity_args = self.context.optimization_level() > 1
            && Some(bytecode_u8_count("fastcall argument", args.len())?)
                == fastcall_fixed_arity(&path);
        if !fixed_arity_args
            && let Some((last, prefix)) = args.split_last()
            && call_uses_multret(last)
            && self.constant_value_expr(last)?.is_none()
        {
            if matches!(path.as_slice(), [name] if name == "select") && prefix.len() == 1 {
                let fallback_register = register_add(register, 1)?;
                let source = if let Some(source) = self.local_expr_register(&prefix[0])? {
                    source
                } else {
                    self.compile_expr_to(&prefix[0], fallback_register)?;
                    fallback_register
                };
                let fallback_move =
                    (source != fallback_register).then_some((fallback_register, source));
                let fastcall =
                    self.builder
                        .emit(Instruction::abc(Opcode::FastCall1, builtin, source, 0));
                self.compile_fastcall_fallback_func_to(func, &path, register)?;
                if let Some((target, source)) = fallback_move {
                    self.builder
                        .emit(Instruction::abc(Opcode::Move, target, source, 0));
                }
                self.compile_multret_arg_to(last, register_add(register, 2)?)?;
                self.patch_fastcall_skip_to_current(fastcall)?;
                self.builder.emit(Instruction::abc(
                    Opcode::Call,
                    register,
                    0,
                    results.operand(),
                ));
                self.builder.set_max_stack_size(register_add(
                    register,
                    bytecode_count_operand("fastcall argument", args.len())?,
                )?);
                return Ok(true);
            }
            for (index, arg) in prefix.iter().enumerate() {
                self.compile_expr_to(
                    arg,
                    register_add(
                        register,
                        bytecode_count_operand("fastcall argument index", index)?,
                    )?,
                )?;
            }
            self.compile_multret_arg_to(
                last,
                register_add(
                    register,
                    bytecode_count_operand("fastcall multret tail index", prefix.len())?,
                )?,
            )?;
            let fastcall = self
                .builder
                .emit(Instruction::abc(Opcode::FastCall, builtin, 0, 0));
            self.compile_fastcall_fallback_func_to(func, &path, register)?;
            self.patch_fastcall_skip_to_current(fastcall)?;
            self.builder.emit(Instruction::abc(
                Opcode::Call,
                register,
                0,
                results.operand(),
            ));
            self.builder.set_max_stack_size(register_add(
                register,
                bytecode_count_operand("fastcall argument", args.len())?,
            )?);
            return Ok(true);
        }

        if args.is_empty() {
            let fastcall = self
                .builder
                .emit(Instruction::abc(Opcode::FastCall, builtin, 0, 0));
            self.compile_fastcall_fallback_func_to(func, &path, register)?;
            self.patch_fastcall_skip_to_current(fastcall)?;
            self.builder.emit(Instruction::abc(
                Opcode::Call,
                register,
                1,
                results.operand(),
            ));
            self.builder.set_max_stack_size(register_add(register, 1)?);
            return Ok(true);
        }

        let mut use_specialized_fastcall = args.len() < 3;
        if !use_specialized_fastcall && args.len() <= 3 {
            for arg in args {
                if self.local_expr_register(arg)?.is_some() {
                    use_specialized_fastcall = true;
                    break;
                }
            }
        }
        if !use_specialized_fastcall {
            for (index, arg) in args.iter().enumerate() {
                self.compile_expr_to(
                    arg,
                    register_add(
                        register,
                        bytecode_count_operand("fastcall argument index", index)?,
                    )?,
                )?;
            }
            let fastcall = self
                .builder
                .emit(Instruction::abc(Opcode::FastCall, builtin, 0, 0));
            self.compile_fastcall_fallback_func_to(func, &path, register)?;
            self.patch_fastcall_skip_to_current(fastcall)?;
            self.builder.emit(Instruction::abc(
                Opcode::Call,
                register,
                bytecode_count_operand("fastcall argument", args.len())?,
                results.operand(),
            ));
            self.builder.set_max_stack_size(register_add(
                register,
                bytecode_count_operand("fastcall argument", args.len())?,
            )?);
            return Ok(true);
        }

        let mut fastcall_args = Vec::with_capacity(args.len());
        let mut fallback_moves = Vec::new();
        for (index, arg) in args.iter().enumerate() {
            let fallback_register = register_add(
                register,
                bytecode_count_operand("fastcall argument index", index)?,
            )?;
            let source = if let Some(source) = self.local_expr_register(arg)? {
                source
            } else {
                self.compile_expr_to(arg, fallback_register)?;
                fallback_register
            };
            fastcall_args.push(source);
            if source != fallback_register {
                fallback_moves.push((fallback_register, source));
            }
        }

        let fastcall = match fastcall_args.as_slice() {
            [first] => self
                .builder
                .emit(Instruction::abc(Opcode::FastCall1, builtin, *first, 0)),
            [first, second] => self.builder.emit(Instruction::abc_with_aux(
                Opcode::FastCall2,
                builtin,
                *first,
                0,
                Some(u32::from(*second)),
            )),
            [first, second, third] => self.builder.emit(Instruction::abc_with_aux(
                Opcode::FastCall3,
                builtin,
                *first,
                0,
                Some(u32::from(*second) | (u32::from(*third) << 8)),
            )),
            _ => unreachable!("fastcall args length already filtered"),
        };

        for (target, source) in fallback_moves {
            self.builder
                .emit(Instruction::abc(Opcode::Move, target, source, 0));
        }
        self.compile_fastcall_fallback_func_to(func, &path, register)?;
        self.patch_fastcall_skip_to_current(fastcall)?;
        self.builder.emit(Instruction::abc(
            Opcode::Call,
            register,
            bytecode_count_operand("fastcall argument", args.len())?,
            results.operand(),
        ));
        self.builder.set_max_stack_size(register_add(
            register,
            bytecode_count_operand("fastcall argument", args.len())?,
        )?);
        Ok(true)
    }

    pub(super) fn compile_namecall_to(
        &mut self,
        func: &Expr,
        args: &[Expr],
        register: u8,
        results: CallResults,
    ) -> Result<(), CompileError> {
        let func = ungroup_expr(func);
        let Expr::IndexName {
            expr: receiver,
            index,
            ..
        } = func
        else {
            return Err(CompileError::new(format!(
                "self call did not contain a named receiver: {func:?}"
            )));
        };

        let receiver = self.namecall_receiver_register(receiver, register)?;
        for (index, arg) in args.iter().enumerate() {
            self.compile_expr_to(
                arg,
                register_add(
                    register,
                    bytecode_u8_count("namecall argument index", index + 2)?,
                )?,
            )?;
        }
        self.set_namecall_debug_line(func);
        let key = self.builder.add_string_constant(index.as_str());
        self.builder.emit(Instruction::abc_with_aux(
            Opcode::NameCall,
            register,
            receiver,
            string_hash(index.as_str()),
            Some(key),
        ));
        self.emit_call_instruction(
            register,
            bytecode_count_operand("namecall argument", args.len() + 1)?,
            results,
            false,
        );
        self.builder.set_max_stack_size(register_add(
            register,
            bytecode_count_operand("namecall argument", args.len() + 1)?,
        )?);
        Ok(())
    }

    pub(super) fn namecall_receiver_register(
        &mut self,
        receiver: &Expr,
        register: u8,
    ) -> Result<u8, CompileError> {
        if let Some(source) = self.local_expr_register(receiver)? {
            return Ok(source);
        }
        self.compile_expr_to(receiver, register)?;
        Ok(register)
    }

    pub(super) fn compile_fastcall_fallback_func_to(
        &mut self,
        func: &Expr,
        path: &[String],
        register: u8,
    ) -> Result<(), CompileError> {
        self.set_expr_debug_line(func);
        if self.direct_import_path(func).is_some() {
            self.compile_import_path(path, register)
        } else {
            self.compile_expr_to(func, register)
        }
    }

    pub(super) fn dynamic_builtin_function_id(&self, path: &[String], args: &[Expr]) -> Option<u8> {
        let parts = path.iter().map(String::as_str).collect::<Vec<_>>();
        let function_id = builtin_function_id(&parts, self.context.options())?;
        if !builtin_args_are_eligible(function_id, args)
            || self.builtin_function_is_disabled(function_id)
        {
            return None;
        }
        Some(function_id)
    }

    pub(super) fn builtin_function_is_disabled(&self, function_id: u8) -> bool {
        self.context
            .options()
            .disabled_builtins
            .iter()
            .filter_map(|disabled| {
                let path = disabled.split('.').collect::<Vec<&str>>();
                builtin_function_id(&path, self.context.options())
            })
            .any(|disabled| disabled == function_id)
    }

    pub(super) fn patch_fastcall_skip_to_current(
        &mut self,
        fastcall: usize,
    ) -> Result<(), CompileError> {
        if self.builder.patch_skip_c_to_current(fastcall) {
            Ok(())
        } else {
            Err(CompileError::new(
                "fastcall fallback skip offset exceeds bytecode limit",
            ))
        }
    }

    pub(super) fn compile_call_func_to(
        &mut self,
        func: &Expr,
        register: u8,
    ) -> Result<(), CompileError> {
        let func = ungroup_expr(func);
        self.set_expr_debug_line(func);
        if let Some(path) = self.direct_import_path(func) {
            self.compile_import_path(&path, register)
        } else {
            self.compile_expr_to(func, register)
        }
    }

    pub(super) fn compile_call_args(
        &mut self,
        args: &[Expr],
        first_register: u8,
    ) -> Result<bool, CompileError> {
        let Some((last, prefix)) = args.split_last() else {
            return Ok(false);
        };
        let multret_tail =
            call_uses_multret(last) && self.analysis_constant_value_expr(last).is_none();
        let fixed_count = if multret_tail {
            prefix.len()
        } else {
            args.len()
        };
        for (index, arg) in args.iter().take(fixed_count).enumerate() {
            self.compile_expr_to(
                arg,
                register_at(first_register, index, "call argument index")?,
            )?;
        }
        if !multret_tail {
            return Ok(false);
        }

        self.compile_multret_arg_to(
            last,
            register_at(first_register, prefix.len(), "call multret tail index")?,
        )?;
        Ok(true)
    }

    pub(super) fn compile_multret_arg_to(
        &mut self,
        arg: &Expr,
        register: u8,
    ) -> Result<(), CompileError> {
        match arg {
            Expr::Call { .. } => self.compile_call_to(arg, register, CallResults::Multret),
            Expr::Instantiate { expr, .. } => self.compile_multret_arg_to(expr, register),
            Expr::Varargs { .. } => {
                self.set_expr_debug_line(arg);
                self.builder
                    .emit(Instruction::abc(Opcode::GetVarargs, register, 0, 0));
                Ok(())
            }
            _ => unreachable!("call_uses_multret only accepts call-like tail expressions"),
        }
    }

    pub(super) fn compile_expr_temp_n(
        &mut self,
        expr: &Expr,
        register: u8,
        target_count: u8,
    ) -> Result<(), CompileError> {
        let target_operand = bytecode_count_operand("temporary result", usize::from(target_count))?;
        match expr {
            Expr::Call { .. } => {
                self.compile_call_to(expr, register, CallResults::Fixed(target_count))
            }
            Expr::Instantiate { expr, .. } => {
                self.compile_expr_temp_n(expr, register, target_count)
            }
            Expr::Varargs { .. } => {
                self.set_expr_debug_line(expr);
                self.builder.emit(Instruction::abc(
                    Opcode::GetVarargs,
                    register,
                    target_operand,
                    0,
                ));
                Ok(())
            }
            _ => {
                self.compile_expr_to(expr, register)?;
                for offset in 1..target_count {
                    self.builder.emit(Instruction::abc(
                        Opcode::LoadNil,
                        register_add(register, offset)?,
                        0,
                        0,
                    ));
                }
                Ok(())
            }
        }
    }
}
