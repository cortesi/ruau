use std::collections::{BTreeMap, BTreeSet};

use ruau_ast::{
    Location,
    json::{JsonBinaryOp, JsonCompoundAssignOp, JsonNumber, JsonTableItemKind, JsonUnaryOp},
    syntax::{Expr, LocalRef, Stat, Type},
};

use super::{
    BreakBranchKind, CompileError, ConstantValue, KnownMember, KnownMemberValue,
    LoopControlBranchKind, TypeAliasInfo,
};
use crate::{
    Instruction,
    opcodes::{
        FORGLOOP_INEXT_BIT, IMPORT_PATH_COMPONENT_BITS, IMPORT_PATH_COMPONENT_MASK,
        IMPORT_PATH_COUNT_SHIFT, Opcode, TypeTag, import_component_shift,
    },
};

pub(super) fn ungroup_expr(expr: &Expr) -> &Expr {
    match expr {
        Expr::Group { expr, .. }
        | Expr::TypeAssertion { expr, .. }
        | Expr::Instantiate { expr, .. } => ungroup_expr(expr),
        _ => expr,
    }
}

pub(super) fn contiguous_local_return_start(
    local_registers: &BTreeMap<u32, u8>,
    values: &[Expr],
) -> Option<u8> {
    if values.is_empty() {
        return Some(0);
    }

    let mut start = None;
    for (index, value) in values.iter().enumerate() {
        let register = local_return_register(local_registers, value)?;
        let first = *start.get_or_insert(register);
        let Ok(index) = u8::try_from(index) else {
            return None;
        };
        if Some(register) != first.checked_add(index) {
            return None;
        }
    }
    start
}

pub(super) fn comparison_jump_opcode(op: JsonBinaryOp, jump_when_truthy: bool) -> Option<Opcode> {
    Some(match (op, jump_when_truthy) {
        (JsonBinaryOp::CompareEq, true) => Opcode::JumpIfEq,
        (JsonBinaryOp::CompareEq, false) => Opcode::JumpIfNotEq,
        (JsonBinaryOp::CompareNe, true) => Opcode::JumpIfNotEq,
        (JsonBinaryOp::CompareNe, false) => Opcode::JumpIfEq,
        (JsonBinaryOp::CompareLt | JsonBinaryOp::CompareGt, true) => Opcode::JumpIfLt,
        (JsonBinaryOp::CompareLt | JsonBinaryOp::CompareGt, false) => Opcode::JumpIfNotLt,
        (JsonBinaryOp::CompareLe | JsonBinaryOp::CompareGe, true) => Opcode::JumpIfLe,
        (JsonBinaryOp::CompareLe | JsonBinaryOp::CompareGe, false) => Opcode::JumpIfNotLe,
        _ => return None,
    })
}

pub(super) fn logical_opcode(op: JsonBinaryOp) -> Opcode {
    match op {
        JsonBinaryOp::And => Opcode::And,
        JsonBinaryOp::Or => Opcode::Or,
        _ => unreachable!("logical operator already filtered"),
    }
}

pub(super) fn logical_k_opcode(op: JsonBinaryOp) -> Opcode {
    match op {
        JsonBinaryOp::And => Opcode::AndK,
        JsonBinaryOp::Or => Opcode::OrK,
        _ => unreachable!("logical operator already filtered"),
    }
}

pub(super) fn arithmetic_opcode(op: JsonBinaryOp) -> Option<Opcode> {
    Some(match op {
        JsonBinaryOp::Add => Opcode::Add,
        JsonBinaryOp::Sub => Opcode::Sub,
        JsonBinaryOp::Mul => Opcode::Mul,
        JsonBinaryOp::Div => Opcode::Div,
        JsonBinaryOp::FloorDiv => Opcode::IDiv,
        JsonBinaryOp::Mod => Opcode::Mod,
        JsonBinaryOp::Pow => Opcode::Pow,
        _ => return None,
    })
}

pub(super) fn arithmetic_k_opcode(op: JsonBinaryOp) -> Option<Opcode> {
    Some(match op {
        JsonBinaryOp::Add => Opcode::AddK,
        JsonBinaryOp::Sub => Opcode::SubK,
        JsonBinaryOp::Mul => Opcode::MulK,
        JsonBinaryOp::Div => Opcode::DivK,
        JsonBinaryOp::FloorDiv => Opcode::IDivK,
        JsonBinaryOp::Mod => Opcode::ModK,
        JsonBinaryOp::Pow => Opcode::PowK,
        _ => return None,
    })
}

pub(super) fn arithmetic_commuted_k_opcode(op: JsonBinaryOp) -> Option<Opcode> {
    Some(match op {
        JsonBinaryOp::Add => Opcode::AddK,
        JsonBinaryOp::Mul => Opcode::MulK,
        _ => return None,
    })
}

pub(super) fn arithmetic_rk_opcode(op: JsonBinaryOp) -> Option<Opcode> {
    Some(match op {
        JsonBinaryOp::Sub => Opcode::SubRk,
        JsonBinaryOp::Div => Opcode::DivRk,
        _ => return None,
    })
}

pub(super) fn compound_assign_binary_op(op: JsonCompoundAssignOp) -> Option<JsonBinaryOp> {
    Some(match op {
        JsonCompoundAssignOp::Add => JsonBinaryOp::Add,
        JsonCompoundAssignOp::Sub => JsonBinaryOp::Sub,
        JsonCompoundAssignOp::Mul => JsonBinaryOp::Mul,
        JsonCompoundAssignOp::Div => JsonBinaryOp::Div,
        JsonCompoundAssignOp::FloorDiv => JsonBinaryOp::FloorDiv,
        JsonCompoundAssignOp::Mod => JsonBinaryOp::Mod,
        JsonCompoundAssignOp::Pow => JsonBinaryOp::Pow,
        JsonCompoundAssignOp::Concat => return None,
    })
}

pub(super) fn local_type_allows_commuted_k(op: JsonBinaryOp, local: &LocalRef) -> bool {
    local
        .luau_type
        .as_deref()
        .is_some_and(|luau_type| type_allows_commuted_k(op, luau_type))
}

pub(super) fn type_allows_commuted_k(op: JsonBinaryOp, luau_type: &Type) -> bool {
    match primitive_type_name(luau_type) {
        Some("number" | "integer") => matches!(op, JsonBinaryOp::Add | JsonBinaryOp::Mul),
        Some("vector") => matches!(op, JsonBinaryOp::Mul),
        _ => false,
    }
}

pub(super) fn expr_is_typed_vector(expr: &Expr) -> bool {
    match expr {
        Expr::Local { local, .. } => local
            .luau_type
            .as_deref()
            .and_then(primitive_type_name)
            .is_some_and(|name| name == "vector"),
        Expr::Group { expr, .. } => expr_is_typed_vector(expr),
        Expr::TypeAssertion {
            expr, annotation, ..
        } => {
            primitive_type_name(annotation).is_some_and(|name| name == "vector")
                || expr_is_typed_vector(expr)
        }
        _ => false,
    }
}

pub(super) fn primitive_type_name(luau_type: &Type) -> Option<&str> {
    match luau_type {
        Type::Reference {
            prefix,
            name,
            parameters,
            ..
        } if prefix.is_none() && parameters.is_empty() => Some(name.as_str()),
        Type::Group { inner, .. } => primitive_type_name(inner),
        _ => None,
    }
}

pub(super) fn primitive_or_userdata_type_tag(name: &str) -> TypeTag {
    match name {
        "nil" => TypeTag::Nil,
        "boolean" => TypeTag::Boolean,
        "number" => TypeTag::Number,
        "string" => TypeTag::String,
        "table" => TypeTag::Table,
        "function" => TypeTag::Function,
        "thread" => TypeTag::Thread,
        "userdata" => TypeTag::Userdata,
        "vector" => TypeTag::Vector,
        "buffer" => TypeTag::Buffer,
        "integer" => TypeTag::Integer,
        _ => TypeTag::Userdata,
    }
}

pub(super) fn type_info_tag(luau_type: &Type) -> Option<u8> {
    let tag = bytecode_type_tag(luau_type);
    (tag != TypeTag::Any as u16 as u8).then_some(tag)
}

pub(super) fn bytecode_type_tag(luau_type: &Type) -> u8 {
    let tag = match luau_type {
        Type::Reference {
            prefix,
            name,
            parameters,
            ..
        } if prefix.is_none() && parameters.is_empty() => {
            primitive_or_userdata_type_tag(name.as_str())
        }
        Type::Optional { .. } => TypeTag::Nil,
        Type::Table { .. } => TypeTag::Table,
        Type::Function { .. } => TypeTag::Function,
        Type::Group { inner, .. } => return bytecode_type_tag(inner),
        Type::SingletonBool { .. } => TypeTag::Boolean,
        Type::SingletonString { .. } => TypeTag::String,
        Type::Union { types, .. } => {
            let mut optional = false;
            let mut tag = None;
            for ty in types {
                let current = bytecode_type_tag(ty);
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

pub(super) fn is_vector_component_name(name: &str) -> bool {
    matches!(name, "x" | "y" | "z" | "X" | "Y" | "Z")
}

pub(super) fn constant_truthiness(value: &ConstantValue) -> bool {
    !matches!(value, ConstantValue::Nil | ConstantValue::Bool(false))
}

pub(super) fn condition_truthiness_with_local_constant(
    expr: &Expr,
    local_id: u32,
    value: &ConstantValue,
) -> Option<bool> {
    match expr {
        Expr::Local { local, .. } if local.id.index() == local_id => {
            Some(constant_truthiness(value))
        }
        Expr::Unary {
            op: JsonUnaryOp::Not,
            expr,
            ..
        } => condition_truthiness_with_local_constant(expr, local_id, value).map(|truthy| !truthy),
        Expr::Group { expr, .. }
        | Expr::TypeAssertion { expr, .. }
        | Expr::Instantiate { expr, .. } => {
            condition_truthiness_with_local_constant(expr, local_id, value)
        }
        _ => None,
    }
}

pub(super) fn break_branch_kind(stat: &Stat) -> Option<BreakBranchKind> {
    match leading_statement(stat) {
        Stat::Break { .. } => Some(BreakBranchKind::Break),
        Stat::If {
            condition,
            then_body,
            else_body: None,
            ..
        } if literal_truthiness_expr(condition) == Some(true) => break_branch_kind(then_body),
        Stat::If {
            condition,
            else_body: Some(else_body),
            ..
        } if literal_truthiness_expr(condition) == Some(false) => break_branch_kind(else_body),
        Stat::While {
            condition, body, ..
        } if literal_truthiness_expr(condition) == Some(true)
            && break_branch_kind(body) == Some(BreakBranchKind::Break) =>
        {
            Some(BreakBranchKind::WhileTrueBreak)
        }
        _ => None,
    }
}

pub(super) fn loop_control_branch_kind(stat: &Stat) -> Option<LoopControlBranchKind> {
    match leading_statement(stat) {
        Stat::Break { .. } => Some(LoopControlBranchKind::Break),
        Stat::Continue { .. } => Some(LoopControlBranchKind::Continue),
        Stat::If {
            condition,
            then_body,
            else_body: None,
            ..
        } if literal_truthiness_expr(condition) == Some(true) => {
            loop_control_branch_kind(then_body)
        }
        Stat::If {
            condition,
            else_body: Some(else_body),
            ..
        } if literal_truthiness_expr(condition) == Some(false) => {
            loop_control_branch_kind(else_body)
        }
        _ => None,
    }
}

pub(super) fn else_body_is_empty(else_body: Option<&Stat>) -> bool {
    match else_body {
        None => true,
        Some(Stat::Block { body, .. }) => body.is_empty(),
        Some(_) => false,
    }
}

pub(super) fn else_if_break_branch(else_body: Option<&Stat>) -> Option<(&Expr, BreakBranchKind)> {
    let Stat::If {
        condition,
        then_body,
        else_body: None,
        ..
    } = single_statement(else_body?)
    else {
        return None;
    };
    Some((condition, break_branch_kind(then_body)?))
}

pub(super) fn single_statement(stat: &Stat) -> &Stat {
    match stat {
        Stat::Block { body, .. } if body.len() == 1 => single_statement(&body[0]),
        _ => stat,
    }
}

pub(super) fn leading_statement(stat: &Stat) -> &Stat {
    match single_statement(stat) {
        Stat::Block { body, .. } if !body.is_empty() => leading_statement(&body[0]),
        stat => stat,
    }
}

pub(super) fn contiguous_array_size(keys: &BTreeSet<u16>) -> u32 {
    let mut expected = 1u16;
    for key in keys {
        if *key != expected {
            break;
        }
        expected = expected.saturating_add(1);
    }
    u32::from(expected - 1)
}

pub(super) fn expr_is_varargs(expr: &Expr) -> bool {
    matches!(expr, Expr::Varargs { .. })
}

pub(super) fn call_uses_multret(expr: &Expr) -> bool {
    match expr {
        Expr::Call { .. } | Expr::Varargs { .. } => true,
        Expr::Instantiate { expr, .. } => call_uses_multret(expr),
        _ => false,
    }
}

pub(super) fn assignment_local_needs_temporary(expr: &Expr) -> bool {
    match expr {
        Expr::Call { .. } | Expr::Varargs { .. } => true,
        // A non-empty table constructor compiles its entries through registers above
        // its base (a list/keyed scratch slot), so building it in place at a named
        // local clobbers any live local sitting above the target. An empty table is a
        // single `NEWTABLE` into the destination and needs no temp.
        Expr::Table { items, .. } => !items.is_empty(),
        Expr::Group { expr, .. }
        | Expr::TypeAssertion { expr, .. }
        | Expr::Instantiate { expr, .. } => assignment_local_needs_temporary(expr),
        _ => false,
    }
}

/// Whether assigning `expr` to a register compiles it using register(s) strictly
/// *above* the destination as scratch — a non-empty table constructor (list/keyed
/// scratch slots) or a call (the callee/argument frame). Building such an RHS in
/// place at a named local would clobber any live local that sits above the target,
/// so the assignment paths route these through a fresh temp above all live locals
/// and `MOVE` the result down (matching upstream `compileExpr(value, reg,
/// targetTemp=false)` → `allocReg` + `MOVE`). An empty table is a lone `NEWTABLE`
/// into the destination and is excluded.
pub(super) fn assignment_value_needs_scratch_above_base(expr: &Expr) -> bool {
    match expr {
        Expr::Call { .. } => true,
        Expr::Table { items, .. } => !items.is_empty(),
        Expr::Group { expr, .. }
        | Expr::TypeAssertion { expr, .. }
        | Expr::Instantiate { expr, .. } => assignment_value_needs_scratch_above_base(expr),
        _ => false,
    }
}

pub(super) fn register_add(register: u8, count: u8) -> Result<u8, CompileError> {
    register
        .checked_add(count)
        .ok_or_else(|| CompileError::new("bytecode compiler exhausted register space"))
}

pub(super) fn bytecode_u8_count(label: &str, count: usize) -> Result<u8, CompileError> {
    u8::try_from(count)
        .map_err(|_| CompileError::new(format!("{label} count {count} exceeds u8 bytecode limit")))
}

pub(super) fn bytecode_count_operand(label: &str, count: usize) -> Result<u8, CompileError> {
    let operand = count
        .checked_add(1)
        .ok_or_else(|| CompileError::new(format!("{label} count {count} overflows usize")))?;
    u8::try_from(operand).map_err(|_| {
        CompileError::new(format!(
            "{label} count {count} exceeds count-plus-one bytecode limit"
        ))
    })
}

pub(super) fn bytecode_fixed_count(label: &str, count: usize) -> Result<u8, CompileError> {
    let _ = bytecode_count_operand(label, count)?;
    bytecode_u8_count(label, count)
}

pub(super) fn register_at(base: u8, offset: usize, label: &str) -> Result<u8, CompileError> {
    register_add(base, bytecode_u8_count(label, offset)?)
}

pub(super) fn register_span_end(base: u8, count: usize, label: &str) -> Result<u8, CompileError> {
    register_add(base, bytecode_u8_count(label, count)?)
}

pub(super) fn table_list_register_span(items: &[ruau_ast::syntax::TableItem]) -> u8 {
    let list_count = items
        .iter()
        .filter(|item| {
            matches!(item.kind, JsonTableItemKind::Item) && !expr_is_varargs(&item.value)
        })
        .count();
    let span = if items.last().is_some_and(|item| {
        matches!(item.kind, JsonTableItemKind::Item) && expr_is_varargs(&item.value)
    }) {
        list_count
    } else {
        list_count.min(16)
    };
    span.min(usize::from(u8::MAX)) as u8
}

pub(super) fn constant_table_key_local_elision(
    stat: &Stat,
    next: Option<&Stat>,
) -> Result<Option<Vec<(u32, ConstantValue)>>, CompileError> {
    let Stat::Local { vars, values, .. } = stat else {
        return Ok(None);
    };
    if vars.is_empty() || vars.len() != values.len() {
        return Ok(None);
    }

    let mut constants = Vec::new();
    for (var, value) in vars.iter().zip(values.iter()) {
        let Some(constant) = constant_value_expr(value, &[], None, None)? else {
            return Ok(None);
        };
        constants.push((var.id.index(), constant));
    }

    let Some(Stat::Return { list, .. }) = next else {
        return Ok(None);
    };
    let [Expr::Table { items, .. }] = list.as_slice() else {
        return Ok(None);
    };

    let local_ids = vars
        .iter()
        .map(|var| var.id.index())
        .collect::<BTreeSet<_>>();
    let mut used_as_keys = BTreeSet::new();
    for item in items {
        if expr_mentions_any_local(&item.value, &local_ids) {
            return Ok(None);
        }
        if let Some(key) = &item.key {
            if let Some(local_id) = local_key_id(key)
                && local_ids.contains(&local_id)
            {
                used_as_keys.insert(local_id);
                continue;
            }
            if expr_mentions_any_local(key, &local_ids) {
                return Ok(None);
            }
        }
    }

    if used_as_keys == local_ids {
        Ok(Some(constants))
    } else {
        Ok(None)
    }
}

pub(super) fn elided_constant_local_initializer(stat: &Stat, next: Option<&Stat>) -> Option<u32> {
    let Stat::Local { vars, values, .. } = stat else {
        return None;
    };
    let [var] = vars.as_slice() else {
        return None;
    };
    let [value] = values.as_slice() else {
        return None;
    };
    static_short_circuit_constant(value)?;

    let Some(Stat::Return { list, .. }) = next else {
        return None;
    };
    let [
        Expr::Binary {
            op, left, right, ..
        },
    ] = list.as_slice()
    else {
        return None;
    };
    if !matches!(op, JsonBinaryOp::And | JsonBinaryOp::Or) {
        return None;
    }

    let local_id = var.id.index();
    (expr_is_local(right, local_id) && !expr_mentions_local(left, local_id)).then_some(local_id)
}

pub(super) fn static_short_circuit_constant(expr: &Expr) -> Option<ConstantValue> {
    match expr {
        Expr::Nil { .. } => Some(ConstantValue::Nil),
        Expr::Bool { value, .. } => Some(ConstantValue::Bool(*value)),
        Expr::Group { expr, .. }
        | Expr::TypeAssertion { expr, .. }
        | Expr::Instantiate { expr, .. } => static_short_circuit_constant(expr),
        _ => None,
    }
}

pub(super) fn expr_is_local(expr: &Expr, local_id: u32) -> bool {
    match expr {
        Expr::Local { local, .. } => local.id.index() == local_id,
        Expr::Group { expr, .. }
        | Expr::TypeAssertion { expr, .. }
        | Expr::Instantiate { expr, .. } => expr_is_local(expr, local_id),
        _ => false,
    }
}

pub(super) fn local_key_id(expr: &Expr) -> Option<u32> {
    match expr {
        Expr::Local { local, .. } => Some(local.id.index()),
        Expr::Group { expr, .. }
        | Expr::TypeAssertion { expr, .. }
        | Expr::Instantiate { expr, .. } => local_key_id(expr),
        _ => None,
    }
}

pub(super) fn expr_mentions_any_local(expr: &Expr, local_ids: &BTreeSet<u32>) -> bool {
    local_ids
        .iter()
        .any(|local_id| expr_mentions_local(expr, *local_id))
}

pub(super) fn mark_expr_local_conflicts(
    expr: &Expr,
    assigned_locals: &BTreeSet<u32>,
    conflicts: &mut BTreeSet<u32>,
) {
    for local_id in assigned_locals {
        if expr_mentions_local(expr, *local_id) {
            conflicts.insert(*local_id);
        }
    }
}

pub(super) fn mark_register_local_conflict(
    register: u8,
    assigned_locals: &BTreeSet<u32>,
    local_registers: &BTreeMap<u32, u8>,
    conflicts: &mut BTreeSet<u32>,
) {
    for local_id in assigned_locals {
        if local_registers.get(local_id) == Some(&register) {
            conflicts.insert(*local_id);
        }
    }
}

pub(super) fn same_local_expr(left: &Expr, right: &Expr) -> bool {
    local_expr_id(left).is_some_and(|left| local_expr_id(right) == Some(left))
}

pub(super) fn local_expr_id(expr: &Expr) -> Option<u32> {
    match expr {
        Expr::Local { local, .. } => Some(local.id.index()),
        Expr::Group { expr, .. }
        | Expr::TypeAssertion { expr, .. }
        | Expr::Instantiate { expr, .. } => local_expr_id(expr),
        _ => None,
    }
}

pub(super) fn local_expr_local_id(expr: &Expr) -> Option<ruau_ast::syntax::LocalId> {
    match expr {
        Expr::Local { local, .. } => Some(local.id),
        Expr::Group { expr, .. }
        | Expr::TypeAssertion { expr, .. }
        | Expr::Instantiate { expr, .. } => local_expr_local_id(expr),
        _ => None,
    }
}

pub(super) fn unroll_right_concat_operands(operands: &mut Vec<&Expr>) {
    while operands.last().is_some_and(|expr| {
        matches!(
            *expr,
            Expr::Binary {
                op: JsonBinaryOp::Concat,
                ..
            }
        )
    }) {
        let Some(Expr::Binary { left, right, .. }) = operands.pop() else {
            unreachable!("last operand was checked as concat");
        };
        operands.push(left.as_ref());
        operands.push(right.as_ref());
    }
}

pub(super) fn push_escaped_format_literal(target: &mut String, value: &str) {
    for ch in value.chars() {
        if ch == '%' {
            target.push('%');
        }
        target.push(ch);
    }
}

pub(super) fn expr_mentions_local(expr: &Expr, local_id: u32) -> bool {
    match expr {
        Expr::Local { local, .. } => local.id.index() == local_id,
        Expr::Group { expr, .. }
        | Expr::TypeAssertion { expr, .. }
        | Expr::Instantiate { expr, .. } => expr_mentions_local(expr, local_id),
        Expr::Binary { left, right, .. } => {
            expr_mentions_local(left, local_id) || expr_mentions_local(right, local_id)
        }
        Expr::Unary { expr, .. } | Expr::IndexName { expr, .. } => {
            expr_mentions_local(expr, local_id)
        }
        Expr::IfElse {
            condition,
            true_expr,
            false_expr,
            ..
        } => {
            expr_mentions_local(condition, local_id)
                || expr_mentions_local(true_expr, local_id)
                || expr_mentions_local(false_expr, local_id)
        }
        Expr::Call { func, args, .. } => {
            expr_mentions_local(func, local_id)
                || args.iter().any(|arg| expr_mentions_local(arg, local_id))
        }
        Expr::IndexExpr { expr, index, .. } => {
            expr_mentions_local(expr, local_id) || expr_mentions_local(index, local_id)
        }
        Expr::Table { items, .. } => items.iter().any(|item| {
            item.key
                .as_ref()
                .is_some_and(|key| expr_mentions_local(key, local_id))
                || expr_mentions_local(&item.value, local_id)
        }),
        Expr::Function { body, .. } => stat_mentions_local(body, local_id),
        _ => false,
    }
}

pub(super) fn stat_mentions_local(stat: &Stat, local_id: u32) -> bool {
    match stat {
        Stat::Block { body, .. } => body.iter().any(|stat| stat_mentions_local(stat, local_id)),
        Stat::Return { list, .. } => list.iter().any(|expr| expr_mentions_local(expr, local_id)),
        Stat::Local { values, .. } => values
            .iter()
            .any(|expr| expr_mentions_local(expr, local_id)),
        Stat::Assign { vars, values, .. } => vars
            .iter()
            .chain(values.iter())
            .any(|expr| expr_mentions_local(expr, local_id)),
        Stat::CompoundAssign { var, value, .. } => {
            expr_mentions_local(var, local_id) || expr_mentions_local(value, local_id)
        }
        Stat::Expr { expr, .. } => expr_mentions_local(expr, local_id),
        Stat::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            expr_mentions_local(condition, local_id)
                || stat_mentions_local(then_body, local_id)
                || else_body
                    .as_deref()
                    .is_some_and(|body| stat_mentions_local(body, local_id))
        }
        _ => false,
    }
}

pub(super) fn stat_assigns_local(stat: &Stat, local_id: u32) -> bool {
    match stat {
        Stat::Block { body, .. } => body.iter().any(|stat| stat_assigns_local(stat, local_id)),
        Stat::Assign { vars, .. } => vars.iter().any(|var| expr_is_local(var, local_id)),
        Stat::CompoundAssign { var, .. } => expr_is_local(var, local_id),
        Stat::If {
            then_body,
            else_body,
            ..
        } => {
            stat_assigns_local(then_body, local_id)
                || else_body
                    .as_deref()
                    .is_some_and(|body| stat_assigns_local(body, local_id))
        }
        Stat::While { body, .. }
        | Stat::Repeat { body, .. }
        | Stat::For { body, .. }
        | Stat::ForIn { body, .. } => stat_assigns_local(body, local_id),
        _ => false,
    }
}

/// Finds upstream's rejected `repeat`/`continue`/`until` shape: a local that the
/// condition reads but a `continue` jumps over. Returns the 1-based condition
/// line and the failure message (without a location prefix), so the strict
/// channel keeps a structured location and the wire channel renders its
/// `:<line>: <message>` form from the same data.
pub(super) fn repeat_continue_condition_error(stat: &Stat) -> Option<(u32, String)> {
    if let Stat::Repeat {
        condition, body, ..
    } = stat
        && let Some((condition_line, local_name, continue_line)) =
            repeat_skipped_condition_local(body, condition)
    {
        return Some((
            condition_line,
            format!(
                "Local {local_name} used in the repeat..until condition is undefined because continue statement on line {continue_line} jumps over it"
            ),
        ));
    }

    match stat {
        Stat::Block { body, .. } => body.iter().find_map(repeat_continue_condition_error),
        Stat::Local { values, .. } => values.iter().find_map(repeat_continue_condition_error_expr),
        Stat::LocalFunction { func, .. } => repeat_continue_condition_error_expr(func),
        Stat::Function { name, func, .. } => repeat_continue_condition_error_expr(name)
            .or_else(|| repeat_continue_condition_error_expr(func)),
        Stat::Assign { vars, values, .. } => vars
            .iter()
            .chain(values.iter())
            .find_map(repeat_continue_condition_error_expr),
        Stat::CompoundAssign { var, value, .. } => repeat_continue_condition_error_expr(var)
            .or_else(|| repeat_continue_condition_error_expr(value)),
        Stat::Return { list, .. } => list.iter().find_map(repeat_continue_condition_error_expr),
        Stat::Expr { expr, .. } => repeat_continue_condition_error_expr(expr),
        Stat::If {
            condition,
            then_body,
            else_body,
            ..
        } => repeat_continue_condition_error_expr(condition)
            .or_else(|| repeat_continue_condition_error(then_body))
            .or_else(|| {
                else_body
                    .as_deref()
                    .and_then(repeat_continue_condition_error)
            }),
        Stat::While {
            condition, body, ..
        }
        | Stat::Repeat {
            condition, body, ..
        } => repeat_continue_condition_error_expr(condition)
            .or_else(|| repeat_continue_condition_error(body)),
        Stat::For {
            from,
            to,
            step,
            body,
            ..
        } => repeat_continue_condition_error_expr(from)
            .or_else(|| repeat_continue_condition_error_expr(to))
            .or_else(|| {
                step.as_deref()
                    .and_then(repeat_continue_condition_error_expr)
            })
            .or_else(|| repeat_continue_condition_error(body)),
        Stat::ForIn { values, body, .. } => values
            .iter()
            .find_map(repeat_continue_condition_error_expr)
            .or_else(|| repeat_continue_condition_error(body)),
        Stat::TypeAlias { .. } | Stat::Break { .. } | Stat::Continue { .. } => None,
        _ => None,
    }
}

pub(super) fn repeat_continue_condition_error_expr(expr: &Expr) -> Option<(u32, String)> {
    match expr {
        Expr::Group { expr, .. }
        | Expr::TypeAssertion { expr, .. }
        | Expr::Instantiate { expr, .. } => repeat_continue_condition_error_expr(expr),
        Expr::Binary { left, right, .. }
        | Expr::IndexExpr {
            expr: left,
            index: right,
            ..
        } => repeat_continue_condition_error_expr(left)
            .or_else(|| repeat_continue_condition_error_expr(right)),
        Expr::Unary { expr, .. } | Expr::IndexName { expr, .. } => {
            repeat_continue_condition_error_expr(expr)
        }
        Expr::IfElse {
            condition,
            true_expr,
            false_expr,
            ..
        } => repeat_continue_condition_error_expr(condition)
            .or_else(|| repeat_continue_condition_error_expr(true_expr))
            .or_else(|| repeat_continue_condition_error_expr(false_expr)),
        Expr::Call { func, args, .. } => repeat_continue_condition_error_expr(func)
            .or_else(|| args.iter().find_map(repeat_continue_condition_error_expr)),
        Expr::Table { items, .. } => items.iter().find_map(|item| {
            item.key
                .as_ref()
                .and_then(repeat_continue_condition_error_expr)
                .or_else(|| repeat_continue_condition_error_expr(&item.value))
        }),
        Expr::Function { body, .. } => repeat_continue_condition_error(body),
        _ => None,
    }
}

pub(super) fn repeat_skipped_condition_local(
    body: &Stat,
    condition: &Expr,
) -> Option<(u32, String, u32)> {
    let Stat::Block { body, .. } = body else {
        return None;
    };
    let mut continue_line = None;
    for stat in body {
        if continue_line.is_none() {
            continue_line = current_loop_continue_line(stat);
        }
        let Some(line) = continue_line else {
            continue;
        };
        if let Stat::Local { vars, .. } = stat {
            for var in vars {
                if expr_mentions_local(condition, var.id.index()) {
                    return Some((
                        expr_line(condition).unwrap_or(0) + 1,
                        var.name.as_str().to_owned(),
                        line,
                    ));
                }
            }
        }
    }
    None
}

pub(super) fn current_loop_continue_line(stat: &Stat) -> Option<u32> {
    match stat {
        Stat::Continue { location } => Some(location.map(|location| location.begin.line + 1)?),
        Stat::Block { body, .. } => body.iter().find_map(current_loop_continue_line),
        Stat::If {
            then_body,
            else_body,
            ..
        } => current_loop_continue_line(then_body)
            .or_else(|| else_body.as_deref().and_then(current_loop_continue_line)),
        Stat::While { .. }
        | Stat::Repeat { .. }
        | Stat::For { .. }
        | Stat::ForIn { .. }
        | Stat::LocalFunction { .. }
        | Stat::Function { .. } => None,
        _ => None,
    }
}

pub(super) fn local_return_register(
    local_registers: &BTreeMap<u32, u8>,
    value: &Expr,
) -> Option<u8> {
    match value {
        Expr::Local { local, .. } => local_registers.get(&local.id.index()).copied(),
        Expr::Group { expr, .. }
        | Expr::TypeAssertion { expr, .. }
        | Expr::Instantiate { expr, .. } => local_return_register(local_registers, expr),
        _ => None,
    }
}

pub(super) fn function_statement_debug_name(name: &Expr) -> &str {
    match name {
        Expr::Global { name, .. } => name.as_str(),
        Expr::IndexName { index, .. } => index.as_str(),
        Expr::Group { expr, .. }
        | Expr::TypeAssertion { expr, .. }
        | Expr::Instantiate { expr, .. } => function_statement_debug_name(expr),
        _ => "",
    }
}

pub(super) fn constant_number_expr(expr: &Expr) -> Result<Option<f64>, CompileError> {
    Ok(match expr {
        Expr::Number { value, .. } => Some(number_value(value)?),
        Expr::Group { expr, .. }
        | Expr::TypeAssertion { expr, .. }
        | Expr::Instantiate { expr, .. } => constant_number_expr(expr)?,
        Expr::Unary {
            op: JsonUnaryOp::Len,
            expr,
            ..
        } => constant_string_expr(expr)?.map(|value| value.len() as f64),
        Expr::Unary {
            op: JsonUnaryOp::Minus,
            expr,
            ..
        } => constant_number_expr(expr)?.map(|value| -value),
        Expr::IfElse {
            condition,
            true_expr,
            false_expr,
            ..
        } => {
            let Some(truthy) = literal_truthiness_expr(condition) else {
                return Ok(None);
            };
            constant_number_expr(if truthy { true_expr } else { false_expr })?
        }
        Expr::Binary {
            op, left, right, ..
        } => {
            let Some(left) = constant_number_expr(left)? else {
                return Ok(None);
            };
            let Some(right) = constant_number_expr(right)? else {
                return Ok(None);
            };
            Some(match op {
                JsonBinaryOp::Add => left + right,
                JsonBinaryOp::Sub => left - right,
                JsonBinaryOp::Mul => left * right,
                JsonBinaryOp::Div => left / right,
                JsonBinaryOp::FloorDiv => (left / right).floor(),
                JsonBinaryOp::Mod => luau_fold_mod(left, right),
                JsonBinaryOp::Pow => left.powf(right),
                _ => return Ok(None),
            })
        }
        _ => None,
    })
}

pub(super) fn constant_string_expr(expr: &Expr) -> Result<Option<&str>, CompileError> {
    // A byte-string literal is stored with the `U+FFFF` byte-preservation marker (see the
    // builder's `decode_ast_string_bytes`); its char/byte length is not the decoded Luau byte
    // length, so a length fold over the marker form would be wrong. Decline to fold such a
    // constant — it falls to a runtime op over the correctly decoded constant instead.
    Ok(match expr {
        Expr::String { value, .. } => byte_literal_constant(value),
        Expr::InterpString {
            strings,
            expressions,
            ..
        } if expressions.is_empty() && strings.len() == 1 => byte_literal_constant(&strings[0]),
        Expr::Group { expr, .. }
        | Expr::TypeAssertion { expr, .. }
        | Expr::Instantiate { expr, .. } => constant_string_expr(expr)?,
        _ => None,
    })
}

/// A string-literal value usable for constant folding, or `None` if it carries the byte
/// preservation marker (a non-ASCII Luau byte) and must be handled at runtime.
fn byte_literal_constant(value: &str) -> Option<&str> {
    (!value.contains('\u{ffff}')).then_some(value)
}

pub(super) fn constant_bool_expr(
    expr: &Expr,
    known_members: &[KnownMember],
    vector_lib: Option<&str>,
    vector_ctor: Option<&str>,
) -> Result<Option<bool>, CompileError> {
    Ok(match expr {
        Expr::Binary {
            op, left, right, ..
        } => {
            let Some(left) = constant_value_expr(left, known_members, vector_lib, vector_ctor)?
            else {
                return Ok(None);
            };
            let Some(right) = constant_value_expr(right, known_members, vector_lib, vector_ctor)?
            else {
                return Ok(None);
            };
            Some(match op {
                JsonBinaryOp::CompareEq => left == right,
                JsonBinaryOp::CompareNe => left != right,
                JsonBinaryOp::CompareLt => match (left, right) {
                    (ConstantValue::Number(left), ConstantValue::Number(right)) => left < right,
                    _ => return Ok(None),
                },
                JsonBinaryOp::CompareLe => match (left, right) {
                    (ConstantValue::Number(left), ConstantValue::Number(right)) => left <= right,
                    _ => return Ok(None),
                },
                JsonBinaryOp::CompareGt => match (left, right) {
                    (ConstantValue::Number(left), ConstantValue::Number(right)) => left > right,
                    _ => return Ok(None),
                },
                JsonBinaryOp::CompareGe => match (left, right) {
                    (ConstantValue::Number(left), ConstantValue::Number(right)) => left >= right,
                    _ => return Ok(None),
                },
                _ => return Ok(None),
            })
        }
        _ => None,
    })
}

pub(super) fn compare_constant_values(
    op: JsonBinaryOp,
    left: ConstantValue,
    right: ConstantValue,
) -> Result<Option<bool>, CompileError> {
    Ok(match op {
        JsonBinaryOp::CompareEq => Some(left == right),
        JsonBinaryOp::CompareNe => Some(left != right),
        JsonBinaryOp::CompareLt => match (left, right) {
            (ConstantValue::Number(left), ConstantValue::Number(right)) => Some(left < right),
            _ => None,
        },
        JsonBinaryOp::CompareLe => match (left, right) {
            (ConstantValue::Number(left), ConstantValue::Number(right)) => Some(left <= right),
            _ => None,
        },
        JsonBinaryOp::CompareGt => match (left, right) {
            (ConstantValue::Number(left), ConstantValue::Number(right)) => Some(left > right),
            _ => None,
        },
        JsonBinaryOp::CompareGe => match (left, right) {
            (ConstantValue::Number(left), ConstantValue::Number(right)) => Some(left >= right),
            _ => None,
        },
        _ => {
            return Err(CompileError::new(
                "compare_constant_values requires a comparison operator",
            ));
        }
    })
}

/// Luau's floored modulo for the constant folder: `a - floor(a / b) * b`,
/// matching upstream `ConstantFolding.cpp` so a folded `a % b` agrees with the
/// runtime's `luai_nummod`. Rust's `%` is truncated (sign of the dividend), so
/// `-7 % 3` would wrongly fold to `-1` instead of `2`.
pub(super) fn luau_fold_mod(left: f64, right: f64) -> f64 {
    left - (left / right).floor() * right
}

pub(super) fn constant_arithmetic_value(
    op: JsonBinaryOp,
    left: &ConstantValue,
    right: &ConstantValue,
) -> Result<Option<ConstantValue>, CompileError> {
    let Some(left) = constant_value_as_number(left) else {
        return Ok(None);
    };
    let Some(right) = constant_value_as_number(right) else {
        return Ok(None);
    };
    Ok(Some(ConstantValue::Number(match op {
        JsonBinaryOp::Add => left + right,
        JsonBinaryOp::Sub => left - right,
        JsonBinaryOp::Mul => left * right,
        JsonBinaryOp::Div => left / right,
        JsonBinaryOp::FloorDiv => (left / right).floor(),
        JsonBinaryOp::Mod => luau_fold_mod(left, right),
        JsonBinaryOp::Pow => left.powf(right),
        _ => {
            return Err(CompileError::new(
                "constant_arithmetic_value requires an arithmetic operator",
            ));
        }
    })))
}

pub(super) fn constant_value_as_number(value: &ConstantValue) -> Option<f64> {
    match value {
        ConstantValue::Number(value) => Some(*value),
        // This revision's integers do not participate in arithmetic, and no
        // opcode produces one; coercing an integer operand here would fold an
        // expression the runtime rejects into a number constant. Refuse it, as
        // the runtime and `analysis::numeric_binary` do.
        ConstantValue::Integer(_)
        | ConstantValue::Nil
        | ConstantValue::Bool(_)
        | ConstantValue::String(_)
        | ConstantValue::Vector { .. } => None,
    }
}

pub(super) fn push_constant_display(out: &mut String, value: &ConstantValue) {
    match value {
        ConstantValue::Nil => out.push_str("nil"),
        ConstantValue::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
        ConstantValue::Number(value) => {
            if value.fract() == 0.0 {
                out.push_str(&format!("{value:.0}"));
            } else {
                out.push_str(&value.to_string());
            }
        }
        ConstantValue::Integer(value) => out.push_str(&value.to_string()),
        ConstantValue::String(value) => out.push_str(value),
        ConstantValue::Vector { .. } => {}
    }
}

pub(super) fn constant_value_expr(
    expr: &Expr,
    known_members: &[KnownMember],
    vector_lib: Option<&str>,
    vector_ctor: Option<&str>,
) -> Result<Option<ConstantValue>, CompileError> {
    Ok(match expr {
        Expr::Nil { .. } => Some(ConstantValue::Nil),
        Expr::Bool { value, .. } => Some(ConstantValue::Bool(*value)),
        Expr::Number { value, .. } => Some(ConstantValue::Number(number_value(value)?)),
        Expr::Integer { value, .. } => Some(ConstantValue::Integer(*value)),
        Expr::String { value, .. } => Some(ConstantValue::String(value.clone())),
        Expr::Unary {
            op: JsonUnaryOp::Not,
            expr,
            ..
        } => constant_value_expr(expr, known_members, vector_lib, vector_ctor)?
            .map(|value| ConstantValue::Bool(!constant_truthiness(&value))),
        Expr::InterpString {
            strings,
            expressions,
            ..
        } if expressions.is_empty() && strings.len() == 1 => {
            Some(ConstantValue::String(strings[0].clone()))
        }
        Expr::Binary {
            op, left, right, ..
        } if arithmetic_opcode(*op).is_some() => {
            let Some(left) = constant_value_expr(left, known_members, vector_lib, vector_ctor)?
            else {
                return Ok(None);
            };
            let Some(right) = constant_value_expr(right, known_members, vector_lib, vector_ctor)?
            else {
                return Ok(None);
            };
            constant_arithmetic_value(*op, &left, &right)?
        }
        Expr::IndexName { expr, index, .. } => {
            known_member_constant(expr, index.as_str(), known_members)?
        }
        Expr::Call { func, args, .. } => {
            vector_constructor_constant(func, args, vector_lib, vector_ctor)?
        }
        Expr::IfElse {
            condition,
            true_expr,
            false_expr,
            ..
        } => {
            let Some(truthy) = literal_truthiness_expr(condition) else {
                return Ok(None);
            };
            constant_value_expr(
                if truthy { true_expr } else { false_expr },
                known_members,
                vector_lib,
                vector_ctor,
            )?
        }
        Expr::Group { expr, .. }
        | Expr::TypeAssertion { expr, .. }
        | Expr::Instantiate { expr, .. } => {
            constant_value_expr(expr, known_members, vector_lib, vector_ctor)?
        }
        _ => None,
    })
}

pub(super) fn literal_truthiness_expr(expr: &Expr) -> Option<bool> {
    match expr {
        Expr::Nil { .. } => Some(false),
        Expr::Bool { value, .. } => Some(*value),
        Expr::Number { .. } | Expr::Integer { .. } | Expr::String { .. } => Some(true),
        Expr::InterpString { expressions, .. } if expressions.is_empty() => Some(true),
        Expr::Group { expr, .. }
        | Expr::TypeAssertion { expr, .. }
        | Expr::Instantiate { expr, .. } => literal_truthiness_expr(expr),
        _ => None,
    }
}

pub(super) fn known_member_constant(
    expr: &Expr,
    member: &str,
    known_members: &[KnownMember],
) -> Result<Option<ConstantValue>, CompileError> {
    let Expr::Global { name, .. } = expr else {
        return Ok(None);
    };
    let library = name.as_str();
    Ok(known_members
        .iter()
        .find(|known| known.library == library && known.member == member)
        .map(|known| known_member_value_to_constant(&known.value)))
}

pub(super) fn known_member_value_to_constant(value: &KnownMemberValue) -> ConstantValue {
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

pub(super) fn vector_constructor_constant(
    func: &Expr,
    args: &[Expr],
    vector_lib: Option<&str>,
    vector_ctor: Option<&str>,
) -> Result<Option<ConstantValue>, CompileError> {
    let Expr::IndexName {
        expr,
        index: member,
        ..
    } = func
    else {
        return Ok(None);
    };
    let Expr::Global { name: library, .. } = expr.as_ref() else {
        return Ok(None);
    };

    let configured_vector = vector_lib.is_some_and(|name| library.as_str() == name)
        && vector_ctor.is_some_and(|name| member.as_str() == name);
    if !configured_vector {
        return Ok(None);
    }
    if !(2..=4).contains(&args.len()) {
        return Ok(None);
    }

    let mut values = [0.0f32; 4];
    for (index, arg) in args.iter().enumerate() {
        let Some(value) = constant_number_expr(arg)? else {
            return Ok(None);
        };
        values[index] = value as f32;
    }

    Ok(Some(ConstantValue::Vector {
        bits: values.map(f32::to_bits),
    }))
}

pub(super) fn stat_line(stat: &Stat) -> Option<u32> {
    let location = stat.location()?;
    Some(location.begin.line + 1)
}

pub(super) fn collect_type_aliases_for_location(
    stat: &Stat,
    location: Option<Location>,
    aliases: &mut BTreeMap<String, TypeAliasInfo>,
) {
    let Some(target) = location else {
        return;
    };
    match stat {
        Stat::Block { body, .. } => collect_type_aliases_in_body(body, target, aliases),
        other => {
            if stat_contains_location(other, target) {
                collect_type_aliases_in_child_stat(other, target, aliases);
            }
        }
    }
}

pub(super) fn collect_type_aliases_in_body(
    body: &[Stat],
    target: Location,
    aliases: &mut BTreeMap<String, TypeAliasInfo>,
) {
    for stat in body {
        if let Stat::TypeAlias {
            name,
            generics,
            value,
            ..
        } = stat
        {
            aliases.insert(
                name.as_str().to_owned(),
                TypeAliasInfo {
                    generics: generics
                        .iter()
                        .map(|generic| generic.name.as_str().to_owned())
                        .collect(),
                    value: value.as_ref().clone(),
                },
            );
        }
    }
    for stat in body {
        if stat_contains_location(stat, target) {
            collect_type_aliases_in_child_stat(stat, target, aliases);
        }
    }
}

pub(super) fn collect_type_aliases_in_child_stat(
    stat: &Stat,
    target: Location,
    aliases: &mut BTreeMap<String, TypeAliasInfo>,
) {
    match stat {
        Stat::Block { body, .. } => collect_type_aliases_in_body(body, target, aliases),
        Stat::If {
            then_body,
            else_body,
            ..
        } => {
            if stat_contains_location(then_body, target) {
                collect_type_aliases_in_child_stat(then_body, target, aliases);
            }
            if let Some(else_body) = else_body.as_deref()
                && stat_contains_location(else_body, target)
            {
                collect_type_aliases_in_child_stat(else_body, target, aliases);
            }
        }
        Stat::While { body, .. }
        | Stat::For { body, .. }
        | Stat::ForIn { body, .. }
        | Stat::Repeat { body, .. } => {
            collect_type_aliases_in_child_stat(body, target, aliases);
        }
        _ => {}
    }
}

pub(super) fn stat_contains_location(stat: &Stat, target: Location) -> bool {
    stat.location().is_some_and(|location| {
        location.begin.line <= target.begin.line && target.end.line <= location.end.line
    })
}

pub(super) fn stat_last_line(stat: &Stat) -> Option<u32> {
    match stat {
        Stat::Block { body, .. } => body.last().and_then(stat_last_line),
        Stat::If {
            then_body,
            else_body,
            ..
        } => else_body
            .as_deref()
            .or(Some(then_body.as_ref()))
            .and_then(stat_last_line),
        _ => stat_line(stat),
    }
}

pub(super) fn stat_guarantees_return(stat: &Stat) -> bool {
    match stat {
        Stat::Return { .. } => true,
        Stat::Block { body, .. } => body.last().is_some_and(stat_guarantees_return),
        Stat::If {
            then_body,
            else_body: Some(else_body),
            ..
        } => stat_guarantees_return(then_body) && stat_guarantees_return(else_body),
        _ => false,
    }
}

pub(super) fn inline_body_has_value_return(stat: &Stat) -> bool {
    match stat {
        Stat::Block { body, .. } => body.iter().any(inline_body_has_value_return),
        Stat::Return { list, .. } => !list.is_empty(),
        Stat::If {
            then_body,
            else_body,
            ..
        } => {
            inline_body_has_value_return(then_body)
                || else_body
                    .as_deref()
                    .is_some_and(inline_body_has_value_return)
        }
        _ => false,
    }
}

pub(super) fn index_expr_needs_distinct_source(expr: &Expr) -> bool {
    match expr {
        Expr::Call { is_self, .. } => !*is_self,
        Expr::Group { expr, .. }
        | Expr::TypeAssertion { expr, .. }
        | Expr::Instantiate { expr, .. } => index_expr_needs_distinct_source(expr),
        _ => false,
    }
}

pub(super) fn expr_line(expr: &Expr) -> Option<u32> {
    expr.location().map(|location| location.begin.line + 1)
}

pub(super) fn expr_end_line(expr: &Expr) -> Option<u32> {
    expr.location().map(|location| location.end.line + 1)
}

pub(super) fn number_value(value: &JsonNumber) -> Result<f64, CompileError> {
    // Includes the non-finite specials: Luau emits `Infinity`/`NaN` (an overflowing
    // literal such as `1e400`) as ordinary `double` constants, matching upstream.
    value
        .to_f64()
        .ok_or_else(|| CompileError::new(format!("unrepresentable number {value:?}")))
}

pub(super) fn table_array_index_operand(value: f64) -> Option<u8> {
    table_array_index(value).map(|index| (index - 1) as u8)
}

pub(super) fn table_hash_size_operand(size: u8) -> u8 {
    if size <= 1 {
        size
    } else {
        (u8::BITS - (size - 1).leading_zeros()) as u8 + 1
    }
}

/// Largest table-array index the `GETTABLEN`/`SETTABLEN` u8 hint field can
/// encode (upstream encodes `index - 1`, so 1..=256 fits in a byte).
const MAX_TABLE_ARRAY_INDEX_HINT: f64 = 256.0;

pub(super) fn table_array_index(value: f64) -> Option<u16> {
    if value.fract() == 0.0
        && (1.0..=MAX_TABLE_ARRAY_INDEX_HINT).contains(&value)
        && !is_negative_zero(value)
    {
        Some(value as u16)
    } else {
        None
    }
}

pub(super) fn is_negative_zero(value: f64) -> bool {
    value == 0.0 && value.to_bits() == (-0.0f64).to_bits()
}

pub(super) fn string_hash(value: &str) -> u8 {
    let mut hash = value.len() as u32;
    for byte in value.bytes().rev() {
        hash ^= (hash << 5)
            .wrapping_add(hash >> 2)
            .wrapping_add(u32::from(byte));
    }
    hash as u8
}

pub(super) fn single_name_import_id(string_constant: u32) -> u32 {
    (1 << IMPORT_PATH_COUNT_SHIFT)
        | (string_constant << (IMPORT_PATH_COUNT_SHIFT - IMPORT_PATH_COMPONENT_BITS))
}

pub(super) fn import_path_id(constants: &[u32]) -> Result<u32, CompileError> {
    if !(1..=3).contains(&constants.len()) {
        return Err(CompileError::new(format!(
            "bytecode imports support one to three path components, got {}",
            constants.len()
        )));
    }

    let mut id = (constants.len() as u32) << IMPORT_PATH_COUNT_SHIFT;
    for (index, constant) in constants.iter().copied().enumerate() {
        if constant > IMPORT_PATH_COMPONENT_MASK {
            return Err(CompileError::new(format!(
                "import path constant index {constant} overflows 10-bit field"
            )));
        }
        id |= constant << import_component_shift(index as u32);
    }
    Ok(id)
}

pub(super) fn numeric_for_prep_offset(
    source_word: u32,
    target_word: u32,
) -> Result<i16, CompileError> {
    let offset = i32::try_from(target_word).expect("bytecode word offset fits i32")
        - i32::try_from(source_word).expect("bytecode word offset fits i32");
    i16::try_from(offset)
        .map_err(|_| CompileError::new(format!("numeric for prep offset {offset} overflows i16")))
}

pub(super) fn generic_for_prep_offset(
    source_word: u32,
    target_word: u32,
) -> Result<i16, CompileError> {
    let offset = i32::try_from(target_word).expect("bytecode word offset fits i32")
        - i32::try_from(source_word).expect("bytecode word offset fits i32")
        - 1;
    i16::try_from(offset)
        .map_err(|_| CompileError::new(format!("generic for prep offset {offset} overflows i16")))
}

pub(super) fn loop_offset(target_word: u32, source_word: u32) -> Result<i16, CompileError> {
    let offset = i32::try_from(target_word).expect("bytecode word offset fits i32")
        - i32::try_from(source_word).expect("bytecode word offset fits i32")
        - 1;
    i16::try_from(offset)
        .map_err(|_| CompileError::new(format!("loop offset {offset} overflows i16")))
}

pub(super) fn ad_with_aux(opcode: Opcode, a: u8, d: i16, aux: Option<u32>) -> Instruction {
    Instruction::abc_with_aux(
        opcode,
        a,
        (d as u16 & 0xff) as u8,
        ((d as u16 >> 8) & 0xff) as u8,
        aux,
    )
}

pub(super) fn forgloop_aux(prep_opcode: Opcode, vars: usize) -> Result<u32, CompileError> {
    let vars = u32::try_from(vars).map_err(|_| {
        CompileError::new(format!("generic for variable count {vars} overflows u32"))
    })?;
    if prep_opcode == Opcode::ForGPrepInext {
        Ok(FORGLOOP_INEXT_BIT | vars)
    } else {
        Ok(vars)
    }
}

pub(super) fn bit32_extract_k_value(offset: &ConstantValue, width: &ConstantValue) -> Option<u16> {
    let offset = constant_integer_operand(offset)?;
    let width = constant_integer_operand(width)?;
    if offset >= 32 || width == 0 || width > 32 - offset {
        return None;
    }
    Some(u16::from(offset) | (u16::from(width - 1) << 5))
}

pub(super) fn constant_integer_operand(value: &ConstantValue) -> Option<u8> {
    match value {
        ConstantValue::Number(value) => {
            let integer = *value as u8;
            (f64::from(integer) == *value).then_some(integer)
        }
        ConstantValue::Integer(value) => u8::try_from(*value).ok(),
        ConstantValue::Nil
        | ConstantValue::Bool(_)
        | ConstantValue::String(_)
        | ConstantValue::Vector { .. } => None,
    }
}

pub(super) fn fastcall_fixed_arity(path: &[String]) -> Option<u8> {
    match path {
        [lib, name] if lib == "math" && name == "abs" => Some(1),
        [name] if name == "setmetatable" => Some(2),
        _ => None,
    }
}

pub(super) fn fastcall_fixed_return(path: &[String]) -> bool {
    matches!(
        path,
        [lib, name]
            if (lib == "math" && matches!(name.as_str(), "abs" | "sin" | "sqrt" | "max" | "min" | "clamp"))
                || (lib == "bit32" && name == "extract")
                || (lib == "buffer"
                    && matches!(
                        name.as_str(),
                        "readi8"
                            | "readu8"
                            | "readi16"
                            | "readu16"
                            | "readi32"
                            | "readu32"
                            | "readf32"
                            | "readf64"
                            | "readinteger"
                    ))
                || (lib == "string" && matches!(name.as_str(), "char" | "sub"))
                || (lib == "table" && name == "unpack")
    ) || matches!(path, [name] if matches!(name.as_str(), "type" | "typeof" | "setmetatable" | "tostring"))
}
