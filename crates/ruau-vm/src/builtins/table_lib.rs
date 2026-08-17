use super::*;

/// An array element `t[index]` (number-keyed, the array representation).
pub(super) fn get_index(heap: &Heap, handle: RawGc<marker::Table>, index: i64) -> RawValue {
    heap.table(handle)
        .map_or(RawValue::Nil, |t| t.get(RawValue::Number(index as f64)))
}

pub(super) fn set_index(
    heap: &mut Heap,
    handle: RawGc<marker::Table>,
    index: i64,
    value: RawValue,
) -> Exec<()> {
    heap.table_mut(handle)
        .ok_or_else(|| err("table not resident"))?
        .set(RawValue::Number(index as f64), value);
    Ok(())
}

/// Rejects a mutation of a frozen (read-only) table. The interpreter enforces
/// `readonly` on bytecode stores, but the `table.*` mutators write through
/// `set` directly and bypass that check; upstream rejects `sort`/`insert`/
/// `remove`/`move`/`rawset` on a frozen table (`ltablib.cpp`), so every public
/// table mutator calls this first.
pub(super) fn require_writable(heap: &Heap, handle: RawGc<marker::Table>) -> Exec<()> {
    if heap.table(handle).is_some_and(|t| t.readonly) {
        return Err(err("attempt to modify a readonly table"));
    }
    Ok(())
}

/// `table.insert(t, [pos,] v)`: append `v`, shift in-range `pos` elements up by
/// one, or write an out-of-range `pos` as a sparse key.
pub(super) fn table_insert(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let RawValue::Table(handle) = args.first().copied().unwrap_or(RawValue::Nil) else {
        return Err(err("bad argument #1 to 'table.insert' (table expected)"));
    };
    require_writable(heap, handle)?;
    let len = heap.table(handle).map_or(0, |t| t.length()) as i64;
    match args.len() {
        2 => set_index(heap, handle, len + 1, args[1])?,
        3 => {
            let pos = arg_int(args, 1).ok_or_else(|| err("bad argument #2 to 'table.insert'"))?;
            if (1..=len + 1).contains(&pos) {
                for i in (pos..=len).rev() {
                    let moved = get_index(heap, handle, i);
                    set_index(heap, handle, i + 1, moved)?;
                }
            }
            set_index(heap, handle, pos, args[2])?;
        }
        _ => return Err(err("wrong number of arguments to 'table.insert'")),
    }
    Ok(Vec::new())
}

/// `table.remove(t, pos?)`: remove and return `t[pos]` (default the last element),
/// shifting later elements down. An out-of-range `pos` is a no-op returning no
/// values (Luau `tremove`: `!(1 <= pos && pos <= n)` ⇒ nothing to remove), not a
/// silent clear of `t[len]`.
pub(super) fn table_remove(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let RawValue::Table(handle) = args.first().copied().unwrap_or(RawValue::Nil) else {
        return Err(err("bad argument #1 to 'table.remove' (table expected)"));
    };
    let len = heap.table(handle).map_or(0, |t| t.length()) as i64;
    let pos = arg_int(args, 1).unwrap_or(len);
    if !(1..=len).contains(&pos) {
        // No write is attempted, so (matching upstream) this does not raise a
        // readonly error on a frozen table either.
        return Ok(Vec::new());
    }
    require_writable(heap, handle)?;
    let removed = get_index(heap, handle, pos);
    for i in pos..len {
        let moved = get_index(heap, handle, i + 1);
        set_index(heap, handle, i, moved)?;
    }
    set_index(heap, handle, len, RawValue::Nil)?;
    Ok(vec![removed])
}

/// `table.concat(t, sep?, i?, j?)`: join the string/number elements `t[i..j]` with
/// `sep`.
pub(super) fn table_concat(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let RawValue::Table(handle) = args.first().copied().unwrap_or(RawValue::Nil) else {
        return Err(err("bad argument #1 to 'table.concat' (table expected)"));
    };
    let sep = match args.get(1).copied() {
        None | Some(RawValue::Nil) => Vec::new(),
        Some(_) => arg_bytes(heap, args, 1)?,
    };
    let len = heap.table(handle).map_or(0, |t| t.length()) as i64;
    let i = arg_int(args, 2).unwrap_or(1);
    let j = arg_int(args, 3).unwrap_or(len);
    // Charge the join's CPU upfront: this whole builtin is one bytecode instruction, so
    // an `O(count)` concat over a large range must cost `O(count)` budget rather than the
    // single tick the `CALL` charged. The output size is data-dependent, so it is still
    // metered against `max_string_bytes`/the memory cap inline as the buffer grows (below),
    // not just at the final intern.
    if i <= j {
        let count = u64::try_from(j - i + 1).unwrap_or(u64::MAX);
        if !heap.charge_gas(count) {
            return Err(err_gas());
        }
    }
    let mut out = Vec::new();
    let mut k = i;
    while k <= j {
        match get_index(heap, handle, k) {
            RawValue::String(handle) => {
                if let Some(s) = heap.string(handle) {
                    out.extend_from_slice(s.bytes());
                }
            }
            RawValue::Number(n) => out.extend_from_slice(vmutils::number_to_string(n).as_bytes()),
            RawValue::Integer(v) => out.extend_from_slice(v.to_string().as_bytes()),
            _ => {
                return Err(err(format!(
                    "invalid value (at index {k}) in table for 'concat'"
                )));
            }
        }
        if k < j {
            out.extend_from_slice(&sep);
        }
        meter_string_growth(heap, out.len(), "table.concat")?;
        k += 1;
    }
    intern_result(heap, &out)
}

/// Requires a table argument at `index`.
pub(super) fn arg_table(args: &[RawValue], index: usize, name: &str) -> Exec<RawGc<marker::Table>> {
    match args.get(index).copied().unwrap_or(RawValue::Nil) {
        RawValue::Table(handle) => Ok(handle),
        _ => Err(err(format!(
            "bad argument #{} to 'table.{name}' (table expected)",
            index + 1
        ))),
    }
}

/// `table.getn(t)`: the array length (`#t`).
pub(super) fn table_getn(heap: &Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let handle = arg_table(args, 0, "getn")?;
    let len = heap.table(handle).map_or(0, LuaTable::length);
    Ok(vec![RawValue::Number(len as f64)])
}

/// `table.maxn(t)`: the largest numeric key with a non-`nil` value, or `0`.
pub(super) fn table_maxn(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let handle = arg_table(args, 0, "maxn")?;
    // `maxn` scans every array and hash slot as one bytecode instruction; charge that
    // scan against the budget so its `O(scan_len)` work is metered.
    let scan_len = heap.table(handle).map_or(0, LuaTable::scan_len);
    if !heap.charge_gas(scan_len as u64) {
        return Err(err_gas());
    }
    let max = heap.table(handle).map_or(0.0, LuaTable::maxn);
    Ok(vec![RawValue::Number(max)])
}

/// `table.freeze(t)`: marks `t` read-only (writes then raise) and returns it.
/// Re-freezing an already-frozen table errors, and a protected metatable rejects
/// the freeze like an attempted metatable mutation.
pub(super) fn table_freeze(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let handle = arg_table(args, 0, "freeze")?;
    let metatable = heap
        .table(handle)
        .ok_or_else(|| err("table.freeze on a non-resident table"))?
        .metatable();
    if let Some(metatable) = metatable
        && metatable_protection(heap, metatable)?.is_some()
    {
        return Err(err("cannot freeze a table with a protected metatable"));
    }
    let table = heap
        .table_mut(handle)
        .ok_or_else(|| err("table.freeze on a non-resident table"))?;
    if table.readonly {
        return Err(err(
            "bad argument #1 to 'table.freeze' (table is already frozen)",
        ));
    }
    table.readonly = true;
    Ok(vec![RawValue::Table(handle)])
}

/// `table.isfrozen(t)`: whether `t` is read-only.
pub(super) fn table_isfrozen(heap: &Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let handle = arg_table(args, 0, "isfrozen")?;
    let frozen = heap.table(handle).is_some_and(|t| t.readonly);
    Ok(vec![RawValue::Boolean(frozen)])
}

/// `table.clone(t)`: a shallow copy — the same entries and metatable, but a fresh
/// mutable table (a frozen source clones to an unfrozen copy).
pub(super) fn table_clone(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let handle = arg_table(args, 0, "clone")?;
    let metatable = heap
        .table(handle)
        .ok_or_else(|| err("table.clone on a non-resident table"))?
        .metatable();
    if let Some(metatable) = metatable
        && metatable_protection(heap, metatable)?.is_some()
    {
        return Err(err("cannot clone a table with a protected metatable"));
    }
    // Pre-charge the copy's footprint against the cap before `shallow_clone`
    // allocates it (the clone duplicates the source's containers, so heap metering
    // would otherwise only notice after the allocation already happened).
    let footprint = heap
        .table(handle)
        .ok_or_else(|| err("table.clone on a non-resident table"))?
        .footprint();
    if heap.would_exceed_cap(footprint) {
        return Err(err_memory_limit());
    }
    // The shallow copy duplicates every array and hash slot as one bytecode
    // instruction; charge that against the budget so its `O(scan_len)` work is metered
    // (the memory pre-charge above bounds the footprint, not the CPU).
    let scan_len = heap.table(handle).map_or(0, LuaTable::scan_len);
    if !heap.charge_gas(scan_len as u64) {
        return Err(err_gas());
    }
    let clone = heap
        .table(handle)
        .ok_or_else(|| err("table.clone on a non-resident table"))?
        .shallow_clone();
    let new = heap
        .alloc_table(clone)
        .ok_or_else(|| err_memory("out of memory for 'table.clone'"))?;
    Ok(vec![RawValue::Table(new)])
}

/// `table.clear(t)`: removes every entry, keeping the allocation. Errors on a
/// frozen table, like a direct write.
pub(super) fn table_clear(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let handle = arg_table(args, 0, "clear")?;
    let table = heap
        .table_mut(handle)
        .ok_or_else(|| err("table.clear on a non-resident table"))?;
    if table.readonly {
        return Err(err("attempt to modify a readonly table"));
    }
    table.clear();
    Ok(Vec::new())
}

/// `table.foreach(t, f)` (deprecated): calls `f(key, value)` for each entry; if a
/// call returns a non-`nil` value, iteration stops and that value is returned.
/// The pairs are snapshotted before the first call so a mutation by `f` cannot
/// invalidate the iteration.
pub(super) fn table_foreach(
    heap: &mut Heap,
    thread: &mut Thread,
    args: &[RawValue],
    host_entry: crate::scope::HostEntry<'_>,
) -> Exec<Vec<RawValue>> {
    let handle = arg_table(args, 0, "foreach")?;
    let func = foreach_function(args, "foreach")?;
    // The snapshot allocates on the order of the table's footprint; pre-charge it
    // against the cap before the bulk copy (the snapshot has no mid-builtin
    // safepoint to catch an over-cap mid-iteration).
    let footprint = heap.table(handle).map_or(0, LuaTable::footprint);
    if heap.would_exceed_cap(footprint) {
        return Err(err_memory_limit());
    }
    let pairs = snapshot_pairs(heap, handle)?;
    for (key, value) in pairs {
        let results = call_value(heap, thread, func, &[key, value], host_entry)?;
        let first = results.into_iter().next().unwrap_or(RawValue::Nil);
        if !matches!(first, RawValue::Nil) {
            return Ok(vec![first]);
        }
    }
    Ok(Vec::new())
}

/// `table.foreachi(t, f)` (deprecated): calls `f(i, t[i])` for `i` in `1..=#t`,
/// stopping and returning the first non-`nil` result.
pub(super) fn table_foreachi(
    heap: &mut Heap,
    thread: &mut Thread,
    args: &[RawValue],
    host_entry: crate::scope::HostEntry<'_>,
) -> Exec<Vec<RawValue>> {
    let handle = arg_table(args, 0, "foreachi")?;
    let func = foreach_function(args, "foreachi")?;
    let len = heap.table(handle).map_or(0, LuaTable::length);
    for i in 1..=len as i64 {
        let value = get_index(heap, handle, i);
        let results = call_value(
            heap,
            thread,
            func,
            &[RawValue::Number(i as f64), value],
            host_entry,
        )?;
        let first = results.into_iter().next().unwrap_or(RawValue::Nil);
        if !matches!(first, RawValue::Nil) {
            return Ok(vec![first]);
        }
    }
    Ok(Vec::new())
}

/// The function argument shared by `foreach`/`foreachi` (upstream requires a
/// function, erroring otherwise).
pub(super) fn foreach_function(args: &[RawValue], name: &str) -> Exec<RawValue> {
    match args.get(1).copied().unwrap_or(RawValue::Nil) {
        func @ RawValue::Function(_) => Ok(func),
        _ => Err(err(format!(
            "bad argument #2 to 'table.{name}' (function expected)"
        ))),
    }
}

/// Collects every live `(key, value)` pair of a table by walking `next`, so an
/// iterating builtin can call back into the VM without holding a table borrow.
pub(super) fn snapshot_pairs(
    heap: &mut Heap,
    handle: RawGc<marker::Table>,
) -> Exec<Vec<(RawValue, RawValue)>> {
    let mut pairs = Vec::new();
    let mut key = RawValue::Nil;
    loop {
        // One budget unit per snapshotted pair: the snapshot walks the whole table up
        // front (so a mutation by the callback cannot invalidate the iteration) as one
        // bytecode instruction, before any metered callback runs — charge it so an
        // `O(n)` table cannot be snapshotted for the single tick the `CALL` charged.
        if !heap.tick_gas() {
            return Err(err_gas());
        }
        let step = match heap.table(handle) {
            Some(table) => table.next(key),
            None => break,
        };
        match step {
            NextStep::Pair(k, v) => {
                pairs.push((k, v));
                key = k;
            }
            NextStep::Done | NextStep::InvalidKey => break,
        }
    }
    Ok(pairs)
}

/// `table.pack(...)`: an array of the arguments plus `n = count`.
pub(super) fn table_pack(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    // One budget unit per packed argument: the whole pack is one bytecode
    // instruction, so without this an `O(n)` pack of a large vararg spread costs only
    // the single tick the `CALL` charged.
    if !heap.charge_gas(args.len() as u64) {
        return Err(err_gas());
    }
    let table = heap
        .alloc_table(LuaTable::new())
        .ok_or_else(|| err_memory("out of memory for 'table.pack'"))?;
    for (i, &value) in args.iter().enumerate() {
        set_index(heap, table, i as i64 + 1, value)?;
    }
    let n_key = RawValue::String(
        heap.intern_str(b"n")
            .ok_or_else(|| err_memory("out of memory for 'table.pack'"))?,
    );
    heap.table_mut(table)
        .ok_or_else(|| err("table not resident"))?
        .set(n_key, RawValue::Number(args.len() as f64));
    Ok(vec![RawValue::Table(table)])
}

/// `table.unpack(list, i?, j?)`: the elements `list[i..=j]` as multiple results.
/// `table.sort(t, comp?)`: sorts the array part in place using `comp` (or `<`).
/// A manual merge sort tolerates an inconsistent comparator without panicking or
/// going out of bounds (unlike `slice::sort_by`, which can panic on a comparator
/// that is not a strict weak order — a script-reachable crash for untrusted input).
pub(super) fn table_sort(
    heap: &mut Heap,
    thread: &mut Thread,
    args: &[RawValue],
    host_entry: crate::scope::HostEntry<'_>,
) -> Exec<Vec<RawValue>> {
    let handle = arg_table(args, 0, "sort")?;
    require_writable(heap, handle)?;
    let comp = args.get(1).copied().unwrap_or(RawValue::Nil);
    if !matches!(comp, RawValue::Nil | RawValue::Function(_)) {
        return Err(err("bad argument #2 to 'table.sort' (function expected)"));
    }
    let len = heap.table(handle).map_or(0, LuaTable::length) as usize;
    // The two temporaries (`elems` + `scratch`) are 2*len*size_of::<RawValue>().
    // Bound `len` absolutely and pre-charge the cap, per the standing no-cap-OOM
    // rule (a no-cap VM has no other backstop, like `unpack`/`move`/`create`).
    if len > heap.limits().max_table_elements {
        return Err(err("table is too large to sort"));
    }
    if heap.would_exceed_cap(len.saturating_mul(2 * std::mem::size_of::<RawValue>())) {
        return Err(err_memory_limit());
    }
    let mut elems: Vec<RawValue> = (1..=len)
        .map(|i| get_index(heap, handle, i as i64))
        .collect();
    let mut scratch = vec![RawValue::Nil; elems.len()];
    merge_sort(heap, thread, comp, &mut elems, &mut scratch, host_entry)?;
    for (i, &value) in elems.iter().enumerate() {
        set_index(heap, handle, i as i64 + 1, value)?;
    }
    Ok(Vec::new())
}

/// Whether `a` should sort before `b`, using the comparator or the default order.
pub(super) fn sort_less(
    heap: &mut Heap,
    thread: &mut Thread,
    comp: RawValue,
    a: RawValue,
    b: RawValue,
    host_entry: crate::scope::HostEntry<'_>,
) -> Exec<bool> {
    // Charge the instruction budget per comparison. `table.sort` is one bytecode
    // instruction, so the dispatch safepoint never fires inside it; without this a
    // tenant could sort a large array (bounded only by `max_table_elements`) and burn
    // O(n log n) comparisons of CPU for the single tick the `CALL` charged. A custom
    // comparator's own bytecode is metered when it runs; this meters the sort's own
    // per-comparison work for the default and metamethod paths too. The sort runs on a
    // copied `Vec` and writes back only on success, so an exhausted budget here leaves
    // the table unchanged.
    if !heap.tick_gas() {
        return Err(err_gas());
    }
    if let RawValue::Function(_) = comp {
        let results = call_value(heap, thread, comp, &[a, b], host_entry)?;
        return Ok(is_truthy(
            results.into_iter().next().unwrap_or(RawValue::Nil),
        ));
    }
    // Default order: the `<` operator itself, so numbers compare numerically,
    // strings by byte order, and same-tag values dispatch a matching `__lt`
    // metamethod — exactly as upstream's `lua_lessthan` does for `table.sort`.
    crate::execute::less_than_op(heap, thread, a, b, host_entry)
}

/// A stable, panic-safe merge sort of `elems`, with `scratch` as merge buffer.
pub(super) fn merge_sort(
    heap: &mut Heap,
    thread: &mut Thread,
    comp: RawValue,
    elems: &mut [RawValue],
    scratch: &mut [RawValue],
    host_entry: crate::scope::HostEntry<'_>,
) -> Exec<()> {
    let n = elems.len();
    if n <= 1 {
        return Ok(());
    }
    let mid = n / 2;
    {
        let (left, right) = elems.split_at_mut(mid);
        let (sl, sr) = scratch.split_at_mut(mid);
        merge_sort(heap, thread, comp, left, sl, host_entry)?;
        merge_sort(heap, thread, comp, right, sr, host_entry)?;
    }
    // Merge the two sorted halves into `scratch`, then copy back. Take the right
    // element only when it is strictly less than the left, so the sort is stable.
    let (mut i, mut j, mut k) = (0usize, mid, 0usize);
    while i < mid && j < n {
        if sort_less(heap, thread, comp, elems[j], elems[i], host_entry)? {
            scratch[k] = elems[j];
            j += 1;
        } else {
            scratch[k] = elems[i];
            i += 1;
        }
        k += 1;
    }
    while i < mid {
        scratch[k] = elems[i];
        i += 1;
        k += 1;
    }
    while j < n {
        scratch[k] = elems[j];
        j += 1;
        k += 1;
    }
    elems.copy_from_slice(&scratch[..n]);
    Ok(())
}

/// `table.unpack(list, i?, j?)`: the elements `list[i..=j]` as multiple results.
pub(super) fn table_unpack(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let handle = arg_table(args, 0, "unpack")?;
    let i = arg_int(args, 1).unwrap_or(1);
    let len = heap.table(handle).map_or(0, LuaTable::length) as i64;
    let j = arg_int(args, 2).unwrap_or(len);
    if i > j {
        return Ok(Vec::new());
    }
    let count = usize::try_from(j - i + 1).unwrap_or(usize::MAX);
    if count > heap.limits().max_table_elements {
        return Err(err("too many results to unpack"));
    }
    // Pre-charge against the cap too: the result count is data-dependent and this
    // whole builtin is one instruction.
    if heap.would_exceed_cap(count.saturating_mul(std::mem::size_of::<RawValue>())) {
        return Err(err_memory_limit());
    }
    // Charge the `count` reads against the budget upfront: the whole unpack is one
    // bytecode instruction, so without this an `O(count)` spread costs only the
    // single tick the `CALL` charged. The memory pre-charge above bounds the result
    // Vec; this bounds the CPU.
    if !heap.charge_gas(count as u64) {
        return Err(err_gas());
    }
    let mut out = Vec::with_capacity(count);
    let mut k = i;
    while k <= j {
        out.push(get_index(heap, handle, k));
        k += 1;
    }
    Ok(out)
}

/// `table.create(count, value?)`: a table whose array part is `count` copies of
/// `value` (`nil` pre-sizes the array so a later positional write lands in it).
pub(super) fn table_create(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let count = arg_int(args, 0)
        .ok_or_else(|| err("bad argument #1 to 'table.create' (number expected)"))?;
    if count < 0 {
        return Err(err(
            "bad argument #1 to 'table.create' (positive number expected)",
        ));
    }
    let count = usize::try_from(count).unwrap_or(usize::MAX);
    if count > heap.limits().max_table_elements {
        return Err(err("table.create count is too large"));
    }
    if heap.would_exceed_cap(count.saturating_mul(std::mem::size_of::<RawValue>())) {
        return Err(err_memory_limit());
    }
    let value = args.get(1).copied().unwrap_or(RawValue::Nil);
    let handle = heap
        .alloc_table(LuaTable::with_array(vec![value; count]))
        .ok_or_else(|| err_memory("out of memory for 'table.create'"))?;
    Ok(vec![RawValue::Table(handle)])
}

/// `table.find(haystack, needle, init?)`: the first consecutive index `>= init`
/// holding `needle` (using `__eq` when applicable), or `nil`. The scan stops at
/// the first absent index, matching upstream's array-prefix behavior.
pub(super) fn table_find(
    heap: &mut Heap,
    thread: &mut Thread,
    args: &[RawValue],
    host_entry: crate::scope::HostEntry<'_>,
) -> Exec<Vec<RawValue>> {
    let handle = arg_table(args, 0, "find")?;
    let needle = args.get(1).copied().unwrap_or(RawValue::Nil);
    let init = arg_int(args, 2).unwrap_or(1);
    if init < 1 {
        return Err(err("bad argument #3 to 'table.find' (index out of range)"));
    }
    let mut index = init;
    loop {
        if !heap.tick_gas() {
            return Err(err_gas());
        }
        let value = get_index(heap, handle, index);
        if matches!(value, RawValue::Nil) {
            return Ok(vec![RawValue::Nil]);
        }
        if table_find_value_matches(heap, thread, value, needle, host_entry)? {
            return Ok(vec![RawValue::Number(index as f64)]);
        }
        let Some(next) = index.checked_add(1) else {
            return Ok(vec![RawValue::Nil]);
        };
        index = next;
    }
}

pub(super) fn table_find_value_matches(
    heap: &mut Heap,
    thread: &mut Thread,
    lhs: RawValue,
    rhs: RawValue,
    host_entry: crate::scope::HostEntry<'_>,
) -> Exec<bool> {
    let comparable = matches!(
        (lhs, rhs),
        (RawValue::Table(_), RawValue::Table(_)) | (RawValue::Userdata(_), RawValue::Userdata(_))
    );
    if !comparable {
        return Ok(vmutils::raw_equal(lhs, rhs));
    }

    let Some(lhs_eq) = tm::get_metamethod(heap, lhs, MetaEvent::Eq)? else {
        return Ok(vmutils::raw_equal(lhs, rhs));
    };
    let Some(rhs_eq) = tm::get_metamethod(heap, rhs, MetaEvent::Eq)? else {
        return Ok(vmutils::raw_equal(lhs, rhs));
    };
    if !vmutils::raw_equal(lhs_eq, rhs_eq) {
        return Ok(vmutils::raw_equal(lhs, rhs));
    }

    let result = call_value(heap, thread, lhs_eq, &[lhs, rhs], host_entry)?;
    Ok(vmutils::truthy(
        result.into_iter().next().unwrap_or(RawValue::Nil),
    ))
}

fn table_live_entry_count(heap: &Heap, handle: RawGc<marker::Table>) -> Exec<usize> {
    let table = heap
        .table(handle)
        .ok_or_else(|| err("table not resident"))?;
    let mut count = 0usize;
    table.for_each_entry(|_, _| count = count.saturating_add(1));
    Ok(count)
}

fn integer_key_in_range(key: RawValue, first: i64, last: i64) -> Option<i64> {
    match key {
        RawValue::Integer(index) if (first..=last).contains(&index) => Some(index),
        RawValue::Number(index)
            if index.fract() == 0.0 && index >= first as f64 && index <= last as f64 =>
        {
            Some(index as i64)
        }
        _ => None,
    }
}

#[derive(Clone, Copy)]
struct TableMoveRange {
    first: i64,
    last: i64,
    target: i64,
}

fn sparse_table_move(
    heap: &mut Heap,
    src: RawGc<marker::Table>,
    dst: RawGc<marker::Table>,
    range: TableMoveRange,
    src_entries: usize,
    dst_entries: usize,
) -> Exec<()> {
    let temporary_bytes = src_entries
        .checked_mul(std::mem::size_of::<(i64, RawValue)>())
        .and_then(|bytes| {
            dst_entries
                .checked_mul(std::mem::size_of::<i64>())
                .and_then(|dst_bytes| bytes.checked_add(dst_bytes))
        })
        .ok_or_else(err_memory_limit)?;
    if heap.would_exceed_cap(temporary_bytes) {
        return Err(err_memory_limit());
    }

    let mut source = Vec::new();
    source
        .try_reserve_exact(src_entries)
        .map_err(|_| err_memory_limit())?;
    heap.table(src)
        .ok_or_else(|| err("table not resident"))?
        .for_each_entry(|key, value| {
            if let Some(index) = integer_key_in_range(key, range.first, range.last) {
                source.push((index, value));
            }
        });

    let span = range.last as i128 - range.first as i128;
    let target_last =
        i64::try_from(range.target as i128 + span).map_err(|_| err("destination wrap around"))?;
    let mut destination_keys = Vec::new();
    destination_keys
        .try_reserve_exact(dst_entries)
        .map_err(|_| err_memory_limit())?;
    heap.table(dst)
        .ok_or_else(|| err("table not resident"))?
        .for_each_entry(|key, _| {
            if let Some(index) = integer_key_in_range(key, range.target, target_last) {
                destination_keys.push(index);
            }
        });

    let work = src_entries
        .saturating_add(dst_entries)
        .saturating_add(source.len())
        .saturating_add(destination_keys.len());
    if !heap.charge_gas(u64::try_from(work).unwrap_or(u64::MAX)) {
        return Err(err_gas());
    }

    for index in destination_keys {
        set_index(heap, dst, index, RawValue::Nil)?;
    }
    for (index, value) in source {
        let destination =
            i64::try_from(range.target as i128 + (index as i128 - range.first as i128))
                .map_err(|_| err("destination wrap around"))?;
        set_index(heap, dst, destination, value)?;
    }
    Ok(())
}

/// `table.move(src, a, b, t, dst?)`: copies `src[a..=b]` to `dst[t..]` (`dst`
/// defaults to `src`), handling overlapping and sparse moves, and returns `dst`.
pub(super) fn table_move(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    // Upstream treats every `table.move` index as a 32-bit `int`; the range
    // checks below bound to this ceiling.
    const INT_MAX: i128 = i32::MAX as i128;
    let src = arg_table(args, 0, "move")?;
    let a =
        arg_int(args, 1).ok_or_else(|| err("bad argument #2 to 'table.move' (number expected)"))?;
    let b =
        arg_int(args, 2).ok_or_else(|| err("bad argument #3 to 'table.move' (number expected)"))?;
    let t =
        arg_int(args, 3).ok_or_else(|| err("bad argument #4 to 'table.move' (number expected)"))?;
    let dst = match args.get(4).copied() {
        Some(RawValue::Table(handle)) => handle,
        None | Some(RawValue::Nil) => src,
        _ => return Err(err("bad argument #5 to 'table.move' (table expected)")),
    };
    if b < a {
        return Ok(vec![RawValue::Table(dst)]);
    }
    require_writable(heap, dst)?;
    // Match upstream `luaB_move`'s two range checks (`ltablib.cpp`), which treat
    // every index as a 32-bit `int`: a source range wider than `INT_MAX`
    // elements ("too many elements to move"), and a destination whose top index
    // would pass `INT_MAX` ("destination wrap around"). The bound arithmetic is
    // done in `i128` so a hostile `i64` index cannot overflow the check itself.
    let (a128, b128, t128) = (a as i128, b as i128, t as i128);
    if !(a128 > 0 || b128 < INT_MAX + a128) {
        return Err(err("too many elements to move"));
    }
    let n = b128 - a128 + 1; // element count; `b >= a` here, so `n >= 1`.
    if t128 > INT_MAX - n + 1 {
        return Err(err("destination wrap around"));
    }

    // Luau 0.733 added a sparse path for ranges that are much wider than either
    // table. It snapshots the source's live integer keys, clears only live keys in
    // the destination range, and copies the snapshot. This makes a 200-million-key
    // range over a five-element table O(table entries), while preserving overlapping
    // in-place moves.
    if n > 32 {
        let src_entries = table_live_entry_count(heap, src)?;
        let dst_entries = if src == dst {
            src_entries
        } else {
            table_live_entry_count(heap, dst)?
        };
        let max_entries = src_entries.max(dst_entries) as i128;
        if n / 2 > max_entries {
            sparse_table_move(
                heap,
                src,
                dst,
                TableMoveRange {
                    first: a,
                    last: b,
                    target: t,
                },
                src_entries,
                dst_entries,
            )?;
            return Ok(vec![RawValue::Table(dst)]);
        }
    }

    // Those checks bound the span whenever indices stay in the `int` range; keep
    // the element- and byte-budget guards as the real allocation backstop for
    // the out-of-range `i64` tail the upstream `int` model never reaches.
    let span = b - a; // last offset; span+1 elements copied.
    let count = usize::try_from(span)
        .ok()
        .and_then(|s| s.checked_add(1))
        .unwrap_or(usize::MAX);
    if count > heap.limits().max_table_elements {
        return Err(err("too many elements to move"));
    }
    if heap.would_exceed_cap(count.saturating_mul(std::mem::size_of::<RawValue>())) {
        return Err(err_memory_limit());
    }
    // The whole move is one bytecode instruction, so charge its `count` element
    // copies against the budget upfront — an `O(count)` move costs `O(count)`
    // budget rather than the single tick the `CALL` charged. (The memory backstop
    // above already bounds the span; this bounds the CPU.) Charging before the
    // copy means an exhausted budget mutates nothing.
    if !heap.charge_gas(count as u64) {
        return Err(err_gas());
    }
    // Copy backwards only for an overlapping in-place forward move, so a source
    // element is read before the copy overwrites it.
    let overlaps = src == dst && t > a && t <= b;
    if overlaps {
        for offset in (0..=span).rev() {
            let value = get_index(heap, src, a + offset);
            set_index(heap, dst, t + offset, value)?;
        }
    } else {
        for offset in 0..=span {
            let value = get_index(heap, src, a + offset);
            set_index(heap, dst, t + offset, value)?;
        }
    }
    Ok(vec![RawValue::Table(dst)])
}
