//! Function and overload-set resolution.

use std::collections::BTreeSet;

use crate::{
    call_pack::{NormalizedCallParameterPack, ReceiverParameter},
    member_access,
    subtype::{SubtypeError, SubtypeErrorKind, Subtyper},
    types::{
        Arena, FunctionType, PrimitiveType, TypeId, TypeKind, TypePackId, TypePackKind,
        TypePackTail,
    },
};

/// Successful overload selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OverloadResolution {
    /// Selected callable type id.
    pub function: TypeId,
    /// Selected callable type.
    pub signature: FunctionType,
    /// Return type pack for the selected callable.
    pub returns: TypePackId,
    /// Whether the selected candidate's first parameter has already been
    /// supplied by the callable value.
    pub receiver: ReceiverParameter,
    /// Whether the selected overload's parameter types should be used to bound
    /// free actual arguments that participated in overload selection.
    pub bind_free_arguments_to_selected_parameters: bool,
}

/// Callable candidate considered during overload resolution.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct OverloadCandidate {
    /// Candidate callable type id.
    pub ty: TypeId,
    /// Whether the first function parameter is already supplied by the
    /// callable value rather than the explicit argument pack.
    pub receiver: ReceiverParameter,
}

/// Candidate buckets produced while resolving an overload set.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OverloadReport {
    /// Callable candidates accepted by the supplied argument pack.
    pub ok: Vec<(OverloadCandidate, FunctionType)>,
    /// Callable candidates rejected by argument type compatibility.
    pub incompatible: Vec<(TypeId, SubtypeError)>,
    /// Callable candidates rejected only because argument counts differ.
    pub arity_mismatches: Vec<TypeId>,
    /// Non-callable members encountered while walking an overload set.
    pub non_functions: Vec<TypeId>,
}

/// Overload resolution failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OverloadError {
    /// The callee is not callable.
    NotCallable {
        /// Callee type.
        callee: TypeId,
    },
    /// No overload accepted the supplied argument pack.
    NoMatch {
        /// Callee type.
        callee: TypeId,
        /// Supplied argument pack.
        arguments: TypePackId,
        /// Subtyping failures from rejected candidates.
        rejected: Vec<(TypeId, SubtypeError)>,
        /// Whether the failing call was a source-level call expression, which
        /// alone lists candidate signatures in a follow-up diagnostic.
        from_call_expression: bool,
    },
    /// More than one overload matched and no best candidate could be chosen.
    Ambiguous {
        /// Matching candidate function ids.
        candidates: Vec<TypeId>,
    },
}

/// Test helper: resolves a callable type against an argument pack.
#[cfg(any())]
pub fn resolve_call(
    arena: &Arena,
    callee: TypeId,
    arguments: TypePackId,
) -> Result<OverloadResolution, OverloadError> {
    resolve_call_with_options(arena, callee, arguments, ResolveCallOptions::default())
}

/// Resolves a callable type for a constraint solve.
pub fn resolve_call_for_constraint(
    arena: &Arena,
    callee: TypeId,
    arguments: TypePackId,
    ignore_return_ambiguity: bool,
    nonstrict_checked_arguments: bool,
    from_call_expression: bool,
) -> Result<OverloadResolution, OverloadError> {
    resolve_call_with_options(
        arena,
        callee,
        arguments,
        ResolveCallOptions {
            ignore_return_ambiguity,
            prefer_first_free_argument_match: true,
            nonstrict_checked_arguments,
            from_call_expression,
        },
    )
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ResolveCallOptions {
    ignore_return_ambiguity: bool,
    prefer_first_free_argument_match: bool,
    nonstrict_checked_arguments: bool,
    from_call_expression: bool,
}

fn resolve_call_with_options(
    arena: &Arena,
    callee: TypeId,
    arguments: TypePackId,
    options: ResolveCallOptions,
) -> Result<OverloadResolution, OverloadError> {
    let report = resolve_overloads_with_options(arena, callee, arguments, options);
    let bind_free_arguments =
        selected_overload_should_bind_free_arguments(arena, arguments, options, &report);
    if report.ok.is_empty() && report.incompatible.is_empty() && report.arity_mismatches.is_empty()
    {
        return match arena.get(arena.follow(callee)) {
            TypeKind::Any
            | TypeKind::Unknown
            | TypeKind::Error
            | TypeKind::Blocked(_)
            | TypeKind::Free(_)
            | TypeKind::Generic(_) => Ok(OverloadResolution {
                function: callee,
                signature: FunctionType::new(arguments, arena.empty_pack()),
                returns: arena.empty_pack(),
                receiver: ReceiverParameter::Explicit,
                bind_free_arguments_to_selected_parameters: false,
            }),
            _ => Err(OverloadError::NotCallable { callee }),
        };
    }

    match report.ok.as_slice() {
        [] => Err(OverloadError::NoMatch {
            callee,
            arguments,
            rejected: report.incompatible,
            from_call_expression: options.from_call_expression,
        }),
        [(candidate, signature)] => Ok(OverloadResolution {
            function: candidate.ty,
            signature: signature.clone(),
            returns: signature.returns,
            receiver: candidate.receiver,
            bind_free_arguments_to_selected_parameters: bind_free_arguments,
        }),
        _ if let Some((candidate, signature)) =
            unique_exact_arity_resolution(arena, arguments, &report.ok) =>
        {
            Ok(OverloadResolution {
                function: candidate.ty,
                returns: signature.returns,
                signature,
                receiver: candidate.receiver,
                bind_free_arguments_to_selected_parameters: bind_free_arguments,
            })
        }
        _ if let Some((candidate, signature)) =
            equivalent_overload_resolution(arena, &report.ok) =>
        {
            Ok(OverloadResolution {
                function: candidate.ty,
                returns: signature.returns,
                signature,
                receiver: candidate.receiver,
                bind_free_arguments_to_selected_parameters: bind_free_arguments,
            })
        }
        _ if options.ignore_return_ambiguity
            && let Some((candidate, signature)) =
                equivalent_overload_resolution_ignoring_returns(arena, &report.ok) =>
        {
            Ok(OverloadResolution {
                function: candidate.ty,
                returns: signature.returns,
                signature,
                receiver: candidate.receiver,
                bind_free_arguments_to_selected_parameters: bind_free_arguments,
            })
        }
        _ if options.prefer_first_free_argument_match
            && pack_contains_free(arena, arguments, &mut BTreeSet::new()) =>
        {
            let (candidate, signature) = report.ok[0].clone();
            Ok(OverloadResolution {
                function: candidate.ty,
                returns: signature.returns,
                signature,
                receiver: candidate.receiver,
                bind_free_arguments_to_selected_parameters: true,
            })
        }
        _ => {
            let candidates = report
                .ok
                .into_iter()
                .map(|(candidate, _)| candidate.ty)
                .collect();
            Err(OverloadError::Ambiguous { candidates })
        }
    }
}

fn selected_overload_should_bind_free_arguments(
    arena: &Arena,
    arguments: TypePackId,
    options: ResolveCallOptions,
    report: &OverloadReport,
) -> bool {
    options.prefer_first_free_argument_match
        && pack_contains_free(arena, arguments, &mut BTreeSet::new())
        && report.ok.len() + report.incompatible.len() + report.arity_mismatches.len() > 1
}

/// Classifies every member of a callable or overloaded type against arguments.
pub fn resolve_overloads(arena: &Arena, callee: TypeId, arguments: TypePackId) -> OverloadReport {
    resolve_overloads_with_options(arena, callee, arguments, ResolveCallOptions::default())
}

/// Returns the result pack of the overload whose failure should still shape the
/// call expression result. Upstream keeps these result types for invalid calls
/// when a best failed overload is identifiable, while still emitting the
/// diagnostic for the argument or arity error.
pub fn failed_overload_return_pack(arena: &Arena, error: &OverloadError) -> Option<TypePackId> {
    match error {
        OverloadError::Ambiguous { .. } => None,
        OverloadError::NoMatch {
            callee,
            arguments,
            rejected,
            ..
        } => {
            let rejected_returns = match rejected.as_slice() {
                [(candidate, _)] => function_returns(arena, *candidate),
                [] => None,
                _ => return None,
            };
            rejected_returns.or_else(|| {
                let report = resolve_overloads(arena, *callee, *arguments);
                let mut too_few = report
                    .arity_mismatches
                    .into_iter()
                    .filter(|candidate| {
                        arity_mismatch_is_too_few_arguments(arena, *arguments, *candidate)
                    })
                    .collect::<Vec<_>>();
                let [candidate] = too_few.as_mut_slice() else {
                    return None;
                };
                function_returns(arena, *candidate)
            })
        }
        OverloadError::NotCallable { .. } => None,
    }
}

fn resolve_overloads_with_options(
    arena: &Arena,
    callee: TypeId,
    arguments: TypePackId,
    options: ResolveCallOptions,
) -> OverloadReport {
    let mut candidates = Vec::new();
    collect_overload_candidates(arena, callee, &mut candidates, &mut BTreeSet::new());

    let mut report = OverloadReport::default();
    for candidate in candidates {
        let TypeKind::Function(signature) = arena.get(candidate.ty).clone() else {
            report.non_functions.push(candidate.ty);
            continue;
        };
        if pack_contains_any(arena, arguments, &mut BTreeSet::new())
            && call_arity_matches(arena, arguments, &signature, candidate.receiver)
        {
            report.ok.push((candidate, signature));
        } else {
            match check_call_arguments(arena, arguments, &signature, candidate.receiver, options) {
                Ok(()) => report.ok.push((candidate, signature)),
                Err(CallMismatch::Arity) => report.arity_mismatches.push(candidate.ty),
                Err(CallMismatch::Incompatible(error)) => {
                    report.incompatible.push((candidate.ty, error));
                }
            }
        }
    }
    report
}

fn unique_exact_arity_resolution(
    arena: &Arena,
    arguments: TypePackId,
    matches: &[(OverloadCandidate, FunctionType)],
) -> Option<(OverloadCandidate, FunctionType)> {
    let actual_count = arena.finite_pack_types(arguments)?.len();
    let exact = matches
        .iter()
        .filter(|(candidate, signature)| {
            exact_fixed_arity_matches(arena, actual_count, signature, candidate.receiver)
        })
        .collect::<Vec<_>>();
    let [(candidate, signature)] = exact.as_slice() else {
        return None;
    };
    Some((*candidate, (*signature).clone()))
}

fn exact_fixed_arity_matches(
    arena: &Arena,
    actual_count: usize,
    signature: &FunctionType,
    receiver: ReceiverParameter,
) -> bool {
    let Some(expected) = NormalizedCallParameterPack::from_pack(arena, signature.arguments)
        .for_explicit_arguments(receiver)
    else {
        return false;
    };
    expected.tail.is_none() && expected.types.len() == actual_count
}

fn function_returns(arena: &Arena, candidate: TypeId) -> Option<TypePackId> {
    let TypeKind::Function(function) = arena.get(arena.follow(candidate)) else {
        return None;
    };
    Some(function.returns)
}

fn arity_mismatch_is_too_few_arguments(
    arena: &Arena,
    arguments: TypePackId,
    candidate: TypeId,
) -> bool {
    let TypeKind::Function(function) = arena.get(arena.follow(candidate)) else {
        return false;
    };
    let Some(actual_count) = arena.finite_pack_types(arguments).map(|types| types.len()) else {
        return false;
    };
    let Some(expected_types) = arena.finite_pack_types(function.arguments) else {
        return false;
    };
    actual_count < required_prefix_len(arena, &expected_types)
}

fn call_arity_matches(
    arena: &Arena,
    actual: TypePackId,
    signature: &FunctionType,
    receiver: ReceiverParameter,
) -> bool {
    let actual_pack = arena.normalize_pack(actual);
    if actual_pack.tail.is_some() {
        return true;
    }
    let Some(expected_pack) = NormalizedCallParameterPack::from_pack(arena, signature.arguments)
        .for_explicit_arguments(receiver)
    else {
        return false;
    };
    let min_expected = required_prefix_len(arena, &expected_pack.types);
    if actual_pack.types.len() < min_expected {
        return false;
    }
    actual_pack.types.len() <= expected_pack.types.len() || expected_pack.tail.is_some()
}

fn equivalent_overload_resolution(
    arena: &Arena,
    matches: &[(OverloadCandidate, FunctionType)],
) -> Option<(OverloadCandidate, FunctionType)> {
    let (first_candidate, first_signature) = matches.first()?;
    matches
        .iter()
        .skip(1)
        .all(|(_, signature)| equivalent_function_signature(arena, first_signature, signature))
        .then(|| (*first_candidate, first_signature.clone()))
}

fn equivalent_function_signature(arena: &Arena, left: &FunctionType, right: &FunctionType) -> bool {
    equivalent_function_call_signature(arena, left, right)
        && equivalent_pack(arena, left.returns, right.returns)
}

fn equivalent_overload_resolution_ignoring_returns(
    arena: &Arena,
    matches: &[(OverloadCandidate, FunctionType)],
) -> Option<(OverloadCandidate, FunctionType)> {
    let (first_candidate, first_signature) = matches.first()?;
    matches
        .iter()
        .skip(1)
        .all(|(_, signature)| equivalent_function_call_signature(arena, first_signature, signature))
        .then(|| (*first_candidate, first_signature.clone()))
}

fn equivalent_function_call_signature(
    arena: &Arena,
    left: &FunctionType,
    right: &FunctionType,
) -> bool {
    left.generics == right.generics
        && left.generic_packs == right.generic_packs
        && left.has_self == right.has_self
        && left.is_checked == right.is_checked
        && equivalent_pack(arena, left.arguments, right.arguments)
}

fn equivalent_pack(arena: &Arena, left: TypePackId, right: TypePackId) -> bool {
    Subtyper::new(arena).is_subtype_pack(left, right).is_ok()
        && Subtyper::new(arena).is_subtype_pack(right, left).is_ok()
}

enum CallMismatch {
    Arity,
    Incompatible(SubtypeError),
}

fn check_call_arguments(
    arena: &Arena,
    actual: TypePackId,
    signature: &FunctionType,
    receiver: ReceiverParameter,
    options: ResolveCallOptions,
) -> Result<(), CallMismatch> {
    let expected = signature.arguments;
    if receiver == ReceiverParameter::Supplied {
        let actual_pack = arena.normalize_pack(actual);
        if actual_pack.tail.is_some() {
            return Err(CallMismatch::Arity);
        }
        let expected_pack = NormalizedCallParameterPack::from_pack(arena, expected)
            .for_explicit_arguments(receiver)
            .ok_or(CallMismatch::Arity)?;
        return check_finite_arguments_against_normalized_expected(
            arena,
            &actual_pack.types,
            &expected_pack.types,
            expected_pack.tail.as_ref(),
            options.nonstrict_checked_arguments && signature.is_checked,
        );
    }

    if !signature.generics.is_empty() || !signature.generic_packs.is_empty() {
        return Subtyper::new(arena)
            .is_subtype_pack_instantiating_function(actual, expected, signature)
            .map_err(|error| match error.kind {
                SubtypeErrorKind::ArityMismatch => CallMismatch::Arity,
                _ => CallMismatch::Incompatible(error),
            });
    }

    if let Some((actual_types, expected_types)) = arena
        .finite_pack_types(actual)
        .zip(arena.finite_pack_types(expected))
    {
        let expected_types = expected_types.as_slice();
        let min_expected = required_prefix_len(arena, expected_types);
        if actual_types.len() < min_expected
            || (actual_types.len() > expected_types.len()
                && !extra_arguments_are_uninhabited(arena, &actual_types[expected_types.len()..]))
        {
            return Err(CallMismatch::Arity);
        }

        for (actual, expected) in actual_types
            .iter()
            .copied()
            .zip(expected_types.iter().copied())
        {
            check_call_argument(
                arena,
                actual,
                expected,
                options.nonstrict_checked_arguments && signature.is_checked,
            )
            .map_err(CallMismatch::Incompatible)?;
        }
        return Ok(());
    }

    if let Some(()) = check_variadic_actual_arguments(
        arena,
        actual,
        expected,
        receiver,
        options.nonstrict_checked_arguments && signature.is_checked,
    )? {
        return Ok(());
    }

    Subtyper::new(arena)
        .is_subtype_pack(actual, expected)
        .map_err(|error| match error.kind {
            SubtypeErrorKind::ArityMismatch => CallMismatch::Arity,
            _ => CallMismatch::Incompatible(error),
        })
}

fn check_variadic_actual_arguments(
    arena: &Arena,
    actual: TypePackId,
    expected: TypePackId,
    receiver: ReceiverParameter,
    allow_nonstrict_union_argument: bool,
) -> Result<Option<()>, CallMismatch> {
    let actual_pack = arena.normalize_pack(actual);
    let Some(TypePackTail::Variadic(actual_tail)) = actual_pack.tail else {
        return Ok(None);
    };
    let Some(expected_pack) =
        NormalizedCallParameterPack::from_pack(arena, expected).for_explicit_arguments(receiver)
    else {
        return Err(CallMismatch::Arity);
    };
    if actual_pack.types.len() > expected_pack.types.len() && expected_pack.tail.is_none() {
        return Err(CallMismatch::Arity);
    }

    let fixed_count = actual_pack.types.len().min(expected_pack.types.len());
    for index in 0..fixed_count {
        check_call_argument(
            arena,
            actual_pack.types[index],
            expected_pack.types[index],
            allow_nonstrict_union_argument,
        )
        .map_err(CallMismatch::Incompatible)?;
    }

    for expected in expected_pack.types.into_iter().skip(fixed_count) {
        check_call_argument(arena, actual_tail, expected, allow_nonstrict_union_argument)
            .map_err(CallMismatch::Incompatible)?;
    }

    match expected_pack.tail {
        Some(TypePackTail::Variadic(expected_tail)) => {
            check_call_argument(
                arena,
                actual_tail,
                expected_tail,
                allow_nonstrict_union_argument,
            )
            .map_err(CallMismatch::Incompatible)?;
        }
        Some(TypePackTail::Error) | None => {}
        Some(TypePackTail::Free { .. } | TypePackTail::Generic(_) | TypePackTail::Cycle(_)) => {
            return Ok(None);
        }
    }

    Ok(Some(()))
}

fn check_finite_arguments_against_normalized_expected(
    arena: &Arena,
    actual_types: &[TypeId],
    expected_types: &[TypeId],
    expected_tail: Option<&TypePackTail>,
    allow_nonstrict_union_argument: bool,
) -> Result<(), CallMismatch> {
    let min_expected = required_prefix_len(arena, expected_types);
    if actual_types.len() < min_expected {
        return Err(CallMismatch::Arity);
    }

    let fixed_count = actual_types.len().min(expected_types.len());
    for index in 0..fixed_count {
        check_call_argument(
            arena,
            actual_types[index],
            expected_types[index],
            allow_nonstrict_union_argument,
        )
        .map_err(CallMismatch::Incompatible)?;
    }

    if actual_types.len() <= expected_types.len() {
        return Ok(());
    }

    match expected_tail {
        Some(TypePackTail::Variadic(expected)) => {
            for actual in &actual_types[expected_types.len()..] {
                check_call_argument(arena, *actual, *expected, allow_nonstrict_union_argument)
                    .map_err(CallMismatch::Incompatible)?;
            }
            Ok(())
        }
        Some(TypePackTail::Free { .. } | TypePackTail::Generic(_) | TypePackTail::Error) => Ok(()),
        Some(TypePackTail::Cycle(_)) | None
            if extra_arguments_are_uninhabited(arena, &actual_types[expected_types.len()..]) =>
        {
            Ok(())
        }
        Some(TypePackTail::Cycle(_)) | None => Err(CallMismatch::Arity),
    }
}

/// Returns whether every supplied argument beyond the callee's arity is
/// uninhabited (`never` or an empty union). Such an argument cannot exist at
/// runtime, so the call passing it is dead code and must not be rejected for
/// argument count.
fn extra_arguments_are_uninhabited(arena: &Arena, extra: &[TypeId]) -> bool {
    !extra.is_empty()
        && extra
            .iter()
            .all(|ty| crate::subtype::definitely_uninhabited_type(arena, *ty))
}

fn check_call_argument(
    arena: &Arena,
    actual: TypeId,
    expected: TypeId,
    allow_nonstrict_union_argument: bool,
) -> Result<(), SubtypeError> {
    if type_contains_dynamic(arena, actual, &mut BTreeSet::new())
        || type_contains_dynamic(arena, expected, &mut BTreeSet::new())
    {
        return Ok(());
    }
    match Subtyper::new(arena).is_subtype(actual, expected) {
        Ok(()) => Ok(()),
        Err(_)
            if allow_nonstrict_union_argument
                && nonstrict_checked_union_argument_suppresses(arena, actual, expected) =>
        {
            Ok(())
        }
        Err(_) if subtype_failure_is_fully_suppressing(arena, actual, expected) => Ok(()),
        Err(error) => Err(error),
    }
}

fn type_contains_dynamic(arena: &Arena, mut ty: TypeId, seen: &mut BTreeSet<TypeId>) -> bool {
    while seen.insert(ty) {
        match arena.get(arena.follow(ty)) {
            TypeKind::Any | TypeKind::Unknown | TypeKind::Error | TypeKind::Blocked(_) => {
                return true;
            }
            TypeKind::Bound(bound) => ty = *bound,
            TypeKind::Union(types) | TypeKind::Intersection(types) => {
                return types
                    .iter()
                    .any(|ty| type_contains_dynamic(arena, *ty, seen));
            }
            _ => return false,
        }
    }
    false
}

fn nonstrict_checked_union_argument_suppresses(
    arena: &Arena,
    actual: TypeId,
    expected: TypeId,
) -> bool {
    let actual = arena.follow(actual);
    if !matches!(arena.get(actual), TypeKind::Union(_)) {
        return false;
    }
    let mut has_matching_option = false;
    for option in arena.union_options(actual) {
        let option = arena.follow(option);
        if matches!(arena.get(option), TypeKind::Primitive(PrimitiveType::Nil)) {
            return false;
        }
        let option_matches = Subtyper::new(arena).is_subtype(option, expected).is_ok();
        has_matching_option |= option_matches;
    }
    has_matching_option
}

fn subtype_failure_is_fully_suppressing(arena: &Arena, actual: TypeId, expected: TypeId) -> bool {
    if !matches!(
        (
            arena.get(arena.follow(actual)),
            arena.get(arena.follow(expected)),
        ),
        (TypeKind::Table(_), TypeKind::Table(_))
    ) {
        return false;
    }
    let suppression = Subtyper::new(arena).suppression(actual, expected);
    suppression.fully_suppressing
        && suppression
            .suppressing_reasonings
            .iter()
            .all(|reasoning| !reasoning.sub_path.is_empty() || !reasoning.sup_path.is_empty())
}

fn required_prefix_len(arena: &Arena, types: &[TypeId]) -> usize {
    types
        .iter()
        .rposition(|ty| !member_access::type_accepts_nil_for_arity(arena, *ty))
        .map_or(0, |index| index + 1)
}

fn collect_overload_candidates(
    arena: &Arena,
    callee: TypeId,
    candidates: &mut Vec<OverloadCandidate>,
    seen: &mut BTreeSet<OverloadCandidate>,
) {
    collect_overload_candidates_with_self(
        arena,
        callee,
        ReceiverParameter::Explicit,
        candidates,
        seen,
    );
}

fn collect_overload_candidates_with_self(
    arena: &Arena,
    callee: TypeId,
    receiver: ReceiverParameter,
    candidates: &mut Vec<OverloadCandidate>,
    seen: &mut BTreeSet<OverloadCandidate>,
) {
    let callee = arena.follow(callee);
    let candidate = OverloadCandidate {
        ty: callee,
        receiver,
    };
    if !seen.insert(candidate) {
        return;
    }
    match arena.get(callee) {
        TypeKind::Function(_) => candidates.push(candidate),
        TypeKind::Metatable { metatable, .. } => {
            if let Some(call) = call_metamethod(arena, *metatable) {
                collect_overload_candidates_with_self(
                    arena,
                    call,
                    ReceiverParameter::Supplied,
                    candidates,
                    seen,
                );
            } else {
                candidates.push(candidate);
            }
        }
        TypeKind::Extern { properties, .. } => {
            if let Some(property) = properties.get("__call") {
                collect_overload_candidates_with_self(
                    arena,
                    property.ty,
                    ReceiverParameter::Supplied,
                    candidates,
                    seen,
                );
            } else {
                candidates.push(candidate);
            }
        }
        TypeKind::Intersection(types) => {
            for ty in types {
                collect_overload_candidates_with_self(arena, *ty, receiver, candidates, seen);
            }
        }
        TypeKind::Union(types) => {
            for ty in types {
                collect_overload_candidates_with_self(arena, *ty, receiver, candidates, seen);
            }
        }
        TypeKind::Bound(_)
        | TypeKind::Primitive(_)
        | TypeKind::Singleton(_)
        | TypeKind::Table(_)
        | TypeKind::TypeFunctionInstance { .. }
        | TypeKind::Negation(_)
        | TypeKind::Free(_)
        | TypeKind::Blocked(_)
        | TypeKind::Generic(_)
        | TypeKind::Error
        | TypeKind::Unknown
        | TypeKind::Never
        | TypeKind::Any => candidates.push(candidate),
    }
}

fn call_metamethod(arena: &Arena, metatable: TypeId) -> Option<TypeId> {
    let TypeKind::Table(table) = arena.get(arena.follow(metatable)) else {
        return None;
    };
    table.properties.get("__call").map(|property| property.ty)
}

fn pack_contains_any(arena: &Arena, pack: TypePackId, seen: &mut BTreeSet<TypePackId>) -> bool {
    let pack = arena.follow_pack(pack);
    if !seen.insert(pack) {
        return false;
    }
    match arena.get_pack(pack) {
        TypePackKind::List { types, tail } => {
            types.iter().any(|ty| type_contains_any(arena, *ty))
                || tail.is_some_and(|tail| pack_contains_any(arena, tail, seen))
        }
        TypePackKind::Variadic { ty } => type_contains_any(arena, *ty),
        TypePackKind::Bound(_) => unreachable!("follow_pack removes bound packs"),
        TypePackKind::Free { .. } | TypePackKind::Generic(_) | TypePackKind::Error => false,
    }
}

fn type_contains_any(arena: &Arena, ty: TypeId) -> bool {
    matches!(arena.get(arena.follow(ty)), TypeKind::Any)
}

fn pack_contains_free(arena: &Arena, pack: TypePackId, seen: &mut BTreeSet<TypePackId>) -> bool {
    let pack = arena.follow_pack(pack);
    if !seen.insert(pack) {
        return false;
    }
    match arena.get_pack(pack) {
        TypePackKind::List { types, tail } => {
            types.iter().any(|ty| type_contains_free(arena, *ty))
                || tail.is_some_and(|tail| pack_contains_free(arena, tail, seen))
        }
        TypePackKind::Variadic { ty } => type_contains_free(arena, *ty),
        TypePackKind::Free { .. } => true,
        TypePackKind::Bound(_) => unreachable!("follow_pack removes bound packs"),
        TypePackKind::Generic(_) | TypePackKind::Error => false,
    }
}

fn type_contains_free(arena: &Arena, ty: TypeId) -> bool {
    matches!(arena.get(arena.follow(ty)), TypeKind::Free(_))
}

#[cfg(any())]
mod tests {
    use super::*;
    use crate::types::{
        Arena, FunctionType, GenericType, GenericTypePack, SingletonType, TableIndexer,
        TableProperty, TableState, TableType, TypeKind, TypeLevel, TypePackKind, TypePathBuilder,
    };

    fn pack(arena: &mut Arena, types: Vec<TypeId>) -> TypePackId {
        arena.alloc_pack(TypePackKind::List { types, tail: None })
    }

    fn function(arena: &mut Arena, args: Vec<TypeId>, returns: Vec<TypeId>) -> TypeId {
        let args = pack(arena, args);
        let returns = pack(arena, returns);
        arena.alloc(TypeKind::Function(FunctionType::new(args, returns)))
    }

    fn first_return_type(arena: &Arena, pack: TypePackId) -> TypeId {
        let TypePackKind::List { types, .. } = arena.get_pack(arena.follow_pack(pack)) else {
            panic!("expected fixed return pack");
        };
        types[0]
    }

    fn table_with(arena: &mut Arena, properties: &[(&str, TypeId)]) -> TypeId {
        let mut table = TableType::new(TableState::Sealed);
        for (name, ty) in properties {
            table
                .properties
                .insert((*name).to_owned(), TableProperty::new(*ty));
        }
        arena.alloc(TypeKind::Table(table))
    }

    fn table_with_call(arena: &mut Arena, call: TypeId) -> TypeId {
        let table = arena.alloc(TypeKind::Table(TableType::new(TableState::Sealed)));
        let mut metatable = TableType::new(TableState::Sealed);
        metatable
            .properties
            .insert("__call".to_owned(), TableProperty::new(call));
        let metatable = arena.alloc(TypeKind::Table(metatable));
        arena.alloc(TypeKind::Metatable {
            table,
            metatable,
            name: None,
        })
    }

    #[test]
    fn resolves_plain_function_calls() {
        let mut arena = Arena::new();
        let primitives = arena.primitives();
        let callee = function(&mut arena, vec![primitives.string], vec![primitives.number]);
        let args = pack(&mut arena, vec![primitives.string]);

        let selected = resolve_call(&arena, callee, args).expect("function resolves");

        assert_eq!(selected.function, callee);
        assert_eq!(selected.receiver, ReceiverParameter::Explicit);
        let TypePackKind::List { types, .. } = arena.get_pack(selected.returns) else {
            panic!("expected return list");
        };
        assert_eq!(types, &[primitives.number]);
    }

    #[test]
    fn resolves_intersection_backed_builtin_overload_sets() {
        ruau_upstream::upstream_case!(
            "OverloadResolver.test.cpp::OverloadResolverTest::new_basic_overload_selection"
        );
        ruau_upstream::upstream_case!(
            "OverloadResolver.test.cpp::OverloadResolverTest::new_basic_overload_selection1"
        );

        let mut arena = Arena::new();
        let primitives = arena.primitives();
        let string_overload =
            function(&mut arena, vec![primitives.string], vec![primitives.string]);
        let number_overload =
            function(&mut arena, vec![primitives.number], vec![primitives.number]);
        let builtin = arena.alloc(TypeKind::Intersection(vec![
            string_overload,
            number_overload,
        ]));
        let args = pack(&mut arena, vec![primitives.number]);

        let selected = resolve_call(&arena, builtin, args).expect("number overload resolves");

        assert_eq!(selected.function, number_overload);

        let args = pack(&mut arena, vec![primitives.string]);
        let selected = resolve_call(&arena, builtin, args).expect("string overload resolves");

        assert_eq!(selected.function, string_overload);
    }

    #[test]
    fn resolves_overloads_with_different_arities() {
        ruau_upstream::upstream_case!(
            "OverloadResolver.test.cpp::OverloadResolverTest::new_overloads_with_different_arities"
        );
        ruau_upstream::upstream_case!(
            "OverloadResolver.test.cpp::OverloadResolverTest::new_overloads_with_different_arities1"
        );

        let mut arena = Arena::new();
        let primitives = arena.primitives();
        let unary = function(&mut arena, vec![primitives.number], vec![primitives.number]);
        let binary = function(
            &mut arena,
            vec![primitives.number, primitives.number],
            vec![primitives.number],
        );
        let overloaded = arena.alloc(TypeKind::Intersection(vec![unary, binary]));

        let args = pack(&mut arena, vec![primitives.number]);
        let selected = resolve_call(&arena, overloaded, args).expect("unary overload resolves");
        assert_eq!(selected.function, unary);

        let args = pack(&mut arena, vec![primitives.number, primitives.number]);
        let selected = resolve_call(&arena, overloaded, args).expect("binary overload resolves");
        assert_eq!(selected.function, binary);
    }

    #[test]
    fn failed_overload_return_pack_uses_best_failed_candidate() {
        let mut arena = Arena::new();
        let primitives = arena.primitives();
        let unary = function(&mut arena, vec![primitives.string], vec![primitives.string]);
        let binary = function(
            &mut arena,
            vec![primitives.number, primitives.number],
            vec![primitives.number],
        );
        let overloaded = arena.alloc(TypeKind::Intersection(vec![unary, binary]));

        let args = pack(&mut arena, vec![primitives.boolean]);
        let error = resolve_call(&arena, overloaded, args).expect_err("argument mismatch");
        let returns = failed_overload_return_pack(&arena, &error).expect("best failed return");
        assert_eq!(first_return_type(&arena, returns), primitives.string);

        let args = pack(&mut arena, vec![primitives.number]);
        let error = resolve_call(&arena, binary, args).expect_err("too few arguments");
        let returns = failed_overload_return_pack(&arena, &error).expect("too-few return");
        assert_eq!(first_return_type(&arena, returns), primitives.number);

        let args = pack(
            &mut arena,
            vec![primitives.number, primitives.number, primitives.number],
        );
        let error = resolve_call(&arena, overloaded, args).expect_err("too many arguments");
        assert_eq!(failed_overload_return_pack(&arena, &error), None);

        let number_or_string =
            arena.alloc(TypeKind::Union(vec![primitives.number, primitives.string]));
        let number_or_boolean =
            arena.alloc(TypeKind::Union(vec![primitives.number, primitives.boolean]));
        let first = function(&mut arena, vec![number_or_string], vec![primitives.string]);
        let second = function(
            &mut arena,
            vec![number_or_boolean],
            vec![primitives.boolean],
        );
        let ambiguous = arena.alloc(TypeKind::Intersection(vec![first, second]));
        let args = pack(&mut arena, vec![primitives.number]);
        let error = resolve_call(&arena, ambiguous, args).expect_err("ambiguous overload");
        assert_eq!(failed_overload_return_pack(&arena, &error), None);
    }

    #[test]
    fn resolves_variadic_actual_tail_against_fixed_parameters() {
        let mut arena = Arena::new();
        let primitives = arena.primitives();
        let callee = function(&mut arena, vec![primitives.string], vec![]);
        let variadic_string = arena.alloc_pack(TypePackKind::Variadic {
            ty: primitives.string,
        });

        let selected =
            resolve_call(&arena, callee, variadic_string).expect("variadic tail supplies string");

        assert_eq!(selected.function, callee);

        let variadic_number = arena.alloc_pack(TypePackKind::Variadic {
            ty: primitives.number,
        });
        let error =
            resolve_call(&arena, callee, variadic_number).expect_err("tail element must match");

        assert!(matches!(error, OverloadError::NoMatch { .. }));
    }

    #[test]
    fn resolves_table_indexer_arguments() {
        ruau_upstream::upstream_case!(
            "OverloadResolver.test.cpp::OverloadResolverTest::new_pass_table_with_indexer"
        );

        let mut arena = Arena::new();
        let primitives = arena.primitives();
        let mut table = TableType::new(TableState::Sealed);
        table.indexer = Some(TableIndexer {
            key: primitives.any,
            value: primitives.number,
            read_only: false,
        });
        let table = arena.alloc(TypeKind::Table(table));
        let callee = function(&mut arena, vec![table], vec![table]);
        let args = pack(&mut arena, vec![table]);

        let selected = resolve_call(&arena, callee, args).expect("indexer table argument resolves");

        assert_eq!(selected.function, callee);
    }

    #[test]
    fn resolves_call_with_fully_suppressing_table_property_mismatch() {
        let mut arena = Arena::new();
        let primitives = arena.primitives();
        let actual = table_with(&mut arena, &[("x", primitives.string)]);
        let expected = table_with(&mut arena, &[("x", primitives.any)]);
        let callee = function(&mut arena, vec![expected], vec![]);
        let args = pack(&mut arena, vec![actual]);

        let selected = resolve_call(&arena, callee, args).expect("suppressing mismatch resolves");

        assert_eq!(selected.function, callee);
    }

    #[test]
    fn rejects_call_with_later_non_suppressing_table_property_mismatch() {
        let mut arena = Arena::new();
        let primitives = arena.primitives();
        let actual = table_with(
            &mut arena,
            &[("x", primitives.string), ("y", primitives.number)],
        );
        let expected = table_with(
            &mut arena,
            &[("x", primitives.any), ("y", primitives.string)],
        );
        let callee = function(&mut arena, vec![expected], vec![]);
        let args = pack(&mut arena, vec![actual]);

        let error = resolve_call(&arena, callee, args).expect_err("call should be rejected");
        let OverloadError::NoMatch { rejected, .. } = error else {
            panic!("expected no-match overload error");
        };

        assert_eq!(rejected.len(), 1);
        let (_, error) = &rejected[0];
        assert_eq!(error.kind, SubtypeErrorKind::PropertyVariance);
        assert_eq!(error.path, TypePathBuilder::new().property("y").build());
    }

    #[test]
    fn resolves_overload_reports_separate_arity_mismatches() {
        ruau_upstream::upstream_case!(
            "OverloadResolver.test.cpp::OverloadResolverTest::new_separate_non_viable_overloads_by_arity_mismatch"
        );

        let mut arena = Arena::new();
        let primitives = arena.primitives();
        let number_to_number =
            function(&mut arena, vec![primitives.number], vec![primitives.number]);
        let number_to_string =
            function(&mut arena, vec![primitives.number], vec![primitives.string]);
        let binary = function(
            &mut arena,
            vec![primitives.number, primitives.number],
            vec![primitives.number],
        );
        let overloaded = arena.alloc(TypeKind::Intersection(vec![
            number_to_number,
            number_to_string,
            binary,
        ]));
        let args = pack(&mut arena, vec![primitives.string]);

        let report = resolve_overloads(&arena, overloaded, args);

        assert!(report.ok.is_empty());
        assert!(report.non_functions.is_empty());
        assert_eq!(report.arity_mismatches, vec![binary]);
        assert_eq!(report.incompatible.len(), 2);
        assert!(
            report
                .incompatible
                .iter()
                .any(|(candidate, _)| *candidate == number_to_number)
        );
        assert!(
            report
                .incompatible
                .iter()
                .any(|(candidate, _)| *candidate == number_to_string)
        );
    }

    #[test]
    fn resolves_optional_trailing_overload_arguments() {
        ruau_upstream::upstream_case!(
            "OverloadResolver.test.cpp::OverloadResolverTest::debug_traceback"
        );

        let mut arena = Arena::new();
        let primitives = arena.primitives();
        let optional_string = arena.alloc(TypeKind::Union(vec![primitives.string, primitives.nil]));
        let optional_number = arena.alloc(TypeKind::Union(vec![primitives.number, primitives.nil]));
        let message_level = function(
            &mut arena,
            vec![optional_string, optional_number],
            vec![primitives.string],
        );
        let thread_message_level = function(
            &mut arena,
            vec![primitives.thread, optional_string, optional_number],
            vec![primitives.string],
        );
        let debug_traceback = arena.alloc(TypeKind::Intersection(vec![
            message_level,
            thread_message_level,
        ]));

        for args in [
            vec![],
            vec![primitives.string],
            vec![primitives.string, primitives.number],
        ] {
            let args = pack(&mut arena, args);
            let report = resolve_overloads(&arena, debug_traceback, args);
            assert_eq!(report.ok.len(), 1);
            assert_eq!(report.ok[0].0.ty, message_level);
        }

        for args in [
            vec![primitives.thread],
            vec![primitives.thread, primitives.string],
            vec![primitives.thread, primitives.string, primitives.number],
        ] {
            let args = pack(&mut arena, args);
            let report = resolve_overloads(&arena, debug_traceback, args);
            assert_eq!(report.ok.len(), 1);
            assert_eq!(report.ok[0].0.ty, thread_message_level);
        }
    }

    #[test]
    fn resolves_call_metamethod_overloads() {
        ruau_upstream::upstream_case!(
            "OverloadResolver.test.cpp::OverloadResolverTest::new_match_call_metamethod"
        );
        ruau_upstream::upstream_case!(
            "OverloadResolver.test.cpp::OverloadResolverTest::new_metamethod_could_be_overloaded"
        );
        ruau_upstream::upstream_case!(
            "OverloadResolver.test.cpp::OverloadResolverTest::new_overload_group_could_include_metamethod"
        );

        let mut arena = Arena::new();
        let primitives = arena.primitives();
        let number_call = function(
            &mut arena,
            vec![primitives.unknown, primitives.number],
            vec![primitives.number],
        );
        let string_call = function(
            &mut arena,
            vec![primitives.unknown, primitives.string],
            vec![primitives.string],
        );
        let overloaded_call = arena.alloc(TypeKind::Intersection(vec![number_call, string_call]));
        let callable_table = table_with_call(&mut arena, overloaded_call);
        let args = pack(&mut arena, vec![primitives.number]);

        let report = resolve_overloads(&arena, callable_table, args);

        assert_eq!(report.ok.len(), 1);
        assert_eq!(report.ok[0].0.ty, number_call);
        assert_eq!(report.ok[0].0.receiver, ReceiverParameter::Supplied);
        assert_eq!(report.incompatible.len(), 1);
        assert_eq!(report.incompatible[0].0, string_call);

        let vararg_tail = arena.alloc_pack(TypePackKind::Variadic { ty: primitives.any });
        let self_and_varargs = arena.alloc_pack(TypePackKind::List {
            types: vec![primitives.unknown],
            tail: Some(vararg_tail),
        });
        let variadic_call = arena.alloc(TypeKind::Function(FunctionType::new(
            self_and_varargs,
            arena.empty_pack(),
        )));
        let callable_variadic_table = table_with_call(&mut arena, variadic_call);
        let args = pack(&mut arena, vec![]);
        let report = resolve_overloads(&arena, callable_variadic_table, args);

        assert_eq!(report.ok.len(), 1);
        assert_eq!(report.ok[0].0.ty, variadic_call);

        let boolean_to_boolean = function(
            &mut arena,
            vec![primitives.boolean],
            vec![primitives.boolean],
        );
        let overload_group = arena.alloc(TypeKind::Intersection(vec![
            callable_table,
            boolean_to_boolean,
        ]));
        let args = pack(&mut arena, vec![primitives.number]);
        let report = resolve_overloads(&arena, overload_group, args);

        assert_eq!(report.ok.len(), 1);
        assert_eq!(report.ok[0].0.ty, number_call);
    }

    #[test]
    fn resolves_generic_pack_overload_arguments() {
        ruau_upstream::upstream_case!(
            "OverloadResolver.test.cpp::OverloadResolverTest::new_select"
        );
        ruau_upstream::upstream_case!(
            "OverloadResolver.test.cpp::OverloadResolverTest::generic_higher_order_function_called_improperly"
        );

        let mut arena = Arena::new();
        let primitives = arena.primitives();
        let number_or_string =
            arena.alloc(TypeKind::Union(vec![primitives.number, primitives.string]));
        let generic_a = arena.alloc(TypeKind::Generic(GenericType {
            name: "A".to_owned(),
            level: TypeLevel(0),
        }));
        let generic_bs = arena.alloc_pack(TypePackKind::Generic(GenericTypePack {
            name: "B".to_owned(),
            level: TypeLevel(0),
        }));
        let generic_cs = arena.alloc_pack(TypePackKind::Generic(GenericTypePack {
            name: "C".to_owned(),
            level: TypeLevel(0),
        }));

        let select_arguments = arena.alloc_pack(TypePackKind::List {
            types: vec![number_or_string],
            tail: Some(generic_bs),
        });
        let select = arena.alloc(TypeKind::Function(FunctionType::new(
            select_arguments,
            arena.empty_pack(),
        )));
        let any_tail = arena.alloc_pack(TypePackKind::Variadic { ty: primitives.any });
        let select_call = arena.alloc_pack(TypePackKind::List {
            types: vec![number_or_string],
            tail: Some(any_tail),
        });
        let report = resolve_overloads(&arena, select, select_call);
        assert_eq!(report.ok.len(), 1);

        let function_argument_args = arena.alloc_pack(TypePackKind::List {
            types: vec![generic_a],
            tail: Some(generic_bs),
        });
        let function_argument = arena.alloc(TypeKind::Function(FunctionType::new(
            function_argument_args,
            generic_cs,
        )));
        let apply_arguments = pack(&mut arena, vec![function_argument, generic_a]);
        let apply = arena.alloc(TypeKind::Function(FunctionType::new(
            apply_arguments,
            generic_cs,
        )));
        let number_number_to_number = function(
            &mut arena,
            vec![primitives.number, primitives.number],
            vec![primitives.number],
        );
        let call_args = pack(&mut arena, vec![number_number_to_number, primitives.number]);
        let report = resolve_overloads(&arena, apply, call_args);
        assert_eq!(report.ok.len(), 1);
    }

    #[test]
    fn uses_subtyping_for_singleton_arguments() {
        let mut arena = Arena::new();
        let primitives = arena.primitives();
        let hello = arena.alloc(TypeKind::Singleton(SingletonType::String(
            "hello".to_owned(),
        )));
        let callee = function(
            &mut arena,
            vec![primitives.string],
            vec![primitives.boolean],
        );
        let args = pack(&mut arena, vec![hello]);

        resolve_call(&arena, callee, args).expect("singleton argument satisfies string parameter");
    }

    #[test]
    fn reports_no_match_and_ambiguity() {
        let mut arena = Arena::new();
        let primitives = arena.primitives();
        let string_overload =
            function(&mut arena, vec![primitives.string], vec![primitives.string]);
        let number_overload =
            function(&mut arena, vec![primitives.number], vec![primitives.number]);
        let overloaded = arena.alloc(TypeKind::Intersection(vec![
            string_overload,
            number_overload,
        ]));
        let boolean_args = pack(&mut arena, vec![primitives.boolean]);
        let any_args = pack(&mut arena, vec![primitives.any]);

        let no_match =
            resolve_call(&arena, overloaded, boolean_args).expect_err("no overload matches");
        assert!(matches!(no_match, OverloadError::NoMatch { .. }));

        let ambiguous = resolve_call(&arena, overloaded, any_args).expect_err("any matches both");
        assert!(matches!(ambiguous, OverloadError::Ambiguous { .. }));
    }

    #[test]
    fn generic_overload_rejects_missing_fixed_arguments() {
        let mut arena = Arena::new();
        let primitives = arena.primitives();
        let generic = arena.alloc(TypeKind::Generic(GenericType {
            name: "V".to_owned(),
            level: TypeLevel(0),
        }));
        let array = {
            let mut table = TableType::new(TableState::Sealed);
            table.indexer = Some(TableIndexer {
                key: primitives.number,
                value: generic,
                read_only: false,
            });
            arena.alloc(TypeKind::Table(table))
        };
        let arguments = pack(&mut arena, vec![array, generic]);
        let returns = pack(&mut arena, Vec::new());
        let function = arena.alloc(TypeKind::Function(FunctionType {
            generics: vec![GenericType {
                name: "V".to_owned(),
                level: TypeLevel(0),
            }],
            ..FunctionType::new(arguments, returns)
        }));
        let supplied = pack(&mut arena, Vec::new());

        assert!(matches!(
            resolve_call(&arena, function, supplied),
            Err(OverloadError::NoMatch { .. })
        ));
    }
}
