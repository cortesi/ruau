//! Constraint-solve errors and their lowering to structured diagnostics.
//!
//! `ConstraintSolveError` is the solver's internal failure type; this module
//! owns it together with the rendering that turns a solve failure into a
//! `Diagnostic` (the `into_diagnostic*` methods and their reason-path,
//! suppression, and overload-summary helpers).

use crate::{
    diagnostics::{
        Diagnostic, DiagnosticCategory, DiagnosticLocation, Payload, ReasonPath, ReasonPathEntry,
        SubtypeContext, SuppressionMetadata, UnionPropertyMissing,
    },
    overload::{OverloadError, resolve_overloads},
    subtype::{
        SubtypeError, SubtypeErrorKind, SubtypeReasoning, SubtypeSuppression, SubtypeTarget,
        Subtyper,
    },
    types::{
        Arena, PackField, PrimitiveType, PropertyAccess, SummaryOptions, TypeField, TypeId,
        TypeKind, TypePackKind, TypePath, TypePathComponent, TypePathRoot,
    },
    unify::{UnifyError, UnifyErrorKind},
};

/// Constraint solve failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConstraintSolveError {
    /// The solve loop hit its iteration limit.
    IterationLimit {
        /// Configured limit.
        limit: usize,
    },
    /// A unification constraint failed.
    Unify(UnifyError),
    /// A subtyping constraint failed.
    Subtype(SubtypeError),
    /// A subtyping constraint failed with retained reason/suppression metadata.
    SubtypeWithMetadata {
        /// Inner subtype failure.
        error: Box<SubtypeError>,
        /// Original subtype relation left-hand side.
        sub: SubtypeTarget,
        /// Original subtype relation right-hand side.
        sup: SubtypeTarget,
        /// Error-suppression summary for the failed relation.
        suppression: crate::subtype::SubtypeSuppression,
    },
    /// Reading a property through a union found members that cannot provide it.
    UnionPropertyRead {
        /// Union type being read.
        union: TypeId,
        /// Property name.
        property: String,
        /// Union members missing the property.
        missing_options: Vec<TypeId>,
        /// Whether every non-nil, non-dynamic union member missed the property.
        all_options_missing: bool,
    },
    /// Reading a property through a type that may be nil.
    NilablePropertyRead {
        /// Original type being read.
        ty: TypeId,
        /// Property name.
        property: String,
    },
    /// A property was used in a direction disallowed by its declaration
    /// modifier.
    PropertyAccessViolation {
        /// Property name.
        property: String,
        /// Attempted access direction.
        access: PropertyAccess,
    },
    /// Several failures arose from one constraint and should be reported
    /// independently by the recovery path.
    Multiple(Vec<Self>),
    /// A call constraint failed.
    Overload(OverloadError),
    /// A type-function instance reduced to `never` after call-site instantiation.
    UninhabitedTypeFunction {
        /// Rendered type-function instance.
        instance: String,
    },
    /// A constraint failed at a known source location.
    Located {
        /// Inner solve failure.
        error: Box<Self>,
        /// Source location for the failure.
        location: DiagnosticLocation,
        /// Whether sibling located subtype failures should be reported
        /// alongside this one.
        aggregate: bool,
    },
    /// A constraint has a source location to use only if it is selected for
    /// reporting and would otherwise render at the missing-location sentinel.
    DefaultLocated {
        /// Inner solve failure.
        error: Box<Self>,
        /// Fallback source location for the rendered diagnostic.
        location: DiagnosticLocation,
    },
}

impl ConstraintSolveError {
    pub(crate) fn with_default_location(self, location: Option<DiagnosticLocation>) -> Self {
        if let Self::Multiple(errors) = self {
            return Self::Multiple(
                errors
                    .into_iter()
                    .map(|error| error.with_default_location(location))
                    .collect(),
            );
        }
        match location {
            Some(location) => Self::DefaultLocated {
                error: Box::new(self),
                location,
            },
            None => self,
        }
    }

    pub(crate) fn with_location(self, location: Option<DiagnosticLocation>) -> Self {
        self.with_aggregate_location(location, false)
    }

    pub(crate) fn with_aggregated_location(self, location: Option<DiagnosticLocation>) -> Self {
        self.with_aggregate_location(location, true)
    }

    pub(crate) fn with_aggregate_location(
        self,
        location: Option<DiagnosticLocation>,
        aggregate: bool,
    ) -> Self {
        if let Self::Multiple(errors) = self {
            return Self::Multiple(
                errors
                    .into_iter()
                    .map(|error| error.with_aggregate_location(location, aggregate))
                    .collect(),
            );
        }
        match location {
            Some(location) => Self::Located {
                error: Box::new(self),
                location,
                aggregate,
            },
            None => self,
        }
    }

    /// Returns whether this error is solely a nilable-property read — reading a
    /// property off a possibly-`nil` value. Nonstrict mode does not enforce this
    /// nil-safety check, so the checker drops it there.
    pub(crate) fn is_nilable_property_read(&self) -> bool {
        match self {
            Self::NilablePropertyRead { .. } => true,
            Self::Located { error, .. } | Self::DefaultLocated { error, .. } => {
                error.is_nilable_property_read()
            }
            Self::Multiple(errors) => {
                !errors.is_empty() && errors.iter().all(Self::is_nilable_property_read)
            }
            _ => false,
        }
    }

    pub(crate) fn is_fully_suppressing(&self) -> bool {
        match self {
            Self::SubtypeWithMetadata { suppression, .. } => {
                suppression.fully_suppressing
                    && suppression.suppressing_reasonings.iter().all(|reasoning| {
                        !reasoning.sub_path.is_empty() || !reasoning.sup_path.is_empty()
                    })
            }
            Self::Located { error, .. } => error.is_fully_suppressing(),
            Self::DefaultLocated { error, .. } => error.is_fully_suppressing(),
            Self::Multiple(errors) => errors.iter().all(Self::is_fully_suppressing),
            Self::Unify(_)
            | Self::UninhabitedTypeFunction { .. }
            | Self::Subtype(_)
            | Self::UnionPropertyRead { .. }
            | Self::NilablePropertyRead { .. }
            | Self::PropertyAccessViolation { .. }
            | Self::Overload(_)
            | Self::IterationLimit { .. } => false,
        }
    }

    pub(crate) fn subtype_error(&self) -> Option<&SubtypeError> {
        match self {
            Self::Subtype(error) => Some(error),
            Self::SubtypeWithMetadata { error, .. } => Some(error),
            Self::Located { error, .. } => error.subtype_error(),
            Self::DefaultLocated { error, .. } => error.subtype_error(),
            Self::Unify(_)
            | Self::UninhabitedTypeFunction { .. }
            | Self::UnionPropertyRead { .. }
            | Self::NilablePropertyRead { .. }
            | Self::PropertyAccessViolation { .. }
            | Self::Multiple(_)
            | Self::Overload(_)
            | Self::IterationLimit { .. } => None,
        }
    }

    pub(crate) fn can_aggregate(&self) -> bool {
        match self {
            Self::Located {
                error, aggregate, ..
            } => *aggregate || error.can_aggregate(),
            Self::DefaultLocated { error, .. } => error.can_aggregate(),
            Self::UnionPropertyRead { .. }
            | Self::NilablePropertyRead { .. }
            | Self::PropertyAccessViolation { .. } => true,
            Self::Multiple(errors) => errors.iter().any(Self::can_aggregate),
            Self::Unify(_)
            | Self::UninhabitedTypeFunction { .. }
            | Self::Subtype(_)
            | Self::SubtypeWithMetadata { .. }
            | Self::Overload(_)
            | Self::IterationLimit { .. } => false,
        }
    }

    pub(crate) fn explicit_location(&self) -> Option<DiagnosticLocation> {
        match self {
            Self::Located {
                error, location, ..
            } => {
                if *location == DiagnosticLocation::missing() {
                    error.explicit_location()
                } else {
                    Some(*location)
                }
            }
            Self::DefaultLocated { error, .. } => error.explicit_location(),
            Self::Unify(_)
            | Self::UninhabitedTypeFunction { .. }
            | Self::Subtype(_)
            | Self::SubtypeWithMetadata { .. }
            | Self::UnionPropertyRead { .. }
            | Self::NilablePropertyRead { .. }
            | Self::PropertyAccessViolation { .. }
            | Self::Multiple(_)
            | Self::Overload(_)
            | Self::IterationLimit { .. } => None,
        }
    }

    pub(crate) fn is_property_read_detail(&self) -> bool {
        match self {
            Self::UnionPropertyRead { .. }
            | Self::NilablePropertyRead { .. }
            | Self::PropertyAccessViolation { .. } => true,
            Self::Located { error, .. } => error.is_property_read_detail(),
            Self::DefaultLocated { error, .. } => error.is_property_read_detail(),
            Self::Multiple(errors) => errors.iter().any(Self::is_property_read_detail),
            Self::Unify(_)
            | Self::UninhabitedTypeFunction { .. }
            | Self::Subtype(_)
            | Self::SubtypeWithMetadata { .. }
            | Self::Overload(_)
            | Self::IterationLimit { .. } => false,
        }
    }

    pub(crate) fn is_partial_union_property_read(&self) -> bool {
        match self {
            Self::UnionPropertyRead {
                all_options_missing,
                ..
            } => !*all_options_missing,
            Self::Located { error, .. } => error.is_partial_union_property_read(),
            Self::DefaultLocated { error, .. } => error.is_partial_union_property_read(),
            Self::Multiple(errors) => errors.iter().any(Self::is_partial_union_property_read),
            Self::Unify(_)
            | Self::UninhabitedTypeFunction { .. }
            | Self::Subtype(_)
            | Self::SubtypeWithMetadata { .. }
            | Self::NilablePropertyRead { .. }
            | Self::PropertyAccessViolation { .. }
            | Self::Overload(_)
            | Self::IterationLimit { .. } => false,
        }
    }

    pub(crate) fn aggregate_key(&self) -> String {
        match self {
            Self::Located { error, .. } => error.aggregate_key(),
            Self::DefaultLocated { error, .. } => error.aggregate_key(),
            Self::UnionPropertyRead {
                property,
                missing_options,
                all_options_missing,
                ..
            } => format!("union-property:{property}:{all_options_missing}:{missing_options:?}"),
            Self::NilablePropertyRead { property, ty } => {
                format!("nilable-property:{property}:{ty:?}")
            }
            Self::PropertyAccessViolation { property, access } => {
                format!("property-access:{property}:{access:?}")
            }
            _ => format!("{self:?}"),
        }
    }

    pub(crate) fn append_flattened(self, errors: &mut Vec<Self>) {
        match self {
            Self::Multiple(inner) => {
                for error in inner {
                    error.append_flattened(errors);
                }
            }
            error => errors.push(error),
        }
    }

    /// Converts this solver failure into a diagnostic ready for reporting.
    #[must_use]
    pub fn into_diagnostic(self) -> crate::diagnostics::Diagnostic {
        self.into_diagnostic_with_arena(None)
    }

    /// Converts this solver failure into a diagnostic ready for reporting,
    /// using arena-backed type summaries when the caller can provide them.
    #[must_use]
    pub fn into_diagnostic_with_arena(self, arena: Option<&Arena>) -> Diagnostic {
        if let Self::Located {
            error, location, ..
        } = self
        {
            let mut diagnostic = error.into_diagnostic_with_arena(arena);
            diagnostic.primary_location = location;
            return diagnostic;
        }
        if let Self::DefaultLocated { error, location } = self {
            let mut diagnostic = error.into_diagnostic_with_arena(arena);
            if diagnostic.primary_location == DiagnosticLocation::missing() {
                diagnostic.primary_location = location;
            }
            return diagnostic;
        }
        if let Self::UninhabitedTypeFunction { instance } = &self {
            return render_type_function_error(instance);
        }
        let mut diagnostic = Diagnostic::error(
            category_for_constraint_error(&self, arena),
            DiagnosticLocation::missing(),
        )
        .with_context(format!("{self:?}"));
        render_constraint_error_payload(&self, arena, &mut diagnostic);
        diagnostic
    }
}

fn category_for_constraint_error(
    error: &ConstraintSolveError,
    arena: Option<&Arena>,
) -> DiagnosticCategory {
    match error {
        ConstraintSolveError::Overload(error)
            if arena.is_some_and(|arena| {
                overload_error_is_generic_tail_pack_mismatch(arena, error)
            }) =>
        {
            DiagnosticCategory::TypeMismatch
        }
        ConstraintSolveError::Overload(error)
            if overload_error_is_single_signature_mismatch(error) =>
        {
            DiagnosticCategory::TypeMismatch
        }
        ConstraintSolveError::Overload(_) => DiagnosticCategory::Call,
        ConstraintSolveError::UnionPropertyRead { .. }
        | ConstraintSolveError::NilablePropertyRead { .. }
        | ConstraintSolveError::PropertyAccessViolation { .. } => DiagnosticCategory::TypeMismatch,
        ConstraintSolveError::Subtype(_)
        | ConstraintSolveError::SubtypeWithMetadata { .. }
        | ConstraintSolveError::Unify(_) => DiagnosticCategory::TypeMismatch,
        ConstraintSolveError::UninhabitedTypeFunction { .. } => DiagnosticCategory::TypeFunction,
        ConstraintSolveError::IterationLimit { .. } => DiagnosticCategory::Constraint,
        ConstraintSolveError::Located { .. }
        | ConstraintSolveError::DefaultLocated { .. }
        | ConstraintSolveError::Multiple(_) => unreachable!(
            "Located/DefaultLocated return early above; Multiple is flattened \
             via append_flattened before conversion"
        ),
    }
}

fn render_constraint_error_payload(
    error: &ConstraintSolveError,
    arena: Option<&Arena>,
    diagnostic: &mut Diagnostic,
) {
    match error {
        ConstraintSolveError::Subtype(subtype) => {
            render_subtype_error(subtype, None, subtype.sub, subtype.sup, arena, diagnostic);
        }
        ConstraintSolveError::SubtypeWithMetadata {
            error,
            sub,
            sup,
            suppression,
        } => {
            render_subtype_error(error, Some(suppression), *sub, *sup, arena, diagnostic);
        }
        ConstraintSolveError::Unify(error) => render_unify_error(error, diagnostic),
        ConstraintSolveError::UnionPropertyRead {
            union,
            property,
            missing_options,
            all_options_missing,
        } => render_union_property_read(
            *union,
            property,
            missing_options,
            *all_options_missing,
            arena,
            diagnostic,
        ),
        ConstraintSolveError::NilablePropertyRead { ty, property } => {
            render_nilable_property_read(*ty, property, arena, diagnostic);
        }
        ConstraintSolveError::PropertyAccessViolation { property, access } => {
            render_property_access_violation(property, *access, diagnostic);
        }
        ConstraintSolveError::Overload(error) => render_overload_error(error, arena, diagnostic),
        ConstraintSolveError::IterationLimit { .. } => {}
        ConstraintSolveError::UninhabitedTypeFunction { .. }
        | ConstraintSolveError::Located { .. }
        | ConstraintSolveError::DefaultLocated { .. }
        | ConstraintSolveError::Multiple(_) => unreachable!(
            "type-function and location wrappers return early; Multiple is flattened before rendering"
        ),
    }
}

fn render_type_function_error(instance: &str) -> Diagnostic {
    Diagnostic::uninhabited_type_function(instance.to_owned(), DiagnosticLocation::missing())
}

fn render_subtype_error(
    error: &SubtypeError,
    suppression: Option<&SubtypeSuppression>,
    root_sub: SubtypeTarget,
    root_sup: SubtypeTarget,
    arena: Option<&Arena>,
    diagnostic: &mut Diagnostic,
) {
    let SubtypeError {
        kind,
        path,
        sub,
        sup,
    } = error;
    let reason_path = reason_path_for_reasoning(&error.reasoning());
    if !reason_path.entries.is_empty() {
        diagnostic.reason_path = Some(reason_path);
    }
    if let Some(suppression) = suppression {
        diagnostic.suppression = suppression_metadata(suppression);
    }
    let mut subtype = SubtypeContext::default();
    if let Some(arena) = arena {
        if let (SubtypeTarget::Type(sub), SubtypeTarget::Type(sup)) = (root_sub, root_sup) {
            subtype.detailed_reason_paths = Subtyper::new(arena)
                .detailed_reasonings(sub, sup)
                .into_iter()
                .map(|reasoning| reason_path_for_reasoning(&reasoning))
                .filter(|path| !path.entries.is_empty())
                .collect();
        }
        subtype.generic_count_mismatch = generic_count_mismatch_marker(arena, root_sub, root_sup);
    }
    let typed = match kind {
        SubtypeErrorKind::MissingProperty => match last_property(path) {
            Some((name, access)) => {
                let owner = target_summary(arena, *sub);
                let expected = target_summary(arena, *sup);
                if !owner.is_empty() && access == PropertyAccess::Write {
                    diagnostic.context =
                        Some(format!("Cannot add property '{name}' to table '{owner}'"));
                } else if !owner.is_empty() && !expected.is_empty() && target_is_table(arena, *sub)
                {
                    diagnostic.context = Some(format!(
                        "Table type '{owner}' not compatible with type '{expected}' because the former is missing field '{name}'"
                    ));
                }
                Payload::MissingProperty {
                    name,
                    owner,
                    union: None,
                    subtype,
                }
            }
            None => Payload::SubtypeMismatch {
                indexer_part: None,
                subtype,
            },
        },
        SubtypeErrorKind::MissingProperties { names } => Payload::MissingProperties {
            names: names.clone(),
            owner: target_summary(arena, *sub),
            subtype,
        },
        SubtypeErrorKind::LikeKeySuggestion { name, suggestions } => Payload::LikeKeySuggestion {
            looked_up: name.clone(),
            suggestions: suggestions.clone(),
            subtype,
        },
        SubtypeErrorKind::PropertyVariance => {
            if let Some(context) = property_variance_context(arena, root_sub, root_sup, path) {
                diagnostic.context = Some(context);
            }
            Payload::PropertyVariance {
                name: last_property_name(path),
                access_target: path_access_target(path).map(str::to_owned),
                properties_searched: property_path_names(path),
                subtype,
            }
        }
        SubtypeErrorKind::ArityMismatch => Payload::ArityMismatch {
            counts: None,
            subtype,
        },
        SubtypeErrorKind::Mismatch => {
            if path.components().is_empty() {
                let actual = target_summary(arena, *sub);
                let expected = target_summary(arena, *sup);
                if !actual.is_empty() && !expected.is_empty() {
                    diagnostic.context = Some(format!(
                        "Type '{actual}' could not be converted into '{expected}'"
                    ));
                }
            }
            Payload::SubtypeMismatch {
                indexer_part: indexer_mismatch_part(path).map(str::to_owned),
                subtype,
            }
        }
    };
    diagnostic.set_typed(typed);
}

fn render_unify_error(error: &UnifyError, diagnostic: &mut Diagnostic) {
    match &error.kind {
        UnifyErrorKind::OccursCheck => {
            diagnostic.set_typed(Payload::OccursCheck);
        }
        UnifyErrorKind::ArityMismatch => {
            diagnostic.set_typed(Payload::ArityMismatch {
                counts: None,
                subtype: SubtypeContext::default(),
            });
        }
        UnifyErrorKind::PropertySetMismatch => {
            diagnostic.set_typed(Payload::PropertySetMismatch);
        }
        UnifyErrorKind::PropertyMetadataMismatch => {
            if let Some(name) = last_property_name(&error.path) {
                diagnostic.set_typed(Payload::PropertyMetadataMismatch { name });
            }
        }
        UnifyErrorKind::Mismatch | UnifyErrorKind::ComplexityExceeded => {}
    }
}

fn render_union_property_read(
    union: TypeId,
    property: &str,
    missing_options: &[TypeId],
    all_options_missing: bool,
    arena: Option<&Arena>,
    diagnostic: &mut Diagnostic,
) {
    let owner = arena
        .map(|arena| non_nil_union_summary(arena, union).unwrap_or_else(|| arena.summary(union)))
        .unwrap_or_default();
    let missing = arena
        .map(|arena| {
            missing_options
                .iter()
                .map(|option| arena.summary(*option))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    diagnostic.set_typed(Payload::MissingProperty {
        name: property.to_owned(),
        owner: owner.clone(),
        union: Some(UnionPropertyMissing {
            missing_options: missing.clone(),
            all_options_missing,
        }),
        subtype: SubtypeContext::default(),
    });
    if !owner.is_empty() {
        diagnostic.context = if all_options_missing {
            Some(format!("Type '{owner}' does not have key '{property}'"))
        } else {
            Some(format!(
                "Key '{property}' is missing from {} in the type '{owner}'",
                missing
                    .iter()
                    .map(|option| format!("'{option}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        };
    }
}

fn render_nilable_property_read(
    ty: TypeId,
    property: &str,
    arena: Option<&Arena>,
    diagnostic: &mut Diagnostic,
) {
    let owner = arena
        .map(|arena| optional_type_summary(arena, ty))
        .unwrap_or_default();
    diagnostic.set_typed(Payload::NilablePropertyRead {
        property: property.to_owned(),
        owner: owner.clone(),
    });
    if !owner.is_empty() {
        diagnostic.context = Some(format!("Value of type '{owner}' could be nil"));
    }
}

fn render_property_access_violation(
    property: &str,
    access: PropertyAccess,
    diagnostic: &mut Diagnostic,
) {
    let (verb, modifier) = match access {
        PropertyAccess::Read => ("read", "write-only"),
        PropertyAccess::Write => ("write to", "read-only"),
        PropertyAccess::ReadWrite => ("access", "restricted"),
    };
    diagnostic.set_typed(Payload::PropertyAccessViolation {
        property: property.to_owned(),
        access,
    });
    diagnostic.context = Some(format!(
        "Cannot {verb} property '{property}' because it is {modifier}"
    ));
}

fn render_overload_error(
    error: &OverloadError,
    arena: Option<&Arena>,
    diagnostic: &mut Diagnostic,
) {
    let typed = match error {
        OverloadError::NotCallable { .. } => Payload::NotCallable,
        OverloadError::NoMatch {
            rejected,
            from_call_expression,
            ..
        } => {
            let available_overloads = if *from_call_expression {
                arena
                    .and_then(|arena| overload_candidate_summaries_for_no_match(arena, error))
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            Payload::NoOverloadMatch {
                rejected: rejected.len(),
                available_overloads,
            }
        }
        OverloadError::Ambiguous { candidates } => Payload::AmbiguousOverload {
            candidates: candidates.len(),
        },
    };
    diagnostic.set_typed(typed);
}

fn last_property_name(path: &TypePath) -> Option<String> {
    path.components()
        .iter()
        .rev()
        .find_map(|component| match component {
            TypePathComponent::Property { name, .. } => Some(name.clone()),
            _ => None,
        })
}

fn last_property(path: &TypePath) -> Option<(String, PropertyAccess)> {
    path.components()
        .iter()
        .rev()
        .find_map(|component| match component {
            TypePathComponent::Property { name, access } => Some((name.clone(), *access)),
            _ => None,
        })
}

fn target_summary(arena: Option<&Arena>, target: SubtypeTarget) -> String {
    let Some(arena) = arena else {
        return String::new();
    };
    let options = SummaryOptions {
        hide_error_properties: true,
        ..SummaryOptions::default()
    };
    match target {
        SubtypeTarget::Type(ty) => arena.summary_with_options(ty, options),
        SubtypeTarget::Pack(pack) => arena.pack_summary(pack),
    }
}

fn target_type(target: SubtypeTarget) -> Option<TypeId> {
    match target {
        SubtypeTarget::Type(ty) => Some(ty),
        SubtypeTarget::Pack(_) => None,
    }
}

fn target_is_table(arena: Option<&Arena>, target: SubtypeTarget) -> bool {
    let (Some(arena), Some(ty)) = (arena, target_type(target)) else {
        return false;
    };
    matches!(arena.get(arena.follow(ty)), TypeKind::Table(_))
}

fn property_chain(components: &[TypePathComponent]) -> Option<String> {
    let mut names = Vec::new();
    for component in components {
        match component {
            TypePathComponent::Property { name, .. } => names.push(name.as_str()),
            _ => return None,
        }
    }
    (!names.is_empty()).then(|| names.join("."))
}

fn property_access_context(path: &TypePath) -> Option<String> {
    let components = path.components();
    match components {
        [TypePathComponent::TypeField(TypeField::Table), rest @ ..] => {
            property_chain(rest).map(|path| format!("in the table portion, accessing `{path}`"))
        }
        [
            TypePathComponent::TypeField(TypeField::Metatable),
            rest @ ..,
        ] => {
            property_chain(rest).map(|path| format!("in the metatable portion, accessing `{path}`"))
        }
        _ => property_chain(components).map(|path| format!("accessing `{path}`")),
    }
}

fn property_path_names(path: &TypePath) -> Vec<String> {
    path.components()
        .iter()
        .filter_map(|component| match component {
            TypePathComponent::Property { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect()
}

fn path_access_target(path: &TypePath) -> Option<&'static str> {
    match path.components().first() {
        Some(TypePathComponent::TypeField(TypeField::Table)) => Some("table"),
        Some(TypePathComponent::TypeField(TypeField::Metatable)) => Some("metatable"),
        _ => None,
    }
}

fn indexer_mismatch_part(path: &TypePath) -> Option<&'static str> {
    path.components()
        .iter()
        .find_map(|component| match component {
            TypePathComponent::TypeField(TypeField::IndexLookup) => Some("key"),
            TypePathComponent::TypeField(TypeField::IndexResult) => Some("value"),
            _ => None,
        })
}

fn property_variance_context(
    arena: Option<&Arena>,
    root_sub: SubtypeTarget,
    root_sup: SubtypeTarget,
    path: &TypePath,
) -> Option<String> {
    let arena = arena?;
    let sub = target_type(root_sub)?;
    let sup = target_type(root_sup)?;
    let sub_leaf = arena.traverse_path_for_type(TypePathRoot::Type(sub), path)?;
    let sup_leaf = arena.traverse_path_for_type(TypePathRoot::Type(sup), path)?;
    let sub_leaf = arena.summary(sub_leaf);
    let sup_leaf = arena.summary(sup_leaf);
    let sub = arena.summary(sub);
    let sup = arena.summary(sup);
    let access = property_access_context(path).unwrap_or_else(|| path.render_human());
    Some(format!(
        "Expected this to be\n\t'{sup}'\nbut got\n\t'{sub}'; \n{access} results in `{sub_leaf}` in the latter type and `{sup_leaf}` in the former type, and `{sub_leaf}` is not exactly `{sup_leaf}`"
    ))
}

fn is_nil_type(arena: &Arena, ty: TypeId) -> bool {
    matches!(
        arena.get(arena.follow(ty)),
        TypeKind::Primitive(PrimitiveType::Nil)
    )
}

fn union_non_nil_summaries(arena: &Arena, ty: TypeId) -> Option<Vec<String>> {
    let TypeKind::Union(options) = arena.get(arena.follow(ty)) else {
        return None;
    };
    let mut saw_nil = false;
    let mut summaries = Vec::new();
    for option in options {
        if is_nil_type(arena, *option) {
            saw_nil = true;
        } else {
            summaries.push(arena.summary(*option));
        }
    }
    if !saw_nil || summaries.is_empty() {
        return None;
    }
    summaries.sort();
    summaries.dedup();
    Some(summaries)
}

fn non_nil_union_summary(arena: &Arena, ty: TypeId) -> Option<String> {
    union_non_nil_summaries(arena, ty).map(|summaries| summaries.join(" | "))
}

fn optional_type_summary(arena: &Arena, ty: TypeId) -> String {
    let Some(summaries) = union_non_nil_summaries(arena, ty) else {
        return arena.summary(ty);
    };
    let non_nil = summaries.join(" | ");
    if summaries.len() == 1 {
        format!("{non_nil}?")
    } else {
        format!("({non_nil})?")
    }
}

fn reason_path_for_type_path(path: &TypePath, prefer_super_path: bool) -> ReasonPath {
    let mut entries = Vec::new();
    let mut pack_field = None;
    for component in path.components() {
        match component {
            TypePathComponent::Property { name, .. } => {
                entries.push(ReasonPathEntry::Property(name.clone()));
            }
            TypePathComponent::Index { index } => match pack_field {
                Some(PackField::Arguments) => {
                    entries.push(ReasonPathEntry::Argument(*index));
                }
                Some(PackField::Returns) => {
                    entries.push(ReasonPathEntry::Return(*index));
                }
                _ if prefer_super_path => {
                    entries.push(ReasonPathEntry::IntersectionMember(*index));
                }
                _ => entries.push(ReasonPathEntry::UnionMember(*index)),
            },
            TypePathComponent::TypeField(TypeField::IndexLookup | TypeField::IndexResult) => {
                entries.push(ReasonPathEntry::Indexer);
            }
            TypePathComponent::TypeField(TypeField::Metatable) => {
                entries.push(ReasonPathEntry::Metatable);
            }
            TypePathComponent::TypeField(TypeField::Negated) => {
                entries.push(ReasonPathEntry::Negation);
            }
            TypePathComponent::TypeField(TypeField::Variadic)
            | TypePathComponent::PackField(PackField::Tail)
            | TypePathComponent::PackSlice { .. } => {
                entries.push(ReasonPathEntry::VariadicTail);
            }
            TypePathComponent::TypeField(
                TypeField::Table | TypeField::UpperBound | TypeField::LowerBound,
            ) => {}
            TypePathComponent::PackField(field @ (PackField::Arguments | PackField::Returns)) => {
                pack_field = Some(*field);
            }
        }
    }
    ReasonPath { entries }
}

fn reason_path_for_reasoning(reasoning: &SubtypeReasoning) -> ReasonPath {
    if !reasoning.sub_path.is_empty() {
        reason_path_for_type_path(&reasoning.sub_path, false)
    } else {
        reason_path_for_type_path(&reasoning.sup_path, true)
    }
}

fn suppression_metadata(suppression: &SubtypeSuppression) -> SuppressionMetadata {
    SuppressionMetadata {
        fully_suppressing: suppression.fully_suppressing,
        suppressing_entries: suppression
            .suppressing_reasonings
            .iter()
            .map(reason_path_for_reasoning)
            .collect(),
    }
}

/// Detects a generic-parameter-count mismatch between two function types in the
/// unsound direction — the candidate (`root_sub`) has fewer generic type or
/// type-pack parameters than the required type (`root_sup`). Upstream reports a
/// `GenericTypeCountMismatch` companion alongside the structural mismatch in
/// this case, naming the more-generic required type as the "subtype" (its
/// internal convention passes the supertype's count first). The returned marker
/// carries those counts so the checker can emit the companion after error
/// aggregation.
fn generic_count_mismatch_marker(
    arena: &Arena,
    root_sub: SubtypeTarget,
    root_sup: SubtypeTarget,
) -> Option<crate::diagnostics::GenericCountMismatch> {
    use crate::diagnostics::{GenericCountMismatch, GenericParameterKind};

    let (SubtypeTarget::Type(sub), SubtypeTarget::Type(sup)) = (root_sub, root_sup) else {
        return None;
    };
    let (TypeKind::Function(sub_fn), TypeKind::Function(sup_fn)) =
        (arena.get(arena.follow(sub)), arena.get(arena.follow(sup)))
    else {
        return None;
    };
    if sub_fn.generics.len() < sup_fn.generics.len() {
        return Some(GenericCountMismatch {
            parameter: GenericParameterKind::Type,
            subtype_count: sup_fn.generics.len(),
            supertype_count: sub_fn.generics.len(),
        });
    }
    if sub_fn.generic_packs.len() < sup_fn.generic_packs.len() {
        return Some(GenericCountMismatch {
            parameter: GenericParameterKind::Pack,
            subtype_count: sup_fn.generic_packs.len(),
            supertype_count: sub_fn.generic_packs.len(),
        });
    }
    None
}

fn overload_candidate_summaries_for_no_match(
    arena: &Arena,
    error: &OverloadError,
) -> Option<Vec<String>> {
    let OverloadError::NoMatch {
        callee, arguments, ..
    } = error
    else {
        return None;
    };
    let report = resolve_overloads(arena, *callee, *arguments);
    let candidates = if report.incompatible.is_empty() {
        report.arity_mismatches
    } else {
        report
            .incompatible
            .into_iter()
            .map(|(candidate, _)| candidate)
            .collect()
    };
    let candidates = candidates
        .into_iter()
        .map(|candidate| arena.summary(candidate))
        .collect::<Vec<_>>();
    (!candidates.is_empty()).then_some(candidates)
}

/// Returns whether an overload error is a single-function signature mismatch
/// surfaced through a desugared call (a metamethod or operator lowering, marked
/// `from_call_expression: false`). There is no overload *set* to choose from —
/// one function rejected one argument on a plain type mismatch — so the fault is
/// an ordinary type mismatch, not an overload-resolution failure. User-written
/// calls (`from_call_expression: true`) keep the `Call` category even with one
/// candidate, because there the call itself is the user-visible construct.
fn overload_error_is_single_signature_mismatch(error: &OverloadError) -> bool {
    let OverloadError::NoMatch {
        rejected,
        from_call_expression,
        ..
    } = error
    else {
        return false;
    };
    if *from_call_expression {
        return false;
    }
    let [(_, error)] = rejected.as_slice() else {
        return false;
    };
    matches!(error.kind, SubtypeErrorKind::Mismatch)
        && matches!(error.sub, SubtypeTarget::Type(_))
        && matches!(error.sup, SubtypeTarget::Type(_))
}

fn overload_error_is_generic_tail_pack_mismatch(arena: &Arena, error: &OverloadError) -> bool {
    let OverloadError::NoMatch { rejected, .. } = error else {
        return false;
    };
    let [(_, error)] = rejected.as_slice() else {
        return false;
    };
    if !matches!(error.kind, SubtypeErrorKind::Mismatch) {
        return false;
    }
    let (SubtypeTarget::Pack(sub), SubtypeTarget::Pack(sup)) = (error.sub, error.sup) else {
        return false;
    };
    let TypePackKind::List {
        tail: Some(sub_tail),
        ..
    } = arena.get_pack(arena.follow_pack(sub))
    else {
        return false;
    };
    let TypePackKind::List {
        tail: Some(sup_tail),
        ..
    } = arena.get_pack(arena.follow_pack(sup))
    else {
        return false;
    };
    matches!(
        arena.get_pack(arena.follow_pack(*sub_tail)),
        TypePackKind::Variadic { .. }
    ) && matches!(
        arena.get_pack(arena.follow_pack(*sup_tail)),
        TypePackKind::Generic(_)
    )
}
