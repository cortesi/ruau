//! Operator constraint-generation metadata and deferred diagnostics.

use std::collections::BTreeMap;

use ruau_ast::{
    Location,
    syntax::{BinaryOp, SyntaxId, UnaryOp},
};

use crate::{
    constraints::Constraint,
    diagnostics::{Diagnostic, DiagnosticLocation},
    type_function::{Reduction, TypeFunctionRuntime},
    types::{Arena, PrimitiveType, SingletonType, TypeId, TypeKind},
};

pub struct BinaryBinding {
    pub(crate) location: Option<Location>,
    pub(crate) syntax_id: SyntaxId,
    pub(crate) expr_ty: TypeId,
    pub(crate) op: BinaryOp,
    pub(crate) left: TypeId,
    pub(crate) right: TypeId,
    pub(crate) expected: Option<TypeId>,
    pub(crate) unknown_global_parameter_operands: bool,
    pub(crate) unannotated_parameter_operands: bool,
    pub(crate) property_free_relational_operands: bool,
    pub(crate) recursive_call_operand: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeferredBinaryOperatorDiagnostic {
    pub(crate) op: BinaryOp,
    pub(crate) left: TypeId,
    pub(crate) right: TypeId,
    pub(crate) location: Option<DiagnosticLocation>,
    pub(crate) global_function_name: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeferredUnaryOperatorDiagnostic {
    pub(crate) op: UnaryOp,
    pub(crate) operand: TypeId,
    pub(crate) location: Option<DiagnosticLocation>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RelationalOperandKind {
    Number,
    String,
    Unknown,
    Free,
    Invalid,
}

pub fn deferred_binary_operator_diagnostic(
    arena: &Arena,
    constraints: &[Constraint],
    global_defs: &BTreeMap<String, TypeId>,
    deferred: &DeferredBinaryOperatorDiagnostic,
) -> Option<Diagnostic> {
    if deferred_binary_operator_has_valid_result(arena, deferred)
        || deferred_global_function_was_called(arena, constraints, global_defs, deferred)
    {
        return None;
    }
    let overload = binary_metamethod_name(deferred.op).unwrap_or("operator");
    let mut diagnostic = Diagnostic::binary_operator_error(
        binary_operator_text(deferred.op),
        "unknown",
        "unknown",
        overload,
    );
    if let Some(location) = deferred.location {
        diagnostic.primary_location = location;
    }
    Some(diagnostic)
}

pub fn deferred_unary_operator_diagnostic(
    arena: &Arena,
    deferred: &DeferredUnaryOperatorDiagnostic,
) -> Option<Diagnostic> {
    let (operator, overload) = match deferred.op {
        UnaryOp::Len => ("#", "__len"),
        UnaryOp::Minus => ("-", "__unm"),
        UnaryOp::Not => return None,
    };
    let invalid_operand = invalid_length_operand_options(arena, deferred.operand)
        .into_iter()
        .next()?;
    let mut diagnostic =
        Diagnostic::unary_operator_error(operator, arena.summary(invalid_operand), overload);
    if let Some(location) = deferred.location {
        diagnostic.primary_location = location;
    }
    Some(diagnostic)
}

pub fn relational_operator_text(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::CompareLt => "<",
        BinaryOp::CompareLe => "<=",
        BinaryOp::CompareGt => ">",
        BinaryOp::CompareGe => ">=",
        _ => "relational",
    }
}

pub fn equality_operator_text(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::CompareEq => "==",
        BinaryOp::CompareNe => "~=",
        _ => "equality",
    }
}

pub fn binary_operator_text(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        BinaryOp::FloorDiv => "//",
        BinaryOp::Mod => "%",
        BinaryOp::Pow => "^",
        BinaryOp::Concat => "..",
        BinaryOp::CompareEq => "==",
        BinaryOp::CompareNe => "~=",
        BinaryOp::CompareLt => "<",
        BinaryOp::CompareLe => "<=",
        BinaryOp::CompareGt => ">",
        BinaryOp::CompareGe => ">=",
        BinaryOp::And => "and",
        BinaryOp::Or => "or",
    }
}

pub fn is_relational_operator(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::CompareLt | BinaryOp::CompareLe | BinaryOp::CompareGt | BinaryOp::CompareGe
    )
}

pub fn binary_metamethod_name(op: BinaryOp) -> Option<&'static str> {
    match op {
        BinaryOp::Add => Some("__add"),
        BinaryOp::Sub => Some("__sub"),
        BinaryOp::Mul => Some("__mul"),
        BinaryOp::Div => Some("__div"),
        BinaryOp::FloorDiv => Some("__idiv"),
        BinaryOp::Mod => Some("__mod"),
        BinaryOp::Pow => Some("__pow"),
        BinaryOp::Concat => Some("__concat"),
        BinaryOp::CompareLt | BinaryOp::CompareGt => Some("__lt"),
        BinaryOp::CompareLe | BinaryOp::CompareGe => Some("__le"),
        BinaryOp::CompareEq | BinaryOp::CompareNe => Some("__eq"),
        BinaryOp::And | BinaryOp::Or => None,
    }
}

pub(super) fn binary_type_function_name(op: BinaryOp) -> Option<&'static str> {
    match op {
        BinaryOp::Add => Some("add"),
        BinaryOp::Sub => Some("sub"),
        BinaryOp::Mul => Some("mul"),
        BinaryOp::Div => Some("div"),
        BinaryOp::FloorDiv => Some("idiv"),
        BinaryOp::Mod => Some("mod"),
        BinaryOp::Pow => Some("pow"),
        BinaryOp::Concat => Some("concat"),
        _ => None,
    }
}

pub fn invalid_length_operand_options(arena: &Arena, ty: TypeId) -> Vec<TypeId> {
    let ty = arena.follow(ty);
    match arena.get(ty) {
        TypeKind::Union(options) => options
            .iter()
            .flat_map(|option| invalid_length_operand_options(arena, *option))
            .collect(),
        TypeKind::Intersection(options) => {
            if options
                .iter()
                .any(|option| invalid_length_operand_options(arena, *option).is_empty())
            {
                Vec::new()
            } else {
                vec![ty]
            }
        }
        TypeKind::Free(variable) => variable
            .upper_bound
            .map(|upper_bound| invalid_length_operand_options(arena, upper_bound))
            .unwrap_or_default(),
        TypeKind::Primitive(PrimitiveType::String)
        | TypeKind::Singleton(SingletonType::String(_))
        | TypeKind::Table(_)
        | TypeKind::Metatable { .. }
        | TypeKind::Any
        | TypeKind::Unknown
        | TypeKind::Error
        | TypeKind::Blocked(_)
        | TypeKind::Never => Vec::new(),
        _ => vec![ty],
    }
}

fn deferred_binary_operator_has_valid_result(
    arena: &Arena,
    deferred: &DeferredBinaryOperatorDiagnostic,
) -> bool {
    let mut scratch = arena.clone();
    matches!(
        TypeFunctionRuntime::new().reduce_allocating(
            &mut scratch,
            "add",
            &[deferred.left, deferred.right],
        ),
        Reduction::Reduced(reduced)
            if !matches!(scratch.get(scratch.follow(reduced)), TypeKind::Never)
    )
}

fn deferred_global_function_was_called(
    arena: &Arena,
    constraints: &[Constraint],
    global_defs: &BTreeMap<String, TypeId>,
    deferred: &DeferredBinaryOperatorDiagnostic,
) -> bool {
    let Some(name) = deferred.global_function_name.as_deref() else {
        return false;
    };
    let Some(function) = global_defs.get(name).copied() else {
        return false;
    };
    let function = arena.follow(function);
    constraints.iter().any(|constraint| {
        constraint
            .call_callee()
            .is_some_and(|callee| arena.follow(callee) == function)
    })
}
