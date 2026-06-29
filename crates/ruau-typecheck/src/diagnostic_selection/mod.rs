//! Selection policy for constraint solver diagnostics.
//!
//! The solver can emit several internal failures for one source construct. This
//! module turns those failures into the smaller set that should be rendered,
//! preferring source-located and path-specific diagnostics while preserving the
//! aggregate property-read cases that carry useful sibling context.

use std::collections::BTreeSet;

use crate::{
    constraints::ConstraintSolveError,
    diagnostics::DiagnosticLocation,
    subtype::{SubtypeError, SubtypeErrorKind},
    types::TypePathComponent,
};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct SubtypeAggregationKey {
    kind: SubtypeErrorKind,
    path: Vec<TypePathComponent>,
}

fn subtype_aggregation_key(error: &SubtypeError) -> SubtypeAggregationKey {
    SubtypeAggregationKey {
        kind: error.kind.clone(),
        path: error.path.components().to_vec(),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DiagnosticCandidateCategory {
    Subtype,
    PropertyRead,
    Other,
}

#[derive(Clone, Debug)]
struct DiagnosticCandidate {
    error: ConstraintSolveError,
    location: Option<DiagnosticLocation>,
    can_aggregate: bool,
    aggregate_key: String,
    category: DiagnosticCandidateCategory,
    subtype_key: Option<SubtypeAggregationKey>,
    subtype_path_is_specific: bool,
    subtype_is_specific_missing_property: bool,
    partial_union_property_read: bool,
}

impl DiagnosticCandidate {
    fn new(error: ConstraintSolveError) -> Self {
        let location = error.explicit_location();
        let can_aggregate = error.can_aggregate();
        let aggregate_key = error.aggregate_key();
        let subtype_error = error.subtype_error();
        let category = if subtype_error.is_some() {
            DiagnosticCandidateCategory::Subtype
        } else if error.is_property_read_detail() {
            DiagnosticCandidateCategory::PropertyRead
        } else {
            DiagnosticCandidateCategory::Other
        };
        let subtype_path_is_specific = subtype_error.is_some_and(|error| !error.path.is_empty());
        let subtype_is_specific_missing_property = subtype_error.is_some_and(|error| {
            !error.path.is_empty()
                && matches!(
                    error.kind,
                    SubtypeErrorKind::MissingProperty
                        | SubtypeErrorKind::MissingProperties { .. }
                        | SubtypeErrorKind::LikeKeySuggestion { .. }
                )
        });
        let subtype_key = subtype_error.map(subtype_aggregation_key);
        let partial_union_property_read = error.is_partial_union_property_read();
        Self {
            error,
            location,
            can_aggregate,
            aggregate_key,
            category,
            subtype_key,
            subtype_path_is_specific,
            subtype_is_specific_missing_property,
            partial_union_property_read,
        }
    }

    fn is_located_subtype(&self) -> bool {
        self.location.is_some() && self.category == DiagnosticCandidateCategory::Subtype
    }

    fn is_aggregatable_subtype(&self) -> bool {
        self.can_aggregate && self.is_located_subtype()
    }

    fn is_aggregatable_property_read(&self) -> bool {
        self.can_aggregate
            && self.location.is_some()
            && self.category == DiagnosticCandidateCategory::PropertyRead
    }

    fn is_aggregatable_non_subtype_site(&self) -> bool {
        self.can_aggregate
            && self.location.is_some()
            && self.category == DiagnosticCandidateCategory::Other
    }
}

#[derive(Clone, Debug)]
struct DiagnosticSelectionFacts {
    has_specific_subtype_error: bool,
    has_specific_missing_property_error: bool,
    has_multiple_aggregatable_subtype_sites: bool,
    first_is_located_subtype: bool,
    first_is_aggregatable_subtype: bool,
    first_is_aggregatable_property_read: bool,
    has_aggregatable_property_read_error: bool,
    property_read_error_sites: BTreeSet<DiagnosticLocation>,
    has_multiple_aggregatable_non_subtype_sites: bool,
    has_partial_union_property_read_error: bool,
    located_subtype_sites: BTreeSet<DiagnosticLocation>,
}

impl DiagnosticSelectionFacts {
    fn from_candidates(candidates: &[DiagnosticCandidate]) -> Self {
        let has_specific_subtype_error = candidates
            .iter()
            .any(|candidate| candidate.location.is_some() && candidate.subtype_path_is_specific);
        let has_specific_missing_property_error = candidates.iter().any(|candidate| {
            candidate.location.is_some() && candidate.subtype_is_specific_missing_property
        });
        let aggregatable_subtype_sites = candidates
            .iter()
            .filter(|candidate| candidate.is_aggregatable_subtype())
            .filter_map(|candidate| candidate.location)
            .collect::<BTreeSet<_>>();
        let property_read_error_sites = candidates
            .iter()
            .filter(|candidate| candidate.is_aggregatable_property_read())
            .filter_map(|candidate| candidate.location)
            .collect::<BTreeSet<_>>();
        let located_subtype_sites = candidates
            .iter()
            .filter(|candidate| candidate.is_located_subtype())
            .filter_map(|candidate| candidate.location)
            .collect::<BTreeSet<_>>();

        Self {
            has_specific_subtype_error,
            has_specific_missing_property_error,
            has_multiple_aggregatable_subtype_sites: aggregatable_subtype_sites.len() > 1,
            first_is_located_subtype: candidates
                .first()
                .is_some_and(DiagnosticCandidate::is_located_subtype),
            first_is_aggregatable_subtype: candidates
                .first()
                .is_some_and(DiagnosticCandidate::is_aggregatable_subtype),
            first_is_aggregatable_property_read: candidates
                .first()
                .is_some_and(DiagnosticCandidate::is_aggregatable_property_read),
            has_aggregatable_property_read_error: candidates
                .iter()
                .any(DiagnosticCandidate::is_aggregatable_property_read),
            property_read_error_sites,
            has_multiple_aggregatable_non_subtype_sites: candidates
                .iter()
                .filter(|candidate| candidate.is_aggregatable_non_subtype_site())
                .count()
                > 1,
            has_partial_union_property_read_error: candidates.iter().any(|candidate| {
                candidate.location.is_some()
                    && candidate.partial_union_property_read
                    && candidate.can_aggregate
            }),
            located_subtype_sites,
        }
    }

    fn aggregate_located_subtype_errors(&self) -> bool {
        (self.has_specific_subtype_error && self.first_is_located_subtype)
            || (self.has_multiple_aggregatable_subtype_sites && self.first_is_aggregatable_subtype)
    }

    fn aggregate_property_read_errors(&self) -> bool {
        self.has_aggregatable_property_read_error
            && (self.has_partial_union_property_read_error
                || self.property_read_error_sites.len() > 1)
            && self.first_is_aggregatable_property_read
    }
}

enum DiagnosticSelectionMode {
    PropertyReads,
    LocatedSubtypes,
    NonSubtypeSites,
    DistinctSubtypeSites,
    SingleLocatedSubtype,
    First,
}

impl DiagnosticSelectionMode {
    fn for_facts(facts: &DiagnosticSelectionFacts) -> Self {
        if facts.aggregate_property_read_errors() {
            Self::PropertyReads
        } else if facts.aggregate_located_subtype_errors() {
            Self::LocatedSubtypes
        } else if facts.has_multiple_aggregatable_non_subtype_sites {
            Self::NonSubtypeSites
        } else if facts.located_subtype_sites.len() > 1 {
            Self::DistinctSubtypeSites
        } else if !facts.located_subtype_sites.is_empty() {
            Self::SingleLocatedSubtype
        } else {
            Self::First
        }
    }
}

/// Returns true when `outer` strictly encloses `inner` (covers its whole span
/// and is wider). Used to prefer a per-span diagnostic over an aggregate one.
fn span_strictly_encloses(outer: &DiagnosticLocation, inner: &DiagnosticLocation) -> bool {
    let pos = |p: &crate::diagnostics::DiagnosticPosition| (p.line, p.column);
    pos(&outer.begin) <= pos(&inner.begin)
        && pos(&inner.end) <= pos(&outer.end)
        && (outer.begin != inner.begin || outer.end != inner.end)
}

fn aggregate_subtype_locations(candidates: &[DiagnosticCandidate]) -> BTreeSet<DiagnosticLocation> {
    let subtype_spans = candidates
        .iter()
        .filter(|candidate| candidate.category == DiagnosticCandidateCategory::Subtype)
        .filter_map(|candidate| candidate.location)
        .collect::<Vec<_>>();

    subtype_spans
        .iter()
        .copied()
        .filter(|location| {
            subtype_spans
                .iter()
                .filter(|inner| span_strictly_encloses(location, inner))
                .map(|inner| (inner.begin, inner.end))
                .collect::<BTreeSet<_>>()
                .len()
                >= 2
        })
        .collect()
}

pub fn select_constraint_errors_for_reporting(
    errors: Vec<ConstraintSolveError>,
) -> Vec<ConstraintSolveError> {
    let candidates = errors
        .into_iter()
        .map(DiagnosticCandidate::new)
        .collect::<Vec<_>>();
    let aggregate_locations = aggregate_subtype_locations(&candidates);
    let selected = select_constraint_candidates_for_reporting(candidates.clone());
    if aggregate_locations.is_empty() {
        return selected
            .into_iter()
            .map(|candidate| candidate.error)
            .collect();
    }

    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for candidate in selected {
        match candidate.location {
            Some(location)
                if candidate.category == DiagnosticCandidateCategory::Subtype
                    && aggregate_locations.contains(&location) =>
            {
                for child in candidates
                    .iter()
                    .filter(|child| child.category == DiagnosticCandidateCategory::Subtype)
                {
                    if let Some(child_location) = child.location
                        && span_strictly_encloses(&location, &child_location)
                        && seen.insert(child_location)
                    {
                        result.push(child.error.clone());
                    }
                }
            }
            other => {
                if let Some(location) = other {
                    seen.insert(location);
                }
                result.push(candidate.error);
            }
        }
    }
    result
}

fn select_constraint_candidates_for_reporting(
    candidates: Vec<DiagnosticCandidate>,
) -> Vec<DiagnosticCandidate> {
    let facts = DiagnosticSelectionFacts::from_candidates(&candidates);
    match DiagnosticSelectionMode::for_facts(&facts) {
        DiagnosticSelectionMode::PropertyReads => {
            select_property_read_candidates(candidates, &facts)
        }
        DiagnosticSelectionMode::LocatedSubtypes => {
            select_located_subtype_candidates(candidates, &facts)
        }
        DiagnosticSelectionMode::NonSubtypeSites => select_non_subtype_site_candidates(candidates),
        DiagnosticSelectionMode::DistinctSubtypeSites => {
            select_distinct_subtype_site_candidates(candidates)
        }
        DiagnosticSelectionMode::SingleLocatedSubtype => candidates
            .into_iter()
            .find(DiagnosticCandidate::is_located_subtype)
            .into_iter()
            .collect(),
        DiagnosticSelectionMode::First => candidates.into_iter().take(1).collect(),
    }
}

fn select_property_read_candidates(
    candidates: Vec<DiagnosticCandidate>,
    facts: &DiagnosticSelectionFacts,
) -> Vec<DiagnosticCandidate> {
    let mut seen = BTreeSet::new();
    candidates
        .into_iter()
        .filter(|candidate| {
            let Some(location) = candidate.location else {
                return false;
            };
            if !candidate.can_aggregate {
                return false;
            }
            if candidate.category == DiagnosticCandidateCategory::PropertyRead {
                return seen.insert((location, candidate.aggregate_key.clone()));
            }
            if facts.property_read_error_sites.contains(&location) {
                return false;
            }
            seen.insert((location, candidate.aggregate_key.clone()))
        })
        .collect()
}

fn select_located_subtype_candidates(
    candidates: Vec<DiagnosticCandidate>,
    facts: &DiagnosticSelectionFacts,
) -> Vec<DiagnosticCandidate> {
    let mut seen = BTreeSet::new();
    candidates
        .into_iter()
        .filter(|candidate| {
            let (Some(location), Some(subtype_key)) =
                (candidate.location, candidate.subtype_key.as_ref())
            else {
                return false;
            };
            if facts.has_specific_subtype_error
                && !candidate.subtype_path_is_specific
                && !(facts.has_specific_missing_property_error && candidate.can_aggregate)
            {
                return false;
            }
            if !facts.has_specific_subtype_error && !candidate.can_aggregate {
                return false;
            }
            seen.insert((location, subtype_key.clone()))
        })
        .collect()
}

fn select_non_subtype_site_candidates(
    candidates: Vec<DiagnosticCandidate>,
) -> Vec<DiagnosticCandidate> {
    let mut seen = BTreeSet::new();
    candidates
        .into_iter()
        .filter(|candidate| {
            let Some(location) = candidate.location else {
                return false;
            };
            if !candidate.is_aggregatable_non_subtype_site() {
                return false;
            }
            seen.insert((location, candidate.aggregate_key.clone()))
        })
        .collect()
}

fn select_distinct_subtype_site_candidates(
    candidates: Vec<DiagnosticCandidate>,
) -> Vec<DiagnosticCandidate> {
    let mut seen = BTreeSet::new();
    candidates
        .into_iter()
        .filter(|candidate| {
            let (Some(location), Some(subtype_key)) =
                (candidate.location, candidate.subtype_key.as_ref())
            else {
                return false;
            };
            seen.insert((location, subtype_key.clone()))
        })
        .collect()
}

#[cfg(any())]
mod tests;
