//! Constraint queue and rollback scaffold for DCR solving.

#![allow(clippy::multiple_inherent_impl)]

use std::collections::{BTreeSet, VecDeque};

use crate::{
    diagnostics::DiagnosticLocation,
    normalize::simplify_type,
    subtype::{SubtypeReasoning, SubtypeTarget, Subtyper},
    types::{Arena, TypeId, TypeKind, TypePackId, TypePath},
    unify::Unifier,
};

mod call;
mod error;
mod member;
mod queue;
mod relation;

pub use error::ConstraintSolveError;

/// One solver queue item: the work to do plus the source policy used when that
/// work fails.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Constraint {
    kind: ConstraintKind,
    location: ConstraintLocation,
}

/// Solver work independent of source-location policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConstraintKind {
    /// Two types must unify.
    Unify {
        /// First type to unify.
        left: TypeId,
        /// Second type to unify.
        right: TypeId,
    },
    /// `sub` must be a subtype of `sup`.
    Subtype {
        /// Candidate subtype.
        sub: TypeId,
        /// Required supertype.
        sup: TypeId,
    },
    /// `sub` must be a subtype of `sup` as a type pack.
    PackSubtype {
        /// Candidate subtype pack.
        sub: TypePackId,
        /// Required supertype pack.
        sup: TypePackId,
    },
    /// Resolve a function call, optionally constraining its return pack.
    Call {
        /// Callee type.
        callee: TypeId,
        /// Argument pack.
        arguments: TypePackId,
        /// Source-call context that affects overload resolution and diagnostics.
        context: CallConstraintContext,
    },
    /// Read through a table indexer expression.
    ReadIndexer {
        /// Table being read.
        table: TypeId,
        /// Index expression type.
        key: TypeId,
        /// Result type.
        value: TypeId,
    },
    /// Read a named table property expression.
    ReadProperty {
        /// Table being read.
        table: TypeId,
        /// Property name.
        name: String,
        /// Result type.
        value: TypeId,
    },
    /// Write a named table property through an lvalue.
    WriteProperty {
        /// Table being written.
        table: TypeId,
        /// Property name.
        name: String,
        /// Property value type.
        value: TypeId,
    },
    /// Write through a table indexer lvalue.
    WriteIndexer {
        /// Table being written.
        table: TypeId,
        /// Index expression type.
        key: TypeId,
        /// Value type.
        value: TypeId,
    },
}

/// Source-call context that is not part of the core call relation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallConstraintContext {
    /// Whether this source call should use nonstrict checked-function argument rules.
    pub nonstrict_checked_arguments: bool,
    /// Source ranges for explicit arguments, in call order.
    pub argument_locations: Vec<Option<DiagnosticLocation>>,
    /// Expected return pack.
    pub expected_returns: Option<TypePackId>,
    /// Whether this call originates from a source-level call expression
    /// `callee(args)`. Synthetic calls — operator metamethods, the for-in
    /// iterator protocol — set this to `false`. Only call expressions list
    /// candidate signatures when no overload matches, mirroring upstream.
    pub from_call_expression: bool,
}

/// How a constraint's source location should be applied to its failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConstraintLocation {
    /// Leave the diagnostic at its intrinsic location.
    None,
    /// Report at the source site when the relation itself fails.
    Primary {
        /// Source range for the failure.
        location: Option<DiagnosticLocation>,
        /// Whether sibling failures at other source ranges should be reported
        /// alongside this one.
        aggregate: bool,
    },
    /// Use the source site only when the rendered diagnostic would otherwise
    /// have the missing-location sentinel.
    Default {
        /// Fallback source range for the rendered diagnostic.
        location: Option<DiagnosticLocation>,
    },
}

impl ConstraintLocation {
    fn location(self) -> Option<DiagnosticLocation> {
        match self {
            Self::None => None,
            Self::Primary { location, .. } | Self::Default { location } => location,
        }
    }

    fn apply(self, error: ConstraintSolveError) -> ConstraintSolveError {
        match self {
            Self::None => error,
            Self::Primary {
                location,
                aggregate,
            } => error.with_aggregate_location(location, aggregate),
            Self::Default { location } => error.with_default_location(location),
        }
    }
}

impl Constraint {
    /// Two types must unify.
    pub fn unify(left: TypeId, right: TypeId) -> Self {
        Self {
            kind: ConstraintKind::Unify { left, right },
            location: ConstraintLocation::None,
        }
    }

    /// Two types must unify, with a source range used only if selected and
    /// otherwise missing a primary span.
    pub fn unify_default_location(
        left: TypeId,
        right: TypeId,
        location: Option<DiagnosticLocation>,
    ) -> Self {
        Self {
            kind: ConstraintKind::Unify { left, right },
            location: ConstraintLocation::Default { location },
        }
    }

    /// `sub` must be a subtype of `sup`.
    pub fn subtype(sub: TypeId, sup: TypeId, location: Option<DiagnosticLocation>) -> Self {
        Self {
            kind: ConstraintKind::Subtype { sub, sup },
            location: ConstraintLocation::Primary {
                location,
                aggregate: false,
            },
        }
    }

    /// `sub` must be a subtype of `sup`, with a fallback source range.
    pub fn subtype_default_location(
        sub: TypeId,
        sup: TypeId,
        location: Option<DiagnosticLocation>,
    ) -> Self {
        Self {
            kind: ConstraintKind::Subtype { sub, sup },
            location: ConstraintLocation::Default { location },
        }
    }

    /// `sub` must satisfy a contextual expected type at a source site.
    pub fn expected_subtype(
        sub: TypeId,
        sup: TypeId,
        location: Option<DiagnosticLocation>,
        aggregate: bool,
    ) -> Self {
        Self {
            kind: ConstraintKind::Subtype { sub, sup },
            location: ConstraintLocation::Primary {
                location,
                aggregate,
            },
        }
    }

    /// `sub` must be a subtype of `sup` as a type pack, with a fallback source
    /// range.
    pub fn pack_subtype_default_location(
        sub: TypePackId,
        sup: TypePackId,
        location: Option<DiagnosticLocation>,
    ) -> Self {
        Self {
            kind: ConstraintKind::PackSubtype { sub, sup },
            location: ConstraintLocation::Default { location },
        }
    }

    /// Resolve a function call, optionally constraining its return pack.
    pub fn call(
        callee: TypeId,
        arguments: TypePackId,
        nonstrict_checked_arguments: bool,
        argument_locations: Vec<Option<DiagnosticLocation>>,
        expected_returns: Option<TypePackId>,
        location: Option<DiagnosticLocation>,
        from_call_expression: bool,
    ) -> Self {
        Self {
            kind: ConstraintKind::Call {
                callee,
                arguments,
                context: CallConstraintContext {
                    nonstrict_checked_arguments,
                    argument_locations,
                    expected_returns,
                    from_call_expression,
                },
            },
            location: ConstraintLocation::Primary {
                location,
                aggregate: false,
            },
        }
    }

    /// Read through a table indexer expression.
    pub fn read_indexer(
        table: TypeId,
        key: TypeId,
        value: TypeId,
        location: Option<DiagnosticLocation>,
    ) -> Self {
        Self {
            kind: ConstraintKind::ReadIndexer { table, key, value },
            location: ConstraintLocation::Primary {
                location,
                aggregate: false,
            },
        }
    }

    /// Read a named table property expression.
    pub fn read_property(
        table: TypeId,
        name: String,
        value: TypeId,
        location: Option<DiagnosticLocation>,
    ) -> Self {
        Self {
            kind: ConstraintKind::ReadProperty { table, name, value },
            location: ConstraintLocation::Primary {
                location,
                aggregate: false,
            },
        }
    }

    /// Write a named table property through an lvalue.
    pub fn write_property(
        table: TypeId,
        name: String,
        value: TypeId,
        location: Option<DiagnosticLocation>,
    ) -> Self {
        Self {
            kind: ConstraintKind::WriteProperty { table, name, value },
            location: ConstraintLocation::Primary {
                location,
                aggregate: true,
            },
        }
    }

    /// Write through a table indexer lvalue.
    pub fn write_indexer(
        table: TypeId,
        key: TypeId,
        value: TypeId,
        location: Option<DiagnosticLocation>,
    ) -> Self {
        Self {
            kind: ConstraintKind::WriteIndexer { table, key, value },
            location: ConstraintLocation::Primary {
                location,
                aggregate: true,
            },
        }
    }

    /// Returns the callee if this constraint is a call relation.
    pub fn call_callee(&self) -> Option<TypeId> {
        match &self.kind {
            ConstraintKind::Call { callee, .. } => Some(*callee),
            _ => None,
        }
    }
}

/// Solver limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConstraintLimits {
    /// Maximum constraints processed by one solve call.
    pub max_iterations: usize,
    /// Maximum unification "complexity steps" allowed during this solve.
    ///
    /// `None` disables the bound.
    pub max_unification_complexity: Option<usize>,
}

/// Default unification budget per solve.
const DEFAULT_MAX_UNIFICATION_COMPLEXITY: usize = 1_000_000;

/// Default solver-iteration budget per solve.
const DEFAULT_MAX_ITERATIONS: usize = 10_000;

impl Default for ConstraintLimits {
    fn default() -> Self {
        Self {
            max_iterations: DEFAULT_MAX_ITERATIONS,
            max_unification_complexity: Some(DEFAULT_MAX_UNIFICATION_COMPLEXITY),
        }
    }
}

/// Constraint solve summary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConstraintSolveSummary {
    /// Number of constraints processed.
    pub solved: usize,
    /// Number of constraints left blocked.
    pub blocked: usize,
    /// Iterations consumed.
    pub iterations: usize,
}

/// Accumulates local write-path failures without changing the subtype relation's
/// single-leaf error contract.
#[derive(Default)]
struct SubtypeFailureSet {
    errors: Vec<ConstraintSolveError>,
}

impl SubtypeFailureSet {
    fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    fn len(&self) -> usize {
        self.errors.len()
    }

    fn push(&mut self, arena: &Arena, error: ConstraintSolveError) {
        self.push_with_fallback_path(arena, error, &None);
    }

    fn push_with_fallback_path(
        &mut self,
        arena: &Arena,
        error: ConstraintSolveError,
        fallback_path: &Option<TypePath>,
    ) {
        let expanded = expand_subtype_failure(arena, error);
        for error in expanded {
            self.errors.push(match fallback_path {
                Some(path) => ensure_subtype_path(error, path.clone()),
                None => error,
            });
        }
    }

    fn into_result(mut self) -> Result<(), ConstraintSolveError> {
        match self.errors.len() {
            0 => Ok(()),
            1 => Err(self.errors.remove(0)),
            _ => Err(ConstraintSolveError::Multiple(self.errors)),
        }
    }
}

fn expand_subtype_failure(arena: &Arena, error: ConstraintSolveError) -> Vec<ConstraintSolveError> {
    let ConstraintSolveError::SubtypeWithMetadata {
        error: subtype_error,
        sub,
        sup,
        suppression,
    } = error
    else {
        return vec![error];
    };

    let reasonings = match (sub, sup) {
        (SubtypeTarget::Type(sub), SubtypeTarget::Type(sup)) => {
            Subtyper::new(arena).detailed_reasonings(sub, sup)
        }
        (SubtypeTarget::Pack(sub), SubtypeTarget::Pack(sup)) => {
            Subtyper::new(arena).pack_reasonings(sub, sup)
        }
        _ => Vec::new(),
    };
    let reason_paths = subtype_reason_paths(reasonings);
    if reason_paths.len() <= 1 {
        return vec![ConstraintSolveError::SubtypeWithMetadata {
            error: subtype_error,
            sub,
            sup,
            suppression,
        }];
    }

    reason_paths
        .into_iter()
        .map(|path| {
            let mut error = (*subtype_error).clone();
            error.path = path;
            ConstraintSolveError::SubtypeWithMetadata {
                error: Box::new(error),
                sub,
                sup,
                suppression: suppression.clone(),
            }
        })
        .collect()
}

fn subtype_reason_paths(reasonings: Vec<SubtypeReasoning>) -> Vec<TypePath> {
    let mut seen = BTreeSet::new();
    let mut paths = Vec::new();
    for reasoning in reasonings {
        let path = if reasoning.sub_path.is_empty() {
            reasoning.sup_path
        } else {
            reasoning.sub_path
        };
        if path.is_empty() {
            continue;
        }
        if seen.insert(path.components().to_vec()) {
            paths.push(path);
        }
    }
    paths
}

fn ensure_subtype_path(
    error: ConstraintSolveError,
    fallback_path: TypePath,
) -> ConstraintSolveError {
    if fallback_path.is_empty() {
        return error;
    }
    match error {
        ConstraintSolveError::Subtype(mut subtype_error) => {
            if subtype_error.path.is_empty() {
                subtype_error.path = fallback_path;
            }
            ConstraintSolveError::Subtype(subtype_error)
        }
        ConstraintSolveError::SubtypeWithMetadata {
            mut error,
            sub,
            sup,
            suppression,
        } => {
            if error.path.is_empty() {
                error.path = fallback_path;
            }
            ConstraintSolveError::SubtypeWithMetadata {
                error,
                sub,
                sup,
                suppression,
            }
        }
        ConstraintSolveError::Located {
            error,
            location,
            aggregate,
        } => ConstraintSolveError::Located {
            error: Box::new(ensure_subtype_path(*error, fallback_path)),
            location,
            aggregate,
        },
        ConstraintSolveError::Multiple(errors) => ConstraintSolveError::Multiple(
            errors
                .into_iter()
                .map(|error| ensure_subtype_path(error, fallback_path.clone()))
                .collect(),
        ),
        error => error,
    }
}

/// Queue-based constraint solver.
pub struct ConstraintSolver<'a> {
    arena: &'a mut Arena,
    pending: VecDeque<Constraint>,
    blocked: VecDeque<Constraint>,
    limits: ConstraintLimits,
    /// Cooperative cancellation for the front-door request path: polled every
    /// iteration. When set, the solve bails out through the iteration-limit
    /// path — the caller is abandoning the result, and the point is to stop
    /// burning CPU past the request deadline.
    cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Free type variables that a generated constraint pins below a concrete
    /// scalar (e.g. `x <: number` from `x + y`). Such a variable is not freely
    /// polymorphic, so call-site generalization must leave it shared — otherwise
    /// each call instantiates a fresh, unconstrained copy and the scalar
    /// requirement (and the error it would surface) is lost.
    scalar_constrained_frees: BTreeSet<TypeId>,
}

impl<'a> ConstraintSolver<'a> {
    /// Creates a solver with default limits.
    pub fn new(arena: &'a mut Arena) -> Self {
        Self::with_limits(arena, ConstraintLimits::default())
    }

    /// Creates a solver with explicit limits.
    pub fn with_limits(arena: &'a mut Arena, limits: ConstraintLimits) -> Self {
        Self {
            arena,
            pending: VecDeque::new(),
            blocked: VecDeque::new(),
            limits,
            cancel: None,
            scalar_constrained_frees: BTreeSet::new(),
        }
    }

    /// Returns a fresh Unifier wired with this solver's unification complexity budget.
    fn unifier(&mut self) -> Unifier<'_> {
        Unifier::with_complexity_budget(self.arena, self.limits.max_unification_complexity)
    }

    fn solve_one(&mut self, constraint: Constraint) -> Result<(), ConstraintSolveError> {
        let location_policy = constraint.location;
        match constraint.kind {
            ConstraintKind::Unify { left, right } => self.solve_unify(left, right, location_policy),
            ConstraintKind::Subtype { sub, sup } => self.solve_subtype(sub, sup, location_policy),
            ConstraintKind::PackSubtype { sub, sup } => {
                self.solve_pack_subtype(sub, sup, location_policy)
            }
            ConstraintKind::Call {
                callee,
                arguments,
                context,
            } => self.solve_call(callee, arguments, context, location_policy.location()),
            ConstraintKind::ReadIndexer { table, key, value } => {
                self.read_indexer(table, key, value)
                    .map_err(|error| location_policy.apply(error))?;
                Ok(())
            }
            ConstraintKind::ReadProperty { table, name, value } => {
                let location = location_policy.location();
                self.read_property(table, name, value).map_err(|error| {
                    if error.is_property_read_detail() {
                        error.with_location(location)
                    } else {
                        error.with_default_location(location)
                    }
                })?;
                Ok(())
            }
            ConstraintKind::WriteProperty { table, name, value } => self
                .write_property(table, name, value)
                .map_err(|error| location_policy.apply(error)),
            ConstraintKind::WriteIndexer { table, key, value } => self
                .write_indexer(table, key, value)
                .map_err(|error| location_policy.apply(error)),
        }
    }

    fn union_type(&mut self, types: Vec<TypeId>) -> TypeId {
        let never = self.arena.primitives().never;
        let mut flattened = Vec::new();
        for ty in types {
            let ty = self.arena.follow(ty);
            if ty == never {
                continue;
            }
            match self.arena.get(ty).clone() {
                TypeKind::Any | TypeKind::Unknown => return ty,
                TypeKind::Union(options) => flattened.extend(options),
                _ => flattened.push(ty),
            }
        }
        flattened.sort_unstable();
        flattened.dedup();
        match flattened.as_slice() {
            [] => never,
            [only] => *only,
            _ => self.arena.alloc(TypeKind::Union(flattened)),
        }
    }

    fn normalized_union_type(&mut self, types: Vec<TypeId>) -> TypeId {
        let union = self.union_type(types);
        simplify_type(self.arena, union)
    }

    fn intersection_type(&mut self, types: Vec<TypeId>) -> TypeId {
        let mut flattened = Vec::new();
        for ty in types {
            let ty = self.arena.follow(ty);
            match self.arena.get(ty).clone() {
                TypeKind::Intersection(options) => flattened.extend(options),
                _ => flattened.push(ty),
            }
        }
        flattened.sort_unstable();
        flattened.dedup();
        match flattened.as_slice() {
            [] => self.arena.primitives().unknown,
            [only] => *only,
            _ => {
                let intersection = self.arena.alloc(TypeKind::Intersection(flattened));
                simplify_type(self.arena, intersection)
            }
        }
    }
}

#[cfg(any())]
mod tests;
