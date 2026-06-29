//! Operator constraint-generation metadata and deferred diagnostics.

use std::collections::BTreeMap;

use ruau_ast::{
    Location,
    json::{JsonBinaryOp, JsonUnaryOp},
    syntax::SyntaxId,
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
    pub(crate) op: JsonBinaryOp,
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
    pub(crate) op: JsonBinaryOp,
    pub(crate) left: TypeId,
    pub(crate) right: TypeId,
    pub(crate) location: Option<DiagnosticLocation>,
    pub(crate) global_function_name: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeferredUnaryOperatorDiagnostic {
    pub(crate) op: JsonUnaryOp,
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
        JsonUnaryOp::Len => ("#", "__len"),
        JsonUnaryOp::Minus => ("-", "__unm"),
        JsonUnaryOp::Not => return None,
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

pub fn relational_operator_text(op: JsonBinaryOp) -> &'static str {
    match op {
        JsonBinaryOp::CompareLt => "<",
        JsonBinaryOp::CompareLe => "<=",
        JsonBinaryOp::CompareGt => ">",
        JsonBinaryOp::CompareGe => ">=",
        _ => "relational",
    }
}

pub fn equality_operator_text(op: JsonBinaryOp) -> &'static str {
    match op {
        JsonBinaryOp::CompareEq => "==",
        JsonBinaryOp::CompareNe => "~=",
        _ => "equality",
    }
}

pub fn binary_operator_text(op: JsonBinaryOp) -> &'static str {
    match op {
        JsonBinaryOp::Add => "+",
        JsonBinaryOp::Sub => "-",
        JsonBinaryOp::Mul => "*",
        JsonBinaryOp::Div => "/",
        JsonBinaryOp::FloorDiv => "//",
        JsonBinaryOp::Mod => "%",
        JsonBinaryOp::Pow => "^",
        JsonBinaryOp::Concat => "..",
        JsonBinaryOp::CompareEq => "==",
        JsonBinaryOp::CompareNe => "~=",
        JsonBinaryOp::CompareLt => "<",
        JsonBinaryOp::CompareLe => "<=",
        JsonBinaryOp::CompareGt => ">",
        JsonBinaryOp::CompareGe => ">=",
        JsonBinaryOp::And => "and",
        JsonBinaryOp::Or => "or",
    }
}

pub fn is_relational_operator(op: JsonBinaryOp) -> bool {
    matches!(
        op,
        JsonBinaryOp::CompareLt
            | JsonBinaryOp::CompareLe
            | JsonBinaryOp::CompareGt
            | JsonBinaryOp::CompareGe
    )
}

pub fn binary_metamethod_name(op: JsonBinaryOp) -> Option<&'static str> {
    match op {
        JsonBinaryOp::Add => Some("__add"),
        JsonBinaryOp::Sub => Some("__sub"),
        JsonBinaryOp::Mul => Some("__mul"),
        JsonBinaryOp::Div => Some("__div"),
        JsonBinaryOp::FloorDiv => Some("__idiv"),
        JsonBinaryOp::Mod => Some("__mod"),
        JsonBinaryOp::Pow => Some("__pow"),
        JsonBinaryOp::Concat => Some("__concat"),
        JsonBinaryOp::CompareLt | JsonBinaryOp::CompareGt => Some("__lt"),
        JsonBinaryOp::CompareLe | JsonBinaryOp::CompareGe => Some("__le"),
        JsonBinaryOp::CompareEq | JsonBinaryOp::CompareNe => Some("__eq"),
        JsonBinaryOp::And | JsonBinaryOp::Or => None,
    }
}

pub(super) fn binary_type_function_name(op: JsonBinaryOp) -> Option<&'static str> {
    match op {
        JsonBinaryOp::Add => Some("add"),
        JsonBinaryOp::Sub => Some("sub"),
        JsonBinaryOp::Mul => Some("mul"),
        JsonBinaryOp::Div => Some("div"),
        JsonBinaryOp::FloorDiv => Some("idiv"),
        JsonBinaryOp::Mod => Some("mod"),
        JsonBinaryOp::Pow => Some("pow"),
        JsonBinaryOp::Concat => Some("concat"),
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
