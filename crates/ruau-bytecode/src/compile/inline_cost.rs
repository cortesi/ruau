use std::collections::BTreeMap;

use ruau_ast::syntax::{Expr, Local, Stat};

use super::{
    CompileError, ConstantValue, FunctionCompiler, LoopUnrollPlan, call_uses_multret,
    constant_truthiness, ungroup_expr,
};

#[allow(clippy::multiple_inherent_impl)]
impl FunctionCompiler {
    /// Scales `base` by the cost/baseline profit ratio, capped by the named
    /// max-boost fast int. Shared by the inline and loop-unroll budgets.
    fn boosted_threshold(&self, base: i32, cost: i32, baseline: i32, max_boost_flag: &str) -> i32 {
        let max_boost = self.context.options().fast_int(max_boost_flag).max(0);
        let profit = if cost == 0 {
            max_boost
        } else {
            max_boost.min(100 * baseline / cost)
        };
        base * profit / 100
    }

    pub(super) fn inline_cost_allows(
        &self,
        params: &[Local],
        args: &[Expr],
        body: &Stat,
    ) -> Result<bool, CompileError> {
        let mut variables = BTreeMap::new();
        for (index, param) in params.iter().take(7).enumerate() {
            variables.insert(param.id.index(), InlineCost::param_mask(index));
        }

        let mut constant_args = [false; 8];
        let mut param_constants = BTreeMap::new();
        let missing_args_are_nil = args.last().is_none_or(|arg| !call_uses_multret(arg));
        for (index, param) in params.iter().enumerate() {
            let constant = if let Some(arg) = args.get(index) {
                self.constant_value_expr(arg)?
            } else if missing_args_are_nil {
                Some(ConstantValue::Nil)
            } else {
                None
            };
            if let Some(constant) = constant {
                if index < 8 {
                    constant_args[index] = true;
                }
                param_constants.insert(param.id.index(), constant);
            }
        }

        if missing_args_are_nil {
            for constant in constant_args
                .iter_mut()
                .take(params.len().min(8))
                .skip(args.len())
            {
                *constant = true;
            }
        }

        let model = self.inline_stat_cost_model(body, &mut variables, &param_constants)?;
        let inlined_cost = model.compute(&constant_args, params.len().min(8));
        let baseline_cost = model.compute(&[false; 8], 0) + 3;
        let threshold_base = self
            .context
            .options()
            .fast_int("LuauCompileInlineThreshold")
            .max(0);
        let threshold = self.boosted_threshold(
            threshold_base,
            inlined_cost,
            baseline_cost,
            "LuauCompileInlineThresholdMaxBoost",
        );

        Ok(inlined_cost <= threshold)
    }

    pub(super) fn loop_unroll_plan(
        &self,
        var: &Local,
        from: &Expr,
        to: &Expr,
        step: Option<&Expr>,
        body: &Stat,
    ) -> Result<Option<LoopUnrollPlan>, CompileError> {
        self.loop_unroll_plan_with_constants(var, from, to, step, body, None)
    }

    pub(super) fn loop_unroll_plan_with_constants(
        &self,
        var: &Local,
        from: &Expr,
        to: &Expr,
        step: Option<&Expr>,
        body: &Stat,
        constants: Option<&BTreeMap<u32, ConstantValue>>,
    ) -> Result<Option<LoopUnrollPlan>, CompileError> {
        let Some(from) = self.numeric_constant_value(from, constants)? else {
            return Ok(None);
        };
        let Some(to) = self.numeric_constant_value(to, constants)? else {
            return Ok(None);
        };
        let step = match step {
            Some(step) => {
                let Some(step) = self.numeric_constant_value(step, constants)? else {
                    return Ok(None);
                };
                step
            }
            None => 1.0,
        };
        let Some(trip_count) = numeric_trip_count(from, to, step) else {
            return Ok(None);
        };

        let threshold_base = self
            .context
            .options()
            .fast_int("LuauCompileLoopUnrollThreshold");
        if trip_count > threshold_base {
            return Ok(None);
        }

        let mut variables = BTreeMap::new();
        variables.insert(var.id.index(), InlineCost::param_mask(0));
        let empty_constants = BTreeMap::new();
        let constants = constants.unwrap_or(&empty_constants);
        let model = self.inline_stat_cost_model(body, &mut variables, constants)?;
        let mut constant_loop_var = [false; 8];
        constant_loop_var[0] = true;
        let unrolled_cost = model.compute(&constant_loop_var, 1) * trip_count;
        let baseline_cost = (model.compute(&[false; 8], 0) + 1) * trip_count;
        let threshold = self.boosted_threshold(
            threshold_base,
            unrolled_cost,
            baseline_cost,
            "LuauCompileLoopUnrollThresholdMaxBoost",
        );

        Ok((unrolled_cost <= threshold).then_some(LoopUnrollPlan {
            trip_count,
            from,
            step,
        }))
    }

    fn numeric_constant_value(
        &self,
        expr: &Expr,
        constants: Option<&BTreeMap<u32, ConstantValue>>,
    ) -> Result<Option<f64>, CompileError> {
        let value = match constants {
            Some(constants) => self.inline_constant_value_expr(expr, constants)?,
            None => self.constant_value_expr(expr)?,
        };
        Ok(match value {
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

    fn inline_stat_cost_model(
        &self,
        stat: &Stat,
        variables: &mut BTreeMap<u32, u64>,
        constants: &BTreeMap<u32, ConstantValue>,
    ) -> Result<InlineCost, CompileError> {
        let cost = match stat {
            Stat::Block { body, .. } => {
                let mut cost = InlineCost::new(0);
                for stat in body {
                    cost.add_assign(self.inline_stat_cost_model(stat, variables, constants)?);
                    if self.inline_stat_terminates_with_constants(stat, constants)? {
                        break;
                    }
                }
                cost
            }
            Stat::Return { list, .. } => {
                let mut cost = InlineCost::new(0);
                for expr in list {
                    cost.add_assign(self.inline_expr_cost_model(expr, variables, constants)?);
                }
                cost
            }
            Stat::Expr { expr, .. } => self.inline_expr_cost_model(expr, variables, constants)?,
            Stat::Local { vars, values, .. } => {
                let mut cost = InlineCost::new(0);
                for (index, value) in values.iter().enumerate() {
                    let value_cost = self.inline_expr_cost_model(value, variables, constants)?;
                    if value_cost.constant != 0
                        && let Some(var) = vars.get(index)
                    {
                        variables.insert(var.id.index(), value_cost.constant);
                    }
                    cost.add_assign(value_cost);
                }
                cost
            }
            Stat::Assign { vars, values, .. } => {
                for var in vars {
                    if let Expr::Local { local, .. } = var {
                        variables.remove(&local.id.index());
                    }
                }

                let mut cost = InlineCost::new(0);
                for index in 0..vars.len().max(values.len()) {
                    let mut item_cost = InlineCost::new(0);
                    if let Some(var) = vars.get(index) {
                        item_cost
                            .add_assign(self.inline_expr_cost_model(var, variables, constants)?);
                    }
                    if let Some(value) = values.get(index) {
                        item_cost
                            .add_assign(self.inline_expr_cost_model(value, variables, constants)?);
                    }
                    if item_cost.is_free() {
                        cost.add_assign(InlineCost::new(1));
                    } else {
                        cost.add_assign(item_cost);
                    }
                }
                cost
            }
            Stat::CompoundAssign { var, value, .. } => {
                if let Expr::Local { local, .. } = var.as_ref() {
                    variables.remove(&local.id.index());
                }

                let mut cost = if matches!(var.as_ref(), Expr::Local { .. }) {
                    InlineCost::new(1)
                } else {
                    InlineCost::new(2)
                };
                cost.add_assign(self.inline_expr_cost_model(var, variables, constants)?);
                cost.add_assign(self.inline_expr_cost_model(value, variables, constants)?);
                cost
            }
            Stat::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                if self
                    .inline_constant_value_expr(condition, constants)?
                    .is_some_and(|value| !constant_truthiness(&value))
                {
                    if let Some(else_body) = else_body {
                        self.inline_stat_cost_model(else_body, variables, constants)?
                    } else {
                        InlineCost::new(0)
                    }
                } else if self
                    .inline_constant_value_expr(condition, constants)?
                    .is_some_and(|value| constant_truthiness(&value))
                {
                    self.inline_stat_cost_model(then_body, variables, constants)?
                } else {
                    let mut cost = self.inline_expr_cost_model(condition, variables, constants)?;
                    cost.add_assign(InlineCost::new(
                        1 + usize::from(
                            else_body
                                .as_deref()
                                .is_some_and(|body| !matches!(body, Stat::If { .. })),
                        ) as i32,
                    ));
                    cost.add_assign(self.inline_stat_cost_model(then_body, variables, constants)?);
                    if let Some(else_body) = else_body {
                        cost.add_assign(
                            self.inline_stat_cost_model(else_body, variables, constants)?,
                        );
                    }
                    cost
                }
            }
            Stat::Break { .. } | Stat::Continue { .. } => InlineCost::new(1),
            Stat::While {
                condition, body, ..
            }
            | Stat::Repeat {
                condition, body, ..
            } => self.inline_loop_cost_model(
                body,
                self.inline_expr_cost_model(condition, variables, constants)?,
                3,
                variables,
                constants,
            )?,
            Stat::For {
                from,
                to,
                step,
                body,
                ..
            } => {
                let mut setup = self.inline_expr_cost_model(from, variables, constants)?;
                setup.add_assign(self.inline_expr_cost_model(to, variables, constants)?);
                if let Some(step) = step {
                    setup.add_assign(self.inline_expr_cost_model(step, variables, constants)?);
                }
                let factor = inline_numeric_trip_count(from, to, step.as_deref()).unwrap_or(3);
                setup.add_assign(self.inline_loop_cost_model(
                    body,
                    InlineCost::new(1),
                    factor,
                    variables,
                    constants,
                )?);
                setup
            }
            Stat::ForIn { values, body, .. } => {
                let mut setup = InlineCost::new(0);
                for value in values {
                    setup.add_assign(self.inline_expr_cost_model(value, variables, constants)?);
                }
                setup.add_assign(self.inline_loop_cost_model(
                    body,
                    InlineCost::new(1),
                    3,
                    variables,
                    constants,
                )?);
                setup
            }
            Stat::Function { name, func, .. } => {
                let mut cost = self.inline_expr_cost_model(name, variables, constants)?;
                cost.add_assign(self.inline_expr_cost_model(func, variables, constants)?);
                cost
            }
            Stat::LocalFunction { name, func, .. } => {
                let cost = self.inline_expr_cost_model(func, variables, constants)?;
                variables.remove(&name.id.index());
                cost
            }
            Stat::DeclareGlobal { .. }
            | Stat::DeclareFunction { .. }
            | Stat::DeclareClass { .. }
            | Stat::TypeAlias { .. }
            | Stat::TypeFunction { .. }
            | Stat::Class { .. }
            | Stat::ClassProperty { .. }
            | Stat::Error { .. } => InlineCost::new(InlineCost::MAX_COST),
        };
        Ok(cost)
    }

    fn inline_loop_cost_model(
        &self,
        body: &Stat,
        iter_cost: InlineCost,
        factor: i32,
        variables: &mut BTreeMap<u32, u64>,
        constants: &BTreeMap<u32, ConstantValue>,
    ) -> Result<InlineCost, CompileError> {
        let body_cost = self.inline_stat_cost_model(body, variables, constants)?;
        let mut iteration = body_cost;
        iteration.add_assign(iter_cost);
        Ok(iteration.mul(factor))
    }

    fn inline_expr_cost_model(
        &self,
        expr: &Expr,
        variables: &BTreeMap<u32, u64>,
        constants: &BTreeMap<u32, ConstantValue>,
    ) -> Result<InlineCost, CompileError> {
        if self.inline_constant_value_expr(expr, constants)?.is_some() {
            return Ok(InlineCost::literal());
        }

        let cost = match expr {
            Expr::Nil { .. }
            | Expr::Bool { .. }
            | Expr::Number { .. }
            | Expr::Integer { .. }
            | Expr::String { .. } => InlineCost::literal(),
            Expr::Local { local, .. } => {
                InlineCost::with_constant(0, *variables.get(&local.id.index()).unwrap_or(&0))
            }
            Expr::Global { .. } => InlineCost::new(1),
            Expr::Varargs { .. } => InlineCost::new(3),
            Expr::Call {
                syntax_id,
                func,
                args,
                ..
            } => {
                let builtin = self.context.builtin_call(*syntax_id).is_some();
                let builtin_short = builtin
                    && args.len()
                        <= if self
                            .context
                            .options()
                            .fast_flag("LuauCompileFastcall3CostModel")
                        {
                            3
                        } else {
                            2
                        };
                let mut cost = if builtin {
                    InlineCost::new(2)
                } else {
                    InlineCost::new(3)
                };
                if !builtin {
                    cost.add_assign(self.inline_expr_cost_model(func, variables, constants)?);
                }
                for arg in args {
                    let arg_cost = self.inline_expr_cost_model(arg, variables, constants)?;
                    if arg_cost.is_free() && !builtin_short {
                        cost.add_assign(InlineCost::new(1));
                    } else {
                        cost.add_assign(arg_cost);
                    }
                }
                cost
            }
            Expr::IndexName { expr, .. } => {
                let mut cost = self.inline_expr_cost_model(expr, variables, constants)?;
                cost.add_assign(InlineCost::new(1));
                cost
            }
            Expr::IndexExpr { expr, index, .. } => {
                let mut cost = self.inline_expr_cost_model(expr, variables, constants)?;
                cost.add_assign(self.inline_expr_cost_model(index, variables, constants)?);
                cost.add_assign(InlineCost::new(1));
                cost
            }
            Expr::Function { .. } => InlineCost::new(10),
            Expr::Table { items, .. } => {
                let mut cost = InlineCost::new(10);
                for item in items {
                    if let Some(key) = &item.key {
                        cost.add_assign(self.inline_expr_cost_model(key, variables, constants)?);
                    }
                    cost.add_assign(self.inline_expr_cost_model(
                        &item.value,
                        variables,
                        constants,
                    )?);
                    cost.add_assign(InlineCost::new(1));
                }
                cost
            }
            Expr::Unary { expr, .. } => InlineCost::fold(
                self.inline_expr_cost_model(expr, variables, constants)?,
                InlineCost::literal(),
            ),
            Expr::Binary { left, right, .. } => InlineCost::fold(
                self.inline_expr_cost_model(left, variables, constants)?,
                self.inline_expr_cost_model(right, variables, constants)?,
            ),
            Expr::IfElse {
                condition,
                true_expr,
                false_expr,
                ..
            } => {
                if self
                    .inline_constant_value_expr(condition, constants)?
                    .is_some_and(|value| constant_truthiness(&value))
                {
                    self.inline_expr_cost_model(true_expr, variables, constants)?
                } else if self
                    .inline_constant_value_expr(condition, constants)?
                    .is_some_and(|value| !constant_truthiness(&value))
                {
                    self.inline_expr_cost_model(false_expr, variables, constants)?
                } else {
                    let mut cost = self.inline_expr_cost_model(condition, variables, constants)?;
                    cost.add_assign(self.inline_expr_cost_model(true_expr, variables, constants)?);
                    cost.add_assign(self.inline_expr_cost_model(false_expr, variables, constants)?);
                    cost.add_assign(InlineCost::new(2));
                    cost
                }
            }
            Expr::InterpString { expressions, .. } => {
                let mut cost = InlineCost::new(3);
                for expr in expressions {
                    cost.add_assign(self.inline_expr_cost_model(expr, variables, constants)?);
                }
                cost
            }
            Expr::Group { expr, .. }
            | Expr::TypeAssertion { expr, .. }
            | Expr::Instantiate { expr, .. } => {
                self.inline_expr_cost_model(expr, variables, constants)?
            }
            Expr::Error { .. } => InlineCost::new(InlineCost::MAX_COST),
        };
        Ok(cost)
    }
}

#[derive(Clone, Copy, Debug)]
struct InlineCost {
    model: u64,
    constant: u64,
}

impl InlineCost {
    /// Saturation ceiling of one 7-bit SWAR cost lane; also the cost
    /// assigned to unmodelable statements and expressions.
    const MAX_COST: i32 = 127;
    const LITERAL: u64 = u64::MAX;
    const LOW_BITS: u64 = 0x007f_007f_007f_007f;
    const SATURATION_BITS: u64 = 0x8000_8000_8000_8000;
    const BYTE_LANES: u64 = 0x0101_0101_0101_0101;

    fn new(cost: i32) -> Self {
        Self::with_constant(cost, 0)
    }

    fn literal() -> Self {
        Self::with_constant(0, Self::LITERAL)
    }

    fn with_constant(cost: i32, constant: u64) -> Self {
        Self {
            model: cost.clamp(0, 0x7f) as u64,
            constant,
        }
    }

    fn param_mask(index: usize) -> u64 {
        0xff_u64 << (index * 8 + 8)
    }

    fn is_free(self) -> bool {
        self.model == 0
    }

    fn add_assign(&mut self, other: Self) {
        self.model = parallel_add_sat(self.model, other.model);
        self.constant = 0;
    }

    fn mul(self, factor: i32) -> Self {
        Self {
            model: parallel_mul_sat(self.model, factor),
            constant: 0,
        }
    }

    fn fold(left: Self, right: Self) -> Self {
        let model = parallel_add_sat(left.model, right.model);
        let constant = left.constant & right.constant;
        let extra = if constant == Self::LITERAL {
            0
        } else {
            1 | (Self::BYTE_LANES & constant)
        };

        Self {
            model: parallel_add_sat(model, extra),
            constant,
        }
    }

    fn compute(self, constant_args: &[bool; 8], count: usize) -> i32 {
        let mut cost = (self.model & 0x7f) as i32;
        if cost == 0x7f {
            return cost;
        }

        for (index, constant) in constant_args.iter().take(count.min(7)).enumerate() {
            if *constant {
                cost -= ((self.model >> (index * 8 + 8)) & 0x7f) as i32;
            }
        }
        cost
    }
}

fn parallel_add_sat(left: u64, right: u64) -> u64 {
    let result = left.wrapping_add(right);
    let saturated = result & 0x8080_8080_8080_8080;
    (result ^ saturated) | saturated.wrapping_sub(saturated >> 7)
}

fn parallel_mul_sat(value: u64, factor: i32) -> u64 {
    let factor = factor.clamp(0, InlineCost::MAX_COST) as u64;
    let low = factor * (value & InlineCost::LOW_BITS);
    let high = factor * ((value >> 8) & InlineCost::LOW_BITS);

    let low_saturated = low + 0x7f80_7f80_7f80_7f80;
    let high_saturated = high + 0x7f80_7f80_7f80_7f80;
    let saturation = (high_saturated & InlineCost::SATURATION_BITS)
        | ((low_saturated & InlineCost::SATURATION_BITS) >> 8);
    let result = ((high & InlineCost::LOW_BITS) << 8) | (low & InlineCost::LOW_BITS);

    result | saturation.wrapping_sub(saturation >> 7)
}

fn inline_numeric_trip_count(from: &Expr, to: &Expr, step: Option<&Expr>) -> Option<i32> {
    let from = inline_numeric_literal(from)?;
    let to = inline_numeric_literal(to)?;
    let step = step.map_or(Some(1.0), inline_numeric_literal)?;

    numeric_trip_count(from, to, step)
}

fn inline_numeric_literal(expr: &Expr) -> Option<f64> {
    match ungroup_expr(expr) {
        Expr::Number { value, .. } => value.as_f64(),
        Expr::Integer { .. } => Some(0.0),
        _ => None,
    }
}

fn inline_bounded_integer(value: f64) -> Option<i32> {
    if (-32767.0..=32767.0).contains(&value) && value.fract() == 0.0 {
        Some(value as i32)
    } else {
        None
    }
}

fn numeric_trip_count(from: f64, to: f64, step: f64) -> Option<i32> {
    let from = inline_bounded_integer(from)?;
    let to = inline_bounded_integer(to)?;
    let step = inline_bounded_integer(step)?;
    if step == 0 {
        return None;
    }
    if (step < 0 && to > from) || (step > 0 && to < from) {
        return Some(0);
    }

    Some((to - from) / step + 1)
}
