use super::*;

impl FunctionCompiler {
    pub(super) fn compile_expr_to(
        &mut self,
        expr: &Expr,
        register: u8,
    ) -> Result<(), CompileError> {
        self.context.check_cancelled()?;
        // A scratch target computed by `register_add` (rather than reserved) can
        // be the last register; the checked add turns the top-of-stack overflow
        // into the register-exhaustion error instead of a panic.
        self.builder.set_max_stack_size(register_add(register, 1)?);
        self.set_expr_debug_line(expr);
        if let Some(value) = self.analysis_constant_value_expr(expr) {
            let value = value.clone();
            return self.compile_constant_value(value, register);
        }
        if self.context.optimization_level() > 0
            && self.expr_uses_unregistered_local_constant(expr)
            && let Some(value) = self.constant_value_expr(expr)?
        {
            return self.compile_constant_value(value, register);
        }
        if self.context.optimization_level() > 0
            && let Some(value) = constant_number_expr(expr)?
        {
            return self.compile_f64(value, register);
        }
        if self.context.optimization_level() > 0
            && let Some(value) = constant_bool_expr(
                expr,
                self.constant_known_members(),
                self.constant_vector_lib(),
                self.constant_vector_ctor(),
            )?
        {
            self.builder.emit(Instruction::abc(
                Opcode::LoadB,
                register,
                u8::from(value),
                0,
            ));
            return Ok(());
        }

        match expr {
            Expr::Nil { .. } => {
                self.builder
                    .emit(Instruction::abc(Opcode::LoadNil, register, 0, 0));
                Ok(())
            }
            Expr::Bool { value, .. } => {
                self.builder.emit(Instruction::abc(
                    Opcode::LoadB,
                    register,
                    u8::from(*value),
                    0,
                ));
                Ok(())
            }
            Expr::Number { value, .. } => self.compile_number(value, register),
            Expr::Integer { value, .. } => {
                self.compile_constant_value(ConstantValue::Integer(*value), register)
            }
            Expr::String { value, .. } => {
                let constant = self.builder.add_string_constant(value);
                self.emit_load_constant_index(register, constant);
                Ok(())
            }
            Expr::InterpString {
                strings,
                expressions,
                ..
            } if expressions.is_empty() && strings.len() == 1 => {
                let constant = self.builder.add_string_constant(&strings[0]);
                self.emit_load_constant_index(register, constant);
                Ok(())
            }
            Expr::InterpString { .. } => self.compile_interp_string(expr, register),
            Expr::Varargs { .. } => {
                self.builder
                    .emit(Instruction::abc(Opcode::GetVarargs, register, 2, 0));
                Ok(())
            }
            Expr::Local { local, .. } => {
                let source = self.local_source_register(local, register)?;
                if source != register {
                    self.builder
                        .emit(Instruction::abc(Opcode::Move, register, source, 0));
                }
                Ok(())
            }
            Expr::Unary {
                op: UnaryOp::Minus,
                expr,
                ..
            } => {
                if let Expr::Integer { value, .. } = expr.as_ref() {
                    self.compile_constant_value(
                        ConstantValue::Integer(value.wrapping_neg()),
                        register,
                    )
                } else if let Expr::Number { value, .. } = expr.as_ref() {
                    if self.context.optimization_level() > 0 {
                        self.compile_f64(-number_value(value)?, register)
                    } else {
                        let source = register_add(register, 1)?;
                        self.builder.set_max_stack_size(source + 1);
                        self.compile_number(value, source)?;
                        self.builder
                            .emit(Instruction::abc(Opcode::Minus, register, source, 0));
                        Ok(())
                    }
                } else {
                    self.compile_dynamic_minus(expr, register)
                }
            }
            Expr::Unary {
                op: UnaryOp::Not,
                expr,
                ..
            } => self.compile_dynamic_unary(Opcode::Not, expr, register),
            Expr::Unary {
                op: UnaryOp::Len,
                expr,
                ..
            } => self.compile_dynamic_length(expr, register),
            Expr::Binary {
                op, left, right, ..
            } if comparison_jump_opcode(*op, true).is_some() => {
                self.compile_compare_to_bool(*op, left, right, register)
            }
            Expr::Binary {
                op, left, right, ..
            } if matches!(op, BinaryOp::And | BinaryOp::Or) => {
                self.compile_logical(*op, left, right, register)
            }
            Expr::Binary {
                op, left, right, ..
            } if arithmetic_opcode(*op).is_some() => {
                self.compile_arithmetic(*op, left, right, register)
            }
            Expr::Binary {
                op: BinaryOp::Concat,
                ..
            } => self.compile_concat(expr, register),
            Expr::IfElse {
                condition,
                true_expr,
                false_expr,
                ..
            } => self.compile_if_else_expr(condition, true_expr, false_expr, register),
            Expr::IndexName {
                expr: indexed,
                index,
                index_location,
                ..
            } => {
                if let Some(value) = self.static_constant_value_expr(expr)? {
                    self.compile_constant_value(value, register)
                } else if let Some(path) = self.direct_import_path(expr) {
                    self.compile_import_path(&path, register)
                } else {
                    self.compile_index_name(indexed, index.as_str(), *index_location, register)
                }
            }
            Expr::IndexExpr { expr, index, .. } => self.compile_index_expr(expr, index, register),
            Expr::Call { .. } => {
                if let Some(value) = self.static_constant_value_expr(expr)? {
                    self.compile_constant_value(value, register)
                } else {
                    self.compile_call_to(expr, register, CallResults::Fixed(1))
                }
            }
            Expr::Table {
                syntax_id, items, ..
            } => {
                let prediction = if self.context.optimization_level() > 0 {
                    self.context.table_shape(*syntax_id)
                } else {
                    TableSizePrediction::default()
                };
                // A constructor fills element scratch above the table register
                // (`register+1`…); if that would cross an enclosing `for`'s reserved control
                // registers, build the table above the watermark and move the handle back.
                let span = items.len().min(usize::from(u8::MAX)) as u8;
                let frame_lo = register_add(register, 1)?;
                // A trailing `...` list item emits an open `SETLIST` (C=0) that fills upward
                // from the list register without bound, so the syntactic `span` undercounts.
                // Extend the overlap extent to the watermark when the final list item is varargs.
                let open_tail = items
                    .iter()
                    .rfind(|item| matches!(item.kind, TableItemKind::Item))
                    .is_some_and(|item| expr_is_varargs(&item.value));
                let frame_hi = if open_tail {
                    self.next_register.max(frame_lo.saturating_add(span))
                } else {
                    frame_lo.saturating_add(span)
                };
                if span > 0 && self.overlaps_reserved(frame_lo, frame_hi) {
                    let frame = self.next_register;
                    self.compile_table_with_prediction(items, frame, prediction)?;
                    self.builder
                        .emit(Instruction::abc(Opcode::Move, register, frame, 0));
                    self.builder
                        .set_max_stack_size(register.max(frame).saturating_add(1));
                    Ok(())
                } else {
                    self.compile_table_with_prediction(items, register, prediction)
                }
            }
            Expr::Function { .. } => self.compile_function_expr_to(expr, register, ""),
            Expr::Global { name, .. } => {
                self.compile_global_load(name.as_str(), register);
                Ok(())
            }
            Expr::Group { expr, .. }
            | Expr::TypeAssertion { expr, .. }
            | Expr::Instantiate { expr, .. } => self.compile_expr_to(expr, register),
            _ => Err(CompileError::new(format!(
                "minimal bytecode compiler does not support expression {expr:?}"
            ))),
        }
    }

    pub(super) fn compile_table_with_prediction(
        &mut self,
        items: &[ruau_ast::syntax::TableItem],
        register: u8,
        prediction: TableSizePrediction,
    ) -> Result<(), CompileError> {
        if items.is_empty() {
            self.emit_new_table(register, prediction.hash_size, prediction.array_size)?;
            return Ok(());
        }

        if items
            .iter()
            .all(|item| matches!(item.kind, TableItemKind::Record))
        {
            return self.compile_record_table(items, register);
        }

        let literal_size = self.table_literal_size(items)?;
        self.emit_new_table(register, literal_size.hash_size, literal_size.array_size)?;
        let list_register = register_add(register, 1)?;
        let scratch_register = register_add(list_register, table_list_register_span(items))?;

        for item in items
            .iter()
            .filter(|item| !matches!(item.kind, TableItemKind::Item))
        {
            self.compile_table_keyed_item(register, item, scratch_register)?;
        }
        self.compile_table_list_items(register, list_register, items)?;
        Ok(())
    }

    pub(super) fn table_literal_size(
        &self,
        items: &[ruau_ast::syntax::TableItem],
    ) -> Result<TableSizePrediction, CompileError> {
        let mut list_count = 0u32;
        let mut hash_count = 0usize;
        let mut numeric_general_count = 0usize;
        let mut numeric_general_indices = BTreeSet::new();
        let mut numeric_sequence_candidate = true;

        for item in items {
            match item.kind {
                TableItemKind::Item => {
                    if !expr_is_varargs(&item.value) {
                        list_count += 1;
                    }
                }
                TableItemKind::Record => hash_count += 1,
                TableItemKind::General => {
                    let Some(key) = &item.key else {
                        hash_count += 1;
                        continue;
                    };
                    let index = match self.constant_value_expr(key)? {
                        Some(ConstantValue::Number(value)) => table_array_index(value),
                        Some(ConstantValue::Integer(value)) => table_array_index(value as f64),
                        _ => None,
                    };
                    if let Some(index) = index {
                        numeric_general_count += 1;
                        numeric_general_indices.insert(index);
                    } else {
                        hash_count += 1;
                        numeric_sequence_candidate = false;
                    }
                }
            }
        }

        let array_from_general_keys = self.context.optimization_level() > 0
            && list_count == 0
            && numeric_sequence_candidate
            && numeric_general_count > 0
            && numeric_general_indices.len() == numeric_general_count
            && contiguous_array_size(&numeric_general_indices) == numeric_general_count as u32;

        let array_size = if array_from_general_keys {
            numeric_general_count as u32
        } else {
            hash_count += numeric_general_count;
            list_count
        };

        Ok(TableSizePrediction {
            hash_size: hash_count.min(usize::from(u8::MAX)) as u8,
            array_size,
        })
    }

    pub(super) fn compile_record_table(
        &mut self,
        items: &[ruau_ast::syntax::TableItem],
        register: u8,
    ) -> Result<(), CompileError> {
        let constant_pack = self.context.optimization_level() > 0
            && self
                .context
                .options()
                .fast_flag("LuauCompileDuptableConstantPack2");
        let mut template: Vec<TableEntry> = Vec::new();
        let mut field_order = Vec::new();

        for item in items {
            let TableItemKind::Record = item.kind else {
                return Err(CompileError::new(format!(
                    "minimal bytecode compiler only supports record table fields: {item:?}"
                )));
            };
            let Some(Expr::String { value: key, .. }) = &item.key else {
                return Err(CompileError::new(format!(
                    "minimal bytecode compiler only supports string record table keys: {item:?}"
                )));
            };
            let key_constant = self.builder.add_string_constant(key);
            field_order.push((key.clone(), item.value.clone()));
            let value = if constant_pack {
                self.table_constant_entry_value(&item.value)?.unwrap_or(-1)
            } else {
                -1
            };

            let Some(entry) = template.iter_mut().find(|entry| entry.key == key_constant) else {
                template.push(TableEntry {
                    key: key_constant,
                    value: if constant_pack { value } else { -1 },
                });
                continue;
            };
            if constant_pack && entry.value != -1 {
                entry.value = value;
            }
        }

        let has_template_constants = constant_pack && template.iter().any(|entry| entry.value >= 0);
        let table = if has_template_constants {
            self.builder.add_table_with_constants(template.clone())
        } else {
            self.builder
                .add_table_shape(template.iter().map(|entry| entry.key).collect())
        };
        let template_packed = if let Some(table) = constant_ad_operand(table) {
            self.builder
                .emit(Instruction::ad(Opcode::DupTable, register, table));
            true
        } else {
            self.emit_new_table(register, template.len().min(usize::from(u8::MAX)) as u8, 0)?;
            false
        };

        for (key, value) in field_order {
            let key_constant = self.builder.add_string_constant(&key);
            if template_packed
                && constant_pack
                && template
                    .iter()
                    .find(|entry| entry.key == key_constant)
                    .is_some_and(|entry| entry.value >= 0)
            {
                continue;
            }
            if let Some(line) = expr_line(&value) {
                self.builder.set_debug_line(line);
            }
            if self.context.optimization_level() == 0 {
                let key_register = register_add(register, 1)?;
                self.emit_coverage();
                self.emit_load_constant_index(key_register, key_constant);
                if self.local_expr_register(&value)?.is_none() {
                    self.emit_one_coverage();
                }
                let source =
                    self.record_field_value_register(&value, &key, register_add(key_register, 1)?)?;
                self.builder.emit(Instruction::abc(
                    Opcode::SetTable,
                    source,
                    register,
                    key_register,
                ));
                self.builder
                    .set_max_stack_size(register_add(key_register.max(source), 1)?);
                continue;
            }
            self.emit_one_coverage();
            let source =
                self.record_field_value_register(&value, &key, register_add(register, 1)?)?;
            self.builder.emit(Instruction::abc_with_aux(
                Opcode::SetTableKs,
                source,
                register,
                string_hash(&key),
                Some(key_constant),
            ));
        }
        Ok(())
    }

    pub(super) fn table_constant_entry_value(
        &mut self,
        expr: &Expr,
    ) -> Result<Option<i32>, CompileError> {
        let Some(value) = self.constant_value_expr(expr)? else {
            return Ok(None);
        };
        let index = self.add_constant_value(&value);
        let index = i32::try_from(index)
            .map_err(|_| CompileError::new(format!("constant index {index} overflows i32")))?;
        Ok(Some(index))
    }

    pub(super) fn record_field_value_register(
        &mut self,
        value: &Expr,
        key: &str,
        scratch: u8,
    ) -> Result<u8, CompileError> {
        if let Some(register) = self.local_expr_register(value)? {
            return Ok(register);
        }
        if matches!(value, Expr::Function { .. }) {
            self.compile_function_expr_to(value, scratch, key)?;
        } else {
            self.compile_expr_to(value, scratch)?;
        }
        Ok(scratch)
    }

    pub(super) fn compile_table_keyed_item(
        &mut self,
        table: u8,
        item: &ruau_ast::syntax::TableItem,
        scratch: u8,
    ) -> Result<(), CompileError> {
        match item.kind {
            TableItemKind::Record => {
                let Some(Expr::String { value: key, .. }) = &item.key else {
                    return Err(CompileError::new(format!(
                        "minimal bytecode compiler only supports string record table keys: {item:?}"
                    )));
                };
                let key_constant = self.builder.add_string_constant(key);
                if self.context.optimization_level() == 0 {
                    self.emit_load_constant_index(scratch, key_constant);
                    self.compile_expr_to(&item.value, register_add(scratch, 1)?)?;
                    self.builder.emit(Instruction::abc(
                        Opcode::SetTable,
                        register_add(scratch, 1)?,
                        table,
                        scratch,
                    ));
                } else {
                    self.compile_expr_to(&item.value, scratch)?;
                    self.builder.emit(Instruction::abc_with_aux(
                        Opcode::SetTableKs,
                        scratch,
                        table,
                        string_hash(key),
                        Some(key_constant),
                    ));
                }
                Ok(())
            }
            TableItemKind::General => {
                let Some(key) = &item.key else {
                    return Err(CompileError::new(format!(
                        "general table item missing key: {item:?}"
                    )));
                };
                if let Some(key_value) = self.constant_value_expr(key)? {
                    match key_value {
                        ConstantValue::String(key) if self.context.optimization_level() > 0 => {
                            self.compile_expr_to(&item.value, scratch)?;
                            let key_constant = self.builder.add_string_constant(&key);
                            self.builder.emit(Instruction::abc_with_aux(
                                Opcode::SetTableKs,
                                scratch,
                                table,
                                string_hash(&key),
                                Some(key_constant),
                            ));
                            return Ok(());
                        }
                        ConstantValue::Number(value) if self.context.optimization_level() > 0 => {
                            if let Some(index) = table_array_index_operand(value) {
                                self.compile_expr_to(&item.value, scratch)?;
                                self.builder.emit(Instruction::abc(
                                    Opcode::SetTableN,
                                    scratch,
                                    table,
                                    index,
                                ));
                                return Ok(());
                            }
                        }
                        ConstantValue::Integer(value) if self.context.optimization_level() > 0 => {
                            if let Some(index) = table_array_index_operand(value as f64) {
                                self.compile_expr_to(&item.value, scratch)?;
                                self.builder.emit(Instruction::abc(
                                    Opcode::SetTableN,
                                    scratch,
                                    table,
                                    index,
                                ));
                                return Ok(());
                            }
                        }
                        ConstantValue::Nil
                        | ConstantValue::Bool(_)
                        | ConstantValue::String(_)
                        | ConstantValue::Number(_)
                        | ConstantValue::Integer(_)
                        | ConstantValue::Vector { .. } => {}
                    }
                }

                if let Some(key) = self.local_expr_register(key)? {
                    let value = if key == scratch {
                        register_add(scratch, 1)?
                    } else {
                        scratch
                    };
                    self.compile_expr_to(&item.value, value)?;
                    self.builder
                        .emit(Instruction::abc(Opcode::SetTable, value, table, key));
                    return Ok(());
                }

                self.compile_expr_to(key, scratch)?;
                self.compile_expr_to(&item.value, register_add(scratch, 1)?)?;
                self.builder.emit(Instruction::abc(
                    Opcode::SetTable,
                    register_add(scratch, 1)?,
                    table,
                    scratch,
                ));
                Ok(())
            }
            TableItemKind::Item => Ok(()),
        }
    }

    pub(super) fn compile_table_list_items(
        &mut self,
        table: u8,
        list_register: u8,
        items: &[ruau_ast::syntax::TableItem],
    ) -> Result<(), CompileError> {
        let list_items = items
            .iter()
            .filter(|item| matches!(item.kind, TableItemKind::Item))
            .collect::<Vec<_>>();
        if list_items.is_empty() {
            return Ok(());
        }

        // A trailing `...` or call produces a multret tail: all of its values flow into the
        // array via an open `SETLIST` (C=0), matching upstream's `compileExprTempMultRet` +
        // `SETLIST … multRet ? 0`. Without this, `{f()}` truncates `f`'s results to one.
        let last = list_items.last().map(|item| &item.value);
        let multret_tail = last.is_some_and(|value| {
            call_uses_multret(value) && self.analysis_constant_value_expr(value).is_none()
        });
        if let Some(last) = last
            && multret_tail
        {
            let register_count =
                bytecode_u8_count("table list item before multret tail", list_items.len())?;
            for (index, item) in list_items[..list_items.len() - 1].iter().enumerate() {
                self.compile_expr_to(
                    &item.value,
                    register_at(list_register, index, "table list item index")?,
                )?;
            }
            let last_register = register_add(list_register, register_count - 1)?;
            if expr_is_varargs(last) {
                self.builder
                    .set_max_stack_size(register_add(list_register, register_count)?);
                self.builder
                    .emit(Instruction::abc(Opcode::GetVarargs, last_register, 0, 0));
            } else {
                self.compile_call_to(last, last_register, CallResults::Multret)?;
            }
            self.builder.emit(Instruction::abc_with_aux(
                Opcode::SetList,
                table,
                list_register,
                0,
                Some(1),
            ));
            return Ok(());
        }

        for (chunk_index, chunk) in list_items.chunks(16).enumerate() {
            let start_index = chunk_index * 16 + 1;
            for (index, item) in chunk.iter().enumerate() {
                self.compile_expr_to(
                    &item.value,
                    register_at(list_register, index, "table list item index")?,
                )?;
            }
            self.builder.set_max_stack_size(register_span_end(
                list_register,
                chunk.len(),
                "table list chunk",
            )?);
            self.builder.emit(Instruction::abc_with_aux(
                Opcode::SetList,
                table,
                list_register,
                bytecode_count_operand("table list chunk", chunk.len())?,
                Some(start_index as u32),
            ));
        }
        Ok(())
    }

    pub(super) fn emit_new_table(
        &mut self,
        register: u8,
        hash_size: u8,
        array_size: u32,
    ) -> Result<(), CompileError> {
        let hash_operand = table_hash_size_operand(hash_size);
        self.builder.emit(Instruction::abc_with_aux(
            Opcode::NewTable,
            register,
            hash_operand,
            0,
            Some(array_size),
        ));
        Ok(())
    }

    pub(super) fn compile_dynamic_unary(
        &mut self,
        opcode: Opcode,
        expr: &Expr,
        register: u8,
    ) -> Result<(), CompileError> {
        let source = if let Expr::Local { local, .. } = expr {
            self.local_source_register(local, register)?
        } else {
            self.compile_expr_to(expr, register)?;
            register
        };
        self.builder
            .emit(Instruction::abc(opcode, register, source, 0));
        Ok(())
    }

    pub(super) fn compile_dynamic_minus(
        &mut self,
        expr: &Expr,
        register: u8,
    ) -> Result<(), CompileError> {
        // The operand scratch must respect the reserved-register watermark, not just
        // `register + 1` (which can land on a numeric-`for`'s anonymous limit/step control).
        let fallback = self.scratch_register_at_or_after(register_add(register, 1)?)?;
        self.compile_dynamic_minus_with_source(expr, register, fallback)
    }

    pub(super) fn compile_dynamic_minus_with_source(
        &mut self,
        expr: &Expr,
        register: u8,
        fallback_source: u8,
    ) -> Result<(), CompileError> {
        let source = if let Expr::Local { local, .. } = expr {
            self.local_source_register(local, register)?
        } else {
            self.compile_expr_to(expr, fallback_source)?;
            fallback_source
        };
        self.builder
            .emit(Instruction::abc(Opcode::Minus, register, source, 0));
        self.builder
            .set_max_stack_size(register.max(source).saturating_add(1));
        Ok(())
    }

    pub(super) fn compile_dynamic_length(
        &mut self,
        expr: &Expr,
        register: u8,
    ) -> Result<(), CompileError> {
        let source = if let Expr::Local { local, .. } = expr {
            self.local_source_register(local, register)?
        } else {
            // Watermark-respecting scratch (see `compile_dynamic_minus`).
            let source = self.scratch_register_at_or_after(register_add(register, 1)?)?;
            self.compile_expr_to(expr, source)?;
            source
        };
        self.builder
            .emit(Instruction::abc(Opcode::Length, register, source, 0));
        self.builder
            .set_max_stack_size(register.max(source).saturating_add(1));
        Ok(())
    }

    pub(super) fn compile_arithmetic(
        &mut self,
        op: BinaryOp,
        left: &Expr,
        right: &Expr,
        register: u8,
    ) -> Result<(), CompileError> {
        if self.context.optimization_level() > 0 {
            if let Some(k_opcode) = arithmetic_k_opcode(op)
                && let Some(value) = self.constant_number_operand(right)?
            {
                let constant = self.builder.add_number(value);
                if let Ok(constant) = u8::try_from(constant) {
                    let source = self.arithmetic_source_register(left, register)?;
                    self.builder
                        .emit(Instruction::abc(k_opcode, register, source, constant));
                    self.builder
                        .set_max_stack_size(register.max(source).saturating_add(1));
                    return Ok(());
                }
            }

            if let Some(k_opcode) = arithmetic_commuted_k_opcode(op)
                && let Expr::Number { value, .. } = left
                && self.arithmetic_rhs_allows_commuted_k(op, right)
            {
                // The `*K` opcode carries the constant in a u8 `C` field; past 255 fall
                // through to the register path rather than truncating the index.
                let constant = self.builder.add_number(number_value(value)?);
                if let Ok(constant) = u8::try_from(constant) {
                    let source = self.arithmetic_source_register(right, register)?;
                    self.builder
                        .emit(Instruction::abc(k_opcode, register, source, constant));
                    self.builder
                        .set_max_stack_size(register.max(source).saturating_add(1));
                    return Ok(());
                }
            }

            if let Some(rk_opcode) = arithmetic_rk_opcode(op)
                && let Some(value) = self.constant_number_operand(left)?
            {
                let constant = self.builder.add_number(value);
                if let Ok(constant) = u8::try_from(constant) {
                    let source = self.arithmetic_source_register(right, register)?;
                    self.builder
                        .emit(Instruction::abc(rk_opcode, register, constant, source));
                    self.builder
                        .set_max_stack_size(register.max(source).saturating_add(1));
                    return Ok(());
                }
            }

            if let Some(k_opcode) = arithmetic_k_opcode(op)
                && let (Expr::Local { local, .. }, Expr::Number { value, .. }) = (left, right)
            {
                let constant = self.builder.add_number(number_value(value)?);
                if let Ok(constant) = u8::try_from(constant) {
                    let source = self.local_source_register(local, register)?;
                    self.builder
                        .emit(Instruction::abc(k_opcode, register, source, constant));
                    self.builder
                        .set_max_stack_size(register.max(source).saturating_add(1));
                    return Ok(());
                }
            }
        }

        let opcode = arithmetic_opcode(op).expect("arithmetic opcode already filtered");
        let first_scratch = self.scratch_register_at_or_after(register_add(register, 1)?)?;
        let second_scratch = self.scratch_register_at_or_after(register_add(first_scratch, 1)?)?;
        let right_local_register = self.local_expr_register(right)?;
        let left_register = if let Some(source) = self.local_expr_register(left)? {
            source
        } else {
            let scratch = if right_local_register == Some(first_scratch) {
                second_scratch
            } else {
                first_scratch
            };
            self.compile_expr_to(left, scratch)?;
            scratch
        };
        let right_register = if let Some(source) = right_local_register {
            source
        } else {
            let scratch = if left_register == first_scratch {
                second_scratch
            } else {
                first_scratch
            };
            self.compile_expr_to(right, scratch)?;
            scratch
        };
        self.builder.emit(Instruction::abc(
            opcode,
            register,
            left_register,
            right_register,
        ));
        self.builder.set_max_stack_size(
            register
                .max(left_register)
                .max(right_register)
                .saturating_add(1),
        );
        Ok(())
    }

    pub(super) fn constant_number_operand(&self, expr: &Expr) -> Result<Option<f64>, CompileError> {
        if let Some(value) = constant_number_expr(expr)? {
            return Ok(Some(value));
        }
        Ok(match self.constant_value_expr(expr)? {
            Some(ConstantValue::Number(value)) => Some(value),
            Some(ConstantValue::Integer(value)) => Some(value as f64),
            Some(
                ConstantValue::Nil
                | ConstantValue::Bool(_)
                | ConstantValue::String(_)
                | ConstantValue::Vector { .. },
            )
            | None => None,
        })
    }

    pub(super) fn arithmetic_source_register(
        &mut self,
        expr: &Expr,
        register: u8,
    ) -> Result<u8, CompileError> {
        if let Some(source) = self.local_expr_register(expr)? {
            return Ok(source);
        }
        let source = self.next_register.max(register.saturating_add(1));
        self.compile_expr_to(expr, source)?;
        Ok(source)
    }

    pub(super) fn arithmetic_rhs_allows_commuted_k(&self, op: BinaryOp, expr: &Expr) -> bool {
        if self.context.optimization_level() < 2 {
            return false;
        }
        match expr {
            Expr::Local { local, .. } => {
                local_type_allows_commuted_k(op, local)
                    || self.active_local_type_allows_commuted_k(op, local.id.index())
            }
            Expr::IndexName {
                expr: indexed,
                index,
                ..
            } => {
                matches!(op, BinaryOp::Add | BinaryOp::Mul)
                    && is_vector_component_name(index.as_str())
                    && expr_is_typed_vector(indexed)
            }
            Expr::Group { expr, .. } => self.arithmetic_rhs_allows_commuted_k(op, expr),
            Expr::TypeAssertion {
                expr, annotation, ..
            } => {
                type_allows_commuted_k(op, annotation)
                    || self.arithmetic_rhs_allows_commuted_k(op, expr)
            }
            _ => false,
        }
    }

    pub(super) fn active_local_type_allows_commuted_k(&self, op: BinaryOp, local_id: u32) -> bool {
        match self.active_local_type_tag(local_id) {
            Some(tag) if tag == TypeTag::Number as u16 as u8 => {
                matches!(op, BinaryOp::Add | BinaryOp::Mul)
            }
            Some(tag) if tag == TypeTag::Vector as u16 as u8 => {
                matches!(op, BinaryOp::Mul)
            }
            _ => false,
        }
    }

    pub(super) fn compile_logical(
        &mut self,
        op: BinaryOp,
        left: &Expr,
        right: &Expr,
        register: u8,
    ) -> Result<(), CompileError> {
        if self.context.optimization_level() > 0 {
            if let Some(left_value) = self.short_circuit_constant_value(left)? {
                let left_truthy = constant_truthiness(&left_value);
                let selected = match (op, left_truthy) {
                    (BinaryOp::And, false) | (BinaryOp::Or, true) => left,
                    (BinaryOp::And, true) | (BinaryOp::Or, false) => right,
                    _ => unreachable!("logical operator already filtered"),
                };
                return self.compile_expr_to(selected, register);
            }

            if !self.is_condition_fast(left)?
                && let Some(right_value) = self.constant_value_expr(right)?
            {
                // ANDK/ORK carry the constant in a u8 `C` field; past 255 fall through to the
                // register path rather than rejecting a legal program.
                let constant = self.add_constant_value(&right_value);
                if let Ok(constant) = u8::try_from(constant) {
                    let source = self.logical_source_register(left, register.saturating_add(1))?;
                    self.builder.emit(Instruction::abc(
                        logical_k_opcode(op),
                        register,
                        source,
                        constant,
                    ));
                    return Ok(());
                }
            }
        }

        if !self.is_condition_fast(left)?
            && let Some(right) = self.local_expr_register(right)?
        {
            let source = self.logical_source_register(left, register.saturating_add(1))?;
            self.builder.emit(Instruction::abc(
                logical_opcode(op),
                register,
                source,
                right,
            ));
            return Ok(());
        }

        self.compile_short_circuit_logical(op, left, right, register)
    }

    pub(super) fn is_condition_fast(&self, expr: &Expr) -> Result<bool, CompileError> {
        if self.constant_value_expr(expr)?.is_some() {
            return Ok(true);
        }
        Ok(match expr {
            Expr::Binary { op, .. } => matches!(
                op,
                BinaryOp::And
                    | BinaryOp::Or
                    | BinaryOp::CompareNe
                    | BinaryOp::CompareEq
                    | BinaryOp::CompareLt
                    | BinaryOp::CompareLe
                    | BinaryOp::CompareGt
                    | BinaryOp::CompareGe
            ),
            Expr::Group { expr, .. }
            | Expr::TypeAssertion { expr, .. }
            | Expr::Instantiate { expr, .. } => self.is_condition_fast(expr)?,
            _ => false,
        })
    }

    pub(super) fn compile_compare_to_bool(
        &mut self,
        op: BinaryOp,
        left: &Expr,
        right: &Expr,
        register: u8,
    ) -> Result<(), CompileError> {
        let scratch_register = self.next_register.max(register + 1);
        if let Some(jump) =
            self.emit_jumpx_constant_compare_at(op, left, right, scratch_register, true)?
        {
            return self.emit_bool_from_jump(register, &jump);
        }

        let (opcode, left, right) =
            self.comparison_jump_operands(op, left, right, true, scratch_register)?;
        let index = self.builder.emit(Instruction::abc_with_aux(
            opcode,
            left,
            0,
            0,
            Some(u32::from(right)),
        ));
        self.emit_bool_from_jump(
            register,
            &PendingJump::Compare {
                index,
                opcode,
                left,
                right,
            },
        )
    }

    pub(super) fn emit_jumpx_constant_compare_at(
        &mut self,
        op: BinaryOp,
        left: &Expr,
        right: &Expr,
        scratch_register: u8,
        jump_when_truthy: bool,
    ) -> Result<Option<PendingJump>, CompileError> {
        if self.context.optimization_level() == 0 {
            return Ok(None);
        }

        let negate = match op {
            BinaryOp::CompareEq | BinaryOp::CompareNe => {
                matches!(op, BinaryOp::CompareEq) != jump_when_truthy
            }
            _ => return Ok(None),
        };

        let Some((value, constant)) = self.constant_compare_operands(left, right)? else {
            return Ok(None);
        };
        if matches!(
            constant,
            ConstantValue::Integer(_) | ConstantValue::Vector { .. }
        ) {
            return Ok(None);
        }

        let register = self.condition_operand_register(value, scratch_register)?;
        let Some((opcode, mut aux)) = self.jumpx_constant_operand(&constant)? else {
            return Ok(None);
        };
        if negate {
            aux |= crate::opcodes::JUMPX_K_NOT_BIT;
        }

        let index = self
            .builder
            .emit(Instruction::abc_with_aux(opcode, register, 0, 0, Some(aux)));
        Ok(Some(PendingJump::AdWithAux {
            index,
            opcode,
            register,
            aux: Some(aux),
        }))
    }

    pub(super) fn constant_compare_operands<'a>(
        &self,
        left: &'a Expr,
        right: &'a Expr,
    ) -> Result<Option<(&'a Expr, ConstantValue)>, CompileError> {
        if let Some(constant) = self.constant_value_expr(right)?
            && self.constant_value_expr(left)?.is_none()
        {
            return Ok(Some((left, constant)));
        }
        if let Some(constant) = self.constant_value_expr(left)?
            && self.constant_value_expr(right)?.is_none()
        {
            return Ok(Some((right, constant)));
        }
        Ok(None)
    }

    pub(super) fn jumpx_constant_operand(
        &mut self,
        constant: &ConstantValue,
    ) -> Result<Option<(Opcode, u32)>, CompileError> {
        // The constant index occupies the low 24 bits of the aux word (bit 31 carries
        // the negate flag — `emit_jumpx_constant_compare_at`), so it is not limited to a
        // `u8`. A pool too large for that field can't be encoded here; fall back to a
        // plain comparison rather than truncating the index.
        Ok(match constant {
            ConstantValue::Nil => Some((Opcode::JumpXEqKNil, 0)),
            ConstantValue::Bool(value) => Some((Opcode::JumpXEqKB, u32::from(*value))),
            ConstantValue::Number(value) => {
                let cid = self.add_constant_value(&ConstantValue::Number(*value));
                (cid <= crate::opcodes::JUMPX_K_INDEX_MASK).then_some((Opcode::JumpXEqKN, cid))
            }
            ConstantValue::String(value) => {
                let cid = self.builder.add_string_constant(value);
                (cid <= crate::opcodes::JUMPX_K_INDEX_MASK).then_some((Opcode::JumpXEqKS, cid))
            }
            ConstantValue::Integer(_) | ConstantValue::Vector { .. } => None,
        })
    }

    pub(super) fn emit_bool_from_jump(
        &mut self,
        register: u8,
        jump: &PendingJump,
    ) -> Result<(), CompileError> {
        self.builder
            .emit(Instruction::abc(Opcode::LoadB, register, 0, 1));
        self.patch_jump_to_current(jump)?;
        self.builder
            .emit(Instruction::abc(Opcode::LoadB, register, 1, 0));
        Ok(())
    }

    pub(super) fn compile_short_circuit_logical(
        &mut self,
        op: BinaryOp,
        left: &Expr,
        right: &Expr,
        register: u8,
    ) -> Result<(), CompileError> {
        let result = if self.register_holds_active_local(register)
            || (register < self.next_register && !self.logical_target_register_is_safe(left)?)
        {
            self.next_register
        } else {
            register
        };
        self.builder.set_max_stack_size(result + 1);

        let mut skip_jumps = Vec::new();
        let scratch_register = self.next_register.max(result.saturating_add(1));
        if result != register && !self.logical_target_register_is_safe(left)? {
            self.compile_expr_to(left, result)?;
            let jump_register = if self.register_holds_active_local(register) {
                result
            } else {
                self.builder
                    .emit(Instruction::abc(Opcode::Move, register, result, 0));
                register
            };
            let opcode = if matches!(op, BinaryOp::Or) {
                Opcode::JumpIf
            } else {
                Opcode::JumpIfNot
            };
            let index = self.builder.emit(Instruction::ad(opcode, jump_register, 0));
            skip_jumps.push(PendingJump::Ad {
                index,
                opcode,
                register: jump_register,
            });
            self.compile_expr_to(right, result)?;
            if jump_register == result {
                self.patch_jumps_to_current(skip_jumps)?;
                self.builder
                    .emit(Instruction::abc(Opcode::Move, register, result, 0));
            } else {
                self.builder
                    .emit(Instruction::abc(Opcode::Move, register, result, 0));
                self.patch_jumps_to_current(skip_jumps)?;
            }
            return Ok(());
        }
        self.compile_condition_value(
            left,
            Some(result),
            scratch_register,
            &mut skip_jumps,
            matches!(op, BinaryOp::Or),
        )?;
        self.compile_expr_to(right, result)?;
        self.patch_jumps_to_current(skip_jumps)?;

        if result != register {
            self.builder
                .emit(Instruction::abc(Opcode::Move, register, result, 0));
        }
        Ok(())
    }

    pub(super) fn logical_target_register_is_safe(
        &self,
        expr: &Expr,
    ) -> Result<bool, CompileError> {
        Ok(match expr {
            Expr::Call { func, is_self, .. } => {
                !*is_self && self.inlinable_function_expr(func).is_some()
            }
            Expr::Binary { op, .. } => matches!(
                op,
                BinaryOp::And
                    | BinaryOp::Or
                    | BinaryOp::CompareNe
                    | BinaryOp::CompareEq
                    | BinaryOp::CompareLt
                    | BinaryOp::CompareLe
                    | BinaryOp::CompareGt
                    | BinaryOp::CompareGe
            ),
            Expr::Unary {
                op: UnaryOp::Not, ..
            } => true,
            Expr::Local { local, .. } => {
                let local_id = local.id.index();
                self.local_registers.contains_key(&local_id)
                    || local.function_depth < self.current_function_depth
                    || self.local_constant(local_id).is_some()
                    || self.context.local_constant(local.id).is_some()
            }
            Expr::Group { expr, .. }
            | Expr::TypeAssertion { expr, .. }
            | Expr::Instantiate { expr, .. } => self.logical_target_register_is_safe(expr)?,
            _ => self.constant_value_expr(expr)?.is_some(),
        })
    }

    pub(super) fn logical_source_register(
        &mut self,
        expr: &Expr,
        register: u8,
    ) -> Result<u8, CompileError> {
        if let Some(source) = self.local_expr_register(expr)? {
            return Ok(source);
        }

        let source = if register < self.next_register {
            self.next_register
        } else {
            register
        };
        self.compile_expr_to(expr, source)?;
        self.builder.set_max_stack_size(source + 1);
        Ok(source)
    }

    pub(super) fn local_expr_register(&self, expr: &Expr) -> Result<Option<u8>, CompileError> {
        match expr {
            Expr::Local { local, .. } => {
                let local_id = local.id.index();
                if let Some(register) = self.local_registers.get(&local_id).copied() {
                    Ok(Some(register))
                } else if local.function_depth < self.current_function_depth
                    || self.local_constant(local_id).is_some()
                    || self.context.local_constant(local.id).is_some()
                {
                    Ok(None)
                } else {
                    Err(CompileError::new(format!(
                        "unknown local id {local_id} in local_expr_register"
                    )))
                }
            }
            Expr::Group { expr, .. }
            | Expr::TypeAssertion { expr, .. }
            | Expr::Instantiate { expr, .. } => self.local_expr_register(expr),
            _ => Ok(None),
        }
    }

    pub(in crate::compile) fn constant_value_expr(
        &self,
        expr: &Expr,
    ) -> Result<Option<ConstantValue>, CompileError> {
        if let Some(value) = self.analysis_constant_value_expr(expr) {
            return Ok(Some(value.clone()));
        }
        Ok(match expr {
            Expr::Local { local, .. } => self.local_constant(local.id.index()),
            Expr::Unary {
                op: UnaryOp::Not,
                expr,
                ..
            } => self
                .constant_value_expr(expr)?
                .map(|value| ConstantValue::Bool(!constant_truthiness(&value))),
            Expr::Binary {
                op, left, right, ..
            } if comparison_jump_opcode(*op, true).is_some() => {
                let Some(left) = self.constant_value_expr(left)? else {
                    return Ok(None);
                };
                let Some(right) = self.constant_value_expr(right)? else {
                    return Ok(None);
                };
                let Some(result) = compare_constant_values(*op, left, right)? else {
                    return Ok(None);
                };
                Some(ConstantValue::Bool(result))
            }
            Expr::Binary {
                op, left, right, ..
            } if arithmetic_opcode(*op).is_some() => {
                let Some(left) = self.constant_value_expr(left)? else {
                    return Ok(None);
                };
                let Some(right) = self.constant_value_expr(right)? else {
                    return Ok(None);
                };
                constant_arithmetic_value(*op, &left, &right)?
            }
            Expr::Call {
                syntax_id,
                func,
                args,
                ..
            } => {
                if self.fold_library_constants()
                    && let Some(builtin) = self
                        .context
                        .builtin_call(*syntax_id)
                        .map(|builtin| builtin.function_id())
                        .or_else(|| {
                            let path = self.import_path(func)?;
                            self.dynamic_builtin_function_id(&path, args)
                        })
                {
                    let args = args
                        .iter()
                        .map(|arg| self.constant_value_expr(arg))
                        .collect::<Result<Vec<_>, _>>()?;
                    if let Some(value) = fold_builtin_constant(builtin, &args) {
                        return Ok(Some(value));
                    }
                }
                self.static_constant_value_expr(expr)?
            }
            Expr::Group { expr, .. }
            | Expr::TypeAssertion { expr, .. }
            | Expr::Instantiate { expr, .. } => self.constant_value_expr(expr)?,
            _ => self.static_constant_value_expr(expr)?,
        })
    }

    pub(super) fn static_constant_value_expr(
        &self,
        expr: &Expr,
    ) -> Result<Option<ConstantValue>, CompileError> {
        constant_value_expr(
            expr,
            self.constant_known_members(),
            self.constant_vector_lib(),
            self.constant_vector_ctor(),
        )
    }

    pub(super) fn fold_library_constants(&self) -> bool {
        self.context.optimization_level() >= 2
            && !self.context.getfenv_used()
            && !self.context.setfenv_used()
    }

    pub(super) fn constant_known_members(&self) -> &[KnownMember] {
        if self.fold_library_constants() {
            self.context.known_members()
        } else {
            &[]
        }
    }

    pub(super) fn constant_vector_lib(&self) -> Option<&str> {
        self.fold_library_constants()
            .then(|| self.context.vector_lib().unwrap_or("vector"))
    }

    pub(super) fn constant_vector_ctor(&self) -> Option<&str> {
        self.fold_library_constants()
            .then(|| self.context.vector_ctor().unwrap_or("create"))
    }

    pub(super) fn condition_truthiness_expr(
        &self,
        expr: &Expr,
    ) -> Result<Option<bool>, CompileError> {
        Ok(self
            .constant_value_expr(expr)?
            .map(|value| constant_truthiness(&value)))
    }

    pub(super) fn optimized_condition_truthiness_expr(
        &self,
        expr: &Expr,
    ) -> Result<Option<bool>, CompileError> {
        if self.context.optimization_level() > 0 {
            self.condition_truthiness_expr(expr)
        } else {
            Ok(None)
        }
    }

    pub(super) fn analysis_constant_value_expr(&self, expr: &Expr) -> Option<&ConstantValue> {
        if self.context.optimization_level() > 0 {
            self.context.constant_expr(expr.syntax_id())
        } else {
            None
        }
    }

    pub(super) fn expr_uses_unregistered_local_constant(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Local { local, .. } => {
                let local_id = local.id.index();
                !self.local_registers.contains_key(&local_id)
                    && self.local_constant(local_id).is_some()
            }
            Expr::Unary { expr, .. }
            | Expr::Group { expr, .. }
            | Expr::TypeAssertion { expr, .. }
            | Expr::Instantiate { expr, .. }
            | Expr::IndexName { expr, .. } => self.expr_uses_unregistered_local_constant(expr),
            Expr::Binary { left, right, .. }
            | Expr::IndexExpr {
                expr: left,
                index: right,
                ..
            } => {
                self.expr_uses_unregistered_local_constant(left)
                    || self.expr_uses_unregistered_local_constant(right)
            }
            Expr::IfElse {
                condition,
                true_expr,
                false_expr,
                ..
            } => {
                self.expr_uses_unregistered_local_constant(condition)
                    || self.expr_uses_unregistered_local_constant(true_expr)
                    || self.expr_uses_unregistered_local_constant(false_expr)
            }
            Expr::Call { func, args, .. } => {
                self.expr_uses_unregistered_local_constant(func)
                    || args
                        .iter()
                        .any(|arg| self.expr_uses_unregistered_local_constant(arg))
            }
            Expr::Table { items, .. } => items.iter().any(|item| {
                item.key
                    .as_ref()
                    .is_some_and(|key| self.expr_uses_unregistered_local_constant(key))
                    || self.expr_uses_unregistered_local_constant(&item.value)
            }),
            Expr::InterpString { expressions, .. } => expressions
                .iter()
                .any(|expr| self.expr_uses_unregistered_local_constant(expr)),
            Expr::Nil { .. }
            | Expr::Bool { .. }
            | Expr::Number { .. }
            | Expr::Integer { .. }
            | Expr::String { .. }
            | Expr::Varargs { .. }
            | Expr::Function { .. }
            | Expr::Global { .. }
            | Expr::Error { .. } => false,
        }
    }

    pub(super) fn short_circuit_constant_value(
        &self,
        expr: &Expr,
    ) -> Result<Option<ConstantValue>, CompileError> {
        Ok(match self.constant_value_expr(expr)? {
            Some(value @ (ConstantValue::Nil | ConstantValue::Bool(_))) => Some(value),
            _ => None,
        })
    }

    /// Emits `LOADK` (constant id in the 16-bit `D` field) or, when the id needs
    /// more than that signed field, `LOADKX` with the id in the aux word — upstream
    /// `emitLoadK` (`Compiler.cpp`).
    pub(super) fn emit_load_constant_index(&mut self, target: u8, constant: u32) {
        if let Ok(small) = i16::try_from(constant) {
            self.builder
                .emit(Instruction::ad(Opcode::LoadK, target, small));
        } else {
            self.builder.emit(Instruction::abc_with_aux(
                Opcode::LoadKx,
                target,
                0,
                0,
                Some(constant),
            ));
        }
    }

    pub(super) fn add_constant_value(&mut self, value: &ConstantValue) -> u32 {
        match value {
            ConstantValue::Nil => self.builder.add_nil(),
            ConstantValue::Bool(value) => self.builder.add_boolean(*value),
            ConstantValue::Number(value) => self.builder.add_number(*value),
            ConstantValue::Integer(value) => self.builder.add_integer(*value),
            ConstantValue::String(value) => self.builder.add_string_constant(value),
            ConstantValue::Vector { bits } => self.builder.add_vector_bits(*bits),
        }
    }

    /// Emits the two-branch core every if-else lowering shares: jump past
    /// the true branch when the condition is falsy, compile both branches
    /// into `register`, and patch the jumps.
    pub(super) fn compile_if_else_branches(
        &mut self,
        condition: &Expr,
        true_expr: &Expr,
        false_expr: &Expr,
        register: u8,
    ) -> Result<(), CompileError> {
        let scratch = self.next_register.max(register + 1);
        let false_jumps = self.emit_condition_jumps_at(condition, false, scratch)?;
        self.compile_expr_to(true_expr, register)?;
        let end_jump = self.builder.emit(Instruction::ad(Opcode::Jump, 0, 0));
        self.patch_jumps_to_current(false_jumps)?;
        self.compile_expr_to(false_expr, register)?;
        self.patch_jump_to_current(&PendingJump::Ad {
            index: end_jump,
            opcode: Opcode::Jump,
            register: 0,
        })?;
        Ok(())
    }

    pub(super) fn compile_if_else_expr(
        &mut self,
        condition: &Expr,
        true_expr: &Expr,
        false_expr: &Expr,
        register: u8,
    ) -> Result<(), CompileError> {
        if let Some(selected) = self.static_if_else_branch(condition, true_expr, false_expr)? {
            return self.compile_expr_to(selected, register);
        }
        if let Some((op, left, right)) =
            self.if_else_logical_rewrite(condition, true_expr, false_expr)
        {
            return self.compile_logical(op, left, right, register);
        }

        self.compile_if_else_branches(condition, true_expr, false_expr, register)
    }

    pub(super) fn compile_if_else_return(
        &mut self,
        condition: &Expr,
        true_expr: &Expr,
        false_expr: &Expr,
        register: u8,
    ) -> Result<(), CompileError> {
        if let Some(selected) = self.static_if_else_branch(condition, true_expr, false_expr)? {
            self.compile_expr_to(selected, register)?;
            self.builder.set_max_stack_size(register + 1);
            self.emit_return(register, 2);
            return Ok(());
        }

        self.compile_if_else_branches(condition, true_expr, false_expr, register)?;
        self.emit_return(register, 2);
        Ok(())
    }

    pub(super) fn compile_if_else_void_tail(
        &mut self,
        condition: &Expr,
        true_expr: &Expr,
        false_expr: &Expr,
        register: u8,
    ) -> Result<(), CompileError> {
        if let Some(selected) = self.static_if_else_branch(condition, true_expr, false_expr)? {
            self.compile_expr_to(selected, register)?;
            self.emit_return(0, 1);
            return Ok(());
        }

        self.compile_if_else_branches(condition, true_expr, false_expr, register)?;
        self.emit_return(0, 1);
        Ok(())
    }

    pub(super) fn static_if_else_branch<'a>(
        &self,
        condition: &Expr,
        true_expr: &'a Expr,
        false_expr: &'a Expr,
    ) -> Result<Option<&'a Expr>, CompileError> {
        Ok(self
            .optimized_condition_truthiness_expr(condition)?
            .map(|truthy| if truthy { true_expr } else { false_expr }))
    }

    pub(super) fn if_else_logical_rewrite<'a>(
        &self,
        condition: &'a Expr,
        true_expr: &'a Expr,
        false_expr: &'a Expr,
    ) -> Option<(BinaryOp, &'a Expr, &'a Expr)> {
        if same_local_expr(condition, true_expr) {
            if self.context.optimization_level() == 0 && local_expr_id(false_expr).is_none() {
                return None;
            }
            Some((BinaryOp::Or, condition, false_expr))
        } else if same_local_expr(condition, false_expr) {
            if self.context.optimization_level() == 0 && local_expr_id(true_expr).is_none() {
                return None;
            }
            Some((BinaryOp::And, condition, true_expr))
        } else {
            None
        }
    }

    pub(super) fn compile_index_name(
        &mut self,
        indexed: &Expr,
        index: &str,
        index_location: Option<Location>,
        register: u8,
    ) -> Result<(), CompileError> {
        if let Some(value) = self.local_table_prop_value(indexed, index) {
            return self.compile_constant_value(value, register);
        }
        let source = self.index_source_register(indexed, register)?;
        if let Some(location) = index_location {
            self.builder.set_debug_line(location.begin.line + 1);
        }
        let constant = self.builder.add_string_constant(index);
        self.builder.emit(Instruction::abc_with_aux(
            Opcode::GetTableKs,
            register,
            source,
            string_hash(index),
            Some(constant),
        ));
        Ok(())
    }

    pub(super) fn compile_index_expr(
        &mut self,
        indexed: &Expr,
        index: &Expr,
        register: u8,
    ) -> Result<(), CompileError> {
        if let Some(ConstantValue::String(key)) = self.constant_value_expr(index)?
            && let Some(value) = self.local_table_prop_value(indexed, &key)
        {
            return self.compile_constant_value(value, register);
        }

        let source = self.index_source_register(indexed, register)?;
        if let Some(key) = self.constant_value_expr(index)? {
            match key {
                ConstantValue::String(key) if self.context.optimization_level() > 0 => {
                    self.set_expr_debug_line(index);
                    let constant = self.builder.add_string_constant(&key);
                    self.builder.emit(Instruction::abc_with_aux(
                        Opcode::GetTableKs,
                        register,
                        source,
                        string_hash(&key),
                        Some(constant),
                    ));
                    return Ok(());
                }
                ConstantValue::Number(value) => {
                    if self.context.optimization_level() > 0
                        && let Some(key_operand) = table_array_index_operand(value)
                    {
                        self.set_expr_debug_line(index);
                        self.builder.emit(Instruction::abc(
                            Opcode::GetTableN,
                            register,
                            source,
                            key_operand,
                        ));
                        return Ok(());
                    }
                }
                ConstantValue::Integer(value) => {
                    if self.context.optimization_level() > 0
                        && let Some(key_operand) = table_array_index_operand(value as f64)
                    {
                        self.set_expr_debug_line(index);
                        self.builder.emit(Instruction::abc(
                            Opcode::GetTableN,
                            register,
                            source,
                            key_operand,
                        ));
                        return Ok(());
                    }
                }
                ConstantValue::Nil
                | ConstantValue::Bool(_)
                | ConstantValue::String(_)
                | ConstantValue::Vector { .. } => {}
            }
        }

        let scratch = self.next_register.max(register + 1);
        let scratch = if scratch == source {
            register_add(scratch, 1)?
        } else {
            scratch
        };
        let scratch = if let Some(index_source) = self.local_expr_register(index)? {
            index_source
        } else {
            self.compile_expr_to(index, scratch)?;
            scratch
        };
        self.builder.emit(Instruction::abc(
            Opcode::GetTable,
            register,
            source,
            scratch,
        ));
        Ok(())
    }

    pub(super) fn index_source_register(
        &mut self,
        expr: &Expr,
        register: u8,
    ) -> Result<u8, CompileError> {
        if let Expr::Global { name, .. } = ungroup_expr(expr) {
            self.compile_global_load(name.as_str(), register);
            self.builder.set_max_stack_size(register + 1);
            return Ok(register);
        }
        if !index_expr_needs_distinct_source(expr) {
            return self.logical_source_register(expr, register);
        }
        if let Some(source) = self.local_expr_register(expr)? {
            return Ok(source);
        }

        let source = self.next_register.max(register.saturating_add(1));
        self.compile_expr_to(expr, source)?;
        self.builder.set_max_stack_size(source + 1);
        Ok(source)
    }

    pub(super) fn local_table_prop_value(
        &self,
        indexed: &Expr,
        key: &str,
    ) -> Option<ConstantValue> {
        let local_id = local_expr_local_id(indexed)?;
        self.context.table_prop(local_id, key).cloned()
    }

    pub(super) fn compile_assignment(
        &mut self,
        target: &Expr,
        value: &Expr,
    ) -> Result<(), CompileError> {
        match target {
            Expr::Local { local, .. } => {
                let Some(register) = self.local_registers.get(&local.id.index()).copied() else {
                    if local.function_depth < self.current_function_depth {
                        let upvalue = self.ensure_upvalue(local.id.index())?;
                        let next_register = self.next_register;
                        let register = next_register;
                        self.compile_expr_to(value, register)?;
                        self.builder
                            .emit(Instruction::abc(Opcode::SetUpval, register, upvalue, 0));
                        self.next_register = next_register;
                        return Ok(());
                    }
                    return Err(CompileError::new(format!(
                        "unknown local id {} in assignment",
                        local.id.index()
                    )));
                };
                if self.context.optimization_level() == 0 && same_local_expr(target, value) {
                    self.builder
                        .emit(Instruction::abc(Opcode::Move, register, register, 0));
                    return Ok(());
                }
                // Upstream's local assignment path passes targetTemp=false for RHS values that
                // build through scratch registers, routing them through allocReg and a MOVE even
                // when the destination is already at the stack top.
                let use_temp = assignment_value_needs_scratch_above_base(value);
                if use_temp {
                    let saved_next = self.next_register;
                    let temp = self.reserve_register()?;
                    self.compile_expr_to(value, temp)?;
                    self.builder
                        .emit(Instruction::abc(Opcode::Move, register, temp, 0));
                    self.clear_scratch_registers(saved_next, self.next_register);
                    self.next_register = saved_next;
                } else {
                    self.compile_expr_to(value, register)?;
                }
                let constant = self.constant_value_expr(value)?;
                let path = self.local_import_path_initializer(value);
                self.set_local_value_facts(local.id.index(), constant, path);
                Ok(())
            }
            Expr::Global { .. } | Expr::IndexName { .. } | Expr::IndexExpr { .. } => {
                let next_register = self.next_register;
                let lvalue = self.compile_lvalue(target)?;
                let source = self.compile_expr_auto(value)?;
                self.compile_lvalue_use(&lvalue, source, LvalueAccess::Set)?;
                self.clear_scratch_registers(next_register, self.next_register);
                self.next_register = next_register;
                Ok(())
            }
            _ => Err(CompileError::new(format!(
                "minimal bytecode compiler does not support assignment target {target:?}"
            ))),
        }
    }

    pub(super) fn compile_multi_assignment(
        &mut self,
        targets: &[Expr],
        values: &[Expr],
    ) -> Result<bool, CompileError> {
        // The simple single-target, single-value assignment (`x = e`) keeps the
        // optimized direct path. A single target with surplus values
        // (`x = a, b`) must route through here too: Lua evaluates the whole
        // value list before storing, and this path evaluates the surplus (and
        // resolves conflicts where a surplus value reads the target) before the
        // store, which the direct path cannot.
        if targets.len() <= 1 && values.len() <= targets.len() {
            return Ok(false);
        }

        let next_register = self.next_register;
        let mut assignments = targets
            .iter()
            .map(|target| {
                Ok(Assignment {
                    lvalue: self.compile_lvalue(target)?,
                    conflict_register: None,
                    value_register: None,
                })
            })
            .collect::<Result<Vec<_>, CompileError>>()?;

        self.resolve_assignment_conflicts(&mut assignments, values)?;

        for index in 0..assignments.len() {
            if index + 1 == values.len() && assignments.len() > values.len() {
                let rest = assignments.len() - values.len() + 1;
                let rest = bytecode_fixed_count("assignment value", rest)?;
                let temp = self.reserve_registers(rest)?;
                self.compile_expr_temp_n(&values[index], temp, rest)?;
                for (offset, assignment) in assignments[index..].iter_mut().enumerate() {
                    assignment.value_register =
                        Some(register_at(temp, offset, "assignment value index")?);
                }
                break;
            }

            let assignment = &mut assignments[index];
            let value = values.get(index);
            let register = assignment
                .conflict_register
                .or_else(|| assignment.lvalue.local_register())
                .unwrap_or(self.next_register);
            let value_register = if let Some(value) = value {
                match assignment.lvalue {
                    LValue::Local { .. } => {
                        if assignment.conflict_register.is_none()
                            && assignment_local_needs_temporary(value)
                        {
                            let scratch = self.next_register;
                            self.compile_expr_to(value, scratch)?;
                            let local = assignment
                                .lvalue
                                .local_register()
                                .expect("local lvalue has a register");
                            if local != scratch {
                                self.builder.emit(Instruction::abc(
                                    Opcode::Move,
                                    local,
                                    scratch,
                                    0,
                                ));
                            }
                            local
                        } else {
                            self.compile_expr_to(value, register)?;
                            register
                        }
                    }
                    _ => self.compile_expr_auto(value)?,
                }
            } else {
                self.builder
                    .emit(Instruction::abc(Opcode::LoadNil, register, 0, 0));
                register
            };
            assignment.value_register = Some(value_register);
        }

        for value in values.iter().skip(targets.len()) {
            self.compile_expr_side(value)?;
        }

        for assignment in &assignments {
            if !assignment.lvalue.is_local() {
                self.compile_lvalue_use(
                    &assignment.lvalue,
                    assignment
                        .value_register
                        .expect("assignment value register is populated"),
                    LvalueAccess::Set,
                )?;
            }
        }

        for (index, assignment) in assignments.iter().enumerate() {
            let LValue::Local {
                local_id, register, ..
            } = assignment.lvalue
            else {
                continue;
            };
            let value_register = assignment
                .value_register
                .expect("assignment value register is populated");
            if value_register != register {
                self.builder
                    .emit(Instruction::abc(Opcode::Move, register, value_register, 0));
            }
            if let Some(value) = values.get(index) {
                let constant = self.constant_value_expr(value)?;
                let import_path = self.local_import_path_initializer(value);
                self.set_local_value_facts(local_id, constant, import_path);
            } else if values.last().is_some_and(call_uses_multret) {
                self.set_local_value_facts(local_id, None, None);
            } else {
                self.set_local_value_facts(local_id, Some(ConstantValue::Nil), None);
            }
        }

        self.clear_scratch_registers(next_register, self.next_register);
        self.next_register = next_register;
        Ok(true)
    }

    pub(super) fn resolve_assignment_conflicts(
        &mut self,
        assignments: &mut [Assignment],
        values: &[Expr],
    ) -> Result<(), CompileError> {
        let mut assigned_locals = BTreeSet::new();
        let mut conflicts = BTreeSet::new();

        for (index, assignment) in assignments.iter().enumerate() {
            if let LValue::Local { local_id, .. } = assignment.lvalue {
                if let Some(value) = values.get(index) {
                    mark_expr_local_conflicts(value, &assigned_locals, &mut conflicts);
                }
                assigned_locals.insert(local_id);
            }
        }

        for (index, assignment) in assignments.iter().enumerate() {
            if !assignment.lvalue.is_local()
                && let Some(value) = values.get(index)
            {
                mark_expr_local_conflicts(value, &assigned_locals, &mut conflicts);
            }
        }

        for value in values.iter().skip(assignments.len()) {
            mark_expr_local_conflicts(value, &assigned_locals, &mut conflicts);
        }

        for assignment in assignments.iter() {
            assignment.lvalue.mark_local_register_conflicts(
                &assigned_locals,
                &self.local_registers,
                &mut conflicts,
            );
        }

        for assignment in assignments {
            if let LValue::Local { local_id, .. } = assignment.lvalue
                && conflicts.contains(&local_id)
            {
                assignment.conflict_register = Some(self.reserve_register()?);
            }
        }

        Ok(())
    }

    pub(super) fn compile_lvalue(&mut self, target: &Expr) -> Result<LValue, CompileError> {
        if let Some(line) = expr_line(target) {
            self.builder.set_debug_line(line);
        }

        match target {
            Expr::Local {
                local, location, ..
            } => {
                if let Some(register) = self.local_registers.get(&local.id.index()).copied() {
                    Ok(LValue::Local {
                        local_id: local.id.index(),
                        register,
                        location: *location,
                    })
                } else if local.function_depth < self.current_function_depth {
                    let upvalue = self.ensure_upvalue(local.id.index())?;
                    Ok(LValue::Upvalue {
                        upvalue,
                        location: *location,
                    })
                } else {
                    Err(CompileError::new(format!(
                        "unknown local id {} in compile_lvalue",
                        local.id.index()
                    )))
                }
            }
            Expr::Global { name, location, .. } => Ok(LValue::Global {
                name: name.as_str().to_owned(),
                location: *location,
            }),
            Expr::IndexName {
                expr,
                index,
                location,
                ..
            } => Ok(LValue::IndexName {
                table: self.compile_expr_auto(expr)?,
                name: index.as_str().to_owned(),
                location: *location,
            }),
            Expr::IndexExpr { expr, index, .. } => {
                let table = self.compile_expr_auto(expr)?;
                self.compile_lvalue_index(table, index, index.location())
            }
            _ => Err(CompileError::new(format!(
                "minimal bytecode compiler does not support assignment target {target:?}"
            ))),
        }
    }

    pub(super) fn compile_lvalue_index(
        &mut self,
        table: u8,
        index: &Expr,
        location: Option<Location>,
    ) -> Result<LValue, CompileError> {
        if let Some(key) = self.constant_value_expr(index)? {
            match key {
                ConstantValue::String(key) if self.context.optimization_level() > 0 => {
                    return Ok(LValue::IndexName {
                        table,
                        name: key,
                        location,
                    });
                }
                ConstantValue::Number(value) => {
                    if self.context.optimization_level() > 0
                        && let Some(index) = table_array_index_operand(value)
                    {
                        return Ok(LValue::IndexNumber {
                            table,
                            index,
                            location,
                        });
                    }
                }
                ConstantValue::Integer(value) => {
                    if self.context.optimization_level() > 0
                        && let Some(index) = table_array_index_operand(value as f64)
                    {
                        return Ok(LValue::IndexNumber {
                            table,
                            index,
                            location,
                        });
                    }
                }
                ConstantValue::Nil
                | ConstantValue::Bool(_)
                | ConstantValue::String(_)
                | ConstantValue::Vector { .. } => {}
            }
        }

        Ok(LValue::IndexExpr {
            table,
            index: self.compile_expr_auto(index)?,
            location,
        })
    }

    pub(super) fn compile_lvalue_use(
        &mut self,
        lvalue: &LValue,
        register: u8,
        access: LvalueAccess,
    ) -> Result<(), CompileError> {
        let set = access == LvalueAccess::Set;
        if let Some(location) = lvalue.location()
            && let Some(line) = location.begin.line.checked_add(1)
        {
            self.builder.set_debug_line(line);
        }

        match lvalue {
            LValue::Local {
                register: local, ..
            } => {
                if set {
                    if *local != register {
                        self.builder
                            .emit(Instruction::abc(Opcode::Move, *local, register, 0));
                    }
                } else {
                    self.builder
                        .emit(Instruction::abc(Opcode::Move, register, *local, 0));
                }
            }
            LValue::Upvalue { upvalue, .. } => {
                self.builder.emit(Instruction::abc(
                    if set {
                        Opcode::SetUpval
                    } else {
                        Opcode::GetUpval
                    },
                    register,
                    *upvalue,
                    0,
                ));
            }
            LValue::Global { name, .. } => {
                let constant = self.builder.add_string_constant(name);
                self.builder.emit(Instruction::abc_with_aux(
                    if set {
                        Opcode::SetGlobal
                    } else {
                        Opcode::GetGlobal
                    },
                    register,
                    0,
                    string_hash(name),
                    Some(constant),
                ));
            }
            LValue::IndexName { table, name, .. } => {
                let constant = self.builder.add_string_constant(name);
                self.builder.emit(Instruction::abc_with_aux(
                    if set {
                        Opcode::SetTableKs
                    } else {
                        Opcode::GetTableKs
                    },
                    register,
                    *table,
                    string_hash(name),
                    Some(constant),
                ));
            }
            LValue::IndexNumber { table, index, .. } => {
                self.builder.emit(Instruction::abc(
                    if set {
                        Opcode::SetTableN
                    } else {
                        Opcode::GetTableN
                    },
                    register,
                    *table,
                    *index,
                ));
            }
            LValue::IndexExpr { table, index, .. } => {
                self.builder.emit(Instruction::abc(
                    if set {
                        Opcode::SetTable
                    } else {
                        Opcode::GetTable
                    },
                    register,
                    *table,
                    *index,
                ));
            }
        }

        Ok(())
    }

    pub(super) fn compile_expr_auto(&mut self, expr: &Expr) -> Result<u8, CompileError> {
        if let Some(register) = self.local_expr_register(expr)? {
            return Ok(register);
        }

        let register = self.reserve_register()?;
        self.compile_expr_to(expr, register)?;
        Ok(register)
    }

    pub(super) fn compile_expr_side(&mut self, expr: &Expr) -> Result<(), CompileError> {
        let constant = self.constant_value_expr(expr)?;
        if self.context.optimization_level() == 0 && constant.is_some() {
            let next_register = self.next_register;
            let register = self.reserve_register()?;
            self.compile_expr_to(expr, register)?;
            self.clear_scratch_registers(next_register, self.next_register);
            self.next_register = next_register;
            return Ok(());
        }
        if self.local_expr_register(expr)?.is_some()
            || constant.is_some()
            || matches!(
                expr,
                Expr::Global { .. } | Expr::Varargs { .. } | Expr::Function { .. }
            )
        {
            return Ok(());
        }

        let next_register = self.next_register;
        let register = if matches!(
            expr,
            Expr::Binary {
                op: BinaryOp::And | BinaryOp::Or,
                ..
            }
        ) {
            next_register
        } else {
            self.reserve_register()?
        };
        self.compile_expr_to(expr, register)?;
        self.clear_scratch_registers(next_register, self.next_register);
        self.next_register = next_register;
        Ok(())
    }

    pub(super) fn compile_compound_assignment(
        &mut self,
        op: CompoundAssignOp,
        target: &Expr,
        value: &Expr,
    ) -> Result<(), CompileError> {
        let frame = self.start_register_frame();
        let lvalue = self.compile_lvalue(target)?;
        let result = if let Some(register) = lvalue.local_register() {
            register
        } else {
            self.reserve_register()?
        };

        if op == CompoundAssignOp::Concat {
            self.compile_compound_concat_assignment(&lvalue, result, value)?;
        } else {
            let Some(binary_op) = compound_assign_binary_op(op) else {
                return Err(CompileError::new(format!(
                    "minimal bytecode compiler does not support compound assignment op {op:?}"
                )));
            };
            self.compile_compound_arithmetic_assignment(&lvalue, result, binary_op, value)?;
        }

        if let LValue::Local { local_id, .. } = &lvalue {
            self.invalidate_local_value(*local_id);
        } else {
            self.compile_lvalue_use(&lvalue, result, LvalueAccess::Set)?;
        }

        self.clear_scratch_registers(frame.next_register, self.next_register);
        self.restore_register_frame(frame);
        Ok(())
    }

    pub(super) fn compile_arithmetic_from_register(
        &mut self,
        op: BinaryOp,
        left_register: u8,
        right: &Expr,
        register: u8,
        allow_k_operand: bool,
    ) -> Result<(), CompileError> {
        if allow_k_operand
            && let Some(k_opcode) = arithmetic_k_opcode(op)
            && let Some(value) = self.constant_number_operand(right)?
        {
            // The `*K` opcode carries the constant in a u8 `C` field; past 255 fall
            // through to the register path rather than truncating the index.
            let constant = self.builder.add_number(value);
            if let Ok(constant) = u8::try_from(constant) {
                self.builder.emit(Instruction::abc(
                    k_opcode,
                    register,
                    left_register,
                    constant,
                ));
                self.builder
                    .set_max_stack_size(register.max(left_register).saturating_add(1));
                return Ok(());
            }
        }

        let opcode = arithmetic_opcode(op).expect("compound arithmetic op already filtered");
        let right_register = if let Some(source) = self.local_expr_register(right)? {
            source
        } else {
            let scratch = self
                .next_register
                .max(register.max(left_register).saturating_add(1));
            self.compile_expr_to(right, scratch)?;
            scratch
        };
        self.builder.emit(Instruction::abc(
            opcode,
            register,
            left_register,
            right_register,
        ));
        self.builder.set_max_stack_size(
            register
                .max(left_register)
                .max(right_register)
                .saturating_add(1),
        );
        Ok(())
    }

    pub(super) fn compile_compound_arithmetic_assignment(
        &mut self,
        lvalue: &LValue,
        result: u8,
        op: BinaryOp,
        value: &Expr,
    ) -> Result<(), CompileError> {
        if !lvalue.is_local() {
            self.compile_lvalue_use(lvalue, result, LvalueAccess::Get)?;
        }

        self.compile_arithmetic_from_register(
            op,
            result,
            value,
            result,
            self.context.optimization_level() > 0,
        )?;
        Ok(())
    }

    pub(super) fn compile_compound_concat_assignment(
        &mut self,
        lvalue: &LValue,
        result: u8,
        value: &Expr,
    ) -> Result<(), CompileError> {
        let mut operands = vec![value];
        unroll_right_concat_operands(&mut operands);
        let count = u8::try_from(operands.len() + 1)
            .map_err(|_| CompileError::new("too many concat operands"))?;
        let first = self.reserve_registers(count)?;
        self.compile_lvalue_use(lvalue, first, LvalueAccess::Get)?;
        let last = self.compile_concat_operands(first.saturating_add(1), &operands)?;
        self.builder
            .emit(Instruction::abc(Opcode::Concat, result, first, last));
        self.builder.set_max_stack_size(last.saturating_add(1));
        Ok(())
    }

    pub(super) fn compile_concat_operands(
        &mut self,
        first_register: u8,
        operands: &[&Expr],
    ) -> Result<u8, CompileError> {
        let mut register = first_register;
        for operand in operands {
            self.compile_expr_to(operand, register)?;
            register = register.saturating_add(1);
        }
        Ok(register.saturating_sub(1))
    }

    pub(super) fn compile_concat(&mut self, expr: &Expr, register: u8) -> Result<(), CompileError> {
        let Expr::Binary {
            op: BinaryOp::Concat,
            left,
            right,
            ..
        } = expr
        else {
            return Err(CompileError::new("compile_concat called for non-concat"));
        };
        let mut operands = vec![left.as_ref(), right.as_ref()];
        unroll_right_concat_operands(&mut operands);
        let count = u8::try_from(operands.len())
            .map_err(|_| CompileError::new("too many concat operands"))?;
        let next_register = self.next_register;
        let first = next_register.max(register_add(register, 1)?);
        self.next_register = register_add(first, count)?;
        self.builder.set_max_stack_size(self.next_register);
        let last = self.compile_concat_operands(first, &operands)?;
        self.next_register = next_register;
        self.builder
            .emit(Instruction::abc(Opcode::Concat, register, first, last));
        self.builder.set_max_stack_size(last.saturating_add(1));
        Ok(())
    }

    pub(super) fn compile_interp_string(
        &mut self,
        expr: &Expr,
        register: u8,
    ) -> Result<(), CompileError> {
        self.compile_interp_string_inner(expr, register, false)
    }

    pub(super) fn compile_interp_string_return(
        &mut self,
        expr: &Expr,
        register: u8,
    ) -> Result<(), CompileError> {
        let scratch_output = register > 0 && !self.interp_string_args_are_locals(expr)?;
        self.compile_interp_string_inner(expr, register, scratch_output)
    }

    pub(super) fn compile_interp_string_inner(
        &mut self,
        expr: &Expr,
        register: u8,
        scratch_output: bool,
    ) -> Result<(), CompileError> {
        let Expr::InterpString {
            strings,
            expressions,
            ..
        } = expr
        else {
            return Err(CompileError::new(
                "compile_interp_string called for non-interpolated string",
            ));
        };

        let mut raw = String::new();
        let mut format = String::new();
        let mut dynamic_args = Vec::new();
        let mut has_dynamic_arg = false;
        for (index, string) in strings.iter().enumerate() {
            raw.push_str(string);
            push_escaped_format_literal(&mut format, string);
            let Some(expression) = expressions.get(index) else {
                continue;
            };
            if self.context.optimization_level() > 0
                && let Some(ConstantValue::String(value)) = self.constant_value_expr(expression)?
            {
                raw.push_str(&value);
                push_escaped_format_literal(&mut format, &value);
            } else {
                has_dynamic_arg = true;
                format.push_str("%*");
                dynamic_args.push(expression);
            }
        }

        if !has_dynamic_arg
            && self.context.optimization_level() > 0
            && raw.len() <= CONSTANT_STRING_FOLD_LIMIT
        {
            self.builder.set_max_stack_size(register + 1);
            let constant = self.builder.add_string_constant(&raw);
            self.emit_load_constant_index(register, constant);
            return Ok(());
        }

        let arg_operand =
            bytecode_u8_count("interpolated string argument", dynamic_args.len() + 2)?;
        let output = if scratch_output {
            self.interp_scratch_output_register(register, dynamic_args.len())?
        } else {
            self.interp_output_register(register, dynamic_args.len())?
        };
        let constant = self.builder.add_string_constant(&format);
        self.emit_load_constant_index(output, constant);
        for (index, argument) in dynamic_args.iter().enumerate() {
            self.compile_expr_to(
                argument,
                register_add(
                    output,
                    bytecode_u8_count("interpolated string argument index", index + 2)?,
                )?,
            )?;
        }
        self.set_expr_debug_line(expr);
        let name = "format";
        let name_constant = self.builder.add_string_constant(name);
        self.builder.emit(Instruction::abc_with_aux(
            Opcode::NameCall,
            output,
            output,
            string_hash(name),
            Some(name_constant),
        ));
        self.builder.emit(Instruction::abc(
            Opcode::Call,
            output,
            arg_operand,
            CallResults::Fixed(1).operand(),
        ));
        self.builder.set_max_stack_size(register_add(
            output,
            bytecode_u8_count("interpolated string argument", dynamic_args.len() + 2)?,
        )?);
        if output != register {
            self.builder
                .emit(Instruction::abc(Opcode::Move, register, output, 0));
        }
        Ok(())
    }

    pub(super) fn interp_output_register(
        &self,
        register: u8,
        dynamic_arg_count: usize,
    ) -> Result<u8, CompileError> {
        let used = bytecode_u8_count("interpolated string argument", dynamic_arg_count + 2)?;
        let end = register_add(register, used)?;
        let overlaps = self.overlaps_reserved(register_add(register, 1)?, end)
            || self
                .active_locals
                .iter()
                .any(|local| local.register > register && local.register < end);
        if !overlaps {
            return Ok(register);
        }
        let active_next = self
            .active_locals
            .iter()
            .map(|local| local.register.saturating_add(1))
            .max()
            .unwrap_or(0);
        Ok(self.next_register.max(active_next))
    }

    pub(super) fn interp_scratch_output_register(
        &self,
        register: u8,
        dynamic_arg_count: usize,
    ) -> Result<u8, CompileError> {
        let output = register_add(register, 1)?;
        let used = bytecode_u8_count("interpolated string argument", dynamic_arg_count + 2)?;
        let end = register_add(output, used)?;
        let overlaps = self.overlaps_reserved(output, end)
            || self
                .active_locals
                .iter()
                .any(|local| local.register >= output && local.register < end);
        if !overlaps {
            return Ok(output);
        }
        let active_next = self
            .active_locals
            .iter()
            .map(|local| local.register.saturating_add(1))
            .max()
            .unwrap_or(0);
        Ok(self.next_register.max(active_next).max(output))
    }

    pub(super) fn interp_string_args_are_locals(&self, expr: &Expr) -> Result<bool, CompileError> {
        let Expr::InterpString { expressions, .. } = expr else {
            return Ok(false);
        };
        for expression in expressions {
            if self.local_expr_register(expression)?.is_none() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(super) fn invalidate_local_value(&mut self, local_id: u32) {
        self.local_values.invalidate_local(local_id);
    }

    pub(super) fn try_elide_redundant_locals(
        &mut self,
        vars: &[ruau_ast::syntax::Local],
        values: &[Expr],
    ) -> Result<bool, CompileError> {
        if self.context.optimization_level() == 0 || self.context.options().debug_level > 1 {
            return Ok(false);
        }
        if vars.is_empty() || values.len() > vars.len() {
            return Ok(false);
        }
        if vars.iter().any(|local| {
            !self
                .context
                .variable(local.id)
                .is_some_and(|variable| variable.is_constant())
        }) {
            return Ok(false);
        }

        let mut constants = Vec::with_capacity(vars.len());
        for (index, local) in vars.iter().enumerate() {
            let constant = if let Some(value) = values.get(index) {
                let Some(constant) = self
                    .constant_value_expr(value)?
                    .or_else(|| self.context.local_constant(local.id).cloned())
                else {
                    return Ok(false);
                };
                constant
            } else {
                ConstantValue::Nil
            };
            constants.push((local.id.index(), constant));
        }

        for (local_id, constant) in constants {
            self.local_values.set_constant(local_id, Some(constant));
            self.local_values.set_import_path(local_id, None);
        }

        Ok(true)
    }

    pub(super) fn try_elide_local_aliases(
        &mut self,
        vars: &[ruau_ast::syntax::Local],
        values: &[Expr],
    ) -> Result<bool, CompileError> {
        if self.context.optimization_level() == 0 || self.context.options().debug_level > 1 {
            return Ok(false);
        }
        if vars.len() != 1 || values.len() != 1 {
            return Ok(false);
        }

        let mut aliases = Vec::with_capacity(vars.len());
        for (var, value) in vars.iter().zip(values) {
            if self
                .context
                .variable(var.id)
                .is_none_or(|variable| variable.is_written())
            {
                return Ok(false);
            }

            let Expr::Local { local, .. } = ungroup_expr(value) else {
                return Ok(false);
            };
            if self
                .context
                .variable(local.id)
                .is_none_or(|variable| variable.is_written())
            {
                return Ok(false);
            }
            let Some(register) = self.local_expr_register(value)? else {
                return Ok(false);
            };
            aliases.push((var, value, register));
        }

        for (var, value, register) in aliases {
            self.local_registers.insert(var.id.index(), register);
            self.active_locals.push(ActiveLocal {
                local_id: var.id.index(),
                register,
                debug_name: var.name.as_str().to_owned(),
                debug_start_pc: (self.context.options().debug_level >= 2)
                    .then(|| self.builder.current_type_info_pc()),
                captured: false,
            });
            let constant = self.constant_value_expr(value)?;
            self.local_values.set_constant(var.id.index(), constant);
            let import_path = self.local_import_path_initializer(value);
            self.local_values
                .set_import_path(var.id.index(), import_path);
        }

        Ok(true)
    }

    pub(super) fn compile_number(
        &mut self,
        value: &Number,
        register: u8,
    ) -> Result<(), CompileError> {
        self.compile_f64(number_value(value)?, register)
    }

    pub(super) fn compile_f64(&mut self, value: f64, register: u8) -> Result<(), CompileError> {
        let integer = value as i16;
        if self.context.optimization_level() > 0
            && f64::from(integer) == value
            && !is_negative_zero(value)
        {
            self.builder
                .emit(Instruction::ad(Opcode::LoadN, register, integer));
        } else {
            let constant = self.builder.add_number(value);
            self.emit_load_constant_index(register, constant);
        }
        Ok(())
    }

    pub(super) fn compile_constant_value(
        &mut self,
        value: ConstantValue,
        register: u8,
    ) -> Result<(), CompileError> {
        self.builder.set_max_stack_size(register + 1);
        match value {
            ConstantValue::Nil => {
                self.builder
                    .emit(Instruction::abc(Opcode::LoadNil, register, 0, 0));
            }
            ConstantValue::Bool(value) => {
                self.builder.emit(Instruction::abc(
                    Opcode::LoadB,
                    register,
                    u8::from(value),
                    0,
                ));
            }
            ConstantValue::Number(value) => self.compile_f64(value, register)?,
            ConstantValue::Integer(value) => {
                let constant = self.builder.add_integer(value);
                self.emit_load_constant_index(register, constant);
            }
            ConstantValue::String(value) => {
                let constant = self.builder.add_string_constant(&value);
                self.emit_load_constant_index(register, constant);
            }
            ConstantValue::Vector { bits } => {
                let constant = self.builder.add_vector_bits(bits);
                self.emit_load_constant_index(register, constant);
            }
        }
        Ok(())
    }
}
