//! Structured type-checker diagnostics.

use std::{
    borrow::Cow,
    collections::{BTreeMap, hash_map::DefaultHasher},
    fmt,
    hash::{Hash, Hasher},
    io,
    ops::Deref,
};

use ruau_source::ModuleName;
use serde::{Deserialize, Serialize};

/// Type-checker diagnostic category.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiagnosticCategory {
    /// Syntax or parser diagnostic forwarded into checker output.
    Parse,
    /// Source graph or require resolver diagnostic.
    Resolver,
    /// General type mismatch.
    TypeMismatch,
    /// Unknown symbol, property, import, or global.
    UnknownSymbol,
    /// Invalid call, arity mismatch, or non-callable callee.
    Call,
    /// Invalid unary or binary operator application.
    Operator,
    /// Table/indexer access failure.
    TableAccess,
    /// Constraint-solver failure or solver limit.
    Constraint,
    /// Type-pack arity or tail mismatch.
    TypePack,
    /// Generic instantiation or generalization failure.
    Generic,
    /// Type-function reduction produced an uninhabited or irreducible result.
    TypeFunction,
    /// Internal recovery diagnostic.
    Internal,
    /// A required export (a global the embedding surface obliges the module
    /// to define) is missing or has a non-conforming type.
    RequiredExport,
    /// A module implementation does not conform to its declaration source.
    Conformance,
    /// Upstream numeric type-error code retained before a narrower category is
    /// available.
    UpstreamCode(u32),
}

impl DiagnosticCategory {
    /// Stable nonzero numeric code for compatibility with upstream-style
    /// diagnostic consumers.
    #[must_use]
    pub const fn code(&self) -> u32 {
        match self {
            Self::Parse => 1000,
            Self::Resolver => 1001,
            Self::TypeMismatch => 1002,
            Self::UnknownSymbol => 1003,
            Self::Call => 1004,
            Self::Operator => 1005,
            Self::TableAccess => 1006,
            Self::Constraint => 1007,
            Self::TypePack => 1008,
            Self::Generic => 1009,
            Self::TypeFunction => 1010,
            Self::Internal => 1011,
            Self::RequiredExport => 1012,
            Self::Conformance => 1013,
            Self::UpstreamCode(code) => *code,
        }
    }

    /// Stable human-readable category label for host diagnostics.
    #[must_use]
    pub(crate) fn display_label(&self) -> Cow<'static, str> {
        match self {
            Self::Parse => Cow::Borrowed("parse"),
            Self::Resolver => Cow::Borrowed("resolver"),
            Self::TypeMismatch => Cow::Borrowed("type-mismatch"),
            Self::UnknownSymbol => Cow::Borrowed("unknown-symbol"),
            Self::Call => Cow::Borrowed("call"),
            Self::Operator => Cow::Borrowed("operator"),
            Self::TableAccess => Cow::Borrowed("table-access"),
            Self::Constraint => Cow::Borrowed("constraint"),
            Self::TypePack => Cow::Borrowed("type-pack"),
            Self::Generic => Cow::Borrowed("generic"),
            Self::TypeFunction => Cow::Borrowed("type-function"),
            Self::Internal => Cow::Borrowed("internal"),
            Self::RequiredExport => Cow::Borrowed("required-export"),
            Self::Conformance => Cow::Borrowed("conformance"),
            Self::UpstreamCode(code) => Cow::Owned(format!("upstream-code-{code}")),
        }
    }
}

impl fmt::Display for DiagnosticCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_label().as_ref())
    }
}

/// Diagnostic severity.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
    /// Hard type-checking error.
    Error,
    /// Warning that does not block checking.
    Warning,
    /// Informational note.
    Info,
}

/// One-based diagnostic position for host editor and LSP-style adapters.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct OneBasedDiagnosticPosition {
    /// One-based line number.
    pub line: u32,
    /// One-based column number.
    pub column: u32,
}

impl OneBasedDiagnosticPosition {
    /// Creates a one-based source position.
    #[must_use]
    pub const fn new(line: u32, column: u32) -> Self {
        Self { line, column }
    }

    /// Missing-position sentinel.
    #[must_use]
    pub const fn missing() -> Self {
        Self::new(u32::MAX, u32::MAX)
    }

    /// Returns true when this is the missing-position sentinel.
    #[must_use]
    pub const fn is_missing(self) -> bool {
        self.line == u32::MAX && self.column == u32::MAX
    }
}

/// One-based diagnostic source range for host editor and LSP-style adapters.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct OneBasedDiagnosticLocation {
    /// First position covered by the range.
    pub begin: OneBasedDiagnosticPosition,
    /// First position after the range.
    pub end: OneBasedDiagnosticPosition,
}

impl OneBasedDiagnosticLocation {
    /// Creates a one-based source range.
    #[must_use]
    pub const fn new(begin: OneBasedDiagnosticPosition, end: OneBasedDiagnosticPosition) -> Self {
        Self { begin, end }
    }

    /// Missing-location sentinel.
    #[must_use]
    pub const fn missing() -> Self {
        Self::new(
            OneBasedDiagnosticPosition::missing(),
            OneBasedDiagnosticPosition::missing(),
        )
    }

    /// Returns true when this is the missing-location sentinel.
    #[must_use]
    pub const fn is_missing(self) -> bool {
        self.begin.is_missing() && self.end.is_missing()
    }
}

/// Property access direction.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PropertyAccess {
    /// Read-only access.
    Read,
    /// Write-only access.
    Write,
    /// Access direction not yet known.
    #[default]
    ReadWrite,
}

impl PropertyAccess {
    /// Upstream-style access label.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::ReadWrite => "read/write",
        }
    }
}

/// Typed machine-readable diagnostic detail.
///
/// This enum is the source of truth for type-checker diagnostic
/// payloads. `Diagnostic::payload` — the canonical wire shape that
/// fixtures and external consumers compare against — is derived from it
/// by the single serializer [`Payload::wire_json`]; producers
/// attach a typed payload with [`Diagnostic::with_typed`] /
/// [`Diagnostic::set_typed`] and never hand-build payload JSON.
///
/// The wire rendering is deliberately *not* a uniform encoding: it
/// reproduces, byte for byte, the per-site payload shapes the checker
/// has always emitted. Several variants therefore render as an empty
/// object (their data rides only on the typed channel), and a few
/// render different key sets depending on which optional fields are
/// set.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Payload {
    /// No structured payload. Renders as an empty object.
    #[default]
    Empty,
    /// Type-mismatch detail: subtype vs supertype names plus optional
    /// structured reason path.
    TypeMismatchDetail {
        /// Subtype display name.
        expected: String,
        /// Supertype display name.
        actual: String,
    },
    /// Missing-property detail for table access.
    MissingProperty {
        /// Property name that was expected.
        name: String,
        /// Table or symbol display name.
        owner: String,
        /// Union-property detail when the read traversed a union whose
        /// members cannot all provide the property. Only this form
        /// renders payload JSON; the plain form rides the typed channel
        /// alone.
        union: Option<UnionPropertyMissing>,
        /// Structural context when this was lowered from a failed
        /// subtype relation.
        subtype: SubtypeContext,
    },
    /// Multiple missing required properties in one table relation.
    MissingProperties {
        /// Property names that were expected.
        names: Vec<String>,
        /// Table or symbol display name.
        owner: String,
        /// Structural context when this was lowered from a failed
        /// subtype relation.
        subtype: SubtypeContext,
    },
    /// `Did you mean …` suggestion list.
    LikeKeySuggestion {
        /// Property name that was looked up.
        looked_up: String,
        /// Available suggestions, in order.
        suggestions: Vec<String>,
        /// Structural context when this was lowered from a failed
        /// subtype relation.
        subtype: SubtypeContext,
    },
    /// Overload-resolution detail.
    OverloadCandidates {
        /// Display names of candidate overloads.
        candidates: Vec<String>,
    },
    /// Unknown-symbol detail (covers globals, locals, properties).
    UnknownSymbol {
        /// Symbol name that was looked up.
        symbol: String,
    },
    /// Unknown-type detail (covers type names in annotations).
    UnknownType {
        /// Type name that was looked up.
        name: String,
    },
    /// Binary operator type mismatch.
    BinaryOperatorMismatch {
        /// Operator symbol (`+`, `..`, `==`, etc.).
        operator: String,
        /// Display name of the left operand type.
        left: String,
        /// Display name of the right operand type.
        right: String,
        /// Missing metamethod overload name (e.g. `__add`).
        overload: String,
        /// True when the operands cannot be compared because their
        /// metatables differ.
        metatable_mismatch: bool,
    },
    /// Unary operator type mismatch.
    UnaryOperatorMismatch {
        /// Operator symbol (`-`, `#`, `not`).
        operator: String,
        /// Display name of the operand type.
        operand: String,
        /// Missing metamethod overload name.
        overload: String,
    },
    /// Property variance mismatch — a writable property's type differs
    /// between the candidate and the required shape.
    PropertyVariance {
        /// Property name whose variance failed, when the failing path
        /// names one.
        name: Option<String>,
        /// Whether the failing path entered the table or metatable
        /// portion of a metatable-carrying type.
        access_target: Option<String>,
        /// Property names along the failing path, outermost first.
        properties_searched: Vec<String>,
        /// Structural context when this was lowered from a failed
        /// subtype relation.
        subtype: SubtypeContext,
    },
    /// Type-pack arity mismatch — call sites, function-pack compares,
    /// `return` mismatches.
    ArityMismatch {
        /// Expected/actual counts when the producer knows them (call
        /// sites); solver-lowered arity mismatches carry none.
        counts: Option<ArityCounts>,
        /// Structural context when this was lowered from a failed
        /// subtype relation.
        subtype: SubtypeContext,
    },
    /// Structural subtype failure without a more specific detail.
    SubtypeMismatch {
        /// Whether the mismatch was found in an indexer key or value.
        indexer_part: Option<String>,
        /// Structural context for the failed subtype relation.
        subtype: SubtypeContext,
    },
    /// Free-variable occurs-check failure: binding the variable would
    /// produce a directly-recursive type.
    OccursCheck,
    /// Two table shapes don't expose the same property names.
    PropertySetMismatch,
    /// Two property entries with the same name differ in metadata
    /// (read-only, write-only, or deprecated flags).
    PropertyMetadataMismatch {
        /// Property name with the metadata mismatch.
        name: String,
    },
    /// Call target is not callable.
    NotCallable,
    /// Explicit type instantiation (`f<T>(...)`) applied to a value
    /// that is not a function.
    ExplicitTypeInstantiationNotFunction,
    /// Explicit type instantiation supplied the wrong number of type or
    /// pack arguments.
    ExplicitTypeInstantiationParameterCount {
        /// Number of generic type parameters the function declares.
        expected_types: usize,
        /// Number of generic pack parameters the function declares.
        expected_packs: usize,
        /// Number of type arguments supplied.
        actual_types: usize,
        /// Number of pack arguments supplied.
        actual_packs: usize,
    },
    /// A call through a generic-pack signature rejected its arguments.
    GenericPackCallArgumentMismatch {
        /// True when a scalar argument failed its expected type
        /// (reported at the argument); false for an argument-count
        /// failure (reported at the call).
        type_mismatch: bool,
    },
    /// No overload matched the supplied arguments.
    NoOverloadMatch {
        /// Number of overload candidates rejected.
        rejected: usize,
        /// Candidate signature summaries, when the call site could
        /// enumerate them.
        available_overloads: Vec<String>,
    },
    /// More than one overload matched and no best candidate was chosen.
    AmbiguousOverload {
        /// Number of competing overloads.
        candidates: usize,
    },
    /// Parse error forwarded from the parser.
    Error {
        /// Parser error kind tag.
        kind: String,
        /// Parser-supplied human-readable message.
        message: String,
    },
    /// Resolver diagnostic forwarded from the source graph.
    ResolverError {
        /// Resolver error kind tag.
        kind: String,
        /// Optional resolved module name.
        module: Option<String>,
        /// Optional human-readable module display name.
        display_name: Option<String>,
        /// Optional resolved import path.
        path: Option<String>,
        /// Optional detail message.
        detail: Option<String>,
    },
    /// A module's require graph contains a cycle.
    RequireCycle {
        /// Module that closed the cycle.
        module: String,
        /// Module names along the cycle, in require order.
        cycle: Vec<String>,
    },
    /// A type alias or type function uses a reserved type name.
    ReservedTypeIdentifier {
        /// The reserved name.
        name: String,
    },
    /// A type alias or type function redefines an existing definition
    /// (or a primitive builtin type).
    DuplicateTypeDefinition {
        /// The redefined name.
        name: String,
    },
    /// Reading a property through a type that may be nil.
    NilablePropertyRead {
        /// Property name that was read.
        property: String,
        /// Display name of the possibly-nil owner type.
        owner: String,
    },
    /// A property was used in a direction disallowed by its declaration
    /// modifier.
    PropertyAccessViolation {
        /// Property name.
        property: String,
        /// Attempted access direction.
        access: PropertyAccess,
    },
    /// Generic-parameter-count mismatch companion between two related
    /// function types. The counts ride the typed channel only; the wire
    /// carries just the kind marker.
    GenericCountMismatch(GenericCountMismatch),
    /// The `luau_force_constraint_solving_incomplete` escape hatch
    /// forced an incomplete constraint-solving diagnostic.
    ConstraintSolvingIncompleteForced,
    /// A builtin type function was applied to a type-pack argument.
    TypeFunctionPackArgument {
        /// Type-function name.
        type_function: String,
    },
    /// A builtin type function was referenced without applying it.
    UnappliedTypeFunction {
        /// Type-function name.
        type_function: String,
    },
    /// A user type-function reduction failed at evaluation time.
    TypeFunctionRuntimeError {
        /// Stable failure-reason tag.
        reason: String,
    },
    /// A generic type alias was instantiated with the wrong number of
    /// type or pack parameters.
    GenericAliasParameterCount {
        /// Alias name.
        alias: String,
        /// Number of type parameters the alias declares.
        expected_type_parameters: usize,
        /// Number of pack parameters the alias declares.
        expected_type_pack_parameters: usize,
        /// Number of type arguments supplied.
        actual_type_parameters: usize,
        /// Number of pack arguments supplied.
        actual_type_pack_parameters: usize,
    },
    /// A generic alias declares pack parameters before type parameters.
    GenericAliasParameterOrder {
        /// Alias name.
        alias: String,
    },
    /// A generic alias received a type pack where a type was expected.
    GenericAliasPackInTypeSlot {
        /// Alias name.
        alias: String,
    },
    /// A regular generic type was used where a generic pack is
    /// expected.
    GenericTypeUsedAsPack {
        /// Generic type parameter name.
        type_parameter: String,
    },
    /// A generic pack was used where a regular type is expected.
    GenericPackUsedAsType {
        /// Generic pack parameter name.
        type_pack_parameter: String,
    },
    /// A recursive alias reference uses different parameters than its
    /// definition.
    RecursiveRestraintViolation {
        /// Alias name.
        alias: String,
    },
    /// A recursive type alias cannot be resolved.
    RecursiveTypeAlias {
        /// Alias name.
        alias: String,
    },
    /// A generic alias declares the same parameter name twice.
    DuplicateGenericParameter {
        /// Alias name.
        alias: String,
        /// Duplicated parameter name.
        parameter: String,
    },
    /// A function declares the same generic parameter name twice.
    DuplicateGenericParameterName {
        /// Duplicated parameter name.
        name: String,
    },
    /// A function whose return pack demands a value can fall off the
    /// end of its body.
    FunctionExitsWithoutReturning,
    /// An `__iter` metamethod returned fewer iteration values than its
    /// next-function requires.
    IterMetamethodMissingState {
        /// Number of arguments the next function requires.
        required: usize,
        /// Number of iteration values the metamethod provided.
        provided: usize,
    },
    /// Diagnostic recommending an explicit function annotation —
    /// emitted when the checker cannot infer a function's return
    /// pack from its body alone.
    ExplicitFunctionAnnotationRecommended {
        /// Rendered recommended return annotation.
        recommended_return: Option<String>,
        /// Recommended parameter annotations, when the producer
        /// inspected the parameters.
        recommended_args: Option<Vec<RecommendedArgument>>,
    },
    /// Return-pack arity mismatch — the function returns a different
    /// number of values than its annotated return pack expects.
    ReturnArityMismatch {
        /// Number of values the annotation expects.
        expected: usize,
        /// Number of values actually produced.
        actual: usize,
    },
    /// A function parameter's inferred type collapsed to `never`
    /// because the body's usage contradicts every possible binding.
    ParameterReducedToNever {
        /// Parameter name (or `<parameter>` placeholder when the
        /// binding has no source-visible name).
        parameter: String,
    },
    /// One required-subtype expectation for a parameter that the
    /// body imposes through use-site context.
    ParameterRequiredSubtype {
        /// Parameter name (or `<parameter>` placeholder).
        parameter: String,
        /// Rendered required-supertype summary at the use site.
        required: String,
    },
    /// A type-function instance reduced to an uninhabited result.
    UninhabitedTypeFunction {
        /// Rendered type-function instance.
        instance: String,
    },
    /// A required export — a global the embedding surface obliges the
    /// module to define — is missing or has a non-conforming type.
    RequiredExport {
        /// Required global name.
        name: String,
        /// Required type, as registered (declaration-syntax text).
        required: String,
        /// Rendered module-defined type when the global was defined but
        /// did not conform; `None` when the global was missing.
        actual: Option<String>,
    },
    /// A module implementation failed its declaration-conformance check.
    Conformance {
        /// Declared root name from the declaration source.
        name: String,
        /// Rendered declaration root type.
        required: String,
        /// Rendered implementation module type, or `None` when the
        /// implementation did not export a value.
        actual: Option<String>,
    },
}

/// Union-property-missing detail: which union members miss the
/// property, and whether every relevant member misses it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnionPropertyMissing {
    /// Display names of union members missing the property.
    pub missing_options: Vec<String>,
    /// Whether every non-nil, non-dynamic union member missed the
    /// property.
    pub all_options_missing: bool,
}

/// Numeric expected/actual counts for an arity mismatch a call site
/// could measure directly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArityCounts {
    /// Number of values expected.
    pub expected: usize,
    /// Number of values supplied.
    pub actual: usize,
}

/// One recommended parameter annotation accompanying an
/// explicit-function-annotation recommendation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecommendedArgument {
    /// Parameter name.
    pub name: String,
    /// Rendered recommended type.
    pub ty: String,
}

/// Which generic parameter list disagreed in a
/// [`GenericCountMismatch`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GenericParameterKind {
    /// Generic type parameters (`<T>`).
    Type,
    /// Generic pack parameters (`<T...>`).
    Pack,
}

/// Generic-parameter-count mismatch between two related function
/// types, in upstream's convention (the supertype's count is passed
/// first as `subtype_count`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GenericCountMismatch {
    /// Which parameter list disagreed.
    pub parameter: GenericParameterKind,
    /// Upstream-convention subtype parameter count.
    pub subtype_count: usize,
    /// Upstream-convention supertype parameter count.
    pub supertype_count: usize,
}

impl GenericCountMismatch {
    fn enrichment_json(&self) -> serde_json::Value {
        serde_json::json!({
            "parameter": match self.parameter {
                GenericParameterKind::Type => "type",
                GenericParameterKind::Pack => "pack",
            },
            "subtype_count": self.subtype_count,
            "supertype_count": self.supertype_count,
        })
    }
}

/// Structural context shared by diagnostics lowered from a failed
/// subtype relation: re-derived reason paths and an optional
/// generic-parameter-count mismatch between the related function
/// types.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SubtypeContext {
    /// Structured reason paths re-derived from the root relation.
    pub detailed_reason_paths: Vec<ReasonPath>,
    /// Generic-parameter-count mismatch between the related function
    /// types, when one was detected.
    pub generic_count_mismatch: Option<GenericCountMismatch>,
}

impl SubtypeContext {
    fn extend_wire(&self, map: &mut serde_json::Map<String, serde_json::Value>) {
        if !self.detailed_reason_paths.is_empty() {
            map.insert(
                "detailed_reason_paths".to_owned(),
                serde_json::json!(
                    self.detailed_reason_paths
                        .iter()
                        .map(reason_path_json)
                        .collect::<Vec<_>>()
                ),
            );
        }
        if let Some(mismatch) = &self.generic_count_mismatch {
            map.insert(
                "generic_count_mismatch".to_owned(),
                mismatch.enrichment_json(),
            );
        }
    }
}

/// Wire rendering for one reason-path entry.
fn reason_path_entry_json(entry: &ReasonPathEntry) -> serde_json::Value {
    match entry {
        ReasonPathEntry::Argument(index) => {
            serde_json::json!({ "kind": "argument", "index": index })
        }
        ReasonPathEntry::Return(index) => {
            serde_json::json!({ "kind": "return", "index": index })
        }
        ReasonPathEntry::VariadicTail => serde_json::json!({ "kind": "variadic-tail" }),
        ReasonPathEntry::Property(name) => {
            serde_json::json!({ "kind": "property", "name": name })
        }
        ReasonPathEntry::Indexer => serde_json::json!({ "kind": "indexer" }),
        ReasonPathEntry::UnionMember(index) => {
            serde_json::json!({ "kind": "union-member", "index": index })
        }
        ReasonPathEntry::IntersectionMember(index) => {
            serde_json::json!({ "kind": "intersection-member", "index": index })
        }
        ReasonPathEntry::Negation => serde_json::json!({ "kind": "negation" }),
        ReasonPathEntry::Metatable => serde_json::json!({ "kind": "metatable" }),
    }
}

/// Wire rendering for one reason path: an array of entry objects.
fn reason_path_json(path: &ReasonPath) -> serde_json::Value {
    serde_json::json!(
        path.entries
            .iter()
            .map(reason_path_entry_json)
            .collect::<Vec<_>>()
    )
}

/// Shorthand for building a wire object from literal key/value pairs.
macro_rules! wire_object {
    ($($key:literal : $value:expr),* $(,)?) => {{
        let mut map = serde_json::Map::new();
        $(map.insert($key.to_owned(), serde_json::json!($value));)*
        map
    }};
}

impl Payload {
    /// Renders the canonical wire JSON for this payload.
    ///
    /// This is the single serializer behind `Diagnostic::payload`.
    /// The shapes reproduce, byte for byte, what each producer site has
    /// always emitted — including the variants whose wire form is an
    /// empty object and the optional keys that appear only when the
    /// corresponding typed fields are set.
    #[must_use]
    pub fn wire_json(&self) -> serde_json::Value {
        let map = match self {
            Self::Empty
            | Self::OccursCheck
            | Self::PropertySetMismatch
            | Self::PropertyMetadataMismatch { .. }
            | Self::NotCallable
            | Self::AmbiguousOverload { .. } => serde_json::Map::new(),
            Self::TypeMismatchDetail { expected, actual } => wire_object! {
                "expected": expected,
                "actual": actual,
            },
            Self::MissingProperty {
                name,
                owner,
                union,
                subtype,
            } => {
                let mut map = match union {
                    Some(union) => wire_object! {
                        "kind": "union-property-missing",
                        "property": name,
                        "owner": owner,
                        "missing_options": union.missing_options,
                        "all_options_missing": union.all_options_missing,
                    },
                    None => serde_json::Map::new(),
                };
                subtype.extend_wire(&mut map);
                map
            }
            Self::MissingProperties { subtype, .. } | Self::LikeKeySuggestion { subtype, .. } => {
                let mut map = serde_json::Map::new();
                subtype.extend_wire(&mut map);
                map
            }
            Self::PropertyVariance {
                access_target,
                properties_searched,
                subtype,
                ..
            } => {
                let mut map = serde_json::Map::new();
                if let Some(access_target) = access_target {
                    map.insert("access_target".to_owned(), serde_json::json!(access_target));
                }
                if !properties_searched.is_empty() {
                    map.insert(
                        "properties_searched".to_owned(),
                        serde_json::json!(properties_searched),
                    );
                }
                subtype.extend_wire(&mut map);
                map
            }
            Self::ArityMismatch { counts, subtype } => {
                let mut map = match counts {
                    Some(counts) => wire_object! {
                        "expected": counts.expected,
                        "actual": counts.actual,
                    },
                    None => serde_json::Map::new(),
                };
                subtype.extend_wire(&mut map);
                map
            }
            Self::SubtypeMismatch {
                indexer_part,
                subtype,
            } => {
                let mut map = serde_json::Map::new();
                if let Some(part) = indexer_part {
                    map.insert("indexer_part".to_owned(), serde_json::json!(part));
                }
                subtype.extend_wire(&mut map);
                map
            }
            Self::OverloadCandidates { candidates } => wire_object! {
                "overload_candidates": candidates,
            },
            Self::UnknownSymbol { symbol } => wire_object! { "symbol": symbol },
            Self::UnknownType { name } => wire_object! { "type": name },
            Self::BinaryOperatorMismatch {
                operator,
                left,
                right,
                overload,
                metatable_mismatch,
            } => {
                let mut map = wire_object! {
                    "operator": operator,
                    "left": left,
                    "right": right,
                    "overload": overload,
                };
                if *metatable_mismatch {
                    map.insert("metatable_mismatch".to_owned(), serde_json::json!(true));
                }
                map
            }
            Self::UnaryOperatorMismatch {
                operator,
                operand,
                overload,
            } => wire_object! {
                "operator": operator,
                "operand": operand,
                "overload": overload,
            },
            Self::ExplicitTypeInstantiationNotFunction => wire_object! {
                "kind": "explicit-type-instantiation-not-function",
            },
            Self::ExplicitTypeInstantiationParameterCount {
                expected_types,
                expected_packs,
                actual_types,
                actual_packs,
            } => wire_object! {
                "kind": "explicit-type-instantiation-parameter-count",
                "expected_types": expected_types,
                "expected_packs": expected_packs,
                "actual_types": actual_types,
                "actual_packs": actual_packs,
            },
            Self::GenericPackCallArgumentMismatch { type_mismatch } => wire_object! {
                "kind": if *type_mismatch {
                    "generic-pack-call-argument-type-mismatch"
                } else {
                    "generic-pack-call-argument-mismatch"
                },
            },
            Self::NoOverloadMatch {
                available_overloads,
                ..
            } => {
                let mut map = serde_json::Map::new();
                if !available_overloads.is_empty() {
                    map.insert(
                        "available_overloads".to_owned(),
                        serde_json::json!(available_overloads),
                    );
                }
                map
            }
            Self::Error { kind, message } => wire_object! {
                "kind": kind,
                "message": message,
            },
            Self::ResolverError {
                kind,
                module,
                display_name,
                path,
                detail,
            } => {
                let mut map = wire_object! { "kind": kind };
                if let Some(module) = module {
                    map.insert("module".to_owned(), serde_json::json!(module));
                }
                if let Some(display_name) = display_name {
                    map.insert("displayName".to_owned(), serde_json::json!(display_name));
                }
                if let Some(path) = path {
                    map.insert("path".to_owned(), serde_json::json!(path));
                }
                if let Some(detail) = detail {
                    map.insert("detail".to_owned(), serde_json::json!(detail));
                }
                map
            }
            Self::RequireCycle { module, cycle } => wire_object! {
                "module": module,
                "cycle": cycle,
            },
            Self::ReservedTypeIdentifier { name } => wire_object! {
                "kind": "reserved-type-identifier",
                "name": name,
            },
            Self::DuplicateTypeDefinition { name } => wire_object! {
                "kind": "duplicate-type-definition",
                "name": name,
            },
            Self::NilablePropertyRead { property, owner } => wire_object! {
                "kind": "nilable-property-read",
                "property": property,
                "owner": owner,
            },
            Self::PropertyAccessViolation { property, access } => wire_object! {
                "kind": "property-access-violation",
                "property": property,
                "access": access,
            },
            Self::GenericCountMismatch(_) => wire_object! {
                "kind": "generic-count-mismatch",
            },
            Self::ConstraintSolvingIncompleteForced => wire_object! {
                "kind": crate::magic_types::FORCED_CONSTRAINT_SOLVING_INCOMPLETE_KIND,
            },
            Self::TypeFunctionPackArgument { type_function } => wire_object! {
                "kind": "type-function-pack-argument",
                "type_function": type_function,
            },
            Self::UnappliedTypeFunction { type_function } => wire_object! {
                "kind": "unapplied-type-function",
                "type_function": type_function,
            },
            Self::TypeFunctionRuntimeError { reason } => wire_object! {
                "kind": "type-function-runtime-error",
                "reason": reason,
            },
            Self::GenericAliasParameterCount {
                alias,
                expected_type_parameters,
                expected_type_pack_parameters,
                actual_type_parameters,
                actual_type_pack_parameters,
            } => wire_object! {
                "kind": "generic-alias-parameter-count",
                "alias": alias,
                "expected_type_parameters": expected_type_parameters,
                "expected_type_pack_parameters": expected_type_pack_parameters,
                "actual_type_parameters": actual_type_parameters,
                "actual_type_pack_parameters": actual_type_pack_parameters,
            },
            Self::GenericAliasParameterOrder { alias } => wire_object! {
                "kind": "generic-alias-parameter-order",
                "alias": alias,
            },
            Self::GenericAliasPackInTypeSlot { alias } => wire_object! {
                "kind": "generic-alias-pack-in-type-slot",
                "alias": alias,
            },
            Self::GenericTypeUsedAsPack { type_parameter } => wire_object! {
                "kind": "generic-type-used-as-pack",
                "type_parameter": type_parameter,
            },
            Self::GenericPackUsedAsType {
                type_pack_parameter,
            } => wire_object! {
                "kind": "generic-pack-used-as-type",
                "type_pack_parameter": type_pack_parameter,
            },
            Self::RecursiveRestraintViolation { alias } => wire_object! {
                "kind": "recursive-restraint-violation",
                "alias": alias,
            },
            Self::RecursiveTypeAlias { alias } => wire_object! {
                "kind": "recursive-type-alias",
                "alias": alias,
            },
            Self::DuplicateGenericParameter { alias, parameter } => wire_object! {
                "kind": "duplicate-generic-parameter",
                "alias": alias,
                "parameter": parameter,
            },
            Self::DuplicateGenericParameterName { name } => wire_object! {
                "kind": "duplicate-generic-parameter",
                "name": name,
            },
            Self::FunctionExitsWithoutReturning => wire_object! {
                "kind": "function-exits-without-returning",
            },
            Self::IterMetamethodMissingState { required, provided } => wire_object! {
                "kind": "iter-metamethod-missing-state",
                "required": required,
                "provided": provided,
            },
            Self::ExplicitFunctionAnnotationRecommended {
                recommended_return,
                recommended_args,
            } => {
                let mut map = wire_object! {
                    "kind": "explicit-function-annotation-recommended",
                };
                if let Some(recommended_return) = recommended_return {
                    map.insert(
                        "recommended_return".to_owned(),
                        serde_json::json!(recommended_return),
                    );
                }
                if let Some(recommended_args) = recommended_args {
                    map.insert(
                        "recommended_args".to_owned(),
                        serde_json::json!(
                            recommended_args
                                .iter()
                                .map(|argument| {
                                    serde_json::json!({
                                        "name": argument.name,
                                        "type": argument.ty,
                                    })
                                })
                                .collect::<Vec<_>>()
                        ),
                    );
                }
                map
            }
            Self::ReturnArityMismatch { expected, actual } => wire_object! {
                "expected": expected,
                "actual": actual,
            },
            Self::ParameterReducedToNever { parameter } => wire_object! {
                "kind": "parameter-reduced-to-never",
                "parameter": parameter,
            },
            Self::ParameterRequiredSubtype {
                parameter,
                required,
            } => wire_object! {
                "kind": "parameter-required-subtype",
                "parameter": parameter,
                "required": required,
            },
            Self::UninhabitedTypeFunction { instance } => wire_object! {
                "kind": "uninhabited",
                "instance": instance,
            },
            Self::RequiredExport {
                name,
                required,
                actual,
            } => {
                let mut map = wire_object! {
                    "kind": "required-export",
                    "name": name,
                    "required": required,
                };
                if let Some(actual) = actual {
                    map.insert("actual".to_owned(), serde_json::json!(actual));
                }
                map
            }
            Self::Conformance {
                name,
                required,
                actual,
            } => {
                let mut map = wire_object! {
                    "kind": "conformance",
                    "name": name,
                    "required": required,
                };
                if let Some(actual) = actual {
                    map.insert("actual".to_owned(), serde_json::json!(actual));
                }
                map
            }
        };
        serde_json::Value::Object(map)
    }

    /// Returns the property name carried by this payload, if any.
    ///
    /// Covers the property-bearing variants:
    /// `MissingProperty`, `PropertyVariance`, `PropertyMetadataMismatch`,
    /// `LikeKeySuggestion::looked_up`. Returns `None` for variants
    /// that don't carry a single property name.
    #[cfg(any())]
    #[must_use]
    pub(crate) fn property_name(&self) -> Option<&str> {
        match self {
            Self::MissingProperty { name, .. } | Self::PropertyMetadataMismatch { name } => {
                Some(name.as_str())
            }
            Self::PropertyVariance { name, .. } => name.as_deref(),
            Self::LikeKeySuggestion { looked_up, .. } => Some(looked_up.as_str()),
            _ => None,
        }
    }

    /// Returns the generic-parameter-count mismatch attached to this
    /// payload's subtype context, if any.
    #[must_use]
    pub(crate) const fn generic_count_mismatch(&self) -> Option<&GenericCountMismatch> {
        match self {
            Self::MissingProperty { subtype, .. }
            | Self::MissingProperties { subtype, .. }
            | Self::LikeKeySuggestion { subtype, .. }
            | Self::PropertyVariance { subtype, .. }
            | Self::ArityMismatch { subtype, .. }
            | Self::SubtypeMismatch { subtype, .. } => subtype.generic_count_mismatch.as_ref(),
            _ => None,
        }
    }

    /// Returns true when this payload carries information about a
    /// type-pack arity disagreement (call site, function pack compare,
    /// return mismatch).
    #[cfg(any())]
    #[must_use]
    pub(crate) const fn is_arity_mismatch(&self) -> bool {
        matches!(
            self,
            Self::ArityMismatch { .. } | Self::ReturnArityMismatch { .. }
        )
    }

    /// Returns true when this payload carries an operator-related
    /// mismatch (`__add` / `__unm` / etc.).
    #[cfg(any())]
    #[must_use]
    pub(crate) const fn is_operator_mismatch(&self) -> bool {
        matches!(
            self,
            Self::BinaryOperatorMismatch { .. } | Self::UnaryOperatorMismatch { .. }
        )
    }
}

/// Structured reason-path entry describing how a subtype check
/// reached its failure point.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReasonPath {
    /// Ordered list of structural reason-path entries.
    pub entries: Vec<ReasonPathEntry>,
}

/// One reason-path entry — e.g. "entered the second argument of the
/// callee", "descended into the `name` property", "expanded the second
/// branch of a union".
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReasonPathEntry {
    /// Argument index inside a function pack.
    Argument(usize),
    /// Return index inside a function pack.
    Return(usize),
    /// Variadic tail step.
    VariadicTail,
    /// Property access by name.
    Property(String),
    /// Indexer step (key or value).
    Indexer,
    /// Union member index.
    UnionMember(usize),
    /// Intersection member index.
    IntersectionMember(usize),
    /// Negation step.
    Negation,
    /// Metatable step.
    Metatable,
}

/// Diagnostic suppression metadata: which branches of a structured
/// type-mismatch should be quieted because they originated from an
/// error-suppressing source (e.g. `error`, `any`, or a deliberately
/// permissive surface).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SuppressionMetadata {
    /// True when every leaf this diagnostic depends on is suppressing.
    pub fully_suppressing: bool,
    /// Specific reason-path entries whose contribution is suppressing.
    pub suppressing_entries: Vec<ReasonPath>,
}

/// Structured diagnostic emitted by the type checker.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Diagnostic {
    /// Stable category.
    pub category: DiagnosticCategory,
    /// Severity.
    pub severity: Severity,
    /// Primary source range.
    pub primary_location: DiagnosticLocation,
    /// Related source ranges.
    pub related_locations: Vec<DiagnosticLocation>,
    /// Machine-readable payload (canonical wire shape — fixtures
    /// compare against this). Derived from `typed_payload` via
    /// [`Payload::wire_json`]; the only writer is [`Self::set_typed`]
    /// (plus the test-only [`Self::with_payload`] escape hatch). Read
    /// it through [`Self::payload`].
    #[serde(default)]
    payload: serde_json::Value,
    /// Typed payload — the source of truth for the wire `payload`. Not
    /// serialized; not part of the fixture wire contract. Public for
    /// read access; write through [`Self::with_typed`] /
    /// [`Self::set_typed`] so the wire JSON stays in lockstep.
    #[serde(skip)]
    pub typed_payload: Payload,
    /// Structured reason path describing how a subtype check reached
    /// its failure point. Not serialized; not part of the fixture wire
    /// contract.
    #[serde(skip)]
    pub reason_path: Option<ReasonPath>,
    /// Structured suppression metadata, when this diagnostic was
    /// derived from a partially error-suppressing structural compare.
    /// Not serialized; not part of the fixture wire contract.
    #[serde(skip)]
    pub suppression: SuppressionMetadata,
    /// Optional prose context. Tests should not use this as an authoritative
    /// oracle.
    #[serde(default)]
    pub context: Option<String>,
}

/// Conversion-friendly diagnostic view for host adapters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticView<'a> {
    /// Stable severity.
    pub severity: Severity,
    /// Stable category.
    pub category: &'a DiagnosticCategory,
    /// Stable category label.
    pub category_label: Cow<'static, str>,
    /// Stable numeric compatibility code.
    pub code: u32,
    /// Primary source range using one-based line and column numbers.
    pub primary_location: OneBasedDiagnosticLocation,
    /// Related source ranges using one-based line and column numbers.
    pub related_locations: Vec<OneBasedDiagnosticLocation>,
    /// Payload-aware human-readable message.
    pub message: String,
    /// Typed machine-readable payload.
    pub payload: &'a Payload,
}

/// Owned, presentation-neutral diagnostic data for host adapters.
///
/// Unlike [`DiagnosticView`], this record can outlive the checker storage that
/// produced it. `payload` preserves the typed Ruau detail while
/// `wire_payload` lets serialization-oriented adapters forward the stable wire
/// shape without matching every [`Payload`] variant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticRecord {
    /// Stable severity.
    pub severity: Severity,
    /// Stable category.
    pub category: DiagnosticCategory,
    /// Stable category label.
    pub category_label: String,
    /// Stable numeric compatibility code.
    pub code: u32,
    /// Primary source range using one-based line and column numbers.
    pub primary_location: OneBasedDiagnosticLocation,
    /// Related source ranges using one-based line and column numbers.
    pub related_locations: Vec<OneBasedDiagnosticLocation>,
    /// Payload-aware human-readable message.
    pub message: String,
    /// Typed machine-readable payload.
    pub payload: Payload,
    /// Stable serialization-oriented payload shape.
    pub wire_payload: serde_json::Value,
}

impl DiagnosticView<'_> {
    /// Copies this borrowed view into an application-owned record.
    #[must_use]
    pub fn to_record(&self) -> DiagnosticRecord {
        DiagnosticRecord {
            severity: self.severity,
            category: self.category.clone(),
            category_label: self.category_label.to_string(),
            code: self.code,
            primary_location: self.primary_location,
            related_locations: self.related_locations.clone(),
            message: self.message.clone(),
            payload: self.payload.clone(),
            wire_payload: self.payload.wire_json(),
        }
    }
}

/// Collection of diagnostics produced by the type checker.
///
/// Dereferences to `[Diagnostic]`, so all read-only slice methods
/// (`len`, `iter`, `first`, indexing, ...) are available directly.
/// Mutation goes through the domain methods (`push`, `extend`, `dedup`,
/// `clear`, `truncate`, `capped`).
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Diagnostics {
    items: Vec<Diagnostic>,
}

impl Diagnostics {
    /// Creates an empty diagnostics collection.
    #[must_use]
    pub const fn new() -> Self {
        Self { items: Vec::new() }
    }

    /// Wraps an existing diagnostic vector.
    #[must_use]
    pub fn from_vec(items: Vec<Diagnostic>) -> Self {
        Self { items }
    }

    /// Consumes the collection and returns the underlying vector.
    #[must_use]
    pub fn into_vec(self) -> Vec<Diagnostic> {
        self.items
    }

    /// Returns diagnostics as a slice.
    #[must_use]
    pub fn as_slice(&self) -> &[Diagnostic] {
        &self.items
    }

    /// Returns diagnostics as a mutable slice.
    ///
    /// Crate-internal: element mutation stays domain-controlled; external
    /// consumers read diagnostics through the `Deref<Target = [Diagnostic]>`
    /// view.
    #[must_use]
    pub(crate) fn as_mut_slice(&mut self) -> &mut [Diagnostic] {
        &mut self.items
    }

    /// Adds one diagnostic.
    pub fn push(&mut self, diagnostic: Diagnostic) {
        self.items.push(diagnostic);
    }

    /// Extends the collection with diagnostics.
    pub fn extend(&mut self, diagnostics: impl IntoIterator<Item = Diagnostic>) {
        self.items.extend(diagnostics);
    }

    /// Removes all diagnostics from the collection.
    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// Truncates the collection to at most `len` diagnostics.
    pub fn truncate(&mut self, len: usize) {
        self.items.truncate(len);
    }

    /// Consumes the collection and returns at most `limit` diagnostics.
    #[must_use]
    pub fn capped(mut self, limit: usize) -> Self {
        self.truncate(limit);
        self
    }

    /// Returns true when at least one diagnostic is present.
    #[must_use]
    pub fn has_issues(&self) -> bool {
        !self.is_empty()
    }

    /// Returns true when at least one error-severity diagnostic is present.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.items
            .iter()
            .any(|diagnostic| diagnostic.severity == Severity::Error)
    }

    /// Counts error-severity diagnostics.
    #[must_use]
    pub fn error_count(&self) -> usize {
        self.items
            .iter()
            .filter(|diagnostic| diagnostic.severity == Severity::Error)
            .count()
    }

    /// Counts warning-severity diagnostics.
    #[must_use]
    pub fn warning_count(&self) -> usize {
        self.items
            .iter()
            .filter(|diagnostic| diagnostic.severity == Severity::Warning)
            .count()
    }

    /// Removes duplicate diagnostics, preserving first occurrence order.
    pub fn dedup(&mut self) {
        let mut keys: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
        let mut unique = Vec::new();
        for diagnostic in self.items.drain(..) {
            let hash = diagnostic_identity_hash(&diagnostic);
            let duplicate = keys.get(&hash).is_some_and(|indices| {
                indices
                    .iter()
                    .any(|index| diagnostic_identity_eq(&unique[*index], &diagnostic))
            });
            if duplicate {
                continue;
            }
            keys.entry(hash).or_default().push(unique.len());
            unique.push(diagnostic);
        }
        self.items = unique;
    }

    /// Renders diagnostics as concise human-readable `source:line:column`
    /// entries.
    #[must_use]
    pub fn render(&self, source_name: &str) -> String {
        self.items
            .iter()
            .map(|diagnostic| {
                format!(
                    "{} {}: {}",
                    diagnostic_site(source_name, diagnostic.primary_location),
                    diagnostic.category,
                    diagnostic.user_message()
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Returns conversion-friendly diagnostic views.
    pub fn views(&self) -> impl Iterator<Item = DiagnosticView<'_>> {
        self.items.iter().map(Diagnostic::view)
    }

    /// Returns application-owned diagnostic records lazily.
    ///
    /// The iterator avoids a collection allocation when callers stream records
    /// into another sink.
    pub fn records(&self) -> impl Iterator<Item = DiagnosticRecord> + '_ {
        self.views().map(|view| view.to_record())
    }
}

impl Deref for Diagnostics {
    type Target = [Diagnostic];

    fn deref(&self) -> &Self::Target {
        &self.items
    }
}

impl From<Vec<Diagnostic>> for Diagnostics {
    fn from(items: Vec<Diagnostic>) -> Self {
        Self::from_vec(items)
    }
}

impl FromIterator<Diagnostic> for Diagnostics {
    fn from_iter<T: IntoIterator<Item = Diagnostic>>(iter: T) -> Self {
        Self::from_vec(iter.into_iter().collect())
    }
}

impl IntoIterator for Diagnostics {
    type Item = Diagnostic;
    type IntoIter = std::vec::IntoIter<Diagnostic>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.into_iter()
    }
}

impl<'a> IntoIterator for &'a Diagnostics {
    type Item = &'a Diagnostic;
    type IntoIter = std::slice::Iter<'a, Diagnostic>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.iter()
    }
}

/// A diagnostic qualified by the module that produced it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleDiagnostic {
    /// Canonical module identity.
    pub module: ModuleName,
    /// User-facing module display name.
    pub display_name: String,
    /// Diagnostic emitted for the module.
    pub diagnostic: Diagnostic,
}

impl ModuleDiagnostic {
    /// Returns a conversion-friendly module-qualified diagnostic view.
    #[must_use]
    pub fn view(&self) -> ModuleDiagnosticView<'_> {
        ModuleDiagnosticView {
            module: &self.module,
            display_name: &self.display_name,
            diagnostic: self.diagnostic.view(),
        }
    }
}

/// Conversion-friendly module-qualified diagnostic view.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleDiagnosticView<'a> {
    /// Canonical module identity.
    pub module: &'a ModuleName,
    /// User-facing module display name.
    pub display_name: &'a str,
    /// Diagnostic emitted for the module.
    pub diagnostic: DiagnosticView<'a>,
}

/// Owned module-qualified diagnostic data for host adapters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleDiagnosticRecord {
    /// Canonical module identity.
    pub module: ModuleName,
    /// User-facing module display name.
    pub display_name: String,
    /// Diagnostic emitted for the module.
    pub diagnostic: DiagnosticRecord,
}

impl ModuleDiagnosticView<'_> {
    /// Copies this borrowed view into an application-owned record.
    #[must_use]
    pub fn to_record(&self) -> ModuleDiagnosticRecord {
        ModuleDiagnosticRecord {
            module: self.module.clone(),
            display_name: self.display_name.to_owned(),
            diagnostic: self.diagnostic.to_record(),
        }
    }
}

/// Collection of module-qualified graph diagnostics.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GraphDiagnostics {
    entries: Vec<ModuleDiagnostic>,
}

impl GraphDiagnostics {
    /// Creates an empty graph diagnostics collection.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Wraps existing graph diagnostic entries.
    #[must_use]
    pub fn from_entries(entries: Vec<ModuleDiagnostic>) -> Self {
        Self { entries }
    }

    /// Returns module-qualified entries.
    #[must_use]
    pub fn entries(&self) -> &[ModuleDiagnostic] {
        &self.entries
    }

    /// Returns conversion-friendly module-qualified diagnostic views.
    pub fn views(&self) -> impl Iterator<Item = ModuleDiagnosticView<'_>> {
        self.entries.iter().map(ModuleDiagnostic::view)
    }

    /// Returns application-owned module diagnostic records lazily.
    pub fn records(&self) -> impl Iterator<Item = ModuleDiagnosticRecord> + '_ {
        self.views().map(|view| view.to_record())
    }

    /// Returns the number of module-qualified diagnostics.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns true when no module-qualified diagnostics are present.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Consumes the collection and returns module-qualified entries.
    #[must_use]
    pub fn into_entries(self) -> Vec<ModuleDiagnostic> {
        self.entries
    }

    /// Consumes graph diagnostics into an unqualified collection.
    #[must_use]
    pub fn into_flat_diagnostics(self) -> Diagnostics {
        self.entries
            .into_iter()
            .map(|entry| entry.diagnostic)
            .collect()
    }

    /// Returns true when at least one diagnostic is present.
    #[must_use]
    pub fn has_issues(&self) -> bool {
        !self.entries.is_empty()
    }

    /// Returns true when at least one error-severity diagnostic is present.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.diagnostic.severity == Severity::Error)
    }

    /// Removes duplicate graph diagnostics, preserving first occurrence order.
    pub fn dedup(&mut self) {
        let mut keys: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
        let mut unique = Vec::new();
        for entry in self.entries.drain(..) {
            let mut hasher = DefaultHasher::new();
            entry.module.hash(&mut hasher);
            diagnostic_identity_hash_into(&entry.diagnostic, &mut hasher);
            let hash = hasher.finish();
            let duplicate = keys.get(&hash).is_some_and(|indices| {
                indices.iter().any(|index| {
                    let existing: &ModuleDiagnostic = &unique[*index];
                    existing.module == entry.module
                        && diagnostic_identity_eq(&existing.diagnostic, &entry.diagnostic)
                })
            });
            if duplicate {
                continue;
            }
            keys.entry(hash).or_default().push(unique.len());
            unique.push(entry);
        }
        self.entries = unique;
    }

    /// Consumes the collection and returns at most `limit` entries.
    #[must_use]
    pub fn capped(mut self, limit: usize) -> Self {
        self.entries.truncate(limit);
        self
    }

    /// Renders diagnostics using each entry's module display name.
    #[must_use]
    pub fn render(&self) -> String {
        self.entries
            .iter()
            .map(|entry| {
                format!(
                    "{} {}: {}",
                    diagnostic_site(&entry.display_name, entry.diagnostic.primary_location),
                    entry.diagnostic.category,
                    entry.diagnostic.user_message()
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

impl From<Vec<ModuleDiagnostic>> for GraphDiagnostics {
    fn from(entries: Vec<ModuleDiagnostic>) -> Self {
        Self::from_entries(entries)
    }
}

impl FromIterator<ModuleDiagnostic> for GraphDiagnostics {
    fn from_iter<T: IntoIterator<Item = ModuleDiagnostic>>(iter: T) -> Self {
        Self::from_entries(iter.into_iter().collect())
    }
}

impl Diagnostic {
    /// Creates an error diagnostic at `primary_location`.
    #[must_use]
    pub fn error(
        category: DiagnosticCategory,
        primary_location: impl Into<DiagnosticLocation>,
    ) -> Self {
        Self::new(category, Severity::Error, primary_location.into())
    }

    /// Creates a diagnostic.
    #[must_use]
    pub fn new(
        category: DiagnosticCategory,
        severity: Severity,
        primary_location: DiagnosticLocation,
    ) -> Self {
        Self {
            category,
            severity,
            primary_location,
            related_locations: Vec::new(),
            payload: serde_json::Value::Object(serde_json::Map::new()),
            typed_payload: Payload::Empty,
            reason_path: None,
            suppression: SuppressionMetadata::default(),
            context: None,
        }
    }

    /// Attaches a related location.
    #[must_use]
    pub fn with_related_location(mut self, location: impl Into<DiagnosticLocation>) -> Self {
        self.related_locations.push(location.into());
        self
    }

    /// Attaches a machine-readable payload.
    ///
    /// Test-only escape hatch for synthesizing wire payloads directly;
    /// producers attach payloads through [`Self::with_typed`] so the
    /// typed channel and the wire stay in lockstep.
    #[must_use]
    #[cfg(any())]
    // Only tests exercise it; keep it compiling (unused) under `fixtures`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn with_payload(mut self, payload: serde_json::Value) -> Self {
        self.payload = payload;
        self
    }

    /// Attaches a typed payload and renders the canonical wire JSON
    /// from it.
    #[must_use]
    pub fn with_typed(mut self, payload: Payload) -> Self {
        self.set_typed(payload);
        self
    }

    /// Sets the typed payload and re-renders `payload` through
    /// [`Payload::wire_json`].
    pub fn set_typed(&mut self, payload: Payload) {
        self.payload = payload.wire_json();
        self.typed_payload = payload;
    }

    /// Attaches non-authoritative prose context.
    #[must_use]
    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        self.context = Some(context.into());
        self
    }

    /// Machine-readable payload in the canonical wire shape (the JSON
    /// fixtures compare against). Derived from [`Self::typed_payload`]
    /// by [`Payload::wire_json`] whenever a typed payload is attached.
    #[must_use]
    pub fn payload(&self) -> &serde_json::Value {
        &self.payload
    }

    /// Stable diagnostic category.
    #[must_use]
    pub const fn category(&self) -> &DiagnosticCategory {
        &self.category
    }

    /// Stable diagnostic severity.
    #[must_use]
    pub const fn severity(&self) -> Severity {
        self.severity
    }

    /// Primary zero-based source range.
    #[must_use]
    pub const fn primary_location(&self) -> DiagnosticLocation {
        self.primary_location
    }

    /// Primary one-based source range for host editor adapters.
    #[must_use]
    pub const fn primary_location_one_based(&self) -> OneBasedDiagnosticLocation {
        self.primary_location.to_one_based()
    }

    /// Related zero-based source ranges.
    #[must_use]
    pub fn related_locations(&self) -> &[DiagnosticLocation] {
        &self.related_locations
    }

    /// Typed machine-readable payload.
    #[must_use]
    pub const fn typed_payload(&self) -> &Payload {
        &self.typed_payload
    }

    /// Returns a stable payload-aware human-readable message for end users.
    #[must_use]
    pub fn message(&self) -> String {
        self.user_message()
    }

    /// Returns a conversion-friendly borrowed view.
    #[must_use]
    pub fn view(&self) -> DiagnosticView<'_> {
        DiagnosticView {
            severity: self.severity(),
            category: self.category(),
            category_label: self.category.display_label(),
            code: self.code(),
            primary_location: self.primary_location_one_based(),
            related_locations: self
                .related_locations
                .iter()
                .copied()
                .map(DiagnosticLocation::to_one_based)
                .collect(),
            message: self.message(),
            payload: self.typed_payload(),
        }
    }

    /// Returns a stable human-readable message for end users.
    ///
    /// This is derived from the typed diagnostic payload when available. For
    /// older diagnostics that only carry prose context, obviously internal
    /// recovery strings are replaced with a category-level fallback.
    #[must_use]
    pub fn user_message(&self) -> String {
        let fallback = default_user_message(&self.category);
        let message = match &self.typed_payload {
            Payload::Empty => self.context.clone().unwrap_or_else(|| fallback.clone()),
            // A bare subtype mismatch renders generically; the solver's prose
            // context carries the human-readable type detail, so prefer it and
            // let the leak guard below sanitize it.
            payload @ Payload::SubtypeMismatch {
                indexer_part: None, ..
            } => self
                .context
                .clone()
                .unwrap_or_else(|| payload_user_message(payload, &self.category)),
            payload => payload_user_message(payload, &self.category),
        };
        if leaks_internal_message_token(&message) {
            fallback
        } else {
            message
        }
    }

    /// Returns the fixture-comparison key: category, severity, locations, and
    /// payload, excluding prose context.
    #[must_use]
    #[cfg(any())]
    fn comparison_key(&self) -> TestDiagnosticKey {
        TestDiagnosticKey {
            category: self.category.clone(),
            severity: self.severity,
            primary_location: self.primary_location,
            related_locations: self.related_locations.clone(),
            payload: self.payload.clone(),
        }
    }

    /// Numeric compatibility code for this diagnostic.
    #[must_use]
    pub const fn code(&self) -> u32 {
        self.category.code()
    }

    /// Creates an unknown-symbol diagnostic.
    #[must_use]
    pub fn unknown_symbol(
        symbol: impl Into<String>,
        location: impl Into<DiagnosticLocation>,
    ) -> Self {
        Self::error(DiagnosticCategory::UnknownSymbol, location).with_typed(
            Payload::UnknownSymbol {
                symbol: symbol.into(),
            },
        )
    }

    /// Creates an unknown-type diagnostic.
    #[must_use]
    pub fn unknown_type(name: impl Into<String>, location: impl Into<DiagnosticLocation>) -> Self {
        let name = name.into();
        Self::error(DiagnosticCategory::UnknownSymbol, location)
            .with_context(format!("Unknown type '{name}'"))
            .with_typed(Payload::UnknownType { name })
    }

    /// Creates a named type mismatch diagnostic.
    #[must_use]
    pub fn type_mismatch(expected: impl Into<String>, actual: impl Into<String>) -> Self {
        let expected = expected.into();
        let actual = actual.into();
        Self::error(
            DiagnosticCategory::TypeMismatch,
            DiagnosticLocation::missing(),
        )
        .with_context(format!(
            "Expected this to be '{expected}', but got '{actual}'"
        ))
        .with_typed(Payload::TypeMismatchDetail { expected, actual })
    }

    /// Creates a binary operator diagnostic.
    #[must_use]
    pub fn binary_operator_error(
        operator: impl Into<String>,
        left: impl Into<String>,
        right: impl Into<String>,
        overload: impl Into<String>,
    ) -> Self {
        let operator = operator.into();
        let left = left.into();
        let right = right.into();
        let overload = overload.into();
        Self::error(
            DiagnosticCategory::Operator,
            DiagnosticLocation::missing(),
        )
        .with_context(format!(
            "Operator '{operator}' could not be applied to operands of types {left} and {right}; there is no corresponding overload for {overload}"
        ))
        .with_typed(Payload::BinaryOperatorMismatch {
            operator,
            left,
            right,
            overload,
            metatable_mismatch: false,
        })
    }

    /// Creates a unary operator diagnostic.
    #[must_use]
    pub fn unary_operator_error(
        operator: impl Into<String>,
        operand: impl Into<String>,
        overload: impl Into<String>,
    ) -> Self {
        let operator = operator.into();
        let operand = operand.into();
        let overload = overload.into();
        Self::error(
            DiagnosticCategory::Operator,
            DiagnosticLocation::missing(),
        )
        .with_context(format!(
            "Operator '{operator}' could not be applied to operand of type {operand}; there is no corresponding overload for {overload}"
        ))
        .with_typed(Payload::UnaryOperatorMismatch {
            operator,
            operand,
            overload,
        })
    }

    /// Creates an uninhabited type-function diagnostic.
    #[must_use]
    pub fn uninhabited_type_function(
        instance: impl Into<String>,
        location: impl Into<DiagnosticLocation>,
    ) -> Self {
        let instance = instance.into();
        Self::error(DiagnosticCategory::TypeFunction, location)
            .with_context(format!("Type function instance {instance} is uninhabited"))
            .with_typed(Payload::UninhabitedTypeFunction { instance })
    }

    /// Converts a parser diagnostic into the shared checker diagnostic model.
    #[must_use]
    pub fn from_parse_error(error: &ruau_syntax::parse::Error) -> Self {
        Self::error(DiagnosticCategory::Parse, error.location).with_typed(Payload::Error {
            kind: parse_error_kind(error.kind).to_owned(),
            message: error.message.clone(),
        })
    }

    /// Converts a source resolver diagnostic into the shared checker diagnostic
    /// model.
    #[must_use]
    pub fn from_resolver_diagnostic(error: &crate::graph::resolve::ResolverError) -> Self {
        Self::from_resolver_diagnostic_with_display_name(error, None)
    }

    /// Converts a source resolver diagnostic into the shared checker diagnostic
    /// model, preferring `display_name` for user-facing graph context while
    /// keeping the canonical module identity in the structured payload.
    #[must_use]
    pub fn from_resolver_diagnostic_with_display_name(
        error: &crate::graph::resolve::ResolverError,
        display_name: Option<&str>,
    ) -> Self {
        let kind = error.kind().to_owned();
        let module = error.module().map(|name| name.as_str().to_owned());
        let display_name = display_name
            .filter(|display_name| {
                !display_name.is_empty()
                    && module
                        .as_deref()
                        .is_none_or(|module| *display_name != module)
            })
            .map(str::to_owned);
        let path = error.path().map(|path| path.to_string_lossy().into_owned());
        let detail = error.detail().map(std::borrow::Cow::into_owned);
        let context = match &display_name {
            Some(display_name) => format!("{display_name}: {error}"),
            None => error.to_string(),
        };
        Self::error(DiagnosticCategory::Resolver, DiagnosticLocation::missing())
            .with_context(context)
            .with_typed(Payload::ResolverError {
                kind,
                module,
                display_name,
                path,
                detail,
            })
    }
}

fn diagnostic_identity_eq(left: &Diagnostic, right: &Diagnostic) -> bool {
    left.category == right.category
        && left.severity == right.severity
        && left.primary_location == right.primary_location
        && left.related_locations == right.related_locations
        && left.payload == right.payload
        && left.typed_payload == right.typed_payload
        && left.reason_path == right.reason_path
        && left.suppression == right.suppression
}

fn diagnostic_identity_hash(diagnostic: &Diagnostic) -> u64 {
    let mut hasher = DefaultHasher::new();
    diagnostic_identity_hash_into(diagnostic, &mut hasher);
    hasher.finish()
}

fn diagnostic_identity_hash_into(diagnostic: &Diagnostic, hasher: &mut impl Hasher) {
    diagnostic.category.code().hash(hasher);
    diagnostic.severity.hash(hasher);
    diagnostic.primary_location.hash(hasher);
    diagnostic.related_locations.hash(hasher);
    serde_json::to_writer(HasherWriter(hasher), &diagnostic.payload)
        .expect("hashing diagnostic JSON cannot fail");
    let mut writer = HasherFormatter(hasher);
    fmt::write(
        &mut writer,
        format_args!(
            "{:?}{:?}{:?}",
            diagnostic.typed_payload, diagnostic.reason_path, diagnostic.suppression
        ),
    )
    .expect("hash formatter is infallible");
}

struct HasherWriter<'a, H>(&'a mut H);

impl<H: Hasher> io::Write for HasherWriter<'_, H> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct HasherFormatter<'a, H>(&'a mut H);

impl<H: Hasher> fmt::Write for HasherFormatter<'_, H> {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        self.0.write(value.as_bytes());
        Ok(())
    }
}

#[cfg(any())]
fn test_snapshot(diagnostics: &[Diagnostic]) -> Vec<TestDiagnosticKey> {
    let mut snapshot = diagnostics
        .iter()
        .map(Diagnostic::comparison_key)
        .collect::<Vec<_>>();
    snapshot.sort_by(|left, right| {
        test_snapshot_key_sort_value(left).cmp(&test_snapshot_key_sort_value(right))
    });
    snapshot
}

#[cfg(any())]
fn render_test_snapshot(diagnostics: &[Diagnostic]) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&test_snapshot(diagnostics))
}

fn payload_user_message(payload: &Payload, category: &DiagnosticCategory) -> String {
    match payload {
        Payload::Empty => default_user_message(category),
        Payload::TypeMismatchDetail { expected, actual } => {
            format!("Expected type '{expected}', got '{actual}'")
        }
        Payload::MissingProperty {
            name, owner, union, ..
        } => match union {
            Some(union) if union.all_options_missing && !owner.is_empty() => {
                format!("Type '{owner}' does not have key '{name}'")
            }
            Some(union) if !union.missing_options.is_empty() && !owner.is_empty() => {
                format!(
                    "Key '{name}' is missing from {} in type '{owner}'",
                    quoted_list(&union.missing_options)
                )
            }
            _ if !owner.is_empty() => format!("Type '{owner}' is missing property '{name}'"),
            _ => format!("Missing property '{name}'"),
        },
        Payload::MissingProperties { names, owner, .. } => {
            let names = quoted_list(names);
            if owner.is_empty() {
                format!("Missing properties {names}")
            } else {
                format!("Type '{owner}' is missing properties {names}")
            }
        }
        Payload::LikeKeySuggestion {
            looked_up,
            suggestions,
            ..
        } => {
            if suggestions.is_empty() {
                format!("Unknown property '{looked_up}'")
            } else {
                format!(
                    "Unknown property '{looked_up}'. Did you mean {}?",
                    quoted_list(suggestions)
                )
            }
        }
        Payload::OverloadCandidates { candidates } => {
            format!("Available overloads: {}", candidates.join("; "))
        }
        Payload::UnknownSymbol { symbol } => format!("Unknown symbol '{symbol}'"),
        Payload::UnknownType { name } => format!("Unknown type '{name}'"),
        Payload::BinaryOperatorMismatch {
            operator,
            left,
            right,
            overload,
            metatable_mismatch,
        } => {
            if *metatable_mismatch {
                format!(
                    "Operator '{operator}' could not be applied to operands of types {left} and {right} because their metatables differ"
                )
            } else {
                format!(
                    "Operator '{operator}' could not be applied to operands of types {left} and {right}; there is no corresponding overload for {overload}"
                )
            }
        }
        Payload::UnaryOperatorMismatch {
            operator,
            operand,
            overload,
        } => {
            format!(
                "Operator '{operator}' could not be applied to operand of type {operand}; there is no corresponding overload for {overload}"
            )
        }
        Payload::PropertyVariance { name, .. } => match name {
            Some(name) => format!("Property '{name}' has incompatible read and write types"),
            None => "Property has incompatible read and write types".to_owned(),
        },
        Payload::ArityMismatch {
            counts: Some(counts),
            ..
        } => {
            format!(
                "Expected {} {}, got {}",
                counts.expected,
                plural(counts.expected, "value"),
                counts.actual
            )
        }
        Payload::ArityMismatch { counts: None, .. } => "Function arity is incompatible".to_owned(),
        Payload::SubtypeMismatch {
            indexer_part: Some(part),
            ..
        } => format!("Indexer {part} type is incompatible"),
        Payload::SubtypeMismatch { .. } => "Types are incompatible".to_owned(),
        Payload::OccursCheck => "Recursive type would be infinite".to_owned(),
        Payload::PropertySetMismatch => "Table property sets are incompatible".to_owned(),
        Payload::PropertyMetadataMismatch { name } => {
            format!("Property '{name}' has incompatible metadata")
        }
        Payload::NotCallable => "Value is not callable".to_owned(),
        Payload::ExplicitTypeInstantiationNotFunction => {
            "Explicit type instantiation requires a function".to_owned()
        }
        Payload::ExplicitTypeInstantiationParameterCount {
            expected_types,
            expected_packs,
            actual_types,
            actual_packs,
        } => format!(
            "Explicit type instantiation expected {expected_types} type {} and {expected_packs} type-pack {}, got {actual_types} and {actual_packs}",
            plural(*expected_types, "argument"),
            plural(*expected_packs, "argument")
        ),
        Payload::GenericPackCallArgumentMismatch {
            type_mismatch: true,
        } => "Generic pack call argument type is incompatible".to_owned(),
        Payload::GenericPackCallArgumentMismatch {
            type_mismatch: false,
        } => "Generic pack call argument count is incompatible".to_owned(),
        Payload::NoOverloadMatch { rejected, .. } => {
            format!(
                "No overload matched the call after checking {rejected} {}",
                plural(*rejected, "candidate")
            )
        }
        Payload::AmbiguousOverload { candidates } => {
            format!("Call is ambiguous between {candidates} overload candidates")
        }
        Payload::Error { message, .. } => message.clone(),
        Payload::ResolverError {
            kind,
            module,
            display_name,
            path,
            detail,
        } => resolver_user_message(
            kind,
            module.as_deref(),
            display_name.as_deref(),
            path.as_deref(),
            detail.as_deref(),
        ),
        Payload::RequireCycle { cycle, .. } => {
            format!("Cyclic module dependency: {}", cycle.join(" -> "))
        }
        Payload::ReservedTypeIdentifier { name } => {
            format!("Type identifier '{name}' is reserved")
        }
        Payload::DuplicateTypeDefinition { name } => {
            format!("Redefinition of type '{name}'")
        }
        Payload::NilablePropertyRead { property, owner } => {
            format!("Value of type '{owner}' could be nil when reading '{property}'")
        }
        Payload::PropertyAccessViolation { property, access } => {
            let (verb, modifier) = match access {
                PropertyAccess::Read => ("read", "write-only"),
                PropertyAccess::Write => ("write to", "read-only"),
                PropertyAccess::ReadWrite => ("access", "restricted"),
            };
            format!("Cannot {verb} property '{property}' because it is {modifier}")
        }
        Payload::GenericCountMismatch(mismatch) => {
            let kind = match mismatch.parameter {
                GenericParameterKind::Type => "generic type parameters",
                GenericParameterKind::Pack => "generic type pack parameters",
            };
            format!(
                "Different number of {kind}: subtype had {}, supertype had {}",
                mismatch.subtype_count, mismatch.supertype_count
            )
        }
        Payload::ConstraintSolvingIncompleteForced => {
            "Constraint solving did not complete".to_owned()
        }
        Payload::TypeFunctionPackArgument { type_function } => {
            format!("Type function '{type_function}' cannot be applied to a type pack")
        }
        Payload::UnappliedTypeFunction { type_function } => {
            format!("Type function '{type_function}' must be applied before use")
        }
        Payload::TypeFunctionRuntimeError { reason } => {
            format!("Type function failed during evaluation: {reason}")
        }
        Payload::GenericAliasParameterCount {
            alias,
            expected_type_parameters,
            expected_type_pack_parameters,
            actual_type_parameters,
            actual_type_pack_parameters,
        } => format!(
            "Generic alias '{alias}' expected {expected_type_parameters} type {} and {expected_type_pack_parameters} type-pack {}, got {actual_type_parameters} and {actual_type_pack_parameters}",
            plural(*expected_type_parameters, "argument"),
            plural(*expected_type_pack_parameters, "argument")
        ),
        Payload::GenericAliasParameterOrder { alias } => {
            format!("Type parameters must come before type pack parameters in alias '{alias}'")
        }
        Payload::GenericAliasPackInTypeSlot { alias } => {
            format!("Generic alias '{alias}' received a type pack where a type was expected")
        }
        Payload::GenericTypeUsedAsPack { type_parameter } => {
            format!(
                "Generic type parameter '{type_parameter}' was used where a type pack was expected"
            )
        }
        Payload::GenericPackUsedAsType {
            type_pack_parameter,
        } => {
            format!(
                "Generic type pack parameter '{type_pack_parameter}' was used where a type was expected"
            )
        }
        Payload::RecursiveRestraintViolation { alias } => {
            format!("Recursive alias '{alias}' was used with different parameters")
        }
        Payload::RecursiveTypeAlias { alias } => {
            format!("Recursive type alias '{alias}' cannot be resolved")
        }
        Payload::DuplicateGenericParameter { alias, parameter } => {
            format!("Generic alias '{alias}' declares parameter '{parameter}' more than once")
        }
        Payload::DuplicateGenericParameterName { name } => {
            format!("Generic parameter '{name}' is declared more than once")
        }
        Payload::FunctionExitsWithoutReturning => {
            "Function exits without returning the annotated values".to_owned()
        }
        Payload::IterMetamethodMissingState { required, provided } => format!(
            "__iter metamethod returned {provided} {}, but the next function requires {required}",
            plural(*provided, "value")
        ),
        Payload::ExplicitFunctionAnnotationRecommended {
            recommended_return,
            recommended_args,
        } => explicit_annotation_user_message(
            recommended_return.as_deref(),
            recommended_args.as_deref(),
        ),
        Payload::ReturnArityMismatch { expected, actual } => {
            format!(
                "Function returns {actual} {}, but the annotation expects {expected}",
                plural(*actual, "value")
            )
        }
        Payload::ParameterReducedToNever { parameter } => {
            format!("Parameter '{parameter}' cannot satisfy all uses")
        }
        Payload::ParameterRequiredSubtype {
            parameter,
            required,
        } => {
            format!("Parameter '{parameter}' must be compatible with '{required}'")
        }
        Payload::UninhabitedTypeFunction { instance } => {
            format!("Type function instance {instance} is uninhabited")
        }
        Payload::RequiredExport {
            name,
            required,
            actual: Some(actual),
        } => {
            format!(
                "Required global '{name}' has type '{actual}', which does not conform to '{required}'"
            )
        }
        Payload::RequiredExport {
            name,
            required,
            actual: None,
        } => {
            format!("Required global '{name}' is not defined; expected '{required}'")
        }
        Payload::Conformance {
            name,
            required,
            actual: Some(actual),
        } => {
            format!("Module '{name}' has type '{actual}', which does not conform to '{required}'")
        }
        Payload::Conformance { name, required, .. } => {
            format!("Module '{name}' does not export a value; expected '{required}'")
        }
    }
}

fn default_user_message(category: &DiagnosticCategory) -> String {
    match category {
        DiagnosticCategory::Parse => "Parse error".to_owned(),
        DiagnosticCategory::Resolver => "Module resolver error".to_owned(),
        DiagnosticCategory::TypeMismatch => "Type mismatch".to_owned(),
        DiagnosticCategory::UnknownSymbol => "Unknown symbol".to_owned(),
        DiagnosticCategory::Call => "Invalid call".to_owned(),
        DiagnosticCategory::Operator => "Invalid operator use".to_owned(),
        DiagnosticCategory::TableAccess => "Invalid table access".to_owned(),
        DiagnosticCategory::Constraint => "Constraint solver error".to_owned(),
        DiagnosticCategory::TypePack => "Type pack mismatch".to_owned(),
        DiagnosticCategory::Generic => "Generic type error".to_owned(),
        DiagnosticCategory::TypeFunction => "Type function error".to_owned(),
        DiagnosticCategory::Internal => "Type checker diagnostic".to_owned(),
        DiagnosticCategory::RequiredExport => "Required export error".to_owned(),
        DiagnosticCategory::Conformance => "Declaration conformance error".to_owned(),
        DiagnosticCategory::UpstreamCode(code) => {
            format!("Type checker diagnostic {code}")
        }
    }
}

fn resolver_user_message(
    kind: &str,
    module: Option<&str>,
    display_name: Option<&str>,
    path: Option<&str>,
    detail: Option<&str>,
) -> String {
    let subject = display_name.or_else(|| module.is_none().then_some(path).flatten());
    let message = match (kind, module, path, detail) {
        (_, _, _, Some(detail)) => detail.to_owned(),
        ("missing-module", Some(module), Some(path), _) => {
            format!("module `{module}` did not resolve (searched {path})")
        }
        ("missing-module", Some(module), None, _) => format!("module `{module}` did not resolve"),
        ("invalid-request", _, _, _) => "module request is invalid".to_owned(),
        _ => kind.replace('-', " "),
    };
    match subject {
        Some(subject) if !subject.is_empty() => format!("{subject}: {message}"),
        _ => message,
    }
}

fn explicit_annotation_user_message(
    recommended_return: Option<&str>,
    recommended_args: Option<&[RecommendedArgument]>,
) -> String {
    let mut parts = Vec::new();
    if let Some(return_ty) = recommended_return {
        parts.push(format!("return type {return_ty}"));
    }
    if let Some(args) = recommended_args
        && !args.is_empty()
    {
        parts.push(format!(
            "parameters {}",
            args.iter()
                .map(|arg| format!("{}: {}", arg.name, arg.ty))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if parts.is_empty() {
        "Add an explicit function annotation".to_owned()
    } else {
        format!(
            "Add explicit function annotation for {}",
            parts.join(" and ")
        )
    }
}

fn quoted_list(items: &[String]) -> String {
    items
        .iter()
        .map(|item| format!("'{item}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn plural(count: usize, singular: &str) -> &str {
    if count == 1 {
        singular
    } else {
        match singular {
            "candidate" => "candidates",
            "argument" => "arguments",
            "value" => "values",
            _ => singular,
        }
    }
}

fn leaks_internal_message_token(message: &str) -> bool {
    [
        "TypeId(",
        "TypePackId(",
        "SubtypeError",
        "SubtypeWithMetadata",
        "UnifyError",
        "ArenaBoundary",
        "TypePathRoot",
        "TypeKind::",
        "TypePackKind::",
    ]
    .iter()
    .any(|token| message.contains(token))
}

fn diagnostic_site(source_name: &str, location: DiagnosticLocation) -> String {
    if location == DiagnosticLocation::missing() {
        format!("{source_name}:?:?")
    } else {
        format!(
            "{source_name}:{}:{}",
            location.begin.line + 1,
            location.begin.column + 1
        )
    }
}

impl From<&ruau_syntax::parse::Error> for Diagnostic {
    fn from(error: &ruau_syntax::parse::Error) -> Self {
        Self::from_parse_error(error)
    }
}

impl From<ruau_syntax::parse::Error> for Diagnostic {
    fn from(error: ruau_syntax::parse::Error) -> Self {
        Self::from_parse_error(&error)
    }
}

impl From<&crate::graph::resolve::ResolverError> for Diagnostic {
    fn from(error: &crate::graph::resolve::ResolverError) -> Self {
        Self::from_resolver_diagnostic(error)
    }
}

impl From<crate::graph::resolve::ResolverError> for Diagnostic {
    fn from(error: crate::graph::resolve::ResolverError) -> Self {
        Self::from_resolver_diagnostic(&error)
    }
}

/// Diagnostic data compared by upstream-derived fixtures.
#[cfg(any())]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct TestDiagnosticKey {
    /// Stable category.
    pub category: DiagnosticCategory,
    /// Severity.
    pub severity: Severity,
    /// Primary source range.
    pub primary_location: DiagnosticLocation,
    /// Related source ranges.
    pub related_locations: Vec<DiagnosticLocation>,
    /// Machine-readable payload.
    pub payload: serde_json::Value,
}

/// Source range shape used by checker fixtures.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct DiagnosticLocation {
    /// First position covered by the range.
    pub begin: DiagnosticPosition,
    /// First position after the range.
    pub end: DiagnosticPosition,
}

impl DiagnosticLocation {
    /// Creates a source range.
    #[must_use]
    pub const fn new(begin: DiagnosticPosition, end: DiagnosticPosition) -> Self {
        Self { begin, end }
    }

    /// Missing-location sentinel used for diagnostics without a source range.
    #[must_use]
    pub const fn missing() -> Self {
        Self::new(DiagnosticPosition::missing(), DiagnosticPosition::missing())
    }

    /// Converts an optional parser location, falling back to
    /// [`Self::missing`] when the parser recorded none.
    #[must_use]
    pub fn from_opt(location: Option<ruau_syntax::Location>) -> Self {
        location.map(Self::from).unwrap_or_else(Self::missing)
    }

    /// Converts this zero-based range into a one-based host-adapter range.
    #[must_use]
    pub const fn to_one_based(self) -> OneBasedDiagnosticLocation {
        if self.is_missing() {
            OneBasedDiagnosticLocation::missing()
        } else {
            OneBasedDiagnosticLocation::new(self.begin.to_one_based(), self.end.to_one_based())
        }
    }

    /// Returns true when this is the missing-location sentinel.
    #[must_use]
    pub const fn is_missing(self) -> bool {
        self.begin.is_missing() && self.end.is_missing()
    }
}

impl From<ruau_syntax::Location> for DiagnosticLocation {
    fn from(location: ruau_syntax::Location) -> Self {
        Self {
            begin: location.begin.into(),
            end: location.end.into(),
        }
    }
}

/// Source position shape used by checker fixtures.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct DiagnosticPosition {
    /// Zero-based line number.
    pub line: u32,
    /// Zero-based column number.
    pub column: u32,
}

impl DiagnosticPosition {
    /// Creates a source position.
    #[must_use]
    pub const fn new(line: u32, column: u32) -> Self {
        Self { line, column }
    }

    /// Missing-position sentinel.
    #[must_use]
    pub const fn missing() -> Self {
        Self::new(u32::MAX, u32::MAX)
    }

    /// Converts this zero-based position into a one-based host-adapter
    /// position.
    #[must_use]
    pub const fn to_one_based(self) -> OneBasedDiagnosticPosition {
        if self.is_missing() {
            OneBasedDiagnosticPosition::missing()
        } else {
            OneBasedDiagnosticPosition::new(
                self.line.saturating_add(1),
                self.column.saturating_add(1),
            )
        }
    }

    /// Returns true when this is the missing-position sentinel.
    #[must_use]
    pub const fn is_missing(self) -> bool {
        self.line == u32::MAX && self.column == u32::MAX
    }
}

impl From<ruau_syntax::Position> for DiagnosticPosition {
    fn from(position: ruau_syntax::Position) -> Self {
        Self {
            line: position.line,
            column: position.column,
        }
    }
}

/// Stable string for a parser diagnostic kind.
fn parse_error_kind(kind: ruau_syntax::parse::ErrorKind) -> &'static str {
    match kind {
        ruau_syntax::parse::ErrorKind::UnsupportedSyntax => "unsupported-syntax",
        ruau_syntax::parse::ErrorKind::ExpectedToken => "expected-token",
        ruau_syntax::parse::ErrorKind::MalformedSyntax => "malformed-syntax",
        ruau_syntax::parse::ErrorKind::ErrorLimit => "error-limit",
    }
}

/// Stable string for a resolver diagnostic kind.
/// Deterministic sort key for diagnostic snapshots.
#[cfg(any())]
fn test_snapshot_key_sort_value(key: &TestDiagnosticKey) -> String {
    serde_json::to_string(key).expect("diagnostic comparison keys serialize")
}

#[cfg(any())]
mod tests {
    use super::*;

    #[test]
    fn comparison_key_omits_prose_context() {
        let location =
            DiagnosticLocation::new(DiagnosticPosition::new(1, 2), DiagnosticPosition::new(1, 8));
        let diagnostic = Diagnostic::error(DiagnosticCategory::TypeMismatch, location)
            .with_related_location(location)
            .with_payload(serde_json::json!({"expected": "number"}))
            .with_context("expected number, got string");

        let key = diagnostic.comparison_key();

        assert_eq!(key.category, DiagnosticCategory::TypeMismatch);
        assert_eq!(key.severity, Severity::Error);
        assert_eq!(key.related_locations, vec![location]);
        assert_eq!(key.payload, serde_json::json!({"expected": "number"}));
    }

    #[test]
    fn type_mismatch_constructor_populates_typed_payload() {
        let diagnostic = Diagnostic::type_mismatch("number", "string");
        assert_eq!(
            diagnostic.typed_payload,
            Payload::TypeMismatchDetail {
                expected: "number".to_owned(),
                actual: "string".to_owned()
            }
        );
        // The JSON payload still matches the canonical wire format.
        assert_eq!(
            diagnostic.payload,
            serde_json::json!({"expected": "number", "actual": "string"})
        );
    }

    #[test]
    fn diagnostic_summary_names_source_site_and_category() {
        let located = Diagnostic::error(
            DiagnosticCategory::TypeMismatch,
            DiagnosticLocation::new(
                DiagnosticPosition::new(2, 16),
                DiagnosticPosition::new(2, 21),
            ),
        )
        .with_context("Expected this to be 'number', but got 'string'");
        let missing = Diagnostic::error(
            DiagnosticCategory::UnknownSymbol,
            DiagnosticLocation::missing(),
        );

        let rendered = Diagnostics::from_vec(vec![located, missing]).render("=bad.luau");

        assert!(rendered.contains("=bad.luau:3:17 type-mismatch"));
        assert!(rendered.contains("Expected this to be 'number'"));
        assert!(rendered.contains("=bad.luau:?:? unknown-symbol"));
    }

    #[test]
    fn diagnostics_serializes_as_plain_diagnostic_array() {
        let diagnostic = Diagnostic::unknown_symbol(
            "foo",
            DiagnosticLocation::new(DiagnosticPosition::new(0, 4), DiagnosticPosition::new(0, 7)),
        );
        let vec_json = serde_json::to_value(vec![diagnostic.clone()]).expect("vec serializes");
        let diagnostics_json = serde_json::to_value(Diagnostics::from_vec(vec![diagnostic]))
            .expect("diagnostics serializes");

        assert_eq!(diagnostics_json, vec_json);
        assert!(diagnostics_json.is_array());
    }

    #[test]
    fn user_message_uses_typed_payloads() {
        let mismatch = Diagnostic::type_mismatch("number", "string");
        assert_eq!(
            mismatch.user_message(),
            "Expected type 'number', got 'string'"
        );

        let unknown = Diagnostic::unknown_type("Widget", DiagnosticLocation::missing());
        assert_eq!(unknown.user_message(), "Unknown type 'Widget'");

        let resolver = Diagnostic::from_resolver_diagnostic_with_display_name(
            &crate::graph::resolve::ResolverError::MissingModule {
                module: ruau_source::ModuleName::from("dep"),
                searched: None,
            },
            Some("display/dep.luau"),
        );
        assert_eq!(
            resolver.user_message(),
            "display/dep.luau: module `dep` did not resolve"
        );
    }

    #[test]
    fn diagnostic_view_uses_one_based_locations_and_typed_messages() {
        let location =
            DiagnosticLocation::new(DiagnosticPosition::new(0, 4), DiagnosticPosition::new(0, 7));
        let related =
            DiagnosticLocation::new(DiagnosticPosition::new(2, 1), DiagnosticPosition::new(2, 3));
        let diagnostic = Diagnostic::unknown_symbol("foo", location).with_related_location(related);

        let view = diagnostic.view();

        assert_eq!(view.category, &DiagnosticCategory::UnknownSymbol);
        assert_eq!(view.category_label, "unknown-symbol");
        assert_eq!(view.severity, Severity::Error);
        assert_eq!(view.code, 1003);
        assert_eq!(
            view.primary_location,
            OneBasedDiagnosticLocation::new(
                OneBasedDiagnosticPosition::new(1, 5),
                OneBasedDiagnosticPosition::new(1, 8),
            )
        );
        assert_eq!(
            view.related_locations,
            vec![OneBasedDiagnosticLocation::new(
                OneBasedDiagnosticPosition::new(3, 2),
                OneBasedDiagnosticPosition::new(3, 4),
            )]
        );
        assert_eq!(view.message, "Unknown symbol 'foo'");
        assert_eq!(
            view.payload,
            &Payload::UnknownSymbol {
                symbol: "foo".to_owned()
            }
        );
        assert_eq!(diagnostic.message(), view.message);
    }

    #[test]
    fn diagnostic_records_own_every_view_field_after_storage_is_dropped() {
        let location =
            DiagnosticLocation::new(DiagnosticPosition::new(0, 4), DiagnosticPosition::new(0, 7));
        let related =
            DiagnosticLocation::new(DiagnosticPosition::new(2, 1), DiagnosticPosition::new(2, 3));
        let diagnostics = Diagnostics::from_vec(vec![
            Diagnostic::unknown_symbol("foo", location).with_related_location(related),
        ]);

        let records = diagnostics.records().collect::<Vec<_>>();
        drop(diagnostics);

        let record = records.first().expect("one record");
        assert_eq!(record.category, DiagnosticCategory::UnknownSymbol);
        assert_eq!(record.category_label, "unknown-symbol");
        assert_eq!(record.severity, Severity::Error);
        assert_eq!(record.code, 1003);
        assert_eq!(
            record.primary_location,
            OneBasedDiagnosticLocation::new(
                OneBasedDiagnosticPosition::new(1, 5),
                OneBasedDiagnosticPosition::new(1, 8),
            )
        );
        assert_eq!(
            record.related_locations,
            vec![OneBasedDiagnosticLocation::new(
                OneBasedDiagnosticPosition::new(3, 2),
                OneBasedDiagnosticPosition::new(3, 4),
            )]
        );
        assert_eq!(record.message, "Unknown symbol 'foo'");
        assert_eq!(
            record.payload,
            Payload::UnknownSymbol {
                symbol: "foo".to_owned()
            }
        );
        assert_eq!(record.wire_payload, record.payload.wire_json());
    }

    #[test]
    fn records_cover_parse_type_warning_required_export_resolver_and_missing_locations() {
        let parse = ruau_syntax::parse::parse("local =")
            .errors
            .first()
            .map(Diagnostic::from_parse_error)
            .expect("invalid source has a parse error");
        let type_error = Diagnostic::type_mismatch("number", "string");
        let warning = Diagnostic::new(
            DiagnosticCategory::UnknownSymbol,
            Severity::Warning,
            DiagnosticLocation::missing(),
        )
        .with_typed(Payload::UnknownSymbol {
            symbol: "optional".to_owned(),
        });
        let required = Diagnostic::error(
            DiagnosticCategory::RequiredExport,
            DiagnosticLocation::missing(),
        )
        .with_typed(Payload::RequiredExport {
            name: "render".to_owned(),
            required: "() -> ()".to_owned(),
            actual: None,
        });
        let resolver = Diagnostic::from_resolver_diagnostic_with_display_name(
            &crate::graph::resolve::ResolverError::MissingModule {
                module: ModuleName::from("dep"),
                searched: None,
            },
            Some("app/dep.luau"),
        );
        let records = Diagnostics::from_vec(vec![parse, type_error, warning, required, resolver])
            .records()
            .collect::<Vec<_>>();

        assert_eq!(records[0].category, DiagnosticCategory::Parse);
        assert_eq!(records[1].category, DiagnosticCategory::TypeMismatch);
        assert_eq!(records[2].severity, Severity::Warning);
        assert_eq!(records[3].category, DiagnosticCategory::RequiredExport);
        assert_eq!(records[4].category, DiagnosticCategory::Resolver);
        assert!(records[1].primary_location.is_missing());
        assert!(records[2].primary_location.is_missing());
        assert!(records[3].primary_location.is_missing());
        assert!(records[4].message.contains("app/dep.luau"));
    }

    #[test]
    fn graph_records_own_module_identity_and_display_name() {
        let graph = GraphDiagnostics::from_entries(vec![ModuleDiagnostic {
            module: ModuleName::from("app/main"),
            display_name: "src/main.luau".to_owned(),
            diagnostic: Diagnostic::unknown_symbol("missing", DiagnosticLocation::missing()),
        }]);

        let records = graph.records().collect::<Vec<_>>();
        drop(graph);

        assert_eq!(records[0].module, ModuleName::from("app/main"));
        assert_eq!(records[0].display_name, "src/main.luau");
        assert_eq!(
            records[0].diagnostic.category,
            DiagnosticCategory::UnknownSymbol
        );
    }

    #[test]
    fn missing_locations_remain_missing_when_converted_to_one_based() {
        let diagnostic =
            Diagnostic::error(DiagnosticCategory::Internal, DiagnosticLocation::missing());

        assert!(diagnostic.primary_location_one_based().is_missing());
        assert!(DiagnosticPosition::missing().to_one_based().is_missing());
    }

    #[test]
    fn user_message_hides_internal_context_for_every_category() {
        let categories = vec![
            DiagnosticCategory::Parse,
            DiagnosticCategory::Resolver,
            DiagnosticCategory::TypeMismatch,
            DiagnosticCategory::UnknownSymbol,
            DiagnosticCategory::Call,
            DiagnosticCategory::Operator,
            DiagnosticCategory::TableAccess,
            DiagnosticCategory::Constraint,
            DiagnosticCategory::TypePack,
            DiagnosticCategory::Generic,
            DiagnosticCategory::TypeFunction,
            DiagnosticCategory::Internal,
            DiagnosticCategory::RequiredExport,
            DiagnosticCategory::UpstreamCode(1234),
        ];

        for category in categories {
            let diagnostic = Diagnostic::error(category, DiagnosticLocation::missing())
                .with_context("SubtypeError(TypeId(1), ArenaBoundary)");
            assert_no_internal_tokens(&diagnostic.user_message());
        }

        let typed = Diagnostic::type_mismatch("TypeId(1)", "SubtypeError");
        assert_eq!(typed.user_message(), "Type mismatch");
    }

    #[test]
    fn diagnostic_categories_have_stable_display_labels() {
        assert_eq!(
            DiagnosticCategory::TypeMismatch.to_string(),
            "type-mismatch"
        );
        assert_eq!(
            DiagnosticCategory::UnknownSymbol.to_string(),
            "unknown-symbol"
        );
        assert_eq!(
            DiagnosticCategory::UpstreamCode(1234).to_string(),
            "upstream-code-1234"
        );
    }

    #[test]
    fn unknown_symbol_constructor_populates_typed_payload() {
        let location = DiagnosticLocation::missing();
        let diagnostic = Diagnostic::unknown_symbol("foo", location);
        assert_eq!(
            diagnostic.typed_payload,
            Payload::UnknownSymbol {
                symbol: "foo".to_owned()
            }
        );
        assert_eq!(diagnostic.payload, serde_json::json!({"symbol": "foo"}));
    }

    #[test]
    fn unknown_type_constructor_populates_typed_payload() {
        let location = DiagnosticLocation::missing();
        let diagnostic = Diagnostic::unknown_type("Bar", location);
        assert_eq!(
            diagnostic.typed_payload,
            Payload::UnknownType {
                name: "Bar".to_owned()
            }
        );
        assert_eq!(diagnostic.payload, serde_json::json!({"type": "Bar"}));
    }

    #[test]
    fn diagnostic_payload_accessors_cover_property_arity_operator_cases() {
        let missing = Payload::MissingProperty {
            name: "foo".to_owned(),
            owner: "Table".to_owned(),
            union: None,
            subtype: SubtypeContext::default(),
        };
        assert_eq!(missing.property_name(), Some("foo"));
        assert!(!missing.is_arity_mismatch());
        assert!(!missing.is_operator_mismatch());

        let variance = Payload::PropertyVariance {
            name: Some("bar".to_owned()),
            access_target: None,
            properties_searched: Vec::new(),
            subtype: SubtypeContext::default(),
        };
        assert_eq!(variance.property_name(), Some("bar"));

        let suggestion = Payload::LikeKeySuggestion {
            looked_up: "lenght".to_owned(),
            suggestions: vec!["length".to_owned()],
            subtype: SubtypeContext::default(),
        };
        assert_eq!(suggestion.property_name(), Some("lenght"));

        let arity = Payload::ArityMismatch {
            counts: None,
            subtype: SubtypeContext::default(),
        };
        assert_eq!(arity.property_name(), None);
        assert!(arity.is_arity_mismatch());
        assert!(!arity.is_operator_mismatch());

        let return_arity = Payload::ReturnArityMismatch {
            expected: 2,
            actual: 1,
        };
        assert!(return_arity.is_arity_mismatch());

        let binary = Payload::BinaryOperatorMismatch {
            operator: "+".to_owned(),
            left: "string".to_owned(),
            right: "number".to_owned(),
            overload: "__add".to_owned(),
            metatable_mismatch: false,
        };
        assert!(binary.is_operator_mismatch());
        assert_eq!(binary.property_name(), None);

        assert!(Payload::Empty.property_name().is_none());
    }

    #[test]
    fn binary_operator_error_populates_typed_payload() {
        let diagnostic = Diagnostic::binary_operator_error("+", "string", "number", "__add");
        assert_eq!(
            diagnostic.typed_payload,
            Payload::BinaryOperatorMismatch {
                operator: "+".to_owned(),
                left: "string".to_owned(),
                right: "number".to_owned(),
                overload: "__add".to_owned(),
                metatable_mismatch: false,
            }
        );
        assert_eq!(
            diagnostic.payload,
            serde_json::json!({
                "operator": "+",
                "left": "string",
                "right": "number",
                "overload": "__add",
            })
        );
    }

    #[test]
    fn unary_operator_error_populates_typed_payload() {
        let diagnostic = Diagnostic::unary_operator_error("-", "string", "__unm");
        assert_eq!(
            diagnostic.typed_payload,
            Payload::UnaryOperatorMismatch {
                operator: "-".to_owned(),
                operand: "string".to_owned(),
                overload: "__unm".to_owned(),
            }
        );
        assert_eq!(
            diagnostic.payload,
            serde_json::json!({
                "operator": "-",
                "operand": "string",
                "overload": "__unm",
            })
        );
    }

    #[test]
    fn uninhabited_type_function_error_populates_typed_payload() {
        let location = DiagnosticLocation::missing();
        let diagnostic = Diagnostic::uninhabited_type_function("index<{| |}, T>", location);

        assert_eq!(diagnostic.category, DiagnosticCategory::TypeFunction);
        assert_eq!(diagnostic.primary_location, location);
        assert_eq!(
            diagnostic.typed_payload,
            Payload::UninhabitedTypeFunction {
                instance: "index<{| |}, T>".to_owned()
            }
        );
        assert_eq!(
            diagnostic.payload,
            serde_json::json!({
                "kind": "uninhabited",
                "instance": "index<{| |}, T>",
            })
        );
    }

    #[test]
    fn converts_ast_locations_to_fixture_shape() {
        let location = ruau_syntax::Location::new(
            ruau_syntax::Position::new(3, 4),
            ruau_syntax::Position::new(3, 10),
        );

        assert_eq!(
            DiagnosticLocation::from(location),
            DiagnosticLocation::new(
                DiagnosticPosition::new(3, 4),
                DiagnosticPosition::new(3, 10)
            )
        );
    }

    #[test]
    fn converts_parse_errors_to_type_diagnostics() {
        let error = ruau_syntax::parse::Error {
            kind: ruau_syntax::parse::ErrorKind::ExpectedToken,
            message: "expected identifier".to_owned(),
            location: ruau_syntax::Location::new(
                ruau_syntax::Position::new(0, 1),
                ruau_syntax::Position::new(0, 2),
            ),
        };

        let diagnostic = Diagnostic::from(&error);

        assert_eq!(diagnostic.category, DiagnosticCategory::Parse);
        assert_eq!(
            diagnostic.payload,
            serde_json::json!({
                "kind": "expected-token",
                "message": "expected identifier",
            })
        );
        assert_eq!(
            diagnostic.primary_location,
            DiagnosticLocation::new(DiagnosticPosition::new(0, 1), DiagnosticPosition::new(0, 2))
        );
        // The typed-payload carrier mirrors the JSON shape.
        assert_eq!(
            diagnostic.typed_payload,
            Payload::Error {
                kind: "expected-token".to_owned(),
                message: "expected identifier".to_owned(),
            }
        );
    }

    #[test]
    fn converts_resolver_errors_to_type_diagnostics() {
        let error = crate::graph::resolve::ResolverError::MissingModule {
            module: ruau_source::ModuleName::from("Workspace.Main"),
            searched: Some(std::path::PathBuf::from("Workspace/Main.luau")),
        };

        let diagnostic = Diagnostic::from(&error);

        assert_eq!(diagnostic.category, DiagnosticCategory::Resolver);
        assert_eq!(diagnostic.primary_location, DiagnosticLocation::missing());
        assert_eq!(
            diagnostic.payload,
            serde_json::json!({
                "kind": "missing-module",
                "module": "Workspace.Main",
                "path": "Workspace/Main.luau",
            })
        );
        // The typed-payload carrier mirrors the JSON shape.
        assert_eq!(
            diagnostic.typed_payload,
            Payload::ResolverError {
                kind: "missing-module".to_owned(),
                module: Some("Workspace.Main".to_owned()),
                display_name: None,
                path: Some("Workspace/Main.luau".to_owned()),
                detail: None,
            }
        );
    }

    #[test]
    fn resolver_diagnostics_can_carry_display_names() {
        let error = crate::graph::resolve::ResolverError::MissingModule {
            module: ruau_source::ModuleName::from("Workspace.Main"),
            searched: Some(std::path::PathBuf::from("Workspace/Main.luau")),
        };

        let diagnostic =
            Diagnostic::from_resolver_diagnostic_with_display_name(&error, Some("display/Main"));

        assert_eq!(diagnostic.category, DiagnosticCategory::Resolver);
        assert_eq!(
            diagnostic.payload,
            serde_json::json!({
                "kind": "missing-module",
                "module": "Workspace.Main",
                "displayName": "display/Main",
                "path": "Workspace/Main.luau",
            })
        );
        assert_eq!(
            diagnostic.typed_payload,
            Payload::ResolverError {
                kind: "missing-module".to_owned(),
                module: Some("Workspace.Main".to_owned()),
                display_name: Some("display/Main".to_owned()),
                path: Some("Workspace/Main.luau".to_owned()),
                detail: None,
            }
        );
        assert_eq!(
            diagnostic.context.as_deref(),
            Some(
                "display/Main: module `Workspace.Main` did not resolve (searched Workspace/Main.luau)"
            )
        );
    }

    #[test]
    fn test_snapshot_sorts_and_ignores_context() {
        let left_location =
            DiagnosticLocation::new(DiagnosticPosition::new(0, 1), DiagnosticPosition::new(0, 2));
        let right_location =
            DiagnosticLocation::new(DiagnosticPosition::new(2, 1), DiagnosticPosition::new(2, 2));
        let diagnostics = vec![
            Diagnostic::error(DiagnosticCategory::UnknownSymbol, right_location)
                .with_context("right"),
            Diagnostic::error(DiagnosticCategory::TypeMismatch, left_location).with_context("left"),
        ];

        let snapshot = test_snapshot(&diagnostics);
        let rendered = render_test_snapshot(&diagnostics).expect("snapshot renders");

        assert_eq!(snapshot[0].category, DiagnosticCategory::TypeMismatch);
        assert_eq!(snapshot[0].primary_location, left_location);
        assert!(!rendered.contains("left"));
        assert!(!rendered.contains("right"));
    }

    #[test]
    fn type_error_codes_are_nonzero() {
        ruau_upstream::upstream_case!(
            "Error.test.cpp::ErrorTests::TypeError_code_should_return_nonzero_code"
        );
        let diagnostic = Diagnostic::unknown_symbol("Foo", DiagnosticLocation::missing());

        assert!(diagnostic.code() >= 1000);
        assert_eq!(diagnostic.category.code(), diagnostic.code());
    }

    #[test]
    fn named_type_mismatch_can_use_alias_names_in_context() {
        ruau_upstream::upstream_case!(
            "Error.test.cpp::ErrorTests::metatable_names_show_instead_of_tables"
        );
        let diagnostic = Diagnostic::type_mismatch("Account", "number");

        assert_eq!(diagnostic.category, DiagnosticCategory::TypeMismatch);
        assert_eq!(
            diagnostic.payload,
            serde_json::json!({
                "expected": "Account",
                "actual": "number",
            })
        );
        assert_eq!(
            diagnostic.context.as_deref(),
            Some("Expected this to be 'Account', but got 'number'")
        );
    }

    #[test]
    fn operator_type_function_errors_are_structured() {
        ruau_upstream::upstream_case!("Error.test.cpp::ErrorTests::binary_op_type_function_errors");
        ruau_upstream::upstream_case!("Error.test.cpp::ErrorTests::unary_op_type_function_errors");
        let binary = Diagnostic::binary_operator_error("+", "number", "string", "__add");
        let unary = Diagnostic::unary_operator_error("-", "string", "__unm");

        assert_eq!(binary.category, DiagnosticCategory::Operator);
        assert_eq!(
            binary.payload,
            serde_json::json!({
                "operator": "+",
                "left": "number",
                "right": "string",
                "overload": "__add",
            })
        );
        assert_eq!(
            binary.context.as_deref(),
            Some(
                "Operator '+' could not be applied to operands of types number and string; there is no corresponding overload for __add"
            )
        );
        assert_eq!(
            unary.payload,
            serde_json::json!({
                "operator": "-",
                "operand": "string",
                "overload": "__unm",
            })
        );
        assert_eq!(
            unary.context.as_deref(),
            Some(
                "Operator '-' could not be applied to operand of type string; there is no corresponding overload for __unm"
            )
        );
    }

    fn assert_no_internal_tokens(message: &str) {
        for token in ["TypeId(", "SubtypeError", "ArenaBoundary"] {
            assert!(
                !message.contains(token),
                "message {message:?} leaked {token:?}"
            );
        }
    }
}
