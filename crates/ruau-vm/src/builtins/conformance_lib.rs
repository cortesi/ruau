use super::*;

pub(super) fn dispatch(
    builtin: Builtin,
    heap: &mut Heap,
    thread: &mut Thread,
    args: &[RawValue],
    host_entry: crate::scope::HostEntry<'_>,
) -> Exec<Vec<RawValue>> {
    match builtin {
        Builtin::ConformanceGetCoverage => conformance_getcoverage(heap, args),
        Builtin::ConformanceResumeError => {
            crate::coroutine::resume_error(heap, thread, args, host_entry)
        }
        Builtin::ConformanceSetBlockAllocations => conformance_set_block_allocations(heap, args),
        Builtin::ConformanceSingleYield
        | Builtin::ConformanceMultipleYields
        | Builtin::ConformanceMultipleYieldsWithNestedCall
        | Builtin::ConformancePassthroughCall
        | Builtin::ConformancePassthroughCallMoreResults
        | Builtin::ConformancePassthroughCallArgReuse
        | Builtin::ConformancePassthroughCallVaradic
        | Builtin::ConformancePassthroughCallWithState => {
            Err(err("conformance yield helper must be called from bytecode"))
        }
        _ => unreachable!("non-conformance builtin routed to conformance_lib"),
    }
}

fn conformance_getcoverage(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let closure = match args.first().copied().unwrap_or(RawValue::Nil) {
        RawValue::Function(closure) => closure,
        _ => return Err(err("bad argument #1 to 'getcoverage' (function expected)")),
    };
    let proto = heap
        .closure(closure)
        .map(|closure| closure.proto)
        .ok_or_else(|| err("bad argument #1 to 'getcoverage' (function expected)"))?;
    let result = heap
        .alloc_table(LuaTable::new())
        .ok_or_else(|| err_memory("out of memory in 'getcoverage'"))?;
    let mut index = 1u32;
    push_conformance_coverage(heap, result, proto, 0, &mut index)?;
    Ok(vec![RawValue::Table(result)])
}

fn push_conformance_coverage(
    heap: &mut Heap,
    result: RawGc<marker::Table>,
    proto: RawGc<Proto>,
    depth: u32,
    index: &mut u32,
) -> Exec<()> {
    let stats = conformance_coverage_table(heap, proto, depth)?;
    heap.table_mut(result)
        .ok_or_else(|| err_memory("out of memory in 'getcoverage'"))?
        .set(RawValue::Number(f64::from(*index)), RawValue::Table(stats));
    *index = index.saturating_add(1);

    let children = heap
        .proto(proto)
        .map(|proto| proto.child_protos().to_vec())
        .ok_or_else(|| err("getcoverage target is not resident"))?;
    for child in children {
        push_conformance_coverage(heap, result, child, depth + 1, index)?;
    }
    Ok(())
}

fn conformance_coverage_table(
    heap: &mut Heap,
    proto: RawGc<Proto>,
    depth: u32,
) -> Exec<RawGc<marker::Table>> {
    let (debug_name, line_defined, coverage) = {
        let proto = heap
            .proto(proto)
            .ok_or_else(|| err("getcoverage target is not resident"))?;
        let debug_name = proto
            .debug_name
            .and_then(|name| heap.string(name).map(|name| name.bytes().to_vec()));
        let mut coverage: Vec<(u32, u32)> = Vec::new();
        for (line, hits) in proto.coverage().filter(|(line, _)| *line > 0) {
            if let Some((_, total)) = coverage.iter_mut().find(|(seen, _)| *seen == line) {
                *total = total.saturating_add(hits);
            } else {
                coverage.push((line, hits));
            }
        }
        (debug_name, proto.line_defined, coverage)
    };

    let table = heap
        .alloc_table(LuaTable::new())
        .ok_or_else(|| err_memory("out of memory in 'getcoverage'"))?;
    if let Some(name) = debug_name {
        let value = RawValue::String(
            heap.intern_str(&name)
                .ok_or_else(|| err_memory("out of memory in 'getcoverage'"))?,
        );
        set_table_member(heap, table, b"name", value)?;
    }
    set_table_member(
        heap,
        table,
        b"linedefined",
        RawValue::Number(f64::from(line_defined)),
    )?;
    set_table_member(heap, table, b"depth", RawValue::Number(f64::from(depth)))?;
    for (line, hits) in coverage {
        heap.table_mut(table)
            .ok_or_else(|| err_memory("out of memory in 'getcoverage'"))?
            .set(
                RawValue::Number(f64::from(line)),
                RawValue::Number(f64::from(hits)),
            );
    }
    Ok(table)
}

fn set_table_member(
    heap: &mut Heap,
    table: RawGc<marker::Table>,
    name: &[u8],
    value: RawValue,
) -> Exec<()> {
    let key = RawValue::String(
        heap.intern_str(name)
            .ok_or_else(|| err_memory("out of memory in 'getcoverage'"))?,
    );
    heap.table_mut(table)
        .ok_or_else(|| err_memory("out of memory in 'getcoverage'"))?
        .set(key, value);
    Ok(())
}

/// Harness-only `setblockallocations(bool)`: accepts upstream's GC robustness
/// helper toggle. Ruau's collector allocation-failure recovery is exercised by
/// Rust-side fault injection; the conformance shim keeps the upstream script
/// verbatim without exposing allocator controls to tenants.
fn conformance_set_block_allocations(_heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let RawValue::Boolean(_block) = args.first().copied().unwrap_or(RawValue::Nil) else {
        return Err(err(
            "bad argument #1 to 'setblockallocations' (boolean expected)",
        ));
    };
    Ok(Vec::new())
}
