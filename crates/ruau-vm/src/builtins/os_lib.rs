use super::*;

pub(super) fn dispatch(
    builtin: Builtin,
    heap: &mut Heap,
    args: &[RawValue],
) -> Exec<Vec<RawValue>> {
    match builtin {
        Builtin::OsTime => os_time(heap, args),
        Builtin::OsClock => os_clock(heap, args),
        Builtin::OsDate => os_date(heap, args),
        Builtin::OsDifftime => os_difftime(args),
        _ => unreachable!("non-os builtin routed to os_lib"),
    }
}

fn os_number_arg(args: &[RawValue], index: usize, name: &str) -> Exec<f64> {
    num_arg(args, index, |index, _| {
        format!(
            "bad argument #{} to 'os.{name}' (number expected)",
            index + 1
        )
    })
}

/// `os.clock()`: monotonic process seconds since the VM was built (frozen at
/// `0.0` under the deterministic seam).
fn os_clock(heap: &Heap, _args: &[RawValue]) -> Exec<Vec<RawValue>> {
    Ok(vec![RawValue::Number(heap.process_clock_secs())])
}

/// `os.difftime(t2, t1?)`: `t2 - t1` in seconds (`t1` defaults to `0`).
fn os_difftime(args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let t2 = os_number_arg(args, 0, "difftime")?;
    let t1 = match args.get(1).copied() {
        None | Some(RawValue::Nil) => 0.0,
        Some(_) => os_number_arg(args, 1, "difftime")?,
    };
    Ok(vec![RawValue::Number(t2 - t1)])
}

/// `os.time(table?)`: the current wall-clock time in seconds, or the UTC
/// timestamp of a `{year, month, day, hour?, min?, sec?}` table. Like upstream
/// this uses `timegm` (UTC), not the host's local zone; a pre-1970 date is `nil`.
fn os_time(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    match args.first().copied().unwrap_or(RawValue::Nil) {
        RawValue::Nil => Ok(vec![RawValue::Number(heap.wall_time_secs())]),
        RawValue::Table(handle) => {
            let sec = date_field(heap, handle, b"sec", Some(0))?;
            let min = date_field(heap, handle, b"min", Some(0))?;
            let hour = date_field(heap, handle, b"hour", Some(12))?;
            let day = date_field(heap, handle, b"day", None)?;
            let month = date_field(heap, handle, b"month", None)?;
            let year = date_field(heap, handle, b"year", None)?;
            let stamp = datetime::timegm(sec, min, hour, day, month - 1, year);
            Ok(vec![
                stamp.map_or(RawValue::Nil, |t| RawValue::Number(t as f64)),
            ])
        }
        _ => Err(err("bad argument #1 to 'os.time' (table expected)")),
    }
}

/// `os.date(format?, time?)`: formats `time` (default: now) as UTC. `"*t"` (and
/// the UTC-explicit `"!*t"`) return a broken-down table; any other format runs
/// through `strftime`. The executor carries no timezone database, so local time
/// is treated as UTC.
fn os_date(heap: &mut Heap, args: &[RawValue]) -> Exec<Vec<RawValue>> {
    let fmt = match args.first().copied().unwrap_or(RawValue::Nil) {
        RawValue::Nil => b"%c".to_vec(),
        RawValue::String(s) => heap.string(s).map_or_else(Vec::new, |s| s.bytes().to_vec()),
        _ => return Err(err("bad argument #1 to 'os.date' (string expected)")),
    };
    let time = match args.get(1).copied() {
        None | Some(RawValue::Nil) => heap.wall_time_secs(),
        Some(_) => os_number_arg(args, 1, "date")?,
    };
    // A leading '!' selects UTC; we are always UTC, so it only governs the "*t"
    // comparison below.
    let stripped = fmt.strip_prefix(b"!").unwrap_or(&fmt);
    // Pre-1970 dates are unsupported, like upstream's local-time path.
    let secs = time as i64;
    if secs < 0 {
        return Ok(vec![RawValue::Nil]);
    }
    let tm = datetime::civil_from_secs(secs);
    if stripped == b"*t" {
        return Ok(vec![RawValue::Table(build_date_table(heap, &tm)?)]);
    }
    let formatted = datetime::strftime(stripped, &tm)
        .map_err(|()| err("bad argument #1 to 'os.date' (invalid conversion specifier)"))?;
    let interned = heap
        .intern_str(&formatted)
        .ok_or_else(|| err_memory("out of memory for 'os.date'"))?;
    Ok(vec![RawValue::String(interned)])
}

/// Reads an integer field from a date table for `os.time`, truncating a number to
/// a C `int` like upstream `getfield` (the `(int)` cast). Truncating to 32 bits
/// also keeps the `timegm` Julian-day arithmetic from overflowing i64 on an
/// absurd `year`. A missing field (or a present non-number) falls back to
/// `default`; with no default it errors ("field '<key>' missing").
fn date_field(
    heap: &mut Heap,
    handle: RawGc<marker::Table>,
    key: &[u8],
    default: Option<i64>,
) -> Exec<i64> {
    let key_value = RawValue::String(
        heap.intern_str(key)
            .ok_or_else(|| err_memory("out of memory in 'os.time'"))?,
    );
    let value = heap
        .table(handle)
        .map_or(RawValue::Nil, |t| t.get(key_value));
    match value {
        RawValue::Number(n) => Ok(i64::from(n as i32)),
        RawValue::Integer(i) => Ok(i64::from(i as i32)),
        _ => default.ok_or_else(|| {
            err(format!(
                "field '{}' missing in date table",
                String::from_utf8_lossy(key)
            ))
        }),
    }
}

/// Builds the broken-down table `os.date("*t")` returns.
fn build_date_table(heap: &mut Heap, tm: &datetime::Tm) -> Exec<RawGc<marker::Table>> {
    let table = heap
        .alloc_table(LuaTable::new())
        .ok_or_else(|| err_memory("out of memory for 'os.date'"))?;
    for (name, value) in [
        (b"sec".as_slice(), tm.sec),
        (b"min", tm.min),
        (b"hour", tm.hour),
        (b"day", tm.mday),
        (b"month", tm.mon + 1),
        (b"year", tm.year),
        (b"wday", tm.wday + 1),
        (b"yday", tm.yday + 1),
    ] {
        let key = RawValue::String(
            heap.intern_str(name)
                .ok_or_else(|| err_memory("out of memory for 'os.date'"))?,
        );
        heap.table_mut(table)
            .ok_or_else(|| err("table not resident"))?
            .set(key, RawValue::Number(value as f64));
    }
    let isdst = RawValue::String(
        heap.intern_str(b"isdst")
            .ok_or_else(|| err_memory("out of memory for 'os.date'"))?,
    );
    heap.table_mut(table)
        .ok_or_else(|| err("table not resident"))?
        .set(isdst, RawValue::Boolean(false));
    Ok(table)
}
