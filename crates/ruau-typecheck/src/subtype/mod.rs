//! Structural subtyping over arena-owned Luau types.

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

use crate::{
    builtins::is_string_library_property,
    member_access,
    normalize::simplify_type,
    type_function::{Reduction, TypeFunctionRuntime, setmetatable_type_function_arguments},
    types::{
        Arena, FlattenedListPack, FunctionType, PackField, PrimitiveType, SingletonType,
        TableIndexer, TableProperty, TableState, TableType, TypeField, TypeId, TypeKind,
        TypePackId, TypePackKind, TypePath, TypePathComponent, compatible_table_state,
        extern_is_subtype, is_top_function_type, negated_disjoint_primitives_cover_unknown,
        same_alias_identity_table_instance, same_named_table_instance,
    },
};

mod generic_instantiation;
mod reasoning;
mod structural_equality;

use generic_instantiation::GenericInstantiationFrame;

/// Target that failed during subtyping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubtypeTarget {
    /// Type node.
    Type(TypeId),
    /// Type-pack node.
    Pack(TypePackId),
}

/// Structured subtyping failure category.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SubtypeErrorKind {
    /// The two type shapes are incompatible.
    Mismatch,
    /// The subtype does not provide a required supertype property.
    MissingProperty,
    /// The subtype does not provide several required supertype properties.
    MissingProperties {
        /// Missing property names.
        names: Vec<String>,
    },
    /// The subtype is missing a property but has similarly named properties.
    LikeKeySuggestion {
        /// Missing property name.
        name: String,
        /// Candidate property names.
        suggestions: Vec<String>,
    },
    /// The subtype contains a property that cannot satisfy writable invariance.
    PropertyVariance,
    /// Type or pack arity differs.
    ArityMismatch,
}

/// Structured subtyping failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubtypeError {
    /// Category.
    pub kind: SubtypeErrorKind,
    /// Path within the compared type.
    pub path: TypePath,
    /// Candidate subtype node.
    pub sub: SubtypeTarget,
    /// Required supertype node.
    pub sup: SubtypeTarget,
}

/// Variance at the point where a subtype relation failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubtypeVariance {
    /// Covariant position.
    Covariant,
    /// Contravariant position.
    Contravariant,
    /// Invariant position.
    Invariant,
}

/// Structured explanation for a failed subtype relation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubtypeReasoning {
    /// Path followed from the candidate subtype.
    pub sub_path: TypePath,
    /// Path followed from the required supertype.
    pub sup_path: TypePath,
    /// Variance at the failed path.
    pub variance: SubtypeVariance,
}

/// Error-suppression summary for a failed subtype relation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SubtypeSuppression {
    /// True when every retained reason path is error-suppressing.
    pub fully_suppressing: bool,
    /// Reason paths whose leaves contain an error-suppressing type.
    pub suppressing_reasonings: Vec<SubtypeReasoning>,
}

impl SubtypeError {
    fn type_error(kind: SubtypeErrorKind, path: TypePath, sub: TypeId, sup: TypeId) -> Self {
        Self {
            kind,
            path,
            sub: SubtypeTarget::Type(sub),
            sup: SubtypeTarget::Type(sup),
        }
    }

    fn pack_error(
        kind: SubtypeErrorKind,
        path: TypePath,
        sub: TypePackId,
        sup: TypePackId,
    ) -> Self {
        Self {
            kind,
            path,
            sub: SubtypeTarget::Pack(sub),
            sup: SubtypeTarget::Pack(sup),
        }
    }

    /// Converts this first-failure error into the single structured reasoning
    /// item it represents.
    #[must_use]
    pub fn reasoning(&self) -> SubtypeReasoning {
        let path = normalize_reason_path(&self.path);
        let (sub_path, sup_path) =
            if path.components() == [TypePathComponent::TypeField(TypeField::Negated)].as_slice() {
                (TypePath::new(), path)
            } else {
                (path.clone(), path)
            };

        SubtypeReasoning {
            sub_path,
            sup_path,
            variance: path_variance(&self.path),
        }
    }
}

fn normalize_reason_path(path: &TypePath) -> TypePath {
    TypePath::from_components(
        path.components()
            .iter()
            .cloned()
            .map(|component| match component {
                TypePathComponent::Property {
                    name,
                    access: crate::types::PropertyAccess::ReadWrite,
                } => TypePathComponent::read_property(name),
                component => component,
            })
            .collect(),
    )
}

fn path_variance(path: &TypePath) -> SubtypeVariance {
    if path.components().iter().any(|component| {
        matches!(
            component,
            TypePathComponent::PackField(PackField::Arguments)
        )
    }) {
        return SubtypeVariance::Contravariant;
    }
    if path.components().iter().any(|component| {
        matches!(
            component,
            TypePathComponent::Property { .. }
                | TypePathComponent::TypeField(TypeField::IndexLookup | TypeField::IndexResult)
        )
    }) {
        return SubtypeVariance::Invariant;
    }
    SubtypeVariance::Covariant
}

/// Shared map of settled subtype proofs (see `Subtyper::settled_subtypes`).
///
/// Each proof is tagged with the depth (`pack_clock` value) of the *deepest*
/// coinductive pack assumption it leaned on, or `None` when it leaned on no pack
/// assumption. A failed alternative that rolls its `seen_packs` back to a floor
/// retracts every assumption opened at or after that floor, so any proof tagged
/// with a depth `>= floor` is evicted (it may have been derived from an
/// assumption that is no longer in force). Proofs tagged `None` (or with a depth
/// below the floor) leaned only on still-valid assumptions and survive.
type SettledSubtypes =
    Rc<RefCell<BTreeMap<(TypeId, TypeId), Vec<(Vec<GenericInstantiationFrame>, Option<usize>)>>>>;

/// Memoized accepting outcomes of the table-intersection arm, keyed by the
/// followed member ids and the followed supertype id.
type TableIntersectionAccepts = Rc<RefCell<BTreeMap<(Vec<TypeId>, TypeId), Option<()>>>>;

/// Arena-borrowing subtype relation.
pub struct Subtyper<'a> {
    arena: &'a Arena,
    type_function_runtime: TypeFunctionRuntime,
    generic_instantiation_frames: Vec<GenericInstantiationFrame>,
    /// Active coinductive `(sub, sup)` assumptions mapped to the depth at which
    /// each was entered (`assumption_clock` value at insertion). Acts as both
    /// the cycle-guard membership set and the per-pair depth used by taint
    /// tracking: a short-circuit on an entry records that depth so a cache owner
    /// can tell whether it leaned only on cycles opened within its own subtree.
    seen_types: BTreeMap<(TypeId, TypeId), usize>,
    /// Active coinductive pack assumptions mapped to the depth (`pack_clock`
    /// value) at which each was opened. Parallels `seen_types`: it is both the
    /// pack cycle-guard and the per-pair depth used by pack taint tracking. A
    /// short-circuit on a pair records its depth so a cache owner learns which
    /// pack assumptions its subtree leaned on.
    seen_packs: BTreeMap<(TypePackId, TypePackId), usize>,
    /// Monotonically increasing entry counter assigned to each `(sub, sup)` pair
    /// as it is pushed onto the `seen_types` assumption stack. The value never
    /// decreases (it is *not* rolled back), so a pair already present when a
    /// cache owner is entered always has a strictly smaller depth than anything
    /// the owner's subtree later opens.
    assumption_clock: usize,
    /// The pack-pair analogue of `assumption_clock`: a monotone counter assigned
    /// to each `(sub, sup)` pack pair as it is opened into `seen_packs`. Never
    /// rolled back, so a pack assumption's depth uniquely (and stably) names the
    /// point in the recursion at which it was opened, which is what lets a failed
    /// alternative express "retract everything opened at or after this floor".
    pack_clock: usize,
    /// Shallowest assumption depth that any coinductive short-circuit relied on
    /// while proving the subtree currently in flight (`usize::MAX` when nothing
    /// was assumed). Reset on entry to each cache owner and folded back into the
    /// parent so an owner can compare it against its own entry depth: a value
    /// below the entry depth means the proof leaned on a strict-ancestor
    /// assumption that may later be rolled back, so the proof is not cacheable.
    ///
    /// Pack-level coinductive cycles (recursive function argument/return packs,
    /// as in a rowset's generic methods) routinely close on a strict-ancestor
    /// pack before any type pair repeats, so *gating* the cache on them would
    /// refuse to cache the very recursive generic proofs this cache exists to
    /// memoize and re-introduce the exponential blow-up. Pack reliance is
    /// therefore not gated; it is tracked separately by
    /// `max_pack_dependency_depth` and handled by rollback-time eviction.
    min_assumption_depth: usize,
    /// Depth (`pack_clock` value) of the *deepest* coinductive pack assumption
    /// the subtree currently in flight leaned on, or `None` when it leaned on
    /// none. Reset on entry to each cache owner and folded back into the parent
    /// with `max`, so an owner learns the deepest pack assumption anywhere in its
    /// subtree. The resulting value tags the owner's settled-cache entry: unlike
    /// type-pair reliance (which is gated out of the cache), pack reliance is
    /// kept in the cache and instead retracted lazily — a failed alternative that
    /// rolls `seen_packs` back to a floor evicts every settled entry whose tag is
    /// `>= floor`, because those proofs may have leaned on an assumption the
    /// branch just removed. The deepest (not shallowest) assumption is the right
    /// tag: an entry must be evicted if *any* assumption it used is retracted, so
    /// the tag has to cross the floor whenever any of its dependencies does.
    max_pack_dependency_depth: Option<usize>,
    /// Active `(sub, sup)` pairs in the diagnostic reasoning recursion. The
    /// `collect_*_reasonings` walk runs on `&self` (it cannot borrow
    /// `seen_types` mutably like `subtype_type` does), so a self-referential
    /// table/metatable (e.g. `T.__index = T`) would recurse forever. This
    /// stack-scoped set, managed by `ReasoningGuard`, skips a pair already on
    /// the active reasoning stack.
    reasoning_seen: RefCell<BTreeSet<(TypeId, TypeId)>>,
    /// Active pack pairs in the diagnostic reasoning recursion. Recursive
    /// function properties can revisit the same argument or return pack pair
    /// before a type pair repeats.
    reasoning_seen_packs: RefCell<BTreeSet<(TypePackId, TypePackId)>>,
    structurally_equal_types: BTreeSet<(TypeId, TypeId)>,
    structurally_equal_packs: BTreeSet<(TypePackId, TypePackId)>,
    structural_equality_types_in_progress: BTreeMap<(TypeId, TypeId), usize>,
    structural_equality_packs_in_progress: BTreeMap<(TypePackId, TypePackId), usize>,
    structural_equality_clock: usize,
    structural_equality_min_dependency: usize,
    /// `(sub, sup)` pairs already proven to subtype under a snapshot of the
    /// active generic instantiation frames. Given the immutable arena and that
    /// exact frame state the relation is deterministic, so a recorded proof
    /// stays valid even after `subtype_type_alternative` rolls back the
    /// in-progress coinductive `seen_types` assumptions for a failed branch —
    /// which is what otherwise re-proves recursive types (including the generic
    /// methods of a rowset `TypedRowset<T>`) exponentially across sibling
    /// retries. Including the frame snapshot keeps the key sound: a generic
    /// instantiation binds the surrounding call, so two contexts share a cached
    /// proof only when their bindings are identical. Only the call that owns the
    /// cycle entry records a proof (a coinductive re-entry returns `Ok` from an
    /// in-flight assumption without completing a real one), and only successes
    /// are cached so a failure always re-derives its precise diagnostic.
    ///
    /// Crucially a proof's reliance on coinductive assumptions is handled in two
    /// ways depending on the assumption kind:
    ///
    /// * *Type-pair* reliance is gated out. A proof is cached only when every
    ///   type-pair short-circuit it used was on a pair entered at or below this
    ///   owner's own entry depth (its own pair plus nested cycles it opened),
    ///   never on a strict-ancestor assumption. A proof that leans on an ancestor
    ///   type assumption (e.g. proving `A <: B` only because the in-flight
    ///   `F <: G` is assumed) is unsound to reuse and is refused, detected by
    ///   `assumption_clock`/`min_assumption_depth`.
    /// * *Pack* reliance is kept but tracked. Pack cycles routinely close on a
    ///   strict-ancestor pack, so gating on them would refuse the recursive
    ///   generic proofs this cache exists to memoize. Instead each cached proof
    ///   is tagged with the deepest pack assumption it leaned on
    ///   (`max_pack_dependency_depth`), and a failed alternative that rolls
    ///   `seen_packs` back to a floor evicts every entry whose tag is `>= floor`.
    ///   This retracts exactly the proofs that leaned on a now-removed pack
    ///   assumption, so no surviving entry depends on a retracted assumption,
    ///   while proofs that closed on still-valid (lower-depth) pack cycles —
    ///   which is what the rowset memoization needs — are kept and reused.
    ///
    /// The cache is shared (via `spawn_same_arena`) with every helper `Subtyper`
    /// the relation creates over the same immutable arena, so the diagnostic
    /// reasoning walk and the many `is_subtype` probes it spawns reuse one
    /// settled map instead of each re-proving the same recursive subtrees. It is
    /// dropped when the root relation completes (before the arena can change).
    settled_subtypes: SettledSubtypes,
    /// Lazily-cloned scratch arena shared by the table-intersection
    /// combination arm for this relation's lifetime. Cloning the arena per
    /// comparison is O(arena size) and the rich declaration graphs hit the
    /// arm thousands of times per constraint; the arena is immutable while a
    /// relation is alive, so one snapshot stays valid and only accumulates
    /// the arm's combined-table allocations.
    table_intersection_scratch: Rc<RefCell<Option<Arena>>>,
    /// Memoized *accepting* outcomes of the table-intersection arm keyed by
    /// the followed member ids and the followed supertype id. The arm runs a
    /// fresh relation with no inherited assumptions, so an accept is
    /// deterministic for the key; failures are recomputed so their diagnostic
    /// paths stay exact, and inapplicable keys are remembered as `None`.
    table_intersection_accepts: TableIntersectionAccepts,
    /// The `(enclosing_sub_table, enclosing_sup_table)` pair currently being
    /// compared property-by-property in `subtype_table`. Threaded down to
    /// `subtype_function`/`subtype_function_arguments` so a method receiver
    /// (`self`) earns the covariant fallback *only* when its declared type is the
    /// recursive enclosing table (the self/this pattern), not when `self` is an
    /// ordinary standalone-function parameter or a method parameter typed to some
    /// unrelated table. `subtype_function` clears it across the nested
    /// argument/return descent so only the directly-compared method functions see
    /// it, and restores it so the reverse-direction property check sees it too.
    method_receiver_context: Option<(TypeId, TypeId)>,
}

/// RAII cycle guard for the diagnostic reasoning recursion: records a
/// `(sub, sup)` pair on entry to a `collect_*_reasonings` frame and removes it
/// on drop, so sibling occurrences (a shared, acyclic subtree) still expand
/// while genuine cycles terminate.
struct ReasoningGuard<'g> {
    seen: &'g RefCell<BTreeSet<(TypeId, TypeId)>>,
    key: (TypeId, TypeId),
}

struct PackReasoningGuard<'g> {
    seen: &'g RefCell<BTreeSet<(TypePackId, TypePackId)>>,
    key: (TypePackId, TypePackId),
}

impl Drop for ReasoningGuard<'_> {
    fn drop(&mut self) {
        self.seen.borrow_mut().remove(&self.key);
    }
}

impl Drop for PackReasoningGuard<'_> {
    fn drop(&mut self) {
        self.seen.borrow_mut().remove(&self.key);
    }
}

#[allow(clippy::multiple_inherent_impl)]
impl<'a> Subtyper<'a> {
    /// Creates a subtype relation over an immutable type arena.
    pub fn new(arena: &'a Arena) -> Self {
        Self {
            arena,
            type_function_runtime: TypeFunctionRuntime::new(),
            generic_instantiation_frames: Vec::new(),
            seen_types: BTreeMap::new(),
            seen_packs: BTreeMap::new(),
            assumption_clock: 0,
            pack_clock: 0,
            min_assumption_depth: usize::MAX,
            max_pack_dependency_depth: None,
            reasoning_seen: RefCell::new(BTreeSet::new()),
            reasoning_seen_packs: RefCell::new(BTreeSet::new()),
            structurally_equal_types: BTreeSet::new(),
            structurally_equal_packs: BTreeSet::new(),
            structural_equality_types_in_progress: BTreeMap::new(),
            structural_equality_packs_in_progress: BTreeMap::new(),
            structural_equality_clock: 0,
            structural_equality_min_dependency: usize::MAX,
            settled_subtypes: Rc::new(RefCell::new(BTreeMap::new())),
            table_intersection_scratch: Rc::new(RefCell::new(None)),
            table_intersection_accepts: Rc::new(RefCell::new(BTreeMap::new())),
            method_receiver_context: None,
        }
    }

    /// Creates a helper relation over the same immutable arena that shares this
    /// relation's settled-outcome cache. Used for the inner `is_subtype` probes
    /// the diagnostic reasoning walk and a few structural checks spawn, so they
    /// do not each re-prove the same recursive subtrees from an empty cache.
    /// Every other relation state (cycle stacks, instantiation frames) starts
    /// empty, matching `new`.
    fn spawn_same_arena(&self) -> Self {
        Self {
            arena: self.arena,
            type_function_runtime: TypeFunctionRuntime::new(),
            generic_instantiation_frames: Vec::new(),
            seen_types: BTreeMap::new(),
            seen_packs: BTreeMap::new(),
            assumption_clock: 0,
            pack_clock: 0,
            min_assumption_depth: usize::MAX,
            max_pack_dependency_depth: None,
            reasoning_seen: RefCell::new(BTreeSet::new()),
            reasoning_seen_packs: RefCell::new(BTreeSet::new()),
            structurally_equal_types: BTreeSet::new(),
            structurally_equal_packs: BTreeSet::new(),
            structural_equality_types_in_progress: BTreeMap::new(),
            structural_equality_packs_in_progress: BTreeMap::new(),
            structural_equality_clock: 0,
            structural_equality_min_dependency: usize::MAX,
            settled_subtypes: Rc::clone(&self.settled_subtypes),
            table_intersection_scratch: Rc::clone(&self.table_intersection_scratch),
            table_intersection_accepts: Rc::clone(&self.table_intersection_accepts),
            method_receiver_context: None,
        }
    }

    /// Enters a diagnostic reasoning frame for `(sub, sup)`. Returns `None`
    /// when the pair is already on the active reasoning stack (a cycle), so the
    /// caller bails with no further reason paths; otherwise returns a guard that
    /// clears the pair when the frame unwinds.
    fn enter_reasoning(&self, sub: TypeId, sup: TypeId) -> Option<ReasoningGuard<'_>> {
        if self.reasoning_seen.borrow_mut().insert((sub, sup)) {
            Some(ReasoningGuard {
                seen: &self.reasoning_seen,
                key: (sub, sup),
            })
        } else {
            None
        }
    }

    fn enter_pack_reasoning(
        &self,
        sub: TypePackId,
        sup: TypePackId,
    ) -> Option<PackReasoningGuard<'_>> {
        if self.reasoning_seen_packs.borrow_mut().insert((sub, sup)) {
            Some(PackReasoningGuard {
                seen: &self.reasoning_seen_packs,
                key: (sub, sup),
            })
        } else {
            None
        }
    }

    /// Returns `Ok(())` when `sub` is a subtype of `sup`.
    pub fn is_subtype(&mut self, sub: TypeId, sup: TypeId) -> Result<(), SubtypeError> {
        self.subtype_type(sub, sup, TypePath::new())
    }

    /// Returns `Ok(())` when `sub` is a subtype of `sup` as a type pack.
    pub fn is_subtype_pack(
        &mut self,
        sub: TypePackId,
        sup: TypePackId,
    ) -> Result<(), SubtypeError> {
        self.subtype_pack(sub, sup, TypePath::new())
    }

    pub(crate) fn is_subtype_return_pack(
        &mut self,
        sub: TypePackId,
        sup: TypePackId,
    ) -> Result<(), SubtypeError> {
        self.subtype_pack(
            sub,
            sup,
            TypePath::new().push(TypePathComponent::PackField(PackField::Returns)),
        )
    }

    pub(crate) fn is_subtype_pack_instantiating_function(
        &mut self,
        sub: TypePackId,
        sup: TypePackId,
        function: &FunctionType,
    ) -> Result<(), SubtypeError> {
        let instantiation_frame = GenericInstantiationFrame::for_function(self.arena, function);
        let pushed_frame = !instantiation_frame.is_empty();
        if pushed_frame {
            self.generic_instantiation_frames.push(instantiation_frame);
        }
        let result = self.subtype_pack(
            sub,
            sup,
            TypePath::new().push(TypePathComponent::PackField(PackField::Arguments)),
        );
        if pushed_frame {
            self.generic_instantiation_frames.pop();
        }
        result
    }

    fn subtype_type(
        &mut self,
        sub: TypeId,
        sup: TypeId,
        path: TypePath,
    ) -> Result<(), SubtypeError> {
        let sub = self.arena.follow(sub);
        let sup = self.arena.follow(sup);
        if let (TypeKind::Table(sub_table), TypeKind::Table(sup_table)) =
            (self.arena.get(sub), self.arena.get(sup))
            && same_named_table_instance(self.arena, sub_table, sup_table)
        {
            return Ok(());
        }
        if self.structurally_equal_type(sub, sup) {
            return Ok(());
        }
        // Reuse a completed outcome for `(sub, sup)` under the current frame
        // snapshot. Given the immutable arena and that exact frame state the
        // relation is deterministic, so the outcome stays valid even after
        // `subtype_type_alternative` rolls back the in-progress `seen_types`
        // assumptions for a failed branch — which is what otherwise re-proves
        // recursive types exponentially across sibling retries. A cache hit
        // carries no *type-pair* dependency (the entry is gated to be type-pair
        // assumption-independent), so it leaves `min_assumption_depth` untouched;
        // but it may have leaned on coinductive *pack* assumptions, which are not
        // gated, so its pack-dependency tag must be folded in to keep this owner
        // evicted in lockstep should a failed branch later retract one of them.
        let pair = (sub, sup);
        let cached = self
            .settled_subtypes
            .borrow()
            .get(&pair)
            .and_then(|proofs| {
                proofs
                    .iter()
                    .find(|(frames, _)| frames == &self.generic_instantiation_frames)
                    .map(|(_, dependency)| *dependency)
            });
        if let Some(pack_dependency) = cached {
            self.max_pack_dependency_depth = self.max_pack_dependency_depth.max(pack_dependency);
            return Ok(());
        }
        // Only the call that owns the cycle entry records an outcome: a re-entry
        // that bottoms out on the coinductive assumption (already on the
        // `seen_types` stack) returns `Ok` without completing a real proof, so
        // recording it would be unsound. Only successful relations are cached:
        // a failing pair re-derives its diagnostic each time, so the precise
        // mismatch reasons reported to callers are never replaced by a cached
        // placeholder.
        let owns_cycle_entry = !self.seen_types.contains_key(&(sub, sup));
        // Taint tracking. `entry_depth` is the assumption depth this owner is
        // entered at: every pair/pack already on the assumption stacks has a
        // smaller depth (a strict ancestor), while the owner's own pair and any
        // cycle it opens are inserted at `entry_depth` or deeper. We reset
        // `min_assumption_depth` so it accumulates only the shallowest
        // assumption this subtree leans on, then fold that back into the parent.
        let entry_depth = self.assumption_clock;
        let frame_depth = self.generic_instantiation_frames.len();
        let saved_min_assumption_depth = self.min_assumption_depth;
        let saved_max_pack_dependency_depth = self.max_pack_dependency_depth;
        self.min_assumption_depth = usize::MAX;
        self.max_pack_dependency_depth = None;
        let result = self.subtype_type_uncached(sub, sup, path);
        let subtree_min_assumption_depth = self.min_assumption_depth;
        let subtree_max_pack_dependency_depth = self.max_pack_dependency_depth;
        self.min_assumption_depth = saved_min_assumption_depth.min(subtree_min_assumption_depth);
        self.max_pack_dependency_depth =
            saved_max_pack_dependency_depth.max(subtree_max_pack_dependency_depth);
        // Cache only a type-pair-assumption-independent success: every type-pair
        // short-circuit the subtree used was on a pair entered at `entry_depth`
        // or deeper, i.e. inside this owner's own subtree (its pair plus nested
        // cycles). A proof that leaned on a shallower (strict-ancestor) type
        // assumption may be undone when that ancestor's branch is rolled back, so
        // it must not be cached. Pack reliance is *not* gated here; instead the
        // entry is tagged with the deepest pack assumption its subtree leaned on
        // so a failed alternative can evict it if that assumption is retracted.
        if result.is_ok() && owns_cycle_entry && subtree_min_assumption_depth >= entry_depth {
            debug_assert_eq!(self.generic_instantiation_frames.len(), frame_depth);
            self.settled_subtypes
                .borrow_mut()
                .entry(pair)
                .or_default()
                .push((
                    self.generic_instantiation_frames.clone(),
                    subtree_max_pack_dependency_depth,
                ));
        }
        result
    }

    fn subtype_type_uncached(
        &mut self,
        sub: TypeId,
        sup: TypeId,
        path: TypePath,
    ) -> Result<(), SubtypeError> {
        let sub = self.arena.follow(sub);
        let sup = self.arena.follow(sup);
        if sub == sup {
            return Ok(());
        }

        let sub_kind = self.arena.get(sub).clone();
        let sup_kind = self.arena.get(sup).clone();
        let frame_sensitive = !self.generic_instantiation_frames.is_empty()
            && (matches!(sub_kind, TypeKind::TypeFunctionInstance { .. })
                || matches!(sup_kind, TypeKind::TypeFunctionInstance { .. }));
        if !frame_sensitive {
            if let Some(&assumption_depth) = self.seen_types.get(&(sub, sup)) {
                // Coinductive short-circuit: this pair is already assumed higher
                // in the recursion. Record the depth it was entered at so the
                // owning cache candidate learns the shallowest assumption its
                // subtree leaned on.
                self.min_assumption_depth = self.min_assumption_depth.min(assumption_depth);
                return Ok(());
            }
            let assumption_depth = self.assumption_clock;
            self.assumption_clock += 1;
            self.seen_types.insert((sub, sup), assumption_depth);
        }

        if definitely_uninhabited_type(self.arena, sub) {
            return Ok(());
        }
        if let TypeKind::TypeFunctionInstance { name, arguments } = &sub_kind
            && let Some(reduced) = self.reduce_type_function(name, arguments)
        {
            return self.subtype_type(reduced, sup, path);
        }
        if let TypeKind::TypeFunctionInstance { name, arguments } = &sup_kind
            && let Some(reduced) = self.reduce_type_function(name, arguments)
        {
            return self.subtype_type(sub, reduced, path);
        }
        if matches!(sub_kind, TypeKind::Generic(_))
            && let Some(frame_index) = self.instantiable_type_frame_index(sub)
        {
            return self.subtype_instantiable_sub_type(frame_index, sub, sup, path);
        }
        if matches!(sup_kind, TypeKind::Generic(_))
            && let Some(frame_index) = self.instantiable_type_frame_index(sup)
        {
            return self.subtype_instantiable_sup_type(frame_index, sub, sup, path);
        }
        if matches!(sup_kind, TypeKind::Generic(_))
            && self
                .generic_instantiation_frames
                .iter()
                .any(|frame| frame.is_rigid_type(sup))
        {
            return Err(SubtypeError::type_error(
                SubtypeErrorKind::Mismatch,
                path,
                sub,
                sup,
            ));
        }
        if let TypeKind::TypeFunctionInstance { name, arguments } = &sub_kind
            && let Some(result) =
                self.subtype_keyof_instance_to_type(sub, name, arguments, sup, path.clone())
        {
            return result;
        }
        if let TypeKind::TypeFunctionInstance { name, arguments } = &sup_kind
            && let Some(result) =
                self.subtype_type_to_keyof_instance(sub, sup, name, arguments, path.clone())
        {
            return result;
        }

        if let TypeKind::Negation(outer) = &sub_kind
            && let TypeKind::Negation(inner) = self.arena.get(*outer)
        {
            return self.subtype_type(*inner, sup, path);
        }
        if let TypeKind::Negation(outer) = &sup_kind
            && let TypeKind::Negation(inner) = self.arena.get(*outer)
        {
            return self.subtype_type(sub, *inner, path);
        }

        match (sub_kind, sup_kind) {
            (TypeKind::Never, _) => Ok(()),
            (TypeKind::Blocked(_), _) | (_, TypeKind::Blocked(_)) => Ok(()),
            (TypeKind::Error, TypeKind::Unknown) => Err(SubtypeError::type_error(
                SubtypeErrorKind::Mismatch,
                path,
                sub,
                sup,
            )),
            (_, TypeKind::Any | TypeKind::Unknown) => Ok(()),
            (TypeKind::Error, _) => Ok(()),
            (TypeKind::Free(_), _) | (_, TypeKind::Free(_)) => Ok(()),
            (TypeKind::Generic(_), _) | (_, TypeKind::Generic(_)) => Ok(()),
            (TypeKind::Any, TypeKind::Union(options))
                if options
                    .iter()
                    .any(|option| matches!(self.arena.get(*option), TypeKind::Unknown)) =>
            {
                Ok(())
            }
            (TypeKind::Any, _) | (_, TypeKind::Error) => Err(SubtypeError::type_error(
                SubtypeErrorKind::Mismatch,
                path,
                sub,
                sup,
            )),
            (TypeKind::Primitive(left), TypeKind::Primitive(right)) if left == right => Ok(()),
            (TypeKind::Singleton(left), TypeKind::Singleton(right)) if left == right => Ok(()),
            (TypeKind::Singleton(singleton), TypeKind::Primitive(primitive))
                if singleton.primitive() == primitive =>
            {
                Ok(())
            }
            (
                TypeKind::Primitive(crate::types::PrimitiveType::String)
                | TypeKind::Singleton(crate::types::SingletonType::String(_)),
                TypeKind::Table(sup_table),
            ) => self.subtype_string_primitive_table(sub, sup, sup_table, path),
            (
                TypeKind::Extern {
                    name: left,
                    parents: left_parents,
                    ..
                },
                TypeKind::Extern { name: right, .. },
            ) if extern_is_subtype(&left, &left_parents, &right) => Ok(()),
            (
                TypeKind::Extern {
                    properties: sub_properties,
                    indexer: sub_indexer,
                    ..
                },
                TypeKind::Table(sup_table),
            ) => self.subtype_extern_table(sub, sup, &sub_properties, sub_indexer, sup_table, path),
            (TypeKind::Union(options), _) => self.subtype_union_to_type(options, sup, &path),
            (TypeKind::Unknown, TypeKind::Union(options))
                if negated_disjoint_primitives_cover_unknown(self.arena, &options) =>
            {
                Ok(())
            }
            (
                TypeKind::Primitive(crate::types::PrimitiveType::Boolean),
                TypeKind::Union(options),
            ) if self.union_covers_boolean_singletons(&options) => Ok(()),
            (_, TypeKind::Union(options)) => self.subtype_type_to_union(sub, options, sup, path),
            (TypeKind::Intersection(sub_options), TypeKind::Intersection(sup_options)) => {
                self.subtype_intersection_options(sub, &sub_options, sup, &sup_options, &path)
            }
            (TypeKind::Intersection(options), _) => {
                self.subtype_intersection_to_type(sub, options, sup, path)
            }
            (_, TypeKind::Intersection(options)) => {
                for (index, option) in options.into_iter().enumerate() {
                    self.subtype_type(sub, option, path.push(TypePathComponent::Index { index }))?;
                }
                Ok(())
            }
            (TypeKind::Negation(sub_negated), TypeKind::Negation(sup_negated)) => self
                .subtype_type(
                    sup_negated,
                    sub_negated,
                    path.push(TypePathComponent::TypeField(TypeField::Negated)),
                ),
            (_, TypeKind::Negation(_)) => Err(SubtypeError::type_error(
                SubtypeErrorKind::Mismatch,
                path.push(TypePathComponent::TypeField(TypeField::Negated)),
                sub,
                sup,
            )),
            (TypeKind::Function(sub_function), TypeKind::Function(sup_function)) => {
                self.subtype_function(&sub_function, &sup_function, path)
            }
            (TypeKind::Table(sub_table), TypeKind::Table(sup_table)) => {
                self.subtype_table(sub, sup, sub_table, sup_table, path, &BTreeSet::new())
            }
            (
                TypeKind::Metatable {
                    table: sub_table,
                    metatable: sub_metatable,
                    ..
                },
                TypeKind::Table(sup_table),
            ) => {
                self.subtype_metatable_to_table(sub, sup, sub_table, sub_metatable, sup_table, path)
            }
            (
                TypeKind::Metatable {
                    table: sub_table,
                    metatable: sub_metatable,
                    name: _,
                },
                TypeKind::Metatable {
                    table: sup_table,
                    metatable: sup_metatable,
                    name: _,
                },
            ) => self.subtype_metatable_parts(
                (sub_table, sub_metatable),
                (sup_table, sup_metatable),
                &path,
            ),
            (
                TypeKind::Metatable {
                    table: sub_table,
                    metatable: sub_metatable,
                    name: _,
                },
                TypeKind::TypeFunctionInstance {
                    name: sup_name,
                    arguments: sup_arguments,
                },
            ) if let Some((sup_table, sup_metatable)) =
                setmetatable_type_function_arguments(&sup_name, &sup_arguments) =>
            {
                self.subtype_metatable_parts(
                    (sub_table, sub_metatable),
                    (sup_table, sup_metatable),
                    &path,
                )
            }
            (
                TypeKind::TypeFunctionInstance {
                    name: sub_name,
                    arguments: sub_arguments,
                },
                TypeKind::Metatable {
                    table: sup_table,
                    metatable: sup_metatable,
                    name: _,
                },
            ) if let Some((sub_table, sub_metatable)) =
                setmetatable_type_function_arguments(&sub_name, &sub_arguments) =>
            {
                self.subtype_metatable_parts(
                    (sub_table, sub_metatable),
                    (sup_table, sup_metatable),
                    &path,
                )
            }
            (
                TypeKind::TypeFunctionInstance {
                    name: sub_name,
                    arguments: sub_arguments,
                },
                TypeKind::TypeFunctionInstance {
                    name: sup_name,
                    arguments: sup_arguments,
                },
            ) if sub_name == sup_name => {
                self.subtype_type_list(sub_arguments, sup_arguments, path, sub, sup)
            }
            _ => Err(SubtypeError::type_error(
                SubtypeErrorKind::Mismatch,
                path,
                sub,
                sup,
            )),
        }
    }

    /// Every option of a `Union` subtype must be a subtype of `sup` (ignoring
    /// `any` options, which the caller permits unconditionally).
    fn subtype_union_to_type(
        &mut self,
        options: Vec<TypeId>,
        sup: TypeId,
        path: &TypePath,
    ) -> Result<(), SubtypeError> {
        for (index, option) in options.into_iter().enumerate() {
            if matches!(self.arena.get(option), TypeKind::Any) {
                continue;
            }
            self.subtype_type(option, sup, path.push(TypePathComponent::Index { index }))?;
        }
        Ok(())
    }

    /// `sub` must be a subtype of at least one option of a `Union` supertype.
    /// On total failure, prefer the error from the best tag-matched option so
    /// the reported mismatch is the most relevant one.
    fn subtype_type_to_union(
        &mut self,
        sub: TypeId,
        options: Vec<TypeId>,
        sup: TypeId,
        path: TypePath,
    ) -> Result<(), SubtypeError> {
        let mut last_error = None;
        let mut tagged_error = None;
        let mut seen_options = BTreeSet::new();
        for (index, option) in options.into_iter().enumerate() {
            if !seen_options.insert(option) {
                continue;
            }
            match self.subtype_type_alternative(
                sub,
                option,
                path.push(TypePathComponent::Index { index }),
            ) {
                Ok(()) => return Ok(()),
                Err(error) => {
                    let tag_score = self.tagged_table_option_match_score(sub, option);
                    if tag_score > 0
                        && tagged_error
                            .as_ref()
                            .is_none_or(|(best_score, _)| tag_score > *best_score)
                    {
                        tagged_error = Some((tag_score, error.clone()));
                    }
                    last_error = Some(error);
                }
            }
        }
        Err(tagged_error.map_or_else(
            || {
                last_error.unwrap_or_else(|| {
                    SubtypeError::type_error(SubtypeErrorKind::Mismatch, path, sub, sup)
                })
            },
            |(_, error)| error,
        ))
    }

    /// An `Intersection` subtype: simplify a non-callable intersection in a
    /// scratch arena first, then require at least one option to be a subtype of
    /// `sup` (with a table-targeted fast path).
    fn subtype_intersection_to_type(
        &mut self,
        sub: TypeId,
        options: Vec<TypeId>,
        sup: TypeId,
        path: TypePath,
    ) -> Result<(), SubtypeError> {
        // Keep callable-table intersections structural here: the
        // normalizer can collapse a function/table intersection to
        // `never`, but subtype obligations must still inspect the table
        // and function arms instead of accepting every target.
        if !intersection_contains_function_and_table_like(self, &options) {
            let mut scratch_slot = self.table_intersection_scratch.borrow_mut();
            let scratch = scratch_slot.get_or_insert_with(|| self.arena.clone());
            let simplified = simplify_type(scratch, sub);
            if !matches!(scratch.get(simplified), TypeKind::Intersection(_)) {
                // The scratch arena clone preserves every existing id,
                // so the in-flight coinductive assumptions remain
                // meaningful there and the scratch subtyper must
                // inherit them. Without this, a recursive alias whose
                // two spellings flip between intersection and table
                // form (a declared `Lease = Message & {...self
                // methods}` against a separately-lowered table-shaped
                // copy) re-enters this arm through a fresh subtyper on
                // every round — each with an empty cycle guard and
                // freshly simplified ids — and recurses to a
                // process-aborting stack overflow. Inherited pairs are
                // seeded at depth 0 with the clocks started at 1 so a
                // failed-arm rollback inside the scratch run never
                // retracts an ancestor assumption it does not own.
                let mut scratch_subtyper = Subtyper::new(scratch);
                scratch_subtyper.seen_types =
                    self.seen_types.keys().map(|&pair| (pair, 0)).collect();
                scratch_subtyper.seen_packs =
                    self.seen_packs.keys().map(|&pair| (pair, 0)).collect();
                scratch_subtyper.assumption_clock = 1;
                scratch_subtyper.pack_clock = 1;
                return scratch_subtyper
                    .subtype_type(simplified, sup, path)
                    .map_err(|error| SubtypeError {
                        kind: error.kind,
                        path: error.path,
                        sub: SubtypeTarget::Type(sub),
                        sup: SubtypeTarget::Type(sup),
                    });
            }
        }
        if matches!(self.arena.get(sup), TypeKind::Table(_))
            && let Some(result) =
                self.subtype_table_intersection(sub, options.clone(), sup, path.clone())
        {
            return result;
        }
        let mut last_error = None;
        for (index, option) in options.into_iter().enumerate() {
            match self.subtype_type_alternative(
                option,
                sup,
                path.push(TypePathComponent::Index { index }),
            ) {
                Ok(()) => return Ok(()),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            SubtypeError::type_error(SubtypeErrorKind::Mismatch, path, sub, sup)
        }))
    }

    /// A `Metatable` subtype against a plain `Table` supertype: relate the
    /// base table, folding in an `__index`-derived indexer and the read-only
    /// properties reachable through a table-valued `__index`.
    fn subtype_metatable_to_table(
        &mut self,
        sub: TypeId,
        sup: TypeId,
        sub_table: TypeId,
        sub_metatable: TypeId,
        sup_table: TableType,
        path: TypePath,
    ) -> Result<(), SubtypeError> {
        let sub_table = self.arena.follow(sub_table);
        let mut sub_table_type = match self.arena.get(sub_table).clone() {
            TypeKind::Table(table) => table,
            _ => {
                return Err(SubtypeError::type_error(
                    SubtypeErrorKind::Mismatch,
                    path,
                    sub,
                    sup,
                ));
            }
        };
        if sub_table_type.indexer.is_none()
            && let Some(indexer) =
                member_access::function_indexer_metatable(self.arena, sub_metatable)
        {
            sub_table_type.indexer = Some(indexer);
        }
        // Surface read-only properties reachable through a table-valued
        // `__index` so the receiver satisfies a target that reads them
        // (`metatable_field_allows_upcast`). The base table's own
        // properties take precedence, and the inherited names stay exempt
        // from read-only relaxation so they cannot satisfy a read-write
        // target (`metatable_field_disallows_invalid_upcast`).
        let mut inherited_exempt = BTreeSet::new();
        if let Some(inherited) =
            member_access::metatable_index_table_properties(self.arena, sub_metatable)
        {
            for (name, property) in inherited {
                if let std::collections::btree_map::Entry::Vacant(entry) =
                    sub_table_type.properties.entry(name)
                {
                    inherited_exempt.insert(entry.key().clone());
                    entry.insert(property);
                }
            }
        }
        self.subtype_table(
            sub_table,
            sup,
            sub_table_type,
            sup_table,
            path.push(TypePathComponent::TypeField(TypeField::Table)),
            &inherited_exempt,
        )
    }

    /// Relates a metatable-shaped pair part-wise: table against table under
    /// the `Table` path component, then metatable against metatable under
    /// `Metatable`. Shared by the Metatable/Metatable arm and the two
    /// `setmetatable` type-function bridging arms.
    fn subtype_metatable_parts(
        &mut self,
        (sub_table, sub_metatable): (TypeId, TypeId),
        (sup_table, sup_metatable): (TypeId, TypeId),
        path: &TypePath,
    ) -> Result<(), SubtypeError> {
        self.subtype_type(
            sub_table,
            sup_table,
            path.push(TypePathComponent::TypeField(TypeField::Table)),
        )?;
        self.subtype_type(
            sub_metatable,
            sup_metatable,
            path.push(TypePathComponent::TypeField(TypeField::Metatable)),
        )
    }

    fn subtype_type_alternative(
        &mut self,
        sub: TypeId,
        sup: TypeId,
        path: TypePath,
    ) -> Result<(), SubtypeError> {
        // `assumption_clock` is monotonic: a failed arm retracts every type-pair
        // assumption it opened, but the depths themselves are never reused.
        let type_floor = self.assumption_clock;
        let seen_packs = self.seen_packs.clone();
        let pack_floor = self.pack_clock;
        let generic_instantiation_frames = self.generic_instantiation_frames.clone();
        let min_assumption_depth = self.min_assumption_depth;
        let max_pack_dependency_depth = self.max_pack_dependency_depth;
        match self.subtype_type(sub, sup, path) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.seen_types.retain(|_, depth| *depth < type_floor);
                self.seen_packs = seen_packs;
                self.generic_instantiation_frames = generic_instantiation_frames;
                // The failed branch's assumptions are rolled back, so the
                // reliance it folded in must be discarded too — otherwise a
                // dead branch could taint a sibling that genuinely succeeds.
                self.min_assumption_depth = min_assumption_depth;
                self.max_pack_dependency_depth = max_pack_dependency_depth;
                // Restoring `seen_packs` retracts every pack assumption this
                // branch opened (depth `>= pack_floor`); evict any settled proof
                // that leaned on one of them so a sibling arm cannot reuse it.
                self.evict_pack_dependent_settled(pack_floor);
                Err(error)
            }
        }
    }

    /// Drops every settled proof whose deepest pack-assumption dependency was
    /// opened at or after `pack_floor`. A failed alternative that rolls its
    /// `seen_packs` back to `pack_floor` retracts exactly those assumptions, so a
    /// cached proof that leaned on one of them must not survive to be reused by a
    /// sibling arm. Proofs tagged with no pack dependency, or one below the
    /// floor, leaned only on still-valid assumptions and are kept — which is what
    /// preserves the recursive-generic memoization (and thus termination).
    fn evict_pack_dependent_settled(&self, pack_floor: usize) {
        self.settled_subtypes.borrow_mut().retain(|_, proofs| {
            proofs.retain(|(_, pack_dependency)| {
                pack_dependency.is_none_or(|depth| depth < pack_floor)
            });
            !proofs.is_empty()
        });
    }

    fn tagged_table_option_match_score(&self, sub: TypeId, sup: TypeId) -> usize {
        let (TypeKind::Table(sub_table), TypeKind::Table(sup_table)) = (
            self.arena.get(self.arena.follow(sub)),
            self.arena.get(self.arena.follow(sup)),
        ) else {
            return 0;
        };
        sub_table
            .properties
            .iter()
            .filter(|(name, sub_property)| {
                sup_table.properties.get(*name).is_some_and(|sup_property| {
                    self.same_singleton_type(sub_property.ty, sup_property.ty)
                })
            })
            .count()
    }

    fn same_singleton_type(&self, left: TypeId, right: TypeId) -> bool {
        matches!(
            (
                self.arena.get(self.arena.follow(left)),
                self.arena.get(self.arena.follow(right)),
            ),
            (TypeKind::Singleton(left), TypeKind::Singleton(right)) if left == right
        )
    }

    fn subtype_property_name_key_alternative(
        &mut self,
        name: &str,
        sup: TypeId,
        path: TypePath,
    ) -> Result<(), SubtypeError> {
        // Mirror `subtype_type_alternative`'s full rollback. A union arm of a
        // property-key check can open both type-pair and pack assumptions
        // (e.g. when `sup` is an indexer whose key is itself a recursive
        // function type), so the same coinductive residue applies: a failed arm
        // must restore `seen_types`/`seen_packs` and evict any settled proof that
        // leaned on a pack assumption it just retracted, or a sibling arm could
        // reuse an assumption-dependent proof.
        let seen_types = self.seen_types.clone();
        let seen_packs = self.seen_packs.clone();
        let pack_floor = self.pack_clock;
        let generic_instantiation_frames = self.generic_instantiation_frames.clone();
        let min_assumption_depth = self.min_assumption_depth;
        let max_pack_dependency_depth = self.max_pack_dependency_depth;
        match self.subtype_property_name_key(name, sup, path) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.seen_types = seen_types;
                self.seen_packs = seen_packs;
                self.generic_instantiation_frames = generic_instantiation_frames;
                self.min_assumption_depth = min_assumption_depth;
                self.max_pack_dependency_depth = max_pack_dependency_depth;
                self.evict_pack_dependent_settled(pack_floor);
                Err(error)
            }
        }
    }

    fn reduce_type_function(&self, name: &str, arguments: &[TypeId]) -> Option<TypeId> {
        self.reduce_type_function_readonly(name, arguments, &mut Vec::new())
    }

    fn reduce_type_function_readonly(
        &self,
        name: &str,
        arguments: &[TypeId],
        active: &mut Vec<TypeId>,
    ) -> Option<TypeId> {
        let mapped_arguments = arguments
            .iter()
            .copied()
            .map(|argument| self.resolve_type_function_operand(argument, active))
            .collect::<Vec<_>>();
        match self
            .type_function_runtime
            .reduce(self.arena, name, &mapped_arguments)
        {
            Reduction::Reduced(reduced) => Some(reduced),
            Reduction::Pending => match name {
                "index" => self.reduce_index_readonly(&mapped_arguments, active),
                _ => None,
            },
        }
    }

    fn substitute_instantiable_type(&self, id: TypeId) -> TypeId {
        let id = self.arena.follow(id);
        if let Some(frame_index) = self.instantiable_type_frame_index(id)
            && let Some(bound) = self.generic_instantiation_frames[frame_index].type_binding(id)
        {
            return self.arena.follow(bound);
        }
        id
    }

    fn resolve_type_function_operand(&self, id: TypeId, active: &mut Vec<TypeId>) -> TypeId {
        let id = self.substitute_instantiable_type(id);
        let id = self.arena.follow(id);
        if active.contains(&id) {
            return id;
        }
        active.push(id);
        let resolved = match self.arena.get(id).clone() {
            TypeKind::TypeFunctionInstance { name, arguments } => self
                .reduce_type_function_readonly(&name, &arguments, active)
                .unwrap_or(id),
            TypeKind::Bound(_) => unreachable!("follow removes bound types"),
            _ => id,
        };
        active.pop();
        resolved
    }

    fn reduce_index_readonly(
        &self,
        arguments: &[TypeId],
        active: &mut Vec<TypeId>,
    ) -> Option<TypeId> {
        let [base, key] = arguments else {
            return None;
        };
        self.reduce_index_pair_readonly(*base, *key, active)
    }

    fn reduce_index_pair_readonly(
        &self,
        base: TypeId,
        key: TypeId,
        active: &mut Vec<TypeId>,
    ) -> Option<TypeId> {
        let base = self.resolve_type_function_operand(base, active);
        let key = self.resolve_type_function_operand(key, active);
        let base = self.arena.follow(base);
        let key = self.arena.follow(key);
        if matches!(self.arena.get(base), TypeKind::Never)
            || matches!(self.arena.get(key), TypeKind::Never)
        {
            return Some(self.arena.primitives().never);
        }

        if let TypeKind::Union(keys) = self.arena.get(key).clone() {
            let values = keys
                .into_iter()
                .map(|key| self.reduce_index_pair_readonly(base, key, active))
                .collect::<Option<Vec<_>>>()?;
            return self.reduce_existing_union(values);
        }

        if let TypeKind::Union(bases) = self.arena.get(base).clone() {
            let values = bases
                .into_iter()
                .map(|base| self.reduce_index_pair_readonly(base, key, active))
                .collect::<Option<Vec<_>>>()?;
            return self.reduce_existing_union(values);
        }

        match self.arena.get(base).clone() {
            TypeKind::Table(table) => self.reduce_index_table_readonly(&table, key),
            TypeKind::Bound(_) => unreachable!("follow removes bound types"),
            _ => None,
        }
    }

    fn reduce_index_table_readonly(&self, table: &TableType, key: TypeId) -> Option<TypeId> {
        if let TypeKind::Singleton(SingletonType::String(name)) = self.arena.get(key).clone()
            && let Some(property) = table.properties.get(&name)
        {
            return Some(self.substitute_instantiable_type(property.ty));
        }

        if let Some(indexer) = &table.indexer
            && self.type_is_subtype_of_index_key(key, indexer.key)
        {
            return Some(self.substitute_instantiable_type(indexer.value));
        }

        if self.index_key_is_concrete_miss(key) {
            return Some(self.arena.primitives().never);
        }

        None
    }

    fn reduce_existing_union(&self, types: Vec<TypeId>) -> Option<TypeId> {
        let never = self.arena.primitives().never;
        let mut flattened = Vec::new();
        for ty in types {
            let ty = self.arena.follow(ty);
            if ty == never {
                continue;
            }
            match self.arena.get(ty).clone() {
                TypeKind::Any | TypeKind::Unknown => return Some(ty),
                TypeKind::Union(options) => flattened.extend(options),
                TypeKind::Never => {}
                _ => flattened.push(ty),
            }
        }

        flattened.sort_unstable();
        flattened.dedup();
        match flattened.as_slice() {
            [] => Some(never),
            [only] => Some(*only),
            _ => None,
        }
    }

    fn type_is_subtype_of_index_key(&self, sub: TypeId, sup: TypeId) -> bool {
        let sub = self.arena.follow(self.substitute_instantiable_type(sub));
        let sup = self.arena.follow(self.substitute_instantiable_type(sup));
        if sub == sup {
            return true;
        }

        match (self.arena.get(sub), self.arena.get(sup)) {
            (TypeKind::Union(options), _) => options
                .iter()
                .all(|option| self.type_is_subtype_of_index_key(*option, sup)),
            (_, TypeKind::Union(options)) => options
                .iter()
                .any(|option| self.type_is_subtype_of_index_key(sub, *option)),
            (
                TypeKind::Singleton(SingletonType::String(_)),
                TypeKind::Primitive(PrimitiveType::String),
            )
            | (
                TypeKind::Singleton(SingletonType::Boolean(_)),
                TypeKind::Primitive(PrimitiveType::Boolean),
            ) => true,
            _ => false,
        }
    }

    fn index_key_is_concrete_miss(&self, key: TypeId) -> bool {
        matches!(
            self.arena.get(self.arena.follow(key)),
            TypeKind::Singleton(_)
                | TypeKind::Primitive(PrimitiveType::Nil)
                | TypeKind::Primitive(PrimitiveType::Boolean)
                | TypeKind::Primitive(PrimitiveType::Number)
                | TypeKind::Primitive(PrimitiveType::String)
                | TypeKind::Primitive(PrimitiveType::Thread)
                | TypeKind::Primitive(PrimitiveType::Buffer)
                | TypeKind::Primitive(PrimitiveType::Vector)
        )
    }

    fn subtype_keyof_instance_to_type(
        &mut self,
        sub: TypeId,
        name: &str,
        arguments: &[TypeId],
        sup: TypeId,
        path: TypePath,
    ) -> Option<Result<(), SubtypeError>> {
        let target = self.keyof_target(name, arguments)?;
        match self.arena.get(self.arena.follow(target)).clone() {
            TypeKind::Table(table) => {
                Some(self.subtype_keyof_table_to_type(sub, &table, sup, path))
            }
            TypeKind::Never => Some(Ok(())),
            TypeKind::Bound(_) => unreachable!("follow removes bound types"),
            _ => None,
        }
    }

    fn subtype_keyof_table_to_type(
        &mut self,
        sub: TypeId,
        table: &TableType,
        sup: TypeId,
        path: TypePath,
    ) -> Result<(), SubtypeError> {
        for name in table.properties.keys() {
            if !self.string_key_is_subtype_of_type(name, sup) {
                return Err(SubtypeError::type_error(
                    SubtypeErrorKind::Mismatch,
                    path,
                    sub,
                    sup,
                ));
            }
        }
        if let Some(indexer) = &table.indexer {
            self.subtype_type(indexer.key, sup, path)?;
        }
        Ok(())
    }

    fn subtype_type_to_keyof_instance(
        &mut self,
        sub: TypeId,
        sup: TypeId,
        name: &str,
        arguments: &[TypeId],
        path: TypePath,
    ) -> Option<Result<(), SubtypeError>> {
        let target = self.keyof_target(name, arguments)?;
        match self.arena.get(self.arena.follow(target)).clone() {
            TypeKind::Table(table) => {
                Some(self.subtype_type_to_keyof_table(sub, sup, &table, path))
            }
            TypeKind::Never => Some(Err(SubtypeError::type_error(
                SubtypeErrorKind::Mismatch,
                path,
                sub,
                sup,
            ))),
            TypeKind::Bound(_) => unreachable!("follow removes bound types"),
            _ => None,
        }
    }

    fn subtype_type_to_keyof_table(
        &mut self,
        sub: TypeId,
        sup: TypeId,
        table: &TableType,
        path: TypePath,
    ) -> Result<(), SubtypeError> {
        let sub = self.arena.follow(sub);
        match self.arena.get(sub).clone() {
            TypeKind::Never => Ok(()),
            TypeKind::Union(options) => {
                for (index, option) in options.into_iter().enumerate() {
                    self.subtype_type_to_keyof_table(
                        option,
                        sup,
                        table,
                        path.push(TypePathComponent::Index { index }),
                    )?;
                }
                Ok(())
            }
            TypeKind::Singleton(SingletonType::String(value))
                if table.properties.contains_key(&value) =>
            {
                Ok(())
            }
            _ if table.indexer.as_ref().is_some_and(|indexer| {
                self.spawn_same_arena()
                    .subtype_type(sub, indexer.key, path.clone())
                    .is_ok()
            }) =>
            {
                Ok(())
            }
            TypeKind::Blocked(_) | TypeKind::Free(_) | TypeKind::Generic(_) | TypeKind::Error => {
                Ok(())
            }
            _ => Err(SubtypeError::type_error(
                SubtypeErrorKind::Mismatch,
                path,
                sub,
                sup,
            )),
        }
    }

    fn keyof_target(&self, name: &str, arguments: &[TypeId]) -> Option<TypeId> {
        if name != "keyof" {
            return None;
        }
        let [target] = arguments else {
            return None;
        };
        Some(self.resolve_type_function_operand(*target, &mut Vec::new()))
    }

    fn string_key_is_subtype_of_type(&self, key: &str, sup: TypeId) -> bool {
        let sup = self.arena.follow(sup);
        match self.arena.get(sup) {
            TypeKind::Primitive(PrimitiveType::String)
            | TypeKind::Any
            | TypeKind::Unknown
            | TypeKind::Blocked(_)
            | TypeKind::Free(_)
            | TypeKind::Generic(_) => true,
            TypeKind::Singleton(SingletonType::String(value)) => value == key,
            TypeKind::Union(options) => options
                .iter()
                .any(|option| self.string_key_is_subtype_of_type(key, *option)),
            TypeKind::Intersection(options) => options
                .iter()
                .all(|option| self.string_key_is_subtype_of_type(key, *option)),
            TypeKind::Bound(_) => unreachable!("follow removes bound types"),
            _ => false,
        }
    }

    fn subtype_string_primitive_table(
        &self,
        sub_id: TypeId,
        sup_id: TypeId,
        sup_table: TableType,
        path: TypePath,
    ) -> Result<(), SubtypeError> {
        if let Some((name, _)) = sup_table.properties.first_key_value()
            && !is_string_library_property(name)
        {
            return Err(SubtypeError::type_error(
                SubtypeErrorKind::MissingProperty,
                path.push(TypePathComponent::read_property(name.clone())),
                sub_id,
                sup_id,
            ));
        }
        if sup_table.properties.is_empty() && sup_table.indexer.is_some() {
            return Err(SubtypeError::type_error(
                SubtypeErrorKind::Mismatch,
                path,
                sub_id,
                sup_id,
            ));
        }
        let mut scratch_slot = self.table_intersection_scratch.borrow_mut();
        let scratch = scratch_slot.get_or_insert_with(|| self.arena.clone());
        for (name, sup_property) in sup_table.properties {
            let Some(property_ty) =
                member_access::primitive_property_type(scratch, PrimitiveType::String, &name)
            else {
                return Err(SubtypeError::type_error(
                    SubtypeErrorKind::MissingProperty,
                    path.push(TypePathComponent::read_property(name)),
                    sub_id,
                    sup_id,
                ));
            };
            Subtyper::new(scratch).subtype_property(
                &TableProperty::new(property_ty),
                &sup_property,
                path.push(TypePathComponent::property(name)),
                false,
            )?;
        }
        if sup_table.indexer.is_some() {
            return Err(SubtypeError::type_error(
                SubtypeErrorKind::Mismatch,
                path,
                sub_id,
                sup_id,
            ));
        }
        Ok(())
    }

    fn subtype_extern_table(
        &mut self,
        sub_id: TypeId,
        sup_id: TypeId,
        sub_properties: &BTreeMap<String, TableProperty>,
        sub_indexer: Option<TableIndexer>,
        sup_table: TableType,
        path: TypePath,
    ) -> Result<(), SubtypeError> {
        if sup_table.properties.is_empty() && sup_table.indexer.is_none() {
            return Err(SubtypeError::type_error(
                SubtypeErrorKind::Mismatch,
                path,
                sub_id,
                sup_id,
            ));
        }

        for (name, sup_property) in sup_table.properties {
            let sub_property = if let Some(sub_property) = sub_properties.get(&name) {
                sub_property.clone()
            } else if let Some(sub_indexer) = &sub_indexer {
                self.subtype_property_name_key(&name, sub_indexer.key, path.clone())?;
                TableProperty {
                    ty: sub_indexer.value,
                    write_ty: None,
                    location: None,
                    documentation_symbol: None,
                    read_only: sub_indexer.read_only,
                    write_only: false,
                    deprecated: false,
                }
            } else {
                return Err(SubtypeError::type_error(
                    SubtypeErrorKind::MissingProperty,
                    path.push(TypePathComponent::read_property(name)),
                    sub_id,
                    sup_id,
                ));
            };
            self.subtype_property(
                &sub_property,
                &sup_property,
                path.push(TypePathComponent::property(name)),
                false,
            )?;
        }

        if let Some(sup_indexer) = sup_table.indexer {
            let Some(sub_indexer) = sub_indexer else {
                return Err(SubtypeError::type_error(
                    SubtypeErrorKind::Mismatch,
                    path.push(TypePathComponent::TypeField(TypeField::IndexLookup)),
                    sub_id,
                    sup_id,
                ));
            };
            self.subtype_indexer(&sub_indexer, &sup_indexer, path, true)?;
        }

        Ok(())
    }

    fn subtype_table_intersection(
        &self,
        sub_id: TypeId,
        options: Vec<TypeId>,
        sup_id: TypeId,
        path: TypePath,
    ) -> Option<Result<(), SubtypeError>> {
        let key = (
            options
                .iter()
                .map(|option| self.arena.follow(*option))
                .collect::<Vec<_>>(),
            self.arena.follow(sup_id),
        );
        if let Some(cached) = self.table_intersection_accepts.borrow().get(&key) {
            return cached.map(|()| Ok(()));
        }
        let combined_id = {
            let mut scratch_slot = self.table_intersection_scratch.borrow_mut();
            let scratch = scratch_slot.get_or_insert_with(|| self.arena.clone());
            let mut tables = options.into_iter().map(|option| {
                match self.arena.get(self.arena.follow(option)).clone() {
                    TypeKind::Table(table) if table.indexer.is_none() => Some(table),
                    _ => None,
                }
            });
            let combined = tables.next().flatten().and_then(|mut combined| {
                for table in tables {
                    combined = combine_table_intersection_for_subtyping(scratch, combined, table?)?;
                }
                Some(combined)
            });
            let Some(combined) = combined else {
                self.table_intersection_accepts
                    .borrow_mut()
                    .insert(key, None);
                return None;
            };
            scratch.alloc(TypeKind::Table(combined))
        };
        let scratch_slot = self.table_intersection_scratch.borrow();
        let scratch = scratch_slot
            .as_ref()
            .expect("scratch arena initialized above");
        let result = Subtyper::new(scratch).subtype_type(combined_id, sup_id, path);
        if result.is_ok() {
            self.table_intersection_accepts
                .borrow_mut()
                .insert(key, Some(()));
        }
        Some(result.map_err(|error| SubtypeError {
            kind: error.kind,
            path: error.path,
            sub: SubtypeTarget::Type(sub_id),
            sup: SubtypeTarget::Type(sup_id),
        }))
    }

    fn subtype_intersection_options(
        &mut self,
        sub_id: TypeId,
        sub_options: &[TypeId],
        sup_id: TypeId,
        sup_options: &[TypeId],
        path: &TypePath,
    ) -> Result<(), SubtypeError> {
        for (sup_index, sup_option) in sup_options.iter().copied().enumerate() {
            let mut matched = false;
            let mut last_error = None;
            for sub_option in sub_options.iter().copied() {
                match self.subtype_type_alternative(
                    sub_option,
                    sup_option,
                    path.push(TypePathComponent::Index { index: sup_index }),
                ) {
                    Ok(()) => {
                        matched = true;
                        break;
                    }
                    Err(error) => last_error = Some(error),
                }
            }
            if !matched {
                return Err(last_error.unwrap_or_else(|| {
                    SubtypeError::type_error(
                        SubtypeErrorKind::Mismatch,
                        path.clone(),
                        sub_id,
                        sup_id,
                    )
                }));
            }
        }
        Ok(())
    }

    fn subtype_function(
        &mut self,
        sub: &FunctionType,
        sup: &FunctionType,
        path: TypePath,
    ) -> Result<(), SubtypeError> {
        // Capture the enclosing table pair (if any) so the receiver fallback can
        // recognise a recursive `self`, then clear it for the duration: only
        // these directly-compared method functions may consult it, never the
        // functions nested in their arguments or returns. Restored on every exit
        // so the reverse-direction property check (and siblings) still see it.
        let receiver_context = self.method_receiver_context.take();
        let result = self.subtype_function_with_receiver_context(sub, sup, path, receiver_context);
        self.method_receiver_context = receiver_context;
        result
    }

    fn subtype_function_with_receiver_context(
        &mut self,
        sub: &FunctionType,
        sup: &FunctionType,
        path: TypePath,
        receiver_context: Option<(TypeId, TypeId)>,
    ) -> Result<(), SubtypeError> {
        let sub_top = is_top_function_type(self.arena, sub);
        let sup_top = is_top_function_type(self.arena, sup);
        if sup_top {
            return Ok(());
        }
        if sub_top {
            return Err(SubtypeError {
                kind: SubtypeErrorKind::Mismatch,
                path,
                sub: SubtypeTarget::Pack(sub.arguments),
                sup: SubtypeTarget::Pack(sup.arguments),
            });
        }
        if sub.has_self != sup.has_self {
            return Err(SubtypeError {
                kind: SubtypeErrorKind::Mismatch,
                path,
                sub: SubtypeTarget::Pack(sub.arguments),
                sup: SubtypeTarget::Pack(sup.arguments),
            });
        }
        // Polymorphism direction: a sub side with more generic
        // parameters than the sup side is acceptable — sub can be
        // instantiated to match. The reverse (sup more polymorphic
        // than sub) is unsound. Generic-pack lists follow the same
        // rule. Same-shape generics fall through to the structural
        // arg/return checks, which already treat `Generic <: anything`
        // and `anything <: Generic` as `Ok` at the type level so the
        // pure-generic body matches structurally.
        let lacks_sup_generics = sub.generics.len() < sup.generics.len();
        let lacks_sup_generic_packs = sub.generic_packs.len() < sup.generic_packs.len();
        let generic_pack_can_instantiate_sup_type_generics =
            lacks_sup_generics && !sub.generic_packs.is_empty();
        let top_variadic_matches_argument_only_generics = (lacks_sup_generics
            || lacks_sup_generic_packs)
            && self.top_variadic_satisfies_argument_only_generics(sub, sup);
        if ((lacks_sup_generics && !generic_pack_can_instantiate_sup_type_generics)
            || lacks_sup_generic_packs)
            && !top_variadic_matches_argument_only_generics
        {
            return Err(SubtypeError {
                kind: SubtypeErrorKind::Mismatch,
                path,
                sub: SubtypeTarget::Pack(sub.arguments),
                sup: SubtypeTarget::Pack(sup.arguments),
            });
        }
        let matching_generic_ranks = sub.generics.len() == sup.generics.len()
            && sub.generic_packs.len() == sup.generic_packs.len()
            && (!sub.generics.is_empty() || !sub.generic_packs.is_empty());
        let instantiation_frame = if matching_generic_ranks {
            GenericInstantiationFrame::for_function_with_matching_generics(self.arena, sub, sup)
        } else if generic_pack_can_instantiate_sup_type_generics {
            GenericInstantiationFrame::for_function_with_rigid_super_generics(self.arena, sub, sup)
        } else {
            GenericInstantiationFrame::for_function(self.arena, sub)
        };
        let pushed_frame = !instantiation_frame.is_empty();
        if pushed_frame {
            self.generic_instantiation_frames.push(instantiation_frame);
        }
        let arguments_path = path.push(TypePathComponent::PackField(PackField::Arguments));
        let returns_path = path.push(TypePathComponent::PackField(PackField::Returns));
        let result =
            match self.subtype_function_arguments(sub, sup, arguments_path, receiver_context) {
                Ok(()) => self.subtype_pack(sub.returns, sup.returns, returns_path),
                Err(argument_error)
                    if receiver_context.is_none()
                        && plain_function_pair_can_probe_return_diagnostic(sub, sup) =>
                {
                    match self.probe_function_returns_for_diagnostic(
                        sub.returns,
                        sup.returns,
                        returns_path,
                    ) {
                        Ok(()) => Err(argument_error),
                        Err(return_error) => Err(return_error),
                    }
                }
                Err(error) => Err(error),
            };
        if pushed_frame {
            self.generic_instantiation_frames.pop();
        }
        result
    }

    fn probe_function_returns_for_diagnostic(
        &mut self,
        sub: TypePackId,
        sup: TypePackId,
        path: TypePath,
    ) -> Result<(), SubtypeError> {
        // Diagnostic-only: run under the live coinductive stacks so recursive
        // aliases short-circuit normally, then restore all branch state before
        // choosing whether to report the argument or return error.
        let seen_types = self.seen_types.clone();
        let seen_packs = self.seen_packs.clone();
        let pack_floor = self.pack_clock;
        let generic_instantiation_frames = self.generic_instantiation_frames.clone();
        let min_assumption_depth = self.min_assumption_depth;
        let max_pack_dependency_depth = self.max_pack_dependency_depth;
        let result = self.subtype_pack(sub, sup, path);
        self.seen_types = seen_types;
        self.seen_packs = seen_packs;
        self.generic_instantiation_frames = generic_instantiation_frames;
        self.min_assumption_depth = min_assumption_depth;
        self.max_pack_dependency_depth = max_pack_dependency_depth;
        self.evict_pack_dependent_settled(pack_floor);
        result
    }

    /// Compares argument packs for a function subtype check. Arguments are
    /// normally contravariant. A method's first parameter (`self`) is bound to
    /// the actual object at every call, so for the *recursive self/this pattern*
    /// strict contravariance wrongly rejects width subtyping between two object
    /// types whose methods take that recursive receiver (e.g. a richer
    /// `TypedRowset<T>` conforming to a thinner declared shape). In that one case
    /// the receiver is discharged by the enclosing-table coinductive assumption
    /// that is already in flight rather than re-proven, which lets a richer object
    /// type width-conform to a thinner declared shape.
    ///
    /// The relaxation fires *only* when both methods' `self` parameter types are
    /// the enclosing tables currently being compared property-by-property — the
    /// genuine self/this pattern. The residual relaxation it permits (a
    /// recursive-self method field-called or detached against a thinner object) is
    /// the standard, Luau-compatible method-receiver bivariance: restricting
    /// covariance to the recursive self/this pattern is exactly what makes
    /// object-conforms-to-interface ergonomic, while the egregious non-recursive
    /// cases — a standalone function whose first parameter merely happens to be
    /// named `self`, or a table method whose `self` is some unrelated type — stay
    /// soundly contravariant. Crucially, discharging the receiver against the
    /// in-flight assumption adds nothing to `seen_types`: the remaining arguments
    /// (and any nested returns) are then compared contravariantly with a clean
    /// assumption stack, so genuinely contravariant obligations are never
    /// vacuously satisfied by receiver residue. Remaining arguments are always
    /// contravariant.
    fn subtype_function_arguments(
        &mut self,
        sub: &FunctionType,
        sup: &FunctionType,
        path: TypePath,
        receiver_context: Option<(TypeId, TypeId)>,
    ) -> Result<(), SubtypeError> {
        let (Some((sub_self, sub_rest)), Some((sup_self, sup_rest))) = (
            self.arena.split_first_in_list_pack(sub.arguments),
            self.arena.split_first_in_list_pack(sup.arguments),
        ) else {
            return self.subtype_pack(sup.arguments, sub.arguments, path);
        };
        let Some((enclosing_sub, enclosing_sup)) = receiver_context else {
            return self.subtype_pack(sup.arguments, sub.arguments, path);
        };
        if !self.is_recursive_self_receiver(sub_self, sup_self, receiver_context) {
            return self.subtype_pack(sup.arguments, sub.arguments, path);
        }
        let receiver_path = path.push(TypePathComponent::Index { index: 0 });
        // The recursive receiver is bound to the actual object at every call, so it
        // is discharged by the in-flight `(enclosing_sub, enclosing_sup)`
        // coinductive assumption: we are inside `subtype_table` comparing a method
        // property, and each `self` is one of those enclosing tables. (Which one is
        // direction-dependent — `subtype_property` relates read-write method fields
        // in both directions — so we consult the enclosing pair itself rather than
        // the receiver parameters, whose order swaps on the reverse check.)
        // Consulting that pair is guaranteed to short-circuit on `seen_types`: it
        // registers the assumption dependency (so the method proof is not cached as
        // assumption-independent) and adds nothing to the stack. Running a
        // contravariant probe here instead would leave a residual `(sup_self,
        // sub_self)` assumption that vacuously discharges genuinely contravariant
        // obligations on other arguments and nested returns.
        self.subtype_type(enclosing_sub, enclosing_sup, receiver_path)?;
        // Remaining arguments stay contravariant, computed with a clean
        // `seen_types` so each obligation is checked on its own merits.
        self.subtype_list_pack_with_index_offset(sup_rest, &sub_rest, path, 1)
    }

    /// Returns whether the two method `self` parameter types are the recursive
    /// enclosing tables of the property comparison in flight — i.e. each `self`
    /// is the same alias instance as one of the two tables currently being
    /// related in `subtype_table`. This is the only shape that earns the covariant
    /// receiver fallback. The match is order-independent because the same property
    /// is compared in both directions (`subtype_property` checks read-write method
    /// fields invariantly): in the forward check `sub_self`/`sup_self` line up with
    /// `enclosing_sub`/`enclosing_sup`, and in the reverse check the roles swap.
    fn is_recursive_self_receiver(
        &self,
        sub_self: TypeId,
        sup_self: TypeId,
        receiver_context: Option<(TypeId, TypeId)>,
    ) -> bool {
        let Some((enclosing_sub, enclosing_sup)) = receiver_context else {
            return false;
        };
        (self.same_alias_identity_table(sub_self, enclosing_sub)
            && self.same_alias_identity_table(sup_self, enclosing_sup))
            || (self.same_alias_identity_table(sub_self, enclosing_sup)
                && self.same_alias_identity_table(sup_self, enclosing_sub))
    }

    /// Whether two type ids both resolve to tables that are the same alias
    /// instance. A `self` parameter typed to an anonymous table (no alias
    /// identity) therefore never matches an enclosing alias table, so the
    /// non-recursive method cases stay contravariant.
    fn same_alias_identity_table(&self, left: TypeId, right: TypeId) -> bool {
        let (TypeKind::Table(left), TypeKind::Table(right)) = (
            self.arena.get(self.arena.follow(left)),
            self.arena.get(self.arena.follow(right)),
        ) else {
            return false;
        };
        same_alias_identity_table_instance(self.arena, left, right)
    }

    fn top_variadic_satisfies_argument_only_generics(
        &self,
        sub: &FunctionType,
        sup: &FunctionType,
    ) -> bool {
        self.is_any_or_unknown_variadic_pack(sub.arguments)
            && GenericInstantiationFrame::for_function_returns(self.arena, sup).is_empty()
    }

    fn is_any_or_unknown_variadic_pack(&self, pack: TypePackId) -> bool {
        let pack = self.arena.follow_pack(pack);
        match self.arena.get_pack(pack) {
            TypePackKind::Variadic { ty } => {
                let ty = self.arena.follow(*ty);
                matches!(self.arena.get(ty), TypeKind::Any | TypeKind::Unknown)
            }
            TypePackKind::List {
                types,
                tail: Some(tail),
            } if types.is_empty() => self.is_any_or_unknown_variadic_pack(*tail),
            _ => false,
        }
    }

    fn instantiable_type_frame_index(&self, id: TypeId) -> Option<usize> {
        self.generic_instantiation_frames
            .iter()
            .rposition(|frame| frame.contains_type(id))
    }

    fn instantiable_pack_frame_index(&self, id: TypePackId) -> Option<usize> {
        self.generic_instantiation_frames
            .iter()
            .rposition(|frame| frame.contains_pack(id))
    }

    fn subtype_instantiable_sub_type(
        &mut self,
        frame_index: usize,
        generic: TypeId,
        sup: TypeId,
        path: TypePath,
    ) -> Result<(), SubtypeError> {
        if let Some(bound) = self.generic_instantiation_frames[frame_index].type_binding(generic) {
            return self.subtype_type(bound, sup, path);
        }
        self.generic_instantiation_frames[frame_index].bind_type(generic, sup);
        Ok(())
    }

    fn subtype_instantiable_sup_type(
        &mut self,
        frame_index: usize,
        sub: TypeId,
        generic: TypeId,
        path: TypePath,
    ) -> Result<(), SubtypeError> {
        if let Some(bound) = self.generic_instantiation_frames[frame_index].type_binding(generic) {
            return self.subtype_type(sub, bound, path);
        }
        self.generic_instantiation_frames[frame_index].bind_type(generic, sub);
        Ok(())
    }

    fn subtype_instantiable_sub_pack(
        &mut self,
        frame_index: usize,
        generic: TypePackId,
        sup: TypePackId,
        path: TypePath,
    ) -> Result<(), SubtypeError> {
        if let Some(bound) = self.generic_instantiation_frames[frame_index].pack_binding(generic) {
            return self.subtype_pack(bound, sup, path);
        }
        self.generic_instantiation_frames[frame_index].bind_pack(generic, sup);
        Ok(())
    }

    fn subtype_instantiable_sup_pack(
        &mut self,
        frame_index: usize,
        sub: TypePackId,
        generic: TypePackId,
        path: TypePath,
    ) -> Result<(), SubtypeError> {
        if let Some(bound) = self.generic_instantiation_frames[frame_index].pack_binding(generic) {
            return self.subtype_pack(sub, bound, path);
        }
        self.generic_instantiation_frames[frame_index].bind_pack(generic, sub);
        Ok(())
    }

    fn subtype_table(
        &mut self,
        sub_id: TypeId,
        sup_id: TypeId,
        sub: TableType,
        sup: TableType,
        path: TypePath,
        relax_exempt: &BTreeSet<String>,
    ) -> Result<(), SubtypeError> {
        let sub_state = sub.state;
        if !compatible_table_state(sub.state, sup.state) {
            return Err(SubtypeError::type_error(
                SubtypeErrorKind::Mismatch,
                path,
                sub_id,
                sup_id,
            ));
        }
        if same_named_table_instance(self.arena, &sub, &sup) {
            return Ok(());
        }
        if !sub.instantiated_type_params.is_empty() && !sup.instantiated_type_params.is_empty() {
            self.subtype_type_list(
                sub.instantiated_type_params,
                sup.instantiated_type_params,
                path.clone(),
                sub_id,
                sup_id,
            )?;
        }
        // Pack analog of the instantiated-type-param comparison above: a
        // phantom pack parameter (`type Phantom<T...> = {}`) leaves no
        // structural trace in the body, so two instantiations are otherwise
        // indistinguishable and `Phantom<number> <: Phantom<boolean>` would
        // vacuously accept.
        if sub.instantiated_type_pack_params.len() == sup.instantiated_type_pack_params.len() {
            for (sub_pack, sup_pack) in sub
                .instantiated_type_pack_params
                .iter()
                .copied()
                .zip(sup.instantiated_type_pack_params.iter().copied())
            {
                self.subtype_pack(sub_pack, sup_pack, path.clone())?;
            }
        }

        let has_relaxed_receiver_probe =
            matches!(sub_state, TableState::Unsealed | TableState::Free)
                && sup.properties.iter().any(|(name, sup_property)| {
                    !sup_property.read_only
                        && sub.properties.get(name).is_some_and(|sub_property| {
                            sub_property.read_only
                                && member_access::type_is_dynamic(self.arena, sub_property.ty)
                        })
                });
        let mut suppressing_error = None;
        let mut missing_properties = Vec::new();
        let sup_property_names: BTreeSet<String> = sup.properties.keys().cloned().collect();
        // Record this table pair as the receiver context for the property
        // comparisons below: a method's `self` parameter earns the covariant
        // receiver fallback only when its type is one of these enclosing tables
        // (the recursive self/this pattern). The restore before indexer checks
        // is deliberate: indexer-stored functions run under the outer context,
        // matching the scope that existed before the property comparison.
        let saved_receiver_context = self.method_receiver_context;
        self.method_receiver_context = Some((sub_id, sup_id));
        let property_result = (|| {
            for (name, sup_property) in sup.properties {
                let Some(sub_property) = sub.properties.get(&name) else {
                    if has_relaxed_receiver_probe && matches!(sub_state, TableState::Free) {
                        continue;
                    }
                    // An unsealed or free table that omits an optional property is a
                    // sound subtype: the absent key reads as `nil`. This holds even
                    // when the table carries a contextual indexer (synthesized from
                    // the expected type to validate the keys that *are* present) —
                    // the indexer governs present keys, not this absent one, so it
                    // must not force the missing optional property through an
                    // invariant indexer-value check.
                    if member_access::type_accepts_nil(self.arena, sup_property.ty)
                        && matches!(sub_state, TableState::Unsealed | TableState::Free)
                    {
                        continue;
                    }
                    if let Some(sub_indexer) = &sub.indexer
                        && self
                            .subtype_property_name_key(&name, sub_indexer.key, path.clone())
                            .is_ok()
                    {
                        if let Err(error) = self.subtype_property(
                            &TableProperty {
                                ty: sub_indexer.value,
                                write_ty: None,
                                location: None,
                                documentation_symbol: None,
                                read_only: sub_indexer.read_only,
                                write_only: false,
                                deprecated: false,
                            },
                            &sup_property,
                            path.push(TypePathComponent::property(name)),
                            false,
                        ) {
                            self.record_suppressing_table_error(&mut suppressing_error, error)?;
                        }
                        continue;
                    }
                    if member_access::type_accepts_nil(self.arena, sup_property.ty)
                        && (matches!(sub_state, TableState::Unsealed | TableState::Free)
                            || (sup_property.read_only && !sup_property.write_only))
                    {
                        continue;
                    }
                    missing_properties.push(name);
                    continue;
                };
                let relax_read_only_sub =
                    matches!(sub_state, TableState::Unsealed | TableState::Free)
                        && !sup_property.read_only
                        && !relax_exempt.contains(&name);
                if let Err(error) = self.subtype_property(
                    sub_property,
                    &sup_property,
                    path.push(TypePathComponent::property(name)),
                    relax_read_only_sub,
                ) {
                    self.record_suppressing_table_error(&mut suppressing_error, error)?;
                }
            }
            Ok(())
        })();
        self.method_receiver_context = saved_receiver_context;
        property_result?;
        if !missing_properties.is_empty() {
            let path = if let [name] = missing_properties.as_slice() {
                path.push(TypePathComponent::read_property(name.clone()))
            } else {
                path
            };
            let kind = if missing_properties.len() == 1 {
                SubtypeErrorKind::MissingProperty
            } else {
                SubtypeErrorKind::MissingProperties {
                    names: missing_properties,
                }
            };
            return Err(SubtypeError::type_error(kind, path, sub_id, sup_id));
        }

        let indexer_result = match (sub.indexer, sup.indexer) {
            (Some(sub_indexer), Some(sup_indexer)) => {
                self.subtype_indexer(&sub_indexer, &sup_indexer, path, false)
            }
            (None, Some(sup_indexer))
                if matches!(sub_state, TableState::Unsealed | TableState::Free) =>
            {
                // An unsealed or free table without its own indexer still
                // satisfies a supertype indexer: its keys are closed, so absent
                // keys read as `nil`. Properties shared with a supertype named
                // property were already validated against that property; any
                // remaining (extra) key is governed by the indexer and its value
                // must conform to the indexer value type.
                let mut result = Ok(());
                for (name, sub_property) in &sub.properties {
                    if sup_property_names.contains(name) {
                        continue;
                    }
                    if self
                        .subtype_property_name_key(name, sup_indexer.key, path.clone())
                        .is_err()
                    {
                        result = Err(SubtypeError::type_error(
                            SubtypeErrorKind::Mismatch,
                            path,
                            sub_id,
                            sup_id,
                        ));
                        break;
                    }
                    if let Err(error) = self.subtype_type(
                        sub_property.ty,
                        sup_indexer.value,
                        path.push(TypePathComponent::property(name.clone())),
                    ) {
                        result = Err(error);
                        break;
                    }
                }
                result
            }
            (None, Some(_)) => Err(SubtypeError::type_error(
                SubtypeErrorKind::Mismatch,
                path,
                sub_id,
                sup_id,
            )),
            (Some(_), None) | (None, None) => Ok(()),
        };
        if let Err(error) = indexer_result {
            self.record_suppressing_table_error(&mut suppressing_error, error)?;
        }
        suppressing_error.map_or(Ok(()), Err)
    }

    fn record_suppressing_table_error(
        &self,
        suppressing_error: &mut Option<SubtypeError>,
        error: SubtypeError,
    ) -> Result<(), SubtypeError> {
        if self.subtype_error_suppresses_errors(&error) {
            if suppressing_error.is_none() {
                *suppressing_error = Some(error);
            }
            Ok(())
        } else {
            Err(error)
        }
    }

    fn subtype_property_name_key(
        &mut self,
        name: &str,
        sup: TypeId,
        path: TypePath,
    ) -> Result<(), SubtypeError> {
        let sup = self.arena.follow(sup);
        match self.arena.get(sup).clone() {
            TypeKind::Primitive(crate::types::PrimitiveType::String)
            | TypeKind::Any
            | TypeKind::Unknown
            | TypeKind::Error
            | TypeKind::Blocked(_)
            | TypeKind::Free(_)
            | TypeKind::Generic(_) => Ok(()),
            TypeKind::Singleton(crate::types::SingletonType::String(value)) if value == name => {
                Ok(())
            }
            TypeKind::Union(options) => {
                let mut last_error = None;
                for (index, option) in options.into_iter().enumerate() {
                    match self.subtype_property_name_key_alternative(
                        name,
                        option,
                        path.push(TypePathComponent::Index { index }),
                    ) {
                        Ok(()) => return Ok(()),
                        Err(error) => last_error = Some(error),
                    }
                }
                Err(last_error.unwrap_or_else(|| {
                    SubtypeError::type_error(SubtypeErrorKind::Mismatch, path, sup, sup)
                }))
            }
            TypeKind::Intersection(options) => {
                for (index, option) in options.into_iter().enumerate() {
                    self.subtype_property_name_key(
                        name,
                        option,
                        path.push(TypePathComponent::Index { index }),
                    )?;
                }
                Ok(())
            }
            TypeKind::Negation(inner) => match self.subtype_property_name_key(name, inner, path) {
                Ok(()) => Err(SubtypeError::type_error(
                    SubtypeErrorKind::Mismatch,
                    TypePath::new(),
                    sup,
                    sup,
                )),
                Err(_) => Ok(()),
            },
            _ => Err(SubtypeError::type_error(
                SubtypeErrorKind::Mismatch,
                path,
                sup,
                sup,
            )),
        }
    }

    fn subtype_property(
        &mut self,
        sub: &TableProperty,
        sup: &TableProperty,
        path: TypePath,
        relax_read_only_sub: bool,
    ) -> Result<(), SubtypeError> {
        let method_probe_shape =
            member_access::method_probe_function_shape_matches(self.arena, sub.ty, sup.ty);
        let relax_read_probe = method_probe_shape || (relax_read_only_sub && sub.read_only);
        if sub.deprecated != sup.deprecated {
            return Err(SubtypeError {
                kind: SubtypeErrorKind::PropertyVariance,
                path,
                sub: SubtypeTarget::Type(sub.ty),
                sup: SubtypeTarget::Type(sup.ty),
            });
        }
        if sub.read_only
            && !sup.read_only
            && !relax_read_probe
            && member_access::property_modifier_is_concrete(self.arena, sub.ty)
            || sub.write_only
                && !sup.write_only
                && member_access::property_modifier_is_concrete(self.arena, sub.ty)
        {
            return Err(SubtypeError {
                kind: SubtypeErrorKind::PropertyVariance,
                path,
                sub: SubtypeTarget::Type(sub.ty),
                sup: SubtypeTarget::Type(sup.ty),
            });
        }
        if sup.read_only {
            return self.subtype_type(sub.ty, sup.ty, path);
        }
        if sup.write_only {
            return self.subtype_type(sup.ty, sub.ty, path);
        }
        if relax_read_probe && method_probe_shape {
            return Ok(());
        }

        let sub_ty = self.arena.follow(sub.ty);
        let sup_ty = self.arena.follow(sup.ty);
        if sub_ty == sup_ty {
            return Ok(());
        }
        if matches!(
            (self.arena.get(sub_ty), self.arena.get(sup_ty)),
            (TypeKind::Any | TypeKind::Unknown | TypeKind::Error, _)
                | (_, TypeKind::Any | TypeKind::Unknown | TypeKind::Error)
        ) {
            if relax_read_probe {
                return Ok(());
            }
            return Err(SubtypeError {
                kind: SubtypeErrorKind::PropertyVariance,
                path,
                sub: SubtypeTarget::Type(sub.ty),
                sup: SubtypeTarget::Type(sup.ty),
            });
        }
        if relax_read_probe {
            return self.subtype_type(sub.ty, sup.ty, path);
        }
        let sub_to_sup = self.subtype_type(sub.ty, sup.ty, path.clone());
        let sup_to_sub = self.subtype_type(sup.ty, sub.ty, path);
        match (sub_to_sup, sup_to_sub) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), _) | (_, Err(error)) => Err(SubtypeError {
                kind: SubtypeErrorKind::PropertyVariance,
                path: error.path,
                sub: SubtypeTarget::Type(sub.ty),
                sup: SubtypeTarget::Type(sup.ty),
            }),
        }
    }

    fn subtype_indexer(
        &mut self,
        sub: &TableIndexer,
        sup: &TableIndexer,
        path: TypePath,
        detailed_key_path: bool,
    ) -> Result<(), SubtypeError> {
        if self.arena.follow(sub.key) != self.arena.follow(sup.key) {
            let key_path = path.push(TypePathComponent::TypeField(TypeField::IndexLookup));
            let sub_to_sup = self.subtype_type(sub.key, sup.key, key_path.clone());
            let sup_to_sub = self.subtype_type(sup.key, sub.key, key_path);
            match (sub_to_sup, sup_to_sub) {
                (Ok(()), Ok(())) => {}
                (Err(error), _) | (_, Err(error)) => {
                    let path = if detailed_key_path { error.path } else { path };
                    return Err(SubtypeError {
                        kind: SubtypeErrorKind::PropertyVariance,
                        path,
                        sub: SubtypeTarget::Type(sub.key),
                        sup: SubtypeTarget::Type(sup.key),
                    });
                }
            }
        }

        if sub.read_only && !sup.read_only {
            return Err(SubtypeError {
                kind: SubtypeErrorKind::PropertyVariance,
                path,
                sub: SubtypeTarget::Type(sub.value),
                sup: SubtypeTarget::Type(sup.value),
            });
        }

        if sup.read_only {
            self.subtype_type(
                sub.value,
                sup.value,
                path.push(TypePathComponent::TypeField(TypeField::IndexResult)),
            )
        } else if self.arena.follow(sub.value) == self.arena.follow(sup.value)
            || !member_access::property_modifier_is_concrete(self.arena, sub.value)
            || !member_access::property_modifier_is_concrete(self.arena, sup.value)
        {
            Ok(())
        } else {
            let value_path = path.push(TypePathComponent::TypeField(TypeField::IndexResult));
            let sub_to_sup = self.subtype_type(sub.value, sup.value, value_path.clone());
            let sup_to_sub = self.subtype_type(sup.value, sub.value, value_path);
            match (sub_to_sup, sup_to_sub) {
                (Ok(()), Ok(())) => Ok(()),
                (Err(error), _) | (_, Err(error)) => Err(SubtypeError {
                    kind: SubtypeErrorKind::PropertyVariance,
                    path: error.path,
                    sub: SubtypeTarget::Type(sub.value),
                    sup: SubtypeTarget::Type(sup.value),
                }),
            }
        }
    }

    fn subtype_type_list(
        &mut self,
        subs: Vec<TypeId>,
        sups: Vec<TypeId>,
        path: TypePath,
        sub: TypeId,
        sup: TypeId,
    ) -> Result<(), SubtypeError> {
        if subs.len() != sups.len() {
            return Err(SubtypeError::type_error(
                SubtypeErrorKind::ArityMismatch,
                path,
                sub,
                sup,
            ));
        }
        for (index, (sub, sup)) in subs.into_iter().zip(sups).enumerate() {
            self.subtype_type(sub, sup, path.push(TypePathComponent::Index { index }))?;
        }
        Ok(())
    }

    fn subtype_pack(
        &mut self,
        sub: TypePackId,
        sup: TypePackId,
        path: TypePath,
    ) -> Result<(), SubtypeError> {
        let sub = self.arena.follow_pack(sub);
        let sup = self.arena.follow_pack(sup);
        if sub == sup {
            return Ok(());
        }
        if let Some(&assumption_depth) = self.seen_packs.get(&(sub, sup)) {
            // Coinductive pack short-circuit: this pack pair is already assumed
            // higher in the recursion. Record the depth it was opened at as a
            // pack dependency so an owning cache candidate learns the deepest
            // pack assumption its subtree leaned on and can be evicted if a
            // failed alternative later rolls that assumption back.
            self.max_pack_dependency_depth =
                self.max_pack_dependency_depth.max(Some(assumption_depth));
            return Ok(());
        }
        let assumption_depth = self.pack_clock;
        self.pack_clock += 1;
        self.seen_packs.insert((sub, sup), assumption_depth);

        let sub_kind = self.arena.get_pack(sub).clone();
        let sup_kind = self.arena.get_pack(sup).clone();

        if matches!(sub_kind, TypePackKind::Generic(_))
            && let Some(frame_index) = self.instantiable_pack_frame_index(sub)
        {
            return self.subtype_instantiable_sub_pack(frame_index, sub, sup, path);
        }
        if matches!(sup_kind, TypePackKind::Generic(_))
            && let Some(frame_index) = self.instantiable_pack_frame_index(sup)
        {
            return self.subtype_instantiable_sup_pack(frame_index, sub, sup, path);
        }
        if matches!(sup_kind, TypePackKind::Generic(_))
            && self
                .generic_instantiation_frames
                .iter()
                .any(|frame| frame.is_rigid_pack(sup))
        {
            return Err(SubtypeError::pack_error(
                SubtypeErrorKind::Mismatch,
                path,
                sub,
                sup,
            ));
        }

        match (sub_kind, sup_kind) {
            (TypePackKind::Error, _) | (_, TypePackKind::Error) => Ok(()),
            (TypePackKind::Free { .. }, _) | (_, TypePackKind::Free { .. }) => Ok(()),
            (TypePackKind::Generic(left), TypePackKind::Generic(right)) if left == right => Ok(()),
            (TypePackKind::List { types, tail: None }, TypePackKind::Generic(_))
                if types.is_empty() && path.ends_in_function_arguments() =>
            {
                Err(SubtypeError::pack_error(
                    SubtypeErrorKind::ArityMismatch,
                    path,
                    sub,
                    sup,
                ))
            }
            (TypePackKind::Generic(_), TypePackKind::List { types, tail: None })
                if types.is_empty() && path_ends_in_function_returns(&path) =>
            {
                Ok(())
            }
            (TypePackKind::Generic(_), TypePackKind::List { types, tail: None })
                if types.is_empty() && path.ends_in_function_arguments() =>
            {
                Ok(())
            }
            (TypePackKind::Generic(_), TypePackKind::List { types, .. })
                if !types.is_empty()
                    && path_ends_in_function_returns(&path)
                    && !types
                        .iter()
                        .all(|ty| self.type_is_return_inference_placeholder(*ty)) =>
            {
                Err(SubtypeError::pack_error(
                    SubtypeErrorKind::ArityMismatch,
                    path,
                    sub,
                    sup,
                ))
            }
            (_, TypePackKind::Generic(_)) if path_ends_in_function_returns(&path) => Ok(()),
            (TypePackKind::Generic(_), TypePackKind::Generic(_)) => Err(SubtypeError::pack_error(
                SubtypeErrorKind::Mismatch,
                path,
                sub,
                sup,
            )),
            (TypePackKind::Generic(_), _) | (_, TypePackKind::Generic(_)) => Ok(()),
            (TypePackKind::Bound(_), _) | (_, TypePackKind::Bound(_)) => {
                unreachable!("follow_pack removes bound packs")
            }
            (TypePackKind::Variadic { ty: sub }, TypePackKind::Variadic { ty: sup }) => self
                .subtype_type(
                    sub,
                    sup,
                    path.push(TypePathComponent::TypeField(TypeField::Variadic)),
                ),
            (
                TypePackKind::List {
                    types: sub_types,
                    tail: sub_tail,
                },
                TypePackKind::List {
                    types: sup_types,
                    tail: sup_tail,
                },
            ) => self.subtype_list_pack(
                self.arena
                    .flatten_list_pack_from_parts(sub, sub_types, sub_tail),
                &self
                    .arena
                    .flatten_list_pack_from_parts(sup, sup_types, sup_tail),
                path,
            ),
            // A finite list pack is a subtype of a variadic pack when
            // every list element (and the list's own tail, if any) is
            // a subtype of the variadic's element type. This is what
            // makes `(number, string) <: (...any)` accept the call.
            (
                TypePackKind::List {
                    types: sub_types,
                    tail: sub_tail,
                },
                TypePackKind::Variadic { ty: sup_element },
            ) => {
                for (index, sub_ty) in sub_types.into_iter().enumerate() {
                    self.subtype_type(
                        sub_ty,
                        sup_element,
                        path.push(TypePathComponent::Index { index }),
                    )?;
                }
                if let Some(sub_tail) = sub_tail {
                    self.subtype_pack(
                        sub_tail,
                        sup,
                        path.push(TypePathComponent::PackField(PackField::Tail)),
                    )?;
                }
                Ok(())
            }
            // A variadic pack is a subtype of a finite list pack only
            // when the list is empty *and* has no tail — otherwise the
            // variadic might produce no values where the list expects
            // some. Empty list with no tail accepts the variadic in a
            // discarded-result position.
            (
                TypePackKind::Variadic { .. },
                TypePackKind::List {
                    types: sup_types,
                    tail: sup_tail,
                },
            ) if sup_types.is_empty() && sup_tail.is_none() => Ok(()),
            _ => Err(SubtypeError::pack_error(
                SubtypeErrorKind::Mismatch,
                path,
                sub,
                sup,
            )),
        }
    }

    fn union_covers_boolean_singletons(&self, options: &[TypeId]) -> bool {
        let mut has_true = false;
        let mut has_false = false;
        for option in options {
            let option = self.arena.follow(*option);
            let kind = self.arena.get(option);
            if definitely_uninhabited_type(self.arena, option) {
                continue;
            }
            match kind {
                TypeKind::Primitive(crate::types::PrimitiveType::Boolean) => return true,
                TypeKind::Singleton(crate::types::SingletonType::Boolean(value)) => {
                    if *value {
                        has_true = true;
                    } else {
                        has_false = true;
                    }
                }
                _ => {}
            }
        }
        has_true && has_false
    }

    fn subtype_list_pack(
        &mut self,
        sub: FlattenedListPack,
        sup: &FlattenedListPack,
        path: TypePath,
    ) -> Result<(), SubtypeError> {
        self.subtype_list_pack_with_index_offset(sub, sup, path, 0)
    }

    fn subtype_list_pack_with_index_offset(
        &mut self,
        sub: FlattenedListPack,
        sup: &FlattenedListPack,
        path: TypePath,
        index_offset: usize,
    ) -> Result<(), SubtypeError> {
        let common_len = sub.types.len().min(sup.types.len());
        for index in 0..common_len {
            self.subtype_type(
                sub.types[index],
                sup.types[index],
                path.push(TypePathComponent::Index {
                    index: index + index_offset,
                }),
            )?;
        }

        match sub.types.len().cmp(&sup.types.len()) {
            std::cmp::Ordering::Equal => match (sub.tail, sup.tail) {
                (Some(sub_tail), Some(sup_tail)) => self.subtype_pack(
                    sub_tail,
                    sup_tail,
                    path.push(TypePathComponent::PackField(PackField::Tail)),
                ),
                (None, Some(sup_tail)) => {
                    if self.empty_pack_satisfies_tail(sup_tail) {
                        Ok(())
                    } else {
                        Err(SubtypeError::pack_error(
                            SubtypeErrorKind::ArityMismatch,
                            path,
                            sub.id,
                            sup.id,
                        ))
                    }
                }
                (Some(sub_tail), None) => {
                    // A generic-pack or free-pack tail can be instantiated
                    // to the empty pack — accept it as a subtype of an
                    // arity-matched list with no tail. Error tails are
                    // soft.
                    let sub_tail_kind = self.arena.get_pack(sub_tail).clone();
                    let argument_tail_can_be_ignored =
                        if let TypePackKind::Variadic { ty } = sub_tail_kind {
                            self.arena.follow(ty) == self.arena.primitives().any
                                && path.ends_in_function_arguments()
                        } else {
                            false
                        };
                    if matches!(
                        sub_tail_kind,
                        TypePackKind::Free { .. } | TypePackKind::Error
                    ) || matches!(sub_tail_kind, TypePackKind::Generic(_))
                        || argument_tail_can_be_ignored
                    {
                        Ok(())
                    } else {
                        Err(SubtypeError::pack_error(
                            SubtypeErrorKind::ArityMismatch,
                            path,
                            sub.id,
                            sup.id,
                        ))
                    }
                }
                (None, None) => Ok(()),
            },
            std::cmp::Ordering::Greater => {
                let Some(sup_tail) = sup.tail else {
                    // Function arg packs (contravariant) tolerate this case:
                    // the annotated function expects more args than the
                    // candidate accepts, and Lua silently discards the
                    // extras at call time. Lua's `mycb: (number, number) ->
                    // () = function() end` style.
                    if path.ends_in_function_arguments() {
                        return Ok(());
                    }
                    return Err(SubtypeError::pack_error(
                        SubtypeErrorKind::ArityMismatch,
                        path,
                        sub.id,
                        sup.id,
                    ));
                };
                let sup_tail_kind = self.arena.get_pack(sup_tail).clone();
                let sup_tail_ty = match sup_tail_kind {
                    TypePackKind::Variadic { ty } => ty,
                    // A generic-pack or free-pack tail on the sup side
                    // can absorb the extra sub types — it can be
                    // instantiated to whatever shape they require.
                    TypePackKind::Generic(_) if sub.tail.is_some() => {
                        return Err(SubtypeError::pack_error(
                            SubtypeErrorKind::Mismatch,
                            path,
                            sub.id,
                            sup.id,
                        ));
                    }
                    TypePackKind::Generic(_)
                        if common_len == 0
                            && path.ends_in_function_arguments()
                            && self.instantiable_pack_frame_index(sup_tail).is_some() =>
                    {
                        return self.subtype_pack(
                            sub.id,
                            sup_tail,
                            path.push(TypePathComponent::PackField(PackField::Tail)),
                        );
                    }
                    TypePackKind::Generic(_)
                        if sub.types[common_len..].iter().any(|ty| {
                            pack_entry_accepts_nil(self.arena, *ty, &mut BTreeSet::new())
                        }) =>
                    {
                        return Err(SubtypeError::pack_error(
                            SubtypeErrorKind::ArityMismatch,
                            path,
                            sub.id,
                            sup.id,
                        ));
                    }
                    TypePackKind::Generic(_) | TypePackKind::Free { .. } | TypePackKind::Error => {
                        return Ok(());
                    }
                    _ => {
                        return Err(SubtypeError::pack_error(
                            SubtypeErrorKind::ArityMismatch,
                            path,
                            sub.id,
                            sup.id,
                        ));
                    }
                };
                for (index, sub_ty) in sub.types.into_iter().enumerate().skip(common_len) {
                    self.subtype_type(
                        sub_ty,
                        sup_tail_ty,
                        path.push(TypePathComponent::Index {
                            index: index + index_offset,
                        }),
                    )?;
                }
                if let Some(sub_tail) = sub.tail {
                    self.subtype_pack(
                        sub_tail,
                        sup_tail,
                        path.push(TypePathComponent::PackField(PackField::Tail)),
                    )?;
                }
                Ok(())
            }
            std::cmp::Ordering::Less => {
                let Some(sub_tail) = sub.tail else {
                    // The candidate (`sup`) declares more parameters than the
                    // annotated type (`sub`). For function arguments that is
                    // sound when every extra candidate parameter accepts `nil`:
                    // an annotated-arity caller leaves them unset, so they read
                    // as `nil` at the call (Luau's droppable optional trailing
                    // parameters). This mirrors the `Greater`-branch tolerance
                    // for the opposite arity gap. A trailing candidate vararg or
                    // otherwise-empty tail is likewise satisfied by zero values.
                    if path.ends_in_function_arguments()
                        && sup.types[common_len..]
                            .iter()
                            .all(|ty| pack_entry_accepts_nil(self.arena, *ty, &mut BTreeSet::new()))
                        && sup
                            .tail
                            .is_none_or(|tail| self.empty_pack_satisfies_tail(tail))
                    {
                        return Ok(());
                    }
                    return Err(SubtypeError::pack_error(
                        SubtypeErrorKind::ArityMismatch,
                        path,
                        sub.id,
                        sup.id,
                    ));
                };
                let sub_tail_kind = self.arena.get_pack(sub_tail).clone();
                match sub_tail_kind {
                    TypePackKind::Generic(_) | TypePackKind::Free { .. } | TypePackKind::Error => {}
                    TypePackKind::Variadic { .. }
                    | TypePackKind::List { .. }
                    | TypePackKind::Bound(_) => {
                        return Err(SubtypeError::pack_error(
                            SubtypeErrorKind::ArityMismatch,
                            path,
                            sub.id,
                            sup.id,
                        ));
                    }
                }
                if let Some(sup_tail) = sup.tail {
                    self.subtype_pack(
                        sub_tail,
                        sup_tail,
                        path.push(TypePathComponent::PackField(PackField::Tail)),
                    )?;
                }
                Ok(())
            }
        }
    }

    fn empty_pack_satisfies_tail(&self, tail: TypePackId) -> bool {
        let tail = self.arena.follow_pack(tail);
        match self.arena.get_pack(tail) {
            TypePackKind::Variadic { .. } => true,
            TypePackKind::Generic(_) => self.instantiable_pack_frame_index(tail).is_some(),
            TypePackKind::Free { .. } | TypePackKind::Error => true,
            TypePackKind::List { types, tail } => {
                types.is_empty() && tail.is_none_or(|tail| self.empty_pack_satisfies_tail(tail))
            }
            TypePackKind::Bound(_) => unreachable!("follow_pack removes bound packs"),
        }
    }

    fn type_is_return_inference_placeholder(&self, ty: TypeId) -> bool {
        let ty = self.arena.follow(ty);
        matches!(
            self.arena.get(ty),
            TypeKind::Free(_) | TypeKind::Blocked(_) | TypeKind::Error
        )
    }
}

fn plain_function_pair_can_probe_return_diagnostic(sub: &FunctionType, sup: &FunctionType) -> bool {
    sub.generics.is_empty()
        && sub.generic_packs.is_empty()
        && sup.generics.is_empty()
        && sup.generic_packs.is_empty()
}

fn combine_table_intersection_for_subtyping(
    arena: &mut Arena,
    left: TableType,
    right: TableType,
) -> Option<TableType> {
    if left.state != right.state
        || right.indexer.is_some()
        || left.instantiated_type_params != right.instantiated_type_params
    {
        return None;
    }

    let mut combined = left;
    for (name, right_property) in right.properties {
        let Some(left_property) = combined.properties.get(&name).cloned() else {
            combined.properties.insert(name, right_property);
            continue;
        };
        combined.properties.insert(
            name,
            combine_table_property_intersection_for_subtyping(
                arena,
                &left_property,
                &right_property,
            )?,
        );
    }
    Some(combined)
}

fn combine_table_property_intersection_for_subtyping(
    arena: &mut Arena,
    left: &TableProperty,
    right: &TableProperty,
) -> Option<TableProperty> {
    if left.deprecated != right.deprecated {
        return None;
    }

    let ty = if arena.follow(left.ty) == arena.follow(right.ty) {
        arena.follow(left.ty)
    } else {
        let intersection = arena.alloc(TypeKind::Intersection(vec![left.ty, right.ty]));
        simplify_type(arena, intersection)
    };
    let (read_only, write_only) =
        if left.read_only == right.read_only && left.write_only == right.write_only {
            (left.read_only, left.write_only)
        } else if ty == arena.follow(left.ty) && ty == arena.follow(right.ty) {
            intersect_property_capabilities_for_subtyping(left, right)
        } else {
            return None;
        };

    Some(TableProperty {
        ty,
        write_ty: None,
        location: left.location.or(right.location),
        documentation_symbol: left
            .documentation_symbol
            .clone()
            .or_else(|| right.documentation_symbol.clone()),
        read_only,
        write_only,
        deprecated: left.deprecated,
    })
}

fn intersect_property_capabilities_for_subtyping(
    left: &TableProperty,
    right: &TableProperty,
) -> (bool, bool) {
    let can_read = !left.write_only || !right.write_only;
    let can_write = !left.read_only || !right.read_only;
    (can_read && !can_write, can_write && !can_read)
}

fn path_ends_in_function_returns(path: &TypePath) -> bool {
    matches!(
        path.components().last(),
        Some(TypePathComponent::PackField(PackField::Returns))
    )
}

pub fn definitely_uninhabited_type(arena: &Arena, ty: TypeId) -> bool {
    definitely_uninhabited_type_with(arena, ty, &mut BTreeSet::new())
}

fn pack_entry_accepts_nil(arena: &Arena, ty: TypeId, seen: &mut BTreeSet<TypeId>) -> bool {
    let ty = arena.follow(ty);
    if !seen.insert(ty) {
        return false;
    }
    match arena.get(ty) {
        TypeKind::Primitive(crate::types::PrimitiveType::Nil)
        | TypeKind::Any
        | TypeKind::Unknown
        | TypeKind::Error
        | TypeKind::Blocked(_)
        | TypeKind::Free(_) => true,
        TypeKind::Union(options) => options
            .iter()
            .any(|option| pack_entry_accepts_nil(arena, *option, seen)),
        TypeKind::Intersection(options) => options
            .iter()
            .all(|option| pack_entry_accepts_nil(arena, *option, seen)),
        TypeKind::Negation(inner) => !matches!(
            arena.get(arena.follow(*inner)),
            TypeKind::Primitive(crate::types::PrimitiveType::Nil)
        ),
        TypeKind::Bound(_) => unreachable!("follow removes bound types"),
        TypeKind::Primitive(_)
        | TypeKind::Singleton(_)
        | TypeKind::Function(_)
        | TypeKind::Table(_)
        | TypeKind::Metatable { .. }
        | TypeKind::Extern { .. }
        | TypeKind::Generic(_)
        | TypeKind::Never
        | TypeKind::TypeFunctionInstance { .. } => false,
    }
}

#[derive(Default)]
struct CallableTableShape {
    has_function: bool,
    has_table_like: bool,
}

impl CallableTableShape {
    fn absorb(&mut self, other: &Self) {
        self.has_function |= other.has_function;
        self.has_table_like |= other.has_table_like;
    }

    fn has_both(&self) -> bool {
        self.has_function && self.has_table_like
    }
}

fn intersection_contains_function_and_table_like(
    subtyper: &Subtyper<'_>,
    options: &[TypeId],
) -> bool {
    let mut shape = CallableTableShape::default();
    let mut seen = BTreeSet::new();
    for option in options {
        shape.absorb(&callable_table_shape(subtyper, *option, &mut seen));
        if shape.has_both() {
            return true;
        }
    }
    false
}

fn callable_table_shape(
    subtyper: &Subtyper<'_>,
    ty: TypeId,
    seen: &mut BTreeSet<TypeId>,
) -> CallableTableShape {
    let arena = subtyper.arena;
    let ty = arena.follow(ty);
    if !seen.insert(ty) {
        return CallableTableShape::default();
    }
    let shape = match arena.get(ty) {
        TypeKind::Function(_) => CallableTableShape {
            has_function: true,
            has_table_like: false,
        },
        TypeKind::Table(_) | TypeKind::Metatable { .. } => CallableTableShape {
            has_function: false,
            has_table_like: true,
        },
        TypeKind::Union(options) | TypeKind::Intersection(options) => {
            let mut shape = CallableTableShape::default();
            for option in options {
                shape.absorb(&callable_table_shape(subtyper, *option, seen));
                if shape.has_both() {
                    break;
                }
            }
            shape
        }
        TypeKind::TypeFunctionInstance { name, arguments } => {
            type_function_callable_table_shape(subtyper, name, arguments, seen)
        }
        _ => CallableTableShape::default(),
    };
    seen.remove(&ty);
    shape
}

fn type_function_callable_table_shape(
    subtyper: &Subtyper<'_>,
    name: &str,
    arguments: &[TypeId],
    seen: &mut BTreeSet<TypeId>,
) -> CallableTableShape {
    if let Some(reduced) = subtyper.reduce_type_function(name, arguments) {
        return callable_table_shape(subtyper, reduced, seen);
    }
    if let Some((table, metatable)) = setmetatable_type_function_arguments(name, arguments) {
        return setmetatable_callable_table_shape(subtyper, table, metatable);
    }
    if let Some(shape) = keyof_callable_table_shape(subtyper, name, arguments, seen) {
        return shape;
    }
    CallableTableShape {
        has_function: true,
        has_table_like: true,
    }
}

fn setmetatable_callable_table_shape(
    subtyper: &Subtyper<'_>,
    table: TypeId,
    metatable: TypeId,
) -> CallableTableShape {
    let mut active = Vec::new();
    let table = subtyper.resolve_type_function_operand(table, &mut active);
    let metatable = subtyper.resolve_type_function_operand(metatable, &mut active);
    let table = subtyper.arena.follow(table);
    let metatable = subtyper.arena.follow(metatable);

    if matches!(
        subtyper.arena.get(table),
        TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Function(_)
            | TypeKind::Unknown
            | TypeKind::Never
    ) || matches!(subtyper.arena.get(metatable), TypeKind::Never)
    {
        return CallableTableShape::default();
    }

    if matches!(
        subtyper.arena.get(table),
        TypeKind::Table(_) | TypeKind::Metatable { .. }
    ) && matches!(
        subtyper.arena.get(metatable),
        TypeKind::Table(_)
            | TypeKind::Metatable { .. }
            | TypeKind::Any
            | TypeKind::Unknown
            | TypeKind::Error
    ) {
        return CallableTableShape {
            has_function: false,
            has_table_like: true,
        };
    }

    CallableTableShape {
        has_function: true,
        has_table_like: true,
    }
}

fn keyof_callable_table_shape(
    subtyper: &Subtyper<'_>,
    name: &str,
    arguments: &[TypeId],
    seen: &mut BTreeSet<TypeId>,
) -> Option<CallableTableShape> {
    let target = subtyper.keyof_target(name, arguments)?;
    let target = subtyper.arena.follow(target);
    match subtyper.arena.get(target) {
        TypeKind::Table(table) => {
            let mut shape = CallableTableShape::default();
            if let Some(indexer) = &table.indexer {
                shape.absorb(&callable_table_shape(subtyper, indexer.key, seen));
            }
            Some(shape)
        }
        TypeKind::Never => Some(CallableTableShape::default()),
        TypeKind::Bound(_) => unreachable!("follow removes bound types"),
        _ => None,
    }
}

fn definitely_uninhabited_type_with(
    arena: &Arena,
    ty: TypeId,
    seen: &mut BTreeSet<TypeId>,
) -> bool {
    if !seen.insert(ty) {
        return false;
    }
    let result = definitely_uninhabited_with(arena, arena.get(ty), seen);
    seen.remove(&ty);
    result
}

fn definitely_uninhabited_with(
    arena: &Arena,
    kind: &TypeKind,
    seen: &mut BTreeSet<TypeId>,
) -> bool {
    match kind {
        TypeKind::Never => true,
        TypeKind::Table(table) => table
            .properties
            .values()
            .any(|property| definitely_uninhabited_type_with(arena, property.ty, seen)),
        TypeKind::Bound(bound) => definitely_uninhabited_type_with(arena, *bound, seen),
        TypeKind::Intersection(options) => {
            incompatible_intersection_primitives(arena, options)
                || incompatible_intersection_singletons(arena, options)
                || incompatible_intersection_negations(arena, options)
                || options
                    .iter()
                    .any(|option| definitely_uninhabited_type_with(arena, *option, seen))
        }
        _ => false,
    }
}

fn incompatible_intersection_primitives(arena: &Arena, options: &[TypeId]) -> bool {
    let mut primitive = None;
    for option in options {
        match arena.get(*option) {
            TypeKind::Primitive(current) => match primitive {
                Some(prior) if prior != *current => return true,
                Some(_) => {}
                None => primitive = Some(*current),
            },
            TypeKind::Singleton(singleton) => {
                let current = singleton.primitive();
                match primitive {
                    Some(prior) if prior != current => return true,
                    Some(_) => {}
                    None => primitive = Some(current),
                }
            }
            _ => {}
        }
    }
    false
}

fn incompatible_intersection_singletons(arena: &Arena, options: &[TypeId]) -> bool {
    let mut seen = Vec::<crate::types::SingletonType>::new();
    for option in options {
        let TypeKind::Singleton(singleton) = arena.get(*option) else {
            continue;
        };
        if seen
            .iter()
            .any(|prior| prior.primitive() == singleton.primitive() && prior != singleton)
        {
            return true;
        }
        seen.push(singleton.clone());
    }
    false
}

fn incompatible_intersection_negations(arena: &Arena, options: &[TypeId]) -> bool {
    for option in options {
        let TypeKind::Negation(target) = arena.get(arena.follow(*option)) else {
            continue;
        };
        if options.iter().any(|candidate| {
            candidate != option && type_definitely_within(arena, *candidate, *target)
        }) {
            return true;
        }
    }
    false
}

fn type_definitely_within(arena: &Arena, sub: TypeId, sup: TypeId) -> bool {
    let sub = arena.follow(sub);
    let sup = arena.follow(sup);
    if sub == sup {
        return true;
    }
    match (arena.get(sub), arena.get(sup)) {
        (TypeKind::Never, _) => true,
        (TypeKind::Bound(bound), _) => type_definitely_within(arena, *bound, sup),
        (_, TypeKind::Bound(bound)) => type_definitely_within(arena, sub, *bound),
        (TypeKind::Primitive(left), TypeKind::Primitive(right)) => left == right,
        (TypeKind::Singleton(left), TypeKind::Primitive(right)) => left.primitive() == *right,
        (TypeKind::Singleton(left), TypeKind::Singleton(right)) => left == right,
        (TypeKind::Union(options), _) => options
            .iter()
            .all(|option| type_definitely_within(arena, *option, sup)),
        (TypeKind::Intersection(options), _) => options
            .iter()
            .any(|option| type_definitely_within(arena, *option, sup)),
        _ => false,
    }
}

#[cfg(any())]
mod tests;
