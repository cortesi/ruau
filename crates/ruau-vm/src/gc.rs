//! Stop-the-world mark-sweep collector.
//!
//! Reachability is computed with the tri-color `Color` already inline on each
//! arena entry and an explicit `Vec<GcRef>` work-queue — never Rust recursion, so a
//! deep object graph (e.g. `gc.luau`'s long chains) cannot overflow the native
//! stack. Each object type enumerates its own outgoing handles through a `gc_trace`
//! method, keeping its fields private. The work-list grows fallibly (`try_reserve`):
//! an allocation failure under memory pressure aborts the cycle — colors reset,
//! nothing swept — rather than aborting the process (a service-survival invariant).
//!
//! It reclaims arena slots (bumping their generation so existing handles go stale),
//! collects cycles, and releases each freed object's metered byte footprint back to
//! the heap's `MemoryMeter` so the memory cap drops on reclamation. The string
//! interner is weak: a swept string drops its interner entry, so an unreachable
//! interned string is collected rather than pinned for the VM's lifetime.

use crate::{
    api::RawValue,
    heap::{Age, Color, Heap},
    state::Thread,
    table::LuaTable,
};

/// A live heap handle tagged by its arena, for the mark work-queue. Carries only
/// the slot index — marking and tracing operate on the current occupant of a slot,
/// which is sound: the occupant is itself a live object.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Deserialize, serde::Serialize)]
pub enum GcRef {
    Str(u32),
    Table(u32),
    Closure(u32),
    Userdata(u32),
    Thread(u32),
    Buffer(u32),
    Proto(u32),
    UpVal(u32),
}

impl GcRef {
    /// The GC handle a value carries, if any — the six handle-bearing `RawValue`
    /// variants. The scalar variants (nil/bool/number/integer/vector/lightuserdata)
    /// are leaves.
    pub(crate) fn from_value(value: RawValue) -> Option<Self> {
        Some(match value {
            RawValue::String(h) => Self::Str(h.index()),
            RawValue::Table(h) => Self::Table(h.index()),
            RawValue::Function(h) => Self::Closure(h.index()),
            RawValue::Userdata(h) => Self::Userdata(h.index()),
            RawValue::Thread(h) => Self::Thread(h.index()),
            RawValue::Buffer(h) => Self::Buffer(h.index()),
            _ => return None,
        })
    }

    /// Like [`from_value`](Self::from_value) but also returns the handle's generation, for
    /// the generation-aware validator. The collector discards the generation (marking is by
    /// slot); the validator checks it to catch a stale handle to a *reused* slot.
    pub(crate) fn from_value_gen(value: RawValue) -> Option<(Self, u32)> {
        Some(match value {
            RawValue::String(h) => (Self::Str(h.index()), h.generation()),
            RawValue::Table(h) => (Self::Table(h.index()), h.generation()),
            RawValue::Function(h) => (Self::Closure(h.index()), h.generation()),
            RawValue::Userdata(h) => (Self::Userdata(h.index()), h.generation()),
            RawValue::Thread(h) => (Self::Thread(h.index()), h.generation()),
            RawValue::Buffer(h) => (Self::Buffer(h.index()), h.generation()),
            _ => return None,
        })
    }
}

/// Receives each outgoing handle of an object as its [`GcRef`] (arena + slot) plus the
/// generation the holder recorded. One enumeration (`gc_trace`) drives two consumers: the
/// collector marks by slot and ignores the generation; the validator checks both. Unifying
/// them here keeps the validator from drifting out of sync with what the collector traces.
pub trait GcVisit {
    fn visit(&mut self, child: GcRef, generation: u32) -> Result<(), GcAbort>;
}

/// The collector's visitor: enqueue each child by slot (the generation is irrelevant to
/// marking — the slot's current occupant is the live object).
struct MarkVisitor<'a>(&'a mut Vec<GcRef>);

impl GcVisit for MarkVisitor<'_> {
    fn visit(&mut self, child: GcRef, _generation: u32) -> Result<(), GcAbort> {
        try_push(self.0, child)
    }
}

/// The current mark color of `gcref`'s slot.
fn color(heap: &Heap, gcref: GcRef) -> Color {
    let o = &heap.objects;
    match gcref {
        GcRef::Str(i) => o.strings.gc_color(i),
        GcRef::Table(i) => o.tables.gc_color(i),
        GcRef::Closure(i) => o.closures.gc_color(i),
        GcRef::Userdata(i) => o.userdata.gc_color(i),
        GcRef::Thread(i) => o.threads.gc_color(i),
        GcRef::Buffer(i) => o.buffers.gc_color(i),
        GcRef::Proto(i) => o.protos.gc_color(i),
        GcRef::UpVal(i) => o.upvals.gc_color(i),
    }
}

/// Set `gcref`'s slot color.
fn set_color(heap: &mut Heap, gcref: GcRef, c: Color) {
    let o = &mut heap.objects;
    match gcref {
        GcRef::Str(i) => o.strings.gc_set_color(i, c),
        GcRef::Table(i) => o.tables.gc_set_color(i, c),
        GcRef::Closure(i) => o.closures.gc_set_color(i, c),
        GcRef::Userdata(i) => o.userdata.gc_set_color(i, c),
        GcRef::Thread(i) => o.threads.gc_set_color(i, c),
        GcRef::Buffer(i) => o.buffers.gc_set_color(i, c),
        GcRef::Proto(i) => o.protos.gc_set_color(i, c),
        GcRef::UpVal(i) => o.upvals.gc_set_color(i, c),
    }
}

/// The generational age of `gcref`'s slot.
fn age(heap: &Heap, gcref: GcRef) -> Age {
    let o = &heap.objects;
    match gcref {
        GcRef::Str(i) => o.strings.gc_age(i),
        GcRef::Table(i) => o.tables.gc_age(i),
        GcRef::Closure(i) => o.closures.gc_age(i),
        GcRef::Userdata(i) => o.userdata.gc_age(i),
        GcRef::Thread(i) => o.threads.gc_age(i),
        GcRef::Buffer(i) => o.buffers.gc_age(i),
        GcRef::Proto(i) => o.protos.gc_age(i),
        GcRef::UpVal(i) => o.upvals.gc_age(i),
    }
}

/// Set `gcref`'s slot age.
fn set_age(heap: &mut Heap, gcref: GcRef, a: Age) {
    let o = &mut heap.objects;
    match gcref {
        GcRef::Str(i) => o.strings.gc_set_age(i, a),
        GcRef::Table(i) => o.tables.gc_set_age(i, a),
        GcRef::Closure(i) => o.closures.gc_set_age(i, a),
        GcRef::Userdata(i) => o.userdata.gc_set_age(i, a),
        GcRef::Thread(i) => o.threads.gc_set_age(i, a),
        GcRef::Buffer(i) => o.buffers.gc_set_age(i, a),
        GcRef::Proto(i) => o.protos.gc_set_age(i, a),
        GcRef::UpVal(i) => o.upvals.gc_set_age(i, a),
    }
}

/// Generational write barrier: call whenever the pre-existing object `holder` is mutated
/// in a way that could store a reference to a younger object. If `holder` is `Old`, it is
/// recorded in the remembered set so the next minor collection traces it as a root and
/// reaches any young object it now points at. A `Young` (or already-`OldRemembered`) holder
/// needs nothing.
///
/// This is the *conservative* form — it remembers an old holder on any in-place mutation
/// without inspecting the stored value, so it can be placed at a single mutation choke
/// point ([`Heap::table_mut`](crate::heap::Heap::table_mut), upvalue close) and cannot miss
/// a store. The cost of remembering an old holder that gained no young reference is a little
/// extra tracing in the next minor; it is bounded (an old holder is remembered at most once
/// per minor, via the `OldRemembered` age). Idempotent.
///
/// Soundness: every mutation of a pre-existing object that can age `Old` and gain a fresh
/// reference — a table (value/key/metatable) and a closed upvalue cell — must call this. The
/// hot register-stack write path is exempt because every resident thread is a minor root and
/// is re-traced wholesale (see [`collect_minor_inner`]).
pub fn remember(heap: &mut Heap, holder: GcRef) {
    if age(heap, holder) != Age::Old {
        return;
    }
    match heap.gc_remembered.try_reserve(1) {
        Ok(()) => {
            heap.gc_remembered.push(holder);
            set_age(heap, holder, Age::OldRemembered);
        }
        // Could not record the mutation under memory pressure: force the next collection to
        // be a major, which re-traces everything and needs no remembered set. The holder
        // stays `Old`, so a later successful barrier can still record it.
        Err(_) => heap.gc_force_major = true,
    }
}

/// Signals that a GC-internal work-list allocation failed under memory pressure. The
/// collector unwinds on this rather than letting an infallible `Vec` growth abort the
/// process — a service-survival violation. The cycle is abandoned with
/// nothing swept and all colors reset to white.
pub struct GcAbort;

/// Fallibly append `gcref` to a GC work buffer: reserve first (so an out-of-memory
/// growth surfaces as [`GcAbort`] instead of aborting), then push. Used for both the
/// gray queue and the per-object children buffer so neither the whole-graph work-list
/// nor a single wide table's fan-out can force an unrecoverable allocation.
pub fn try_push(buf: &mut Vec<GcRef>, gcref: GcRef) -> Result<(), GcAbort> {
    buf.try_reserve(1).map_err(|_| GcAbort)?;
    buf.push(gcref);
    Ok(())
}

/// Mark a white slot gray and enqueue it; a gray/black slot is already scheduled.
/// Reserves the queue slot before mutating color, so an allocation failure leaves the
/// object white (consistent) and aborts the cycle.
fn mark(heap: &mut Heap, gcref: GcRef, queue: &mut Vec<GcRef>) -> Result<(), GcAbort> {
    if color(heap, gcref) == Color::White {
        queue.try_reserve(1).map_err(|_| GcAbort)?;
        set_color(heap, gcref, Color::Gray);
        queue.push(gcref);
    }
    Ok(())
}

/// Append `gcref`'s outgoing handles to `out` (an immutable read of the object, so
/// the caller can then mark the children — possibly in the same arena — without
/// aliasing). Fallible: a wide object's fan-out reserves through [`try_push`].
fn collect_children(heap: &Heap, gcref: GcRef, out: &mut Vec<GcRef>) -> Result<(), GcAbort> {
    let mut v = MarkVisitor(out);
    let o = &heap.objects;
    match gcref {
        GcRef::Table(i) => {
            if let Some(t) = o.tables.gc_value(i) {
                t.gc_trace(&mut v)?;
            }
        }
        GcRef::Closure(i) => {
            if let Some(c) = o.closures.gc_value(i) {
                c.gc_trace(&mut v)?;
            }
        }
        GcRef::Proto(i) => {
            if let Some(p) = o.protos.gc_value(i) {
                p.gc_trace(&mut v)?;
            }
        }
        GcRef::UpVal(i) => {
            if let Some(u) = o.upvals.gc_value(i) {
                u.gc_trace(&mut v)?;
            }
        }
        GcRef::Thread(i) => {
            if let Some(t) = o.threads.gc_value(i) {
                t.gc_trace(&mut v)?;
            }
        }
        // Leaves: strings, buffers, and userdata.
        GcRef::Str(_) | GcRef::Buffer(_) | GcRef::Userdata(_) => {}
    }
    Ok(())
}

/// Reset every live slot in every arena to white — the abort recovery. Leftover
/// gray/black marks from a half-finished cycle would corrupt the next collection (a
/// stale-black reachable object would be skipped by `mark` and its children left
/// untraced, then freed: a use-after-free), so an aborted cycle must scrub all colors.
fn reset_all_colors(heap: &mut Heap) {
    let o = &mut heap.objects;
    o.strings.gc_reset_colors();
    o.tables.gc_reset_colors();
    o.closures.gc_reset_colors();
    o.userdata.gc_reset_colors();
    o.threads.gc_reset_colors();
    o.buffers.gc_reset_colors();
    o.protos.gc_reset_colors();
    o.upvals.gc_reset_colors();
}

/// Runs a full stop-the-world mark-sweep over `heap`. `roots` are the caller-supplied
/// roots — the resident main thread for [`Vm::collect`](crate::Vm::collect), or the
/// taken-out active thread's slot plus its directly-traced children for
/// [`collect_active`] mid-execution; the heap's own roots — the string metatable and the
/// registry pins — are added automatically.
///
/// Returns `Some(reclaimed)` on a completed cycle, or `None` if a work-list allocation
/// failed under memory pressure — in which case the cycle is abandoned with nothing
/// swept and all colors reset to white, so the heap stays consistent and the process
/// survives (the triggering allocation then fails gracefully at its own call site).
///
/// Each freed object's metered byte footprint is released to the heap meter as it is
/// swept, and a swept string drops its (weak) interner entry. A free function rather
/// than a `Heap` method to keep the collector's machinery in this module (one inherent
/// `impl`).
pub fn collect(
    heap: &mut Heap,
    payloads: &crate::host_type::HostPayloadStore,
    roots: &[GcRef],
) -> Option<usize> {
    // A major (full) collection runs when one is forced (a barrier could not record an
    // edge) or periodically (to reclaim old garbage and unreachable coroutines a minor
    // keeps alive); otherwise a cheap minor collects only the young generation.
    let major = heap.gc_should_major();
    let result = if major {
        collect_major_inner(heap, payloads, roots)
    } else {
        collect_minor_inner(heap, payloads, roots)
    };
    match result {
        Ok(reclaimed) => {
            // A completed cycle re-paces the next one and advances the minor/major
            // schedule (which also clears the remembered set — a major re-traced
            // everything, a minor promoted every young survivor so its old→young edges
            // became old→old). An aborted cycle (below) reclaimed nothing and must not
            // reset the threshold or touch the remembered set, so the retry stays sound.
            if major {
                heap.gc_note_major();
            } else {
                heap.gc_note_minor();
            }
            heap.note_collection();
            Some(reclaimed)
        }
        Err(GcAbort) => {
            // The cycle is abandoned with nothing swept; scrub the half-finished marks so
            // the next collection starts clean. Force the retry to be a *major*: an aborted
            // minor may have left `OldRemembered` holders whose recording is incomplete, and
            // only a major (which re-traces everything from real roots and needs no
            // remembered set) is unconditionally sound regardless of generational state. The
            // remembered set itself is preserved (the minor iterates it by index, never
            // taking it), so even the next *minor* would be sound — forcing a major is
            // belt-and-suspenders and also reclaims whatever the aborted cycle could not.
            reset_all_colors(heap);
            heap.gc_force_major = true;
            None
        }
    }
}

/// The full mark-sweep body: trace from every root, sweep all white, and settle every
/// survivor into the old generation. Fallible on work-list growth; on `Err(GcAbort)` the
/// caller ([`collect`]) scrubs colors and sweeps nothing.
fn collect_major_inner(
    heap: &mut Heap,
    payloads: &crate::host_type::HostPayloadStore,
    roots: &[GcRef],
) -> Result<usize, GcAbort> {
    let mut queue: Vec<GcRef> = Vec::new();
    let mut marked: Vec<GcRef> = Vec::new(); // unused for a major (its sweep scans all slots)
    mark_heap_roots(heap, roots, &mut queue)?;
    trace(heap, &mut queue, GcCycle::Full, &mut marked)?;
    sweep(heap, payloads, GcCycle::Full)
}

/// The minor mark-sweep body: trace from the roots plus all resident threads plus the
/// remembered set, but skip every plain `Old` object (presumed live, no young children by
/// the remembered-set invariant); sweep only the young generation, promoting survivors.
/// This is the generational fast path — its cost is bounded by the young set, not the
/// whole live heap.
fn collect_minor_inner(
    heap: &mut Heap,
    payloads: &crate::host_type::HostPayloadStore,
    roots: &[GcRef],
) -> Result<usize, GcAbort> {
    let mut queue: Vec<GcRef> = Vec::new();
    mark_heap_roots(heap, roots, &mut queue)?;
    // Every resident thread is a minor root: its register stack mutates with no barrier,
    // so the minor must re-trace each thread's current stack wholesale to find the young
    // values it now holds. (An unreachable coroutine is thereby kept alive until a major.)
    for index in 0..heap.objects.threads.len() as u32 {
        if heap.objects.threads.gc_value(index).is_some() {
            mark_minor(heap, GcRef::Thread(index), &mut queue)?;
        }
    }
    // The remembered set: old objects a barrier saw gain a young reference. Iterate it *by
    // index* rather than `std::mem::take`-ing it, so an abort (`?`) mid-loop cannot drop the
    // set — leaving its `OldRemembered` holders unrecorded would make a later minor free
    // their live young children (a use-after-free). The set is not mutated during a
    // collection (the barrier only runs during mutation), so indexing is stable; a `GcRef`
    // is `Copy`, so reading one ends the borrow before `mark_minor` takes `&mut heap`.
    let mut i = 0;
    while i < heap.gc_remembered.len() {
        let gcref = heap.gc_remembered[i];
        mark_minor(heap, gcref, &mut queue)?;
        i += 1;
    }
    // Test-only: simulate a work-list allocation failure here to exercise the abort path.
    #[cfg(any())]
    if std::mem::take(&mut heap.gc_test_abort_minor) {
        return Err(GcAbort);
    }
    let mut marked: Vec<GcRef> = Vec::new();
    trace(heap, &mut queue, GcCycle::Minor, &mut marked)?;
    let freed = sweep(heap, payloads, GcCycle::Minor)?;
    // A minor's sweep touched only young slots, so the old slots this minor blackened (the
    // roots, remembered holders, and threads) still carry a `Black` mark; reset just those —
    // not the whole arena — to `White` for the next cycle, and revert any `OldRemembered`
    // holder to `Old` now that the minor promoted whatever young object it pointed at. (On an
    // abort this is skipped, but the abort path scrubs all colors and forces a major.)
    for &gcref in &marked {
        set_color(heap, gcref, Color::White);
        if age(heap, gcref) == Age::OldRemembered {
            set_age(heap, gcref, Age::Old);
        }
    }
    Ok(freed)
}

/// Enqueue the heap's own roots (caller roots, the string/vector metatables, and the
/// registry pins) — shared by minor and major. The minor enqueue (via the trace's child
/// marking) still age-filters; these top-level roots are marked with the age-agnostic
/// [`mark`] because a root is reachable by definition.
fn mark_heap_roots(
    heap: &mut Heap,
    roots: &[GcRef],
    queue: &mut Vec<GcRef>,
) -> Result<(), GcAbort> {
    for &root in roots {
        mark(heap, root, queue)?;
    }
    if let Some(metatable) = heap.string_metatable() {
        mark(heap, GcRef::Table(metatable.index()), queue)?;
    }
    // The pre-interned metamethod names are roots: the interner is weak, so
    // an unrooted cached handle would dangle after its string was swept.
    for name in heap.metamethod_names.into_iter().flatten() {
        mark(heap, GcRef::Str(name.index()), queue)?;
    }
    if let Some(metatable) = heap.vector_metatable() {
        mark(heap, GcRef::Table(metatable.index()), queue)?;
    }
    if let Some(metatable) = heap.structured_error_metatable() {
        mark(heap, GcRef::Table(metatable.index()), queue)?;
    }
    let mut anchors: Vec<GcRef> = Vec::new();
    for value in heap.registry().gc_anchors() {
        if let Some(anchor) = GcRef::from_value(value) {
            try_push(&mut anchors, anchor)?;
        }
    }
    for anchor in anchors {
        mark(heap, anchor, queue)?;
    }
    Ok(())
}

/// Drain the gray queue with no recursion (so a deep graph cannot overflow the native
/// stack), tracing each object's children. A weak table (a `__mode` metatable) traces only
/// its strong component(s) and is recorded for the atomic clear afterward. `minor` selects
/// the age-aware child marking ([`mark_minor`], which skips plain-`Old` children) versus
/// the age-agnostic [`mark`] for a major.
///
/// For a minor, every blackened object is recorded in `marked` so the caller can reset just
/// those slots' colors (and revert `OldRemembered`→`Old`) afterward — a minor must not scan
/// the whole arena, and the objects it blackens are exactly its (bounded) reachable set.
/// Which collection is running: a young-generation minor or a full major.
#[derive(Clone, Copy, Eq, PartialEq)]
enum GcCycle {
    Minor,
    Full,
}

fn trace(
    heap: &mut Heap,
    queue: &mut Vec<GcRef>,
    cycle: GcCycle,
    marked: &mut Vec<GcRef>,
) -> Result<(), GcAbort> {
    let minor = cycle == GcCycle::Minor;
    let mut children: Vec<GcRef> = Vec::new();
    let mut weak_tables: Vec<(u32, bool, bool)> = Vec::new();
    while let Some(gcref) = queue.pop() {
        children.clear();
        let weak = if let GcRef::Table(i) = gcref {
            heap.objects
                .tables
                .gc_value(i)
                .and_then(|t| weak_mode(heap, t).map(|(wk, wv)| (i, wk, wv)))
        } else {
            None
        };
        if let Some((i, weak_keys, weak_values)) = weak {
            if let Some(t) = heap.objects.tables.gc_value(i) {
                t.gc_trace_weak(&mut children, weak_keys, weak_values)?;
            }
            weak_tables.try_reserve(1).map_err(|_| GcAbort)?;
            weak_tables.push((i, weak_keys, weak_values));
        } else {
            collect_children(heap, gcref, &mut children)?;
        }
        for &child in &children {
            if minor {
                mark_minor(heap, child, queue)?;
            } else {
                mark(heap, child, queue)?;
            }
        }
        set_color(heap, gcref, Color::Black);
        if minor {
            try_push(marked, gcref)?;
        }
    }
    clear_weak_tables(heap, &weak_tables, cycle)?;
    Ok(())
}

/// Reserve free lists, sweep, and compact. `minor` selects the young-only sweep (free
/// young white, promote young survivors, leave old resident) versus the full sweep (free
/// all white, promote survivors to old). Reservation makes the sweep's `free.push`
/// non-allocating, upholding the "a GC cycle never aborts the process" guarantee; a failed
/// reservation aborts before anything is swept.
fn sweep(
    heap: &mut Heap,
    payloads: &crate::host_type::HostPayloadStore,
    cycle: GcCycle,
) -> Result<usize, GcAbort> {
    let minor = cycle == GcCycle::Minor;
    let userdata_reclaims = if minor {
        heap.objects.userdata.gc_pending_minor_free_count()
    } else {
        heap.objects.userdata.gc_pending_free_count()
    };
    payloads
        .try_reserve_reclaims(userdata_reclaims)
        .map_err(|_| GcAbort)?;
    if minor {
        heap.objects
            .gc_reserve_free_lists_minor()
            .map_err(|_| GcAbort)?;
    } else {
        heap.objects.gc_reserve_free_lists().map_err(|_| GcAbort)?;
    }
    // Each freed object releases its metered byte footprint to the shared meter so the cap
    // drops on reclamation; upvalues carry no charged footprint of their own. A swept
    // string also drops its (weak) interner entry, so an unreachable interned
    // string is reclaimed rather than pinned for the VM's lifetime.
    let meter = heap.meter();
    let interner = &mut heap.interner;
    let o = &mut heap.objects;
    let freed = if minor {
        o.tables
            .gc_sweep_minor_with(|t| meter.adjust(t.gc_footprint(), 0))
            + o.closures
                .gc_sweep_minor_with(|closure| meter.adjust(closure.gc_footprint(), 0))
            + o.userdata
                .gc_sweep_minor_with(|userdata| payloads.reclaim(userdata.payload_id()))
            + o.threads
                .gc_sweep_minor_with(|t| meter.adjust(t.gc_footprint(), 0))
            + o.buffers
                .gc_sweep_minor_with(|b| meter.adjust(b.gc_footprint(), 0))
            + o.protos
                .gc_sweep_minor_with(|p| meter.adjust(p.footprint(), 0))
            + o.upvals.gc_sweep_minor()
            + o.strings.gc_sweep_minor_with(|s| {
                meter.adjust(s.gc_footprint(), 0);
                interner.remove(s.bytes());
            })
    } else {
        o.tables
            .gc_sweep_with(|t| meter.adjust(t.gc_footprint(), 0))
            + o.closures
                .gc_sweep_with(|closure| meter.adjust(closure.gc_footprint(), 0))
            + o.userdata
                .gc_sweep_with(|userdata| payloads.reclaim(userdata.payload_id()))
            + o.threads
                .gc_sweep_with(|t| meter.adjust(t.gc_footprint(), 0))
            + o.buffers
                .gc_sweep_with(|b| meter.adjust(b.gc_footprint(), 0))
            + o.protos.gc_sweep_with(|p| meter.adjust(p.footprint(), 0))
            + o.upvals.gc_sweep()
            + o.strings.gc_sweep_with(|s| {
                meter.adjust(s.gc_footprint(), 0);
                interner.remove(s.bytes());
            })
    };
    // A major releases each arena's reclaimed trailing capacity now the sweep has freed dead
    // slots, so a heap that spiked and shrank returns the memory. Compaction scans for the
    // last live slot and sorts the free list (both `O(arena)`), so a minor skips it — keeping
    // the minor bounded by the young set — and leaves capacity release to the next major.
    // Compaction never moves a live object, so every live handle stays valid.
    if !minor {
        o.gc_compact_all();
    }
    Ok(freed)
}

/// Minor-collection enqueue: like [`mark`] but skips a plain `Old` object — a minor does
/// not trace through it (its young children, if any, are reachable via the remembered set).
/// A thread is always enqueued regardless of age, because a minor must re-scan every
/// thread's barrier-free register stack.
fn mark_minor(heap: &mut Heap, gcref: GcRef, queue: &mut Vec<GcRef>) -> Result<(), GcAbort> {
    if color(heap, gcref) != Color::White {
        return Ok(()); // already gray/black this cycle
    }
    let trace_it = matches!(gcref, GcRef::Thread(_))
        || matches!(age(heap, gcref), Age::Young | Age::OldRemembered);
    if trace_it {
        queue.try_reserve(1).map_err(|_| GcAbort)?;
        set_color(heap, gcref, Color::Gray);
        queue.push(gcref);
    }
    Ok(())
}

/// Collects while a thread is *taken out of the arena* for execution — its arena slot
/// is an empty placeholder, so the live `active` thread is traced from this reference
/// instead, and its slot (via `active.id`) is kept as a root so handles to the thread
/// stay valid. Used by the dispatch memory safepoint to reclaim under pressure
/// mid-execution.
///
/// **Soundness contract:** sound only when `active` is the *sole* thread taken out of
/// the arena (take-out count one). A running coroutine's resumer chain is then parked
/// arena-resident and reached as a GC root — `active`'s `Thread::resumer` link is
/// traced to its resumer, that resumer's link to the next, up to the main thread. A
/// *second* taken-out thread would be invisible to the trace (its live objects swept
/// — a use-after-free), so the caller must guarantee single-take-out: the root
/// main-thread dispatch and the synchronous coroutine resume (which parks the resumer
/// and sets `resumer`). A count-two context — e.g. an async-driver coroutine resume,
/// where the main thread is still out — must run a non-collecting dispatch mode.
pub fn collect_active(
    heap: &mut Heap,
    payloads: &crate::host_type::HostPayloadStore,
    active: &Thread,
) -> Option<usize> {
    if heap.taken_out_thread_count() != 1 {
        return None;
    }
    // Real callers (the dispatch safepoint) hand in the taken-out main thread, which is
    // always arena-resident, so its `id` is set and the placeholder-slot root below
    // applies. (A thread with no `id` is not in the arena and has no handle to keep
    // valid, so skipping its root is also sound.)
    debug_assert!(
        active.id.is_some(),
        "collect_active expects an arena-resident thread"
    );
    // Root the active thread's own (placeholder) arena slot so handles to it stay valid,
    // then trace its live roots directly (the slot is an empty placeholder). Both
    // reservations are fallible; a failure before marking yields `None` (nothing has
    // been marked, so there is nothing to scrub).
    let mut roots: Vec<GcRef> = Vec::new();
    if let Some(id) = active.id {
        try_push(&mut roots, GcRef::Thread(id.index())).ok()?;
    }
    active.gc_trace(&mut MarkVisitor(&mut roots)).ok()?;
    collect(heap, payloads, &roots)
}

/// The weak mode of `table` from its metatable's `__mode` (`"k"` weak keys, `"v"` weak
/// values, combinations like `"kv"`): `Some((weak_keys, weak_values))` for a weak table,
/// `None` for a strong one. Resolving requires the heap to deref the metatable and the
/// interned `__mode` string; if `__mode` was never interned, no table is weak.
fn weak_mode(heap: &Heap, table: &LuaTable) -> Option<(bool, bool)> {
    let metatable = table.metatable()?;
    let mode_key = heap.interner.lookup(b"__mode")?;
    let mode = heap.table(metatable)?.get(RawValue::String(mode_key));
    let RawValue::String(handle) = mode else {
        return None;
    };
    let bytes = heap.string(handle)?.bytes();
    let weak_keys = bytes.contains(&b'k');
    let weak_values = bytes.contains(&b'v');
    (weak_keys || weak_values).then_some((weak_keys, weak_values))
}

/// Whether a weak table's `value` component is dead — unreached by the mark, so an entry
/// keyed/valued only by it should be cleared. A scalar is never dead.
///
/// In a `minor` collection a plain `Old` component is presumed live even though the mark
/// left it white (a minor never traces the old graph), so only an unreached *young*
/// component is dead; a major treats any white component as dead.
fn is_dead_component(heap: &Heap, value: RawValue, cycle: GcCycle) -> bool {
    // A string is never "dead" for a weak table — Luau treats strings as values that are
    // never weak (lgc.cpp:608-619). `gc_trace_weak` always traces them so a reached string
    // is black anyway; this guard makes the never-clear invariant explicit and total.
    if matches!(value, RawValue::String(_)) {
        return false;
    }
    let Some(gcref) = GcRef::from_value(value) else {
        return false;
    };
    if color(heap, gcref) != Color::White {
        return false; // reached this cycle → live
    }
    cycle == GcCycle::Full || age(heap, gcref) == Age::Young
}

/// The atomic weak-table clear (runs after marking, before sweeping): for each weak
/// table, remove every entry whose declared-weak component (key and/or value) is still
/// white, so the sweep then reclaims it. Two phases per table — an immutable scan that
/// collects the dead keys, then a mutable pass that nils them — because the scan reads
/// arena colors while the clear mutates the table's arena slot.
///
/// Fallible: the dead-key scratch grows through `try_reserve`, so this stays inside the
/// collector's "never abort the process" discipline. Phase 2 only nils existing entries
/// (`set(key, nil)` tombstones or clears an array slot — never grows), so it cannot
/// allocate. On `Err(GcAbort)` the caller abandons the cycle and scrubs colors; any
/// already-nilled entries were provably dead (their weak component was unreached), so a
/// partial clear with no sweep is consistent.
fn clear_weak_tables(
    heap: &mut Heap,
    weak_tables: &[(u32, bool, bool)],
    cycle: GcCycle,
) -> Result<(), GcAbort> {
    let mut dead_keys: Vec<RawValue> = Vec::new();
    for &(index, weak_keys, weak_values) in weak_tables {
        dead_keys.clear();
        // Phase 1 — immutable: find entries whose weak component is dead (unreached).
        let mut abort = false;
        if let Some(table) = heap.objects.tables.gc_value(index) {
            table.for_each_entry(|key, value| {
                if abort {
                    return;
                }
                let entry_dead = (weak_keys && is_dead_component(heap, key, cycle))
                    || (weak_values && is_dead_component(heap, value, cycle));
                if entry_dead {
                    if dead_keys.try_reserve(1).is_err() {
                        abort = true;
                    } else {
                        dead_keys.push(key);
                    }
                }
            });
        }
        if abort {
            return Err(GcAbort);
        }
        // Phase 2 — mutable: nil out the dead entries (the scan's borrows have ended).
        if !dead_keys.is_empty()
            && let Some(table) = heap.objects.tables.gc_value_mut(index)
        {
            for &key in &dead_keys {
                table.set(key, RawValue::Nil);
            }
        }
    }
    Ok(())
}

/// Whether `gcref`'s slot holds the live object the handle's `generation` names — a
/// *generation-checked* resolve through the arena getters (`Arena::get`). Unlike a bare
/// occupancy check it catches a stale handle to a slot already *reused* by a newer object,
/// not only a handle to a still-empty freed slot — which the GC-stress check needs, since
/// it validates after allocations have recycled swept slots.
fn is_live_gen(heap: &Heap, gcref: GcRef, generation: u32) -> bool {
    let o = &heap.objects;
    match gcref {
        GcRef::Str(i) => o.strings.get(i, generation).is_some(),
        GcRef::Table(i) => o.tables.get(i, generation).is_some(),
        GcRef::Closure(i) => o.closures.get(i, generation).is_some(),
        GcRef::Userdata(i) => o.userdata.get(i, generation).is_some(),
        GcRef::Thread(i) => o.threads.get(i, generation).is_some(),
        GcRef::Buffer(i) => o.buffers.get(i, generation).is_some(),
        GcRef::Proto(i) => o.protos.get(i, generation).is_some(),
        GcRef::UpVal(i) => o.upvals.get(i, generation).is_some(),
    }
}

/// The validator's visitor: record the first outgoing handle that fails a
/// generation-checked resolve. Shares the `gc_trace` enumeration with the collector's
/// [`MarkVisitor`], so the validator checks exactly the handles the collector marks — it
/// cannot silently drift out of sync. It never allocates, so it never returns `GcAbort`.
struct ValidateVisitor<'a> {
    heap: &'a Heap,
    parent: GcRef,
    error: Option<String>,
}

impl GcVisit for ValidateVisitor<'_> {
    fn visit(&mut self, child: GcRef, generation: u32) -> Result<(), GcAbort> {
        if self.error.is_none() && !is_live_gen(self.heap, child, generation) {
            self.error = Some(format!(
                "dangling handle {child:?} (generation {generation}) reachable from {:?}",
                self.parent
            ));
        }
        Ok(())
    }
}

/// Checks every outgoing handle of the object at `gcref` resolves (generation-checked).
fn validate_handles_of(heap: &Heap, gcref: GcRef) -> Result<(), String> {
    let mut visitor = ValidateVisitor {
        heap,
        parent: gcref,
        error: None,
    };
    let o = &heap.objects;
    let traced = match gcref {
        GcRef::Table(i) => o.tables.gc_value(i).map(|t| t.gc_trace(&mut visitor)),
        GcRef::Closure(i) => o.closures.gc_value(i).map(|c| c.gc_trace(&mut visitor)),
        GcRef::Proto(i) => o.protos.gc_value(i).map(|p| p.gc_trace(&mut visitor)),
        GcRef::UpVal(i) => o.upvals.gc_value(i).map(|u| u.gc_trace(&mut visitor)),
        GcRef::Thread(i) => o.threads.gc_value(i).map(|t| t.gc_trace(&mut visitor)),
        GcRef::Str(_) | GcRef::Buffer(_) | GcRef::Userdata(_) => None,
    };
    // `ValidateVisitor` never allocates, so this is unreachable today; it is kept because
    // the `GcVisit` trait is shared with the fallible collector, so if the validator ever
    // grows an allocating visitor an OOM there must surface, not be silently swallowed.
    if let Some(Err(GcAbort)) = traced {
        return Err("validate: out of memory enumerating handles".to_string());
    }
    visitor.error.map_or(Ok(()), Err)
}

/// Walks every live object in every arena and verifies that each handle it holds — and
/// each root handle (the string metatable and the registry pins) — resolves to the live
/// object its generation names. A dangling handle means the collector freed a
/// still-referenced object: a use-after-free waiting to happen. Read-only and non-recursive
/// (a per-object work buffer, never the native stack), so it is safe to run after every
/// collection and, being generation-aware, as the GC-stress invariant check (where slots
/// have been recycled between collection and the check). Returns the first violation.
///
/// `Vm::validate` additionally checks the VM-owned `main_thread` root before delegating
/// here; each live thread's own `id` self-handle (which `gc_trace` skips) is checked below.
///
/// # Errors
/// Returns a description of the first dangling handle (or an out-of-memory note).
pub fn validate(heap: &Heap) -> Result<(), String> {
    let o = &heap.objects;
    // The five handle-bearing arenas; strings/buffers/userdata are leaves.
    for i in 0..o.tables.len() as u32 {
        if o.tables.gc_value(i).is_some() {
            validate_handles_of(heap, GcRef::Table(i))?;
        }
    }
    for i in 0..o.closures.len() as u32 {
        if o.closures.gc_value(i).is_some() {
            validate_handles_of(heap, GcRef::Closure(i))?;
        }
    }
    for i in 0..o.protos.len() as u32 {
        if o.protos.gc_value(i).is_some() {
            validate_handles_of(heap, GcRef::Proto(i))?;
        }
    }
    for i in 0..o.upvals.len() as u32 {
        if o.upvals.gc_value(i).is_some() {
            validate_handles_of(heap, GcRef::UpVal(i))?;
        }
    }
    for i in 0..o.threads.len() as u32 {
        if let Some(thread) = o.threads.gc_value(i) {
            validate_handles_of(heap, GcRef::Thread(i))?;
            // The thread's own `id` self-handle is deliberately skipped by `gc_trace`
            // (the collector reaches a thread through its roots), so check it here.
            if let Some(id) = thread.id
                && heap.thread(id).is_none()
            {
                return Err(format!("thread {i} has a dangling self-handle (id)"));
            }
        }
    }
    // Roots must resolve too (generation-checked through the typed getters).
    if let Some(metatable) = heap.string_metatable()
        && heap.table(metatable).is_none()
    {
        return Err("dangling string-metatable root".to_string());
    }
    if let Some(metatable) = heap.vector_metatable()
        && heap.table(metatable).is_none()
    {
        return Err("dangling vector-metatable root".to_string());
    }
    if let Some(metatable) = heap.structured_error_metatable()
        && heap.table(metatable).is_none()
    {
        return Err("dangling structured-error-metatable root".to_string());
    }
    for value in heap.registry().gc_anchors() {
        if let Some((anchor, generation)) = GcRef::from_value_gen(value)
            && !is_live_gen(heap, anchor, generation)
        {
            return Err(format!("dangling registry anchor {anchor:?}"));
        }
    }
    Ok(())
}

#[cfg(any())]
mod tests {
    use crate::{
        api::{HeapId, RawValue},
        gc::{GcRef, collect as collect_raw, collect_active as collect_active_raw, validate},
        heap::Heap,
        state::Thread,
        table::LuaTable,
    };

    /// Records the generational win: a minor collection over a large *retained, old* heap is
    /// bounded by the young set, so it stays ~constant while a full major scales with the
    /// heap. Ignored by default (it is a timing probe, not a correctness gate); run with
    /// `cargo test -p ruau-vm --release gc::tests::minor_collection_is_bounded_by_young_set
    /// -- --ignored --nocapture`. Asserts only the *shape* (a minor is much cheaper than a
    /// major over a big old heap), since absolute timings are hardware-dependent.
    #[test]
    #[ignore = "timing probe — run with --release --ignored --nocapture"]
    fn minor_collection_is_bounded_by_young_set() {
        use std::time::Instant;
        for n in [1_000usize, 10_000, 50_000] {
            let mut h = heap();
            // Realistic shape: a small `holder` (the GC root, like the globals table) points
            // at a large retained structure (`big` + n children) reached as its child — so a
            // minor reaches `big` as a plain-old child and skips it, never touching the n.
            let holder = h.alloc_table(LuaTable::new()).unwrap();
            let big = h.alloc_table(LuaTable::new()).unwrap();
            h.table_mut(holder)
                .unwrap()
                .set(RawValue::Number(1.0), RawValue::Table(big));
            for i in 0..n {
                let t = h.alloc_table(LuaTable::new()).unwrap();
                h.table_mut(big)
                    .unwrap()
                    .set(RawValue::Number(i as f64), RawValue::Table(t));
            }
            let roots = [GcRef::Table(holder.index())];
            h.gc_force_major = true;
            collect_no_userdata(&mut h, &roots); // age the whole structure old
            let reps = 50;
            let t0 = Instant::now();
            for _ in 0..reps {
                collect_no_userdata(&mut h, &roots); // minor: reaches `big` as old, skips the n children
            }
            let minor = t0.elapsed() / reps;
            let t1 = Instant::now();
            for _ in 0..reps {
                h.gc_force_major = true;
                collect_no_userdata(&mut h, &roots); // major: traces the whole old heap
            }
            let major = t1.elapsed() / reps;
            eprintln!(
                "n={n:>6}  minor={minor:>10.3?}  major={major:>10.3?}  major/minor={:.0}x",
                major.as_nanos() as f64 / minor.as_nanos().max(1) as f64
            );
            assert!(
                major > minor * 4,
                "a minor over a {n}-table old heap should be far cheaper than a major \
                 (minor={minor:?}, major={major:?})"
            );
        }
    }

    fn heap() -> Heap {
        Heap::new(HeapId(1), crate::Ambient::deterministic(0).config)
    }

    fn collect_no_userdata(heap: &mut Heap, roots: &[GcRef]) -> Option<usize> {
        let payloads = crate::host_type::HostPayloadStore::new(heap.meter());
        collect_raw(heap, &payloads, roots)
    }

    fn collect_active_no_userdata(heap: &mut Heap, active: &Thread) -> Option<usize> {
        let payloads = crate::host_type::HostPayloadStore::new(heap.meter());
        collect_active_raw(heap, &payloads, active)
    }

    /// A full collection that must complete (no work-list allocation pressure in tests).
    /// Forces a major so these tests assert the full-reclamation guarantee — every
    /// unreachable object is reclaimed — rather than a minor's young-only reclamation
    /// (which leaves old garbage for a later major). The generational minor path has its
    /// own tests below.
    fn collect_ok(heap: &mut Heap, roots: &[GcRef]) -> usize {
        heap.gc_force_major = true;
        collect_no_userdata(heap, roots)
            .expect("collect should not abort under test memory pressure")
    }

    #[test]
    fn sweeps_unrooted_and_keeps_rooted() {
        let mut h = heap();
        let kept = h.alloc_table(LuaTable::new()).unwrap();
        let dropped = h.alloc_table(LuaTable::new()).unwrap();
        let freed = collect_ok(&mut h, &[GcRef::Table(kept.index())]);
        assert_eq!(freed, 1, "the unrooted table is reclaimed");
        assert!(h.table(kept).is_some(), "the rooted table survives");
        assert!(
            h.table(dropped).is_none(),
            "the unrooted handle is now stale"
        );
    }

    #[test]
    fn collects_an_unrooted_cycle() {
        let mut h = heap();
        let a = h.alloc_table(LuaTable::new()).unwrap();
        let b = h.alloc_table(LuaTable::new()).unwrap();
        // a <-> b reference each other but nothing roots them.
        h.table_mut(a)
            .unwrap()
            .set(RawValue::Number(1.0), RawValue::Table(b));
        h.table_mut(b)
            .unwrap()
            .set(RawValue::Number(1.0), RawValue::Table(a));
        let freed = collect_ok(&mut h, &[]);
        assert_eq!(freed, 2, "the cycle is reclaimed (the mark-sweep win)");
        assert!(h.table(a).is_none() && h.table(b).is_none());
    }

    #[test]
    fn rooted_cycle_survives() {
        let mut h = heap();
        let a = h.alloc_table(LuaTable::new()).unwrap();
        let b = h.alloc_table(LuaTable::new()).unwrap();
        h.table_mut(a)
            .unwrap()
            .set(RawValue::Number(1.0), RawValue::Table(b));
        h.table_mut(b)
            .unwrap()
            .set(RawValue::Number(1.0), RawValue::Table(a));
        let freed = collect_ok(&mut h, &[GcRef::Table(a.index())]);
        assert_eq!(freed, 0, "a reachable from the root keeps b alive");
        assert!(h.table(a).is_some() && h.table(b).is_some());
    }

    #[test]
    fn deep_chain_traces_without_stack_overflow() {
        let mut h = heap();
        // A chain of 100_000 tables, each holding the next. Recursion would overflow.
        let head = h.alloc_table(LuaTable::new()).unwrap();
        let mut prev = head;
        for _ in 0..100_000 {
            let next = h.alloc_table(LuaTable::new()).unwrap();
            h.table_mut(prev)
                .unwrap()
                .set(RawValue::Number(1.0), RawValue::Table(next));
            prev = next;
        }
        let freed = collect_ok(&mut h, &[GcRef::Table(head.index())]);
        assert_eq!(freed, 0, "the whole chain is reachable from the head");
        assert!(h.table(head).is_some() && h.table(prev).is_some());
        // Now drop the root: the whole chain is unreachable.
        let freed = collect_ok(&mut h, &[]);
        assert_eq!(freed, 100_001);
    }

    /// A minor reclaims unreachable *young* garbage but not *old* garbage (only a major
    /// does), and the write barrier keeps an old→young edge's target alive across a minor.
    /// The barrier is the soundness linchpin: without it, the minor would skip the old
    /// holder, never reach the young target, and free a live object.
    #[test]
    fn generational_minor_reclaims_young_keeps_barriered_and_old_survives() {
        let mut heap = heap();
        // `root -> old`, both promoted to old by a major while rooted at `root`.
        let root = heap.alloc_table(LuaTable::new()).unwrap();
        let old = heap.alloc_table(LuaTable::new()).unwrap();
        heap.table_mut(root)
            .unwrap()
            .set(RawValue::Number(1.0), RawValue::Table(old));
        collect_ok(&mut heap, &[GcRef::Table(root.index())]); // major: root, old survive -> old

        // Store a fresh young table into the *old* `old`: the barrier records `old`.
        let young = heap.alloc_table(LuaTable::new()).unwrap();
        heap.table_mut(old)
            .unwrap()
            .set(RawValue::Number(1.0), RawValue::Table(young));
        // A separate unreachable young table is garbage.
        let garbage = heap.alloc_table(LuaTable::new()).unwrap();

        // A minor (not forced major): reachable via `root`. `old` is only reached as an
        // old child of `root`, so `young` survives only because the barrier remembered `old`.
        let freed =
            collect_no_userdata(&mut heap, &[GcRef::Table(root.index())]).expect("minor completes");
        assert_eq!(
            freed, 1,
            "the minor reclaims exactly the unreachable young table"
        );
        assert!(
            heap.table(garbage).is_none(),
            "minor reclaims unreachable young garbage"
        );
        assert!(
            heap.table(young).is_some(),
            "the write barrier kept the old->young edge's target alive across the minor"
        );
        assert!(heap.table(old).is_some() && heap.table(root).is_some());

        // Drop the root and run a minor: `root`, `old`, `young` are now old garbage — a minor
        // does not reclaim them.
        let freed = collect_no_userdata(&mut heap, &[]).expect("minor completes");
        assert_eq!(freed, 0, "a minor leaves old garbage for a major");
        assert!(
            heap.table(root).is_some() && heap.table(old).is_some() && heap.table(young).is_some()
        );

        // A major reclaims the old garbage.
        let freed = collect_ok(&mut heap, &[]);
        assert_eq!(freed, 3, "the major reclaims the old garbage");
        assert!(
            heap.table(root).is_none() && heap.table(old).is_none() && heap.table(young).is_none()
        );
    }

    /// Regression for the abort-recovery soundness hole the early review found: an aborted
    /// minor must not lose the remembered set (which would let a later minor free a live
    /// young child of an `OldRemembered` holder — a use-after-free), and must force the
    /// retry to be a major.
    #[test]
    fn aborted_minor_preserves_remembered_set_and_forces_major() {
        let mut h = heap();
        // `r -> o`, both aged old; then store young `y` into the old `o` so the barrier
        // records `o` (an old→young edge that only the remembered set protects).
        let r = h.alloc_table(LuaTable::new()).unwrap();
        let o = h.alloc_table(LuaTable::new()).unwrap();
        h.table_mut(r)
            .unwrap()
            .set(RawValue::Number(1.0), RawValue::Table(o));
        collect_ok(&mut h, &[GcRef::Table(r.index())]); // major: r, o -> old
        let y = h.alloc_table(LuaTable::new()).unwrap();
        h.table_mut(o)
            .unwrap()
            .set(RawValue::Number(1.0), RawValue::Table(y));
        assert!(!h.gc_remembered.is_empty(), "the barrier recorded o");

        // Force the next minor to abort after marking its roots.
        h.gc_test_abort_minor = true;
        let outcome = collect_no_userdata(&mut h, &[GcRef::Table(r.index())]);
        assert!(
            outcome.is_none(),
            "the aborted minor reports no reclamation"
        );
        assert!(
            !h.gc_remembered.is_empty(),
            "an aborted minor must not lose the remembered set"
        );
        assert!(
            h.gc_force_major,
            "an aborted minor forces the retry to be a major"
        );
        assert!(h.table(y).is_some(), "nothing was swept on abort");

        // The retry (now a major) keeps the barriered young child alive — no UAF.
        let outcome = collect_no_userdata(&mut h, &[GcRef::Table(r.index())]);
        assert!(outcome.is_some(), "the retry completes");
        assert!(
            h.table(y).is_some(),
            "the old->young edge's target survives the abort+retry (no use-after-free)"
        );
        assert!(h.table(o).is_some() && h.table(r).is_some());
    }

    #[test]
    fn sweep_releases_the_metered_footprint() {
        let mut h = heap();
        let garbage = h.alloc_table(LuaTable::new()).unwrap();
        // Grow the table so it charges real bytes to the heap meter.
        for i in 0u8..64 {
            h.table_mut(garbage).unwrap().set(
                RawValue::Number(f64::from(i)),
                RawValue::Number(f64::from(i)),
            );
        }
        let footprint = h.table(garbage).unwrap().gc_footprint();
        assert!(footprint > 0, "growing the table charged the meter");
        let before = h.meter().used();
        let freed = collect_ok(&mut h, &[]);
        assert_eq!(freed, 1, "the unrooted table is reclaimed");
        // The sweep releases at least the table's payload footprint; post-sweep compaction
        // also returns the now-empty arena slot's capacity, so the drop is footprint or more.
        assert!(
            before - h.meter().used() >= footprint,
            "sweeping the table releases at least its metered footprint ({} released, {} footprint)",
            before - h.meter().used(),
            footprint
        );
    }

    #[test]
    fn gcinfo_uses_live_resident_footprint_not_free_arena_holes() {
        let mut h = heap();
        let root = h.alloc_table(LuaTable::new()).unwrap();
        for i in 0u8..64 {
            let keep = h.alloc_table(LuaTable::new()).unwrap();
            let _garbage = h.alloc_table(LuaTable::new()).unwrap();
            h.table_mut(root)
                .unwrap()
                .set(RawValue::Number(f64::from(i) + 1.0), RawValue::Table(keep));
        }

        let before = h.gcinfo_bytes();
        let freed = collect_ok(&mut h, &[GcRef::Table(root.index())]);
        let after = h.gcinfo_bytes();

        assert_eq!(freed, 64, "the alternating unrooted tables are reclaimed");
        assert!(
            after < before,
            "gcinfo should drop when non-tail arena holes are swept ({before} -> {after})"
        );
    }

    #[test]
    fn abort_recovery_scrubs_colors_and_a_later_cycle_is_correct() {
        use crate::heap::Color;
        let mut h = heap();
        let a = h.alloc_table(LuaTable::new()).unwrap();
        let b = h.alloc_table(LuaTable::new()).unwrap();
        // Simulate a cycle abandoned mid-mark: a left black, b left gray.
        h.objects.tables.gc_set_color(a.index(), Color::Black);
        h.objects.tables.gc_set_color(b.index(), Color::Gray);
        super::reset_all_colors(&mut h);
        assert_eq!(h.objects.tables.gc_color(a.index()), Color::White);
        assert_eq!(h.objects.tables.gc_color(b.index()), Color::White);
        // A normal collection after the scrub is correct: a leftover black would
        // have made `mark` skip a reachable object and free its children.
        let freed = collect_ok(&mut h, &[GcRef::Table(a.index())]);
        assert_eq!(freed, 1, "b unrooted is reclaimed; a rooted survives");
        assert!(h.table(a).is_some() && h.table(b).is_none());
    }

    #[test]
    fn weak_interner_sweeps_unreachable_strings_with_fresh_reintern() {
        let mut h = heap();
        // Intern a string referenced only by a table.
        let holder = h.alloc_table(LuaTable::new()).unwrap();
        let s = h.intern_str(b"ephemeral").unwrap();
        h.table_mut(holder)
            .unwrap()
            .set(RawValue::Number(1.0), RawValue::String(s));

        // Reached through the rooted table: the string survives and stays interned.
        collect_ok(&mut h, &[GcRef::Table(holder.index())]);
        assert!(h.string(s).is_some(), "a reached string survives");
        assert_eq!(
            h.intern_str(b"ephemeral"),
            Some(s),
            "still the same interned handle"
        );

        // Drop the root: the table and its only string reference are unreachable.
        collect_ok(&mut h, &[]);
        assert!(
            h.string(s).is_none(),
            "the unreachable interned string is swept (its handle is stale)"
        );

        // Re-interning the same bytes yields a FRESH handle — the weak entry was
        // removed, so the stale handle cannot leak back out (a use-after-free).
        let s2 = h.intern_str(b"ephemeral").unwrap();
        assert_ne!(
            (s.index(), s.generation()),
            (s2.index(), s2.generation()),
            "re-intern after a sweep is a fresh handle, not the stale one"
        );
        assert!(h.string(s).is_none() && h.string(s2).is_some());
    }

    #[test]
    fn collect_active_traces_a_taken_out_thread() {
        let mut h = heap();
        // A thread resident in the arena; during execution its slot holds a placeholder
        // while the live thread is a `&Thread` local — modelled here by `active`.
        let thread_handle = h.alloc_thread(Thread::new()).unwrap();
        h.thread_mut(thread_handle).unwrap().id = Some(thread_handle);
        let mut active = h.take_thread(thread_handle).expect("take active thread");
        // A table the active thread holds in a register (live) and one it does not.
        let live = h.alloc_table(LuaTable::new()).unwrap();
        let garbage = h.alloc_table(LuaTable::new()).unwrap();
        active.stacks.set(0, RawValue::Table(live));
        active.top = 1;
        let freed = collect_active_no_userdata(&mut h, &active).expect("collect must not abort");
        assert!(
            h.table(live).is_some(),
            "a table the taken-out active thread references survives"
        );
        assert!(
            h.table(garbage).is_none(),
            "an unreferenced table is reclaimed by an active-thread collection"
        );
        assert_eq!(freed, 1, "exactly the one garbage table was reclaimed");
        assert!(h.put_thread(thread_handle, active));
    }

    #[test]
    fn collect_active_requires_exactly_one_taken_out_thread() {
        let mut h = heap();
        let thread_a = h.alloc_thread(Thread::new()).unwrap();
        let thread_b = h.alloc_thread(Thread::new()).unwrap();
        h.thread_mut(thread_a).unwrap().id = Some(thread_a);
        h.thread_mut(thread_b).unwrap().id = Some(thread_b);

        let mut active = Thread::new();
        active.id = Some(thread_a);
        let garbage = h.alloc_table(LuaTable::new()).unwrap();
        assert!(
            collect_active_no_userdata(&mut h, &active).is_none(),
            "zero registered take-outs must skip active collection"
        );
        assert!(
            h.table(garbage).is_some(),
            "a skipped active collection must not sweep"
        );

        let active_a = h.take_thread(thread_a).expect("take active thread");
        let active_b = h.take_thread(thread_b).expect("take second thread");
        assert!(
            collect_active_no_userdata(&mut h, &active_a).is_none(),
            "two registered take-outs must skip active collection"
        );
        assert!(
            h.table(garbage).is_some(),
            "a skipped nested active collection must not sweep"
        );
        assert!(h.put_thread(thread_b, active_b));
        assert!(h.put_thread(thread_a, active_a));
    }

    #[test]
    fn open_upvalue_keeps_its_thread_and_captured_value_alive() {
        use crate::{
            builtins::Builtin,
            func::{Closure, UpVal},
            object::Proto,
        };
        // An open upvalue references a register slot of its owning thread, so the cell
        // keeps the whole thread alive (the captured value lives in that register and is
        // traced there). A closure reachable only through such a cell preserves both —
        // the marking that lets scope-exit handle closing without a dead-thread
        // atomic pass.
        let mut h = heap();
        let thread = h.alloc_thread(Thread::new()).unwrap();
        let captured = h.alloc_table(LuaTable::new()).unwrap();
        let owner = h.thread_mut(thread).unwrap();
        owner.id = Some(thread);
        owner.stacks.set(0, RawValue::Table(captured));
        owner.top = 1;
        let upval = h.alloc_upval(UpVal::Open { thread, slot: 0 }).unwrap();
        // Faithful to the running VM: the cell is on the thread's open list and captured
        // by a closure (the thread<->upval edge is exercised either way).
        h.thread_mut(thread).unwrap().open_upvals.push(upval);
        let proto = h.alloc_proto(Proto::native(Builtin::Type)).unwrap();
        let mut closure = Closure::new(proto);
        closure.upvals.push(upval);
        let closure = h.alloc_closure(closure).unwrap();
        let garbage = h.alloc_table(LuaTable::new()).unwrap();
        // Root only the closure: the thread and the captured table are reachable solely
        // through its open upvalue.
        collect_ok(&mut h, &[GcRef::Closure(closure.index())]);
        assert!(
            h.thread(thread).is_some(),
            "the open upvalue's owning thread survives"
        );
        assert!(h.upval(upval).is_some(), "the open upvalue cell survives");
        assert!(
            h.table(captured).is_some(),
            "the captured register value survives through the thread"
        );
        assert!(
            h.table(garbage).is_none(),
            "an unreferenced table is still reclaimed"
        );
        validate(&h).expect("no dangling handle along the closure->upval->thread chain");
    }

    #[test]
    fn validate_passes_on_a_healthy_heap_and_after_collection() {
        let mut h = heap();
        let a = h.alloc_table(LuaTable::new()).unwrap();
        let b = h.alloc_table(LuaTable::new()).unwrap();
        h.table_mut(a)
            .unwrap()
            .set(RawValue::Number(1.0), RawValue::Table(b));
        validate(&h).expect("a consistent heap validates");
        // After a collection that keeps the reachable graph, every handle still resolves.
        collect_ok(&mut h, &[GcRef::Table(a.index())]);
        validate(&h).expect("the heap is consistent after a collection");
    }

    #[test]
    fn validate_catches_a_dangling_handle() {
        let mut h = heap();
        let a = h.alloc_table(LuaTable::new()).unwrap();
        let b = h.alloc_table(LuaTable::new()).unwrap();
        h.table_mut(a)
            .unwrap()
            .set(RawValue::Number(1.0), RawValue::Table(b));
        validate(&h).expect("healthy before the forced free");
        // Forcibly free b's slot out from under a — exactly the dangling handle a real
        // GC bug (sweeping a still-referenced object) would leave behind.
        h.objects.tables.free(b.index());
        assert!(
            validate(&h).is_err(),
            "validate detects a's now-dangling handle to the freed b"
        );
    }

    #[test]
    fn collection_truncates_a_spike_then_a_regrown_slot_rejects_the_stale_handle() {
        // End-to-end through the real collector: a spike of garbage tables drives the table
        // arena's high-water up; collecting sweeps them and compaction truncates the reclaimed
        // tail. Regrowing then reuses a truncated index — and the stale handle to that index's
        // former occupant must stay rejected, neither aliasing the regrown table nor passing
        // `validate`. The bare-`Arena` test covers the same invariant in isolation; this one
        // exercises it through `collect` → `gc_compact_all` inside a full `Heap`.
        let mut h = heap();
        let survivor = h.alloc_table(LuaTable::new()).unwrap();
        let stale = h.alloc_table(LuaTable::new()).unwrap(); // the lowest garbage index
        for _ in 0..62 {
            h.alloc_table(LuaTable::new()).unwrap();
        }
        let before = h.meter().used();
        // Collect rooting only the survivor: every garbage table is swept and the tail
        // truncated back toward the survivor.
        let freed = collect_ok(&mut h, &[GcRef::Table(survivor.index())]);
        assert!(
            freed >= 63,
            "the garbage spike is reclaimed ({freed} freed)"
        );
        assert!(
            h.meter().used() < before,
            "compaction returned the reclaimed slot capacity"
        );
        assert!(h.table(stale).is_none(), "the swept handle is stale");
        // Regrow: the first new table reuses the lowest truncated index — `stale`'s — but at a
        // bumped generation, so the stale handle never aliases it.
        let regrown = h.alloc_table(LuaTable::new()).unwrap();
        assert_eq!(
            regrown.index(),
            stale.index(),
            "the regrown table reused the reclaimed index"
        );
        assert_ne!(
            regrown.generation(),
            stale.generation(),
            "at a fresh generation"
        );
        assert!(
            h.table(stale).is_none(),
            "the stale handle does not alias the regrown table"
        );
        assert!(h.table(regrown).is_some(), "the regrown handle is valid");
        validate(&h).expect("the heap is consistent after truncate + regrow");
    }

    #[test]
    fn validate_catches_a_stale_handle_to_a_reused_slot() {
        // The generation-aware check's reason for being: after a slot is freed *and reused*
        // by a newer object, a stale handle to it points at an occupied slot. A bare
        // occupancy check would pass; the generation check must fail. (This is the case the
        // GC-stress gate hits, where allocations recycle swept slots before the check.)
        let mut h = heap();
        let a = h.alloc_table(LuaTable::new()).unwrap();
        let b = h.alloc_table(LuaTable::new()).unwrap();
        h.table_mut(a)
            .unwrap()
            .set(RawValue::Number(1.0), RawValue::Table(b));
        validate(&h).expect("healthy before the forced free");
        // Free b, then reuse its slot with a fresh table: a's stored handle now carries b's
        // index but a stale generation.
        h.objects.tables.free(b.index());
        let c = h.alloc_table(LuaTable::new()).unwrap();
        assert_eq!(
            c.index(),
            b.index(),
            "the fresh table reused b's freed slot"
        );
        assert_ne!(c.generation(), b.generation(), "with a bumped generation");
        assert!(
            validate(&h).is_err(),
            "validate catches a's handle to the reused slot via its stale generation"
        );
    }

    /// Builds a `{__mode = mode}` metatable and returns its handle.
    fn weak_meta(h: &mut Heap, mode: &[u8]) -> crate::api::RawGc<crate::api::marker::Table> {
        let mode_key = h.intern_str(b"__mode").unwrap();
        let mode_val = h.intern_str(mode).unwrap();
        let meta = h.alloc_table(LuaTable::new()).unwrap();
        h.table_mut(meta)
            .unwrap()
            .set(RawValue::String(mode_key), RawValue::String(mode_val));
        meta
    }

    #[test]
    fn weak_value_table_clears_unreachable_values() {
        let mut h = heap();
        let meta = weak_meta(&mut h, b"v");
        let weak = h.alloc_table(LuaTable::new()).unwrap();
        h.table_mut(weak).unwrap().set_metatable(Some(meta));
        let dead = h.alloc_table(LuaTable::new()).unwrap();
        let live = h.alloc_table(LuaTable::new()).unwrap();
        h.table_mut(weak)
            .unwrap()
            .set(RawValue::Number(1.0), RawValue::Table(dead));
        h.table_mut(weak)
            .unwrap()
            .set(RawValue::Number(2.0), RawValue::Table(live));
        // A rooted strong table holds `live` (so its weak entry must survive).
        let strong = h.alloc_table(LuaTable::new()).unwrap();
        h.table_mut(strong)
            .unwrap()
            .set(RawValue::Number(1.0), RawValue::Table(live));
        collect_ok(
            &mut h,
            &[GcRef::Table(weak.index()), GcRef::Table(strong.index())],
        );
        assert!(
            h.table(dead).is_none(),
            "the unreachable weak value is reclaimed"
        );
        assert!(
            h.table(live).is_some(),
            "a weak value held strongly survives"
        );
        assert_eq!(
            h.table(weak).unwrap().get(RawValue::Number(1.0)),
            RawValue::Nil,
            "the entry whose value died is cleared"
        );
        assert_eq!(
            h.table(weak).unwrap().get(RawValue::Number(2.0)),
            RawValue::Table(live),
            "the entry whose value survives is kept"
        );
        // No dangling handle remains — proving the dead entry was actually cleared.
        validate(&h).expect("the weak table holds no handle to a freed value");
    }

    #[test]
    fn weak_key_table_clears_unreachable_keys() {
        let mut h = heap();
        let meta = weak_meta(&mut h, b"k");
        let weak = h.alloc_table(LuaTable::new()).unwrap();
        h.table_mut(weak).unwrap().set_metatable(Some(meta));
        let dead_key = h.alloc_table(LuaTable::new()).unwrap();
        let live_key = h.alloc_table(LuaTable::new()).unwrap();
        h.table_mut(weak)
            .unwrap()
            .set(RawValue::Table(dead_key), RawValue::Number(1.0));
        h.table_mut(weak)
            .unwrap()
            .set(RawValue::Table(live_key), RawValue::Number(2.0));
        let strong = h.alloc_table(LuaTable::new()).unwrap();
        h.table_mut(strong)
            .unwrap()
            .set(RawValue::Number(1.0), RawValue::Table(live_key));
        collect_ok(
            &mut h,
            &[GcRef::Table(weak.index()), GcRef::Table(strong.index())],
        );
        assert!(
            h.table(dead_key).is_none(),
            "the unreachable weak key is reclaimed"
        );
        assert!(
            h.table(live_key).is_some(),
            "a weak key held strongly survives"
        );
        assert_eq!(
            h.table(weak).unwrap().get(RawValue::Table(live_key)),
            RawValue::Number(2.0),
            "the live-key entry is kept"
        );
        // The dead-key entry was cleared, so no handle dangles to the freed key.
        validate(&h).expect("the weak table holds no handle to a freed key");
    }

    #[test]
    fn weak_value_table_keeps_string_values() {
        // Luau treats strings as values that are never weak: a string held only through a
        // weak-value table still survives and its entry is kept (lgc.cpp:608-619). A table
        // value in the same position would be cleared.
        let mut h = heap();
        let meta = weak_meta(&mut h, b"v");
        let weak = h.alloc_table(LuaTable::new()).unwrap();
        h.table_mut(weak).unwrap().set_metatable(Some(meta));
        let s = h.intern_str(b"weak-but-immortal").unwrap();
        h.table_mut(weak)
            .unwrap()
            .set(RawValue::Number(1.0), RawValue::String(s));
        // Only the weak table references the string (an integer key, so it lives in the
        // array part — the branch most at risk of skipping the string).
        collect_ok(&mut h, &[GcRef::Table(weak.index())]);
        assert_eq!(
            h.table(weak).unwrap().get(RawValue::Number(1.0)),
            RawValue::String(s),
            "a string value in a weak-value table is never cleared"
        );
        // The string slot is still live, so the kept entry holds no dangling handle.
        validate(&h).expect("the surviving string entry resolves to a live string");
    }

    #[test]
    fn weak_key_table_keeps_string_keys() {
        // Mirror of the value test for the key side (a string key lives in the hash part):
        // a string key in a weak-key table is never weak, so it survives and its entry is
        // kept.
        let mut h = heap();
        let meta = weak_meta(&mut h, b"k");
        let weak = h.alloc_table(LuaTable::new()).unwrap();
        h.table_mut(weak).unwrap().set_metatable(Some(meta));
        let s = h.intern_str(b"weak-key-but-immortal").unwrap();
        h.table_mut(weak)
            .unwrap()
            .set(RawValue::String(s), RawValue::Number(7.0));
        collect_ok(&mut h, &[GcRef::Table(weak.index())]);
        assert_eq!(
            h.table(weak).unwrap().get(RawValue::String(s)),
            RawValue::Number(7.0),
            "a string key in a weak-key table is never cleared"
        );
        validate(&h).expect("the surviving string-key entry resolves to a live string");
    }

    #[test]
    fn weak_kv_table_clears_when_either_side_dies() {
        // With `__mode="kv"` an entry is cleared if EITHER its key or its value is white;
        // it survives only when both sides are reachable independently of the table.
        let mut h = heap();
        let meta = weak_meta(&mut h, b"kv");
        let weak = h.alloc_table(LuaTable::new()).unwrap();
        h.table_mut(weak).unwrap().set_metatable(Some(meta));
        // A: live key, dead value -> cleared. B: dead key, live value -> cleared.
        // C: both live -> kept.
        let key_a = h.alloc_table(LuaTable::new()).unwrap();
        let val_a = h.alloc_table(LuaTable::new()).unwrap();
        let key_b = h.alloc_table(LuaTable::new()).unwrap();
        let val_b = h.alloc_table(LuaTable::new()).unwrap();
        let key_c = h.alloc_table(LuaTable::new()).unwrap();
        let val_c = h.alloc_table(LuaTable::new()).unwrap();
        for (k, v) in [(key_a, val_a), (key_b, val_b), (key_c, val_c)] {
            h.table_mut(weak)
                .unwrap()
                .set(RawValue::Table(k), RawValue::Table(v));
        }
        // A strong table roots A's key, B's value, and both of C's sides.
        let strong = h.alloc_table(LuaTable::new()).unwrap();
        for (i, t) in [key_a, val_b, key_c, val_c].into_iter().enumerate() {
            h.table_mut(strong)
                .unwrap()
                .set(RawValue::Number((i + 1) as f64), RawValue::Table(t));
        }
        collect_ok(
            &mut h,
            &[GcRef::Table(weak.index()), GcRef::Table(strong.index())],
        );
        // A: its value died, so the entry is cleared; its key survives (held strongly).
        assert!(
            h.table(val_a).is_none(),
            "A's unreachable value is reclaimed"
        );
        assert!(h.table(key_a).is_some(), "A's key is held strongly");
        assert_eq!(
            h.table(weak).unwrap().get(RawValue::Table(key_a)),
            RawValue::Nil,
            "entry A is cleared because its value side died"
        );
        // B: its key died, so the entry is cleared; its value survives (held strongly).
        assert!(h.table(key_b).is_none(), "B's unreachable key is reclaimed");
        assert!(h.table(val_b).is_some(), "B's value is held strongly");
        // C: both sides reachable, so the entry is kept.
        assert_eq!(
            h.table(weak).unwrap().get(RawValue::Table(key_c)),
            RawValue::Table(val_c),
            "entry C survives because both sides are reachable"
        );
        validate(&h).expect("no entry dangles after the kv clear");
    }

    #[test]
    fn self_metatable_weak_table_keeps_its_mode_string() {
        // A table that is its own `__mode="kv"` metatable must not clear its own `__mode`
        // entry: both its key and value are strings, which are never weak — otherwise the
        // table would erase the very mode that makes it weak (codex panel).
        let mut h = heap();
        let t = h.alloc_table(LuaTable::new()).unwrap();
        let mode_key = h.intern_str(b"__mode").unwrap();
        let mode_val = h.intern_str(b"kv").unwrap();
        h.table_mut(t)
            .unwrap()
            .set(RawValue::String(mode_key), RawValue::String(mode_val));
        // The table is its own metatable: a weak (kv) table over its own entries.
        h.table_mut(t).unwrap().set_metatable(Some(t));
        collect_ok(&mut h, &[GcRef::Table(t.index())]);
        assert_eq!(
            h.table(t).unwrap().get(RawValue::String(mode_key)),
            RawValue::String(mode_val),
            "a self-metatable weak table keeps its own __mode string entry"
        );
        validate(&h).expect("the kept __mode entry resolves to live strings");
    }
}
