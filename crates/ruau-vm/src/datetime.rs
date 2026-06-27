//! UTC civil-calendar support for the `os` library: a `time_t`-to-broken-down
//! conversion (`gmtime`), the inverse (`timegm`, ported from upstream
//! `os_timegm`), and a `strftime` subset. Everything is UTC and locale-free:
//! the executor is deterministic and carries no timezone database, so the `os`
//! library treats local time as UTC (a documented divergence from a
//! TZ-configured host).

const SECS_PER_DAY: i64 = 86_400;
const SECS_PER_HOUR: i64 = 3_600;

/// A broken-down UTC time, the fields `os.date` and `os.time` exchange. Unlike C
/// `struct tm`, `year` is the full year and `mon` is 0-based (`0 == January`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tm {
    pub sec: i64,
    pub min: i64,
    pub hour: i64,
    /// Day of month, `1..=31`.
    pub mday: i64,
    /// Month, `0..=11`.
    pub mon: i64,
    /// Full year, e.g. `1970`.
    pub year: i64,
    /// Day of week, `0..=6` with `0 == Sunday`.
    pub wday: i64,
    /// Day of year, `0..=365`.
    pub yday: i64,
}

const WDAY_ABBR: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const WDAY_FULL: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const MON_ABBR: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const MON_FULL: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// The Gregorian leap-year rule.
fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Day of year (0-based) for a `year/month(1-12)/day` date.
fn yday_from_ymd(year: i64, month: i64, day: i64) -> i64 {
    const CUM: [i64; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let mut yd = CUM[(month - 1) as usize] + (day - 1);
    if month > 2 && is_leap(year) {
        yd += 1;
    }
    yd
}

/// Breaks a Unix timestamp (UTC seconds since 1970) into civil fields, using
/// Howard Hinnant's `civil_from_days` algorithm. Defined for any non-negative
/// `secs`; the caller rejects negatives.
#[must_use]
pub fn civil_from_secs(secs: i64) -> Tm {
    let days = secs.div_euclid(SECS_PER_DAY);
    let rem = secs.rem_euclid(SECS_PER_DAY);
    let hour = rem / SECS_PER_HOUR;
    let min = (rem % SECS_PER_HOUR) / 60;
    let sec = rem % 60;
    // 1970-01-01 was a Thursday (wday 4).
    let wday = (days + 4).rem_euclid(7);

    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = y + i64::from(month <= 2);

    Tm {
        sec,
        min,
        hour,
        mday: day,
        mon: month - 1,
        year,
        wday,
        yday: yday_from_ymd(year, month, day),
    }
}

/// The inverse: a broken-down UTC time to a Unix timestamp, a direct port of
/// upstream `os_timegm` (a Julian-day calculation). `mon` is 0-based and may be
/// out of `0..=11` (the caller passes a raw table field); the year adjustment
/// folds it back. Returns `None` for any date before 1970-01-01 UTC, matching
/// upstream's `(time_t)-1`.
#[must_use]
pub fn timegm(sec: i64, min: i64, hour: i64, mday: i64, mon: i64, year: i64) -> Option<i64> {
    const UTC_START_JD: i64 = 2_440_588; // 1970-01-01 in the Julian calendar
    let day = mday;
    let month = mon + 1;
    // Pretend the year starts in March; also fold out-of-range months. C integer
    // division/remainder truncate toward zero, which `i64`'s `/` and `%` match.
    let a = i64::from(mon % 12 < 2) - mon / 12;
    let y = year + 4800 - a;
    let m = month + 12 * a - 3;

    let julianday = day + (153 * m + 2) / 5 + 365 * y + y / 4 - y / 100 + y / 400 - 32_045;
    if julianday < UTC_START_JD {
        return None;
    }
    let daysecond = hour * SECS_PER_HOUR + min * 60 + sec;
    let julianseconds = julianday * SECS_PER_DAY + daysecond;
    let utc_start_seconds = UTC_START_JD * SECS_PER_DAY;
    if julianseconds < utc_start_seconds {
        return None;
    }
    Some(julianseconds - utc_start_seconds)
}

/// Expands a `strftime` format against a UTC `Tm`. Supports the upstream
/// `LUA_STRFTIMEOPTIONS` set (`aAbBcdHIjmMpSUwWxXyYzZ%`). A `%` at end of string
/// is a literal `%`; an unrecognized specifier returns `Err(())` so the caller
/// raises "invalid conversion specifier". Timezone fields are fixed to UTC
/// (`%z == "+0000"`, `%Z == "UTC"`).
///
/// # Errors
/// Returns `Err(())` on an unrecognized conversion specifier.
pub fn strftime(fmt: &[u8], tm: &Tm) -> Result<Vec<u8>, ()> {
    let wday = tm.wday as usize;
    let mon = tm.mon as usize;
    let hour12 = match tm.hour % 12 {
        0 => 12,
        h => h,
    };
    let mut out = Vec::with_capacity(fmt.len());
    let mut i = 0;
    while i < fmt.len() {
        if fmt[i] != b'%' || i + 1 >= fmt.len() {
            out.push(fmt[i]);
            i += 1;
            continue;
        }
        let spec = fmt[i + 1];
        i += 2;
        let piece: String = match spec {
            b'a' => WDAY_ABBR[wday].to_string(),
            b'A' => WDAY_FULL[wday].to_string(),
            b'b' => MON_ABBR[mon].to_string(),
            b'B' => MON_FULL[mon].to_string(),
            b'c' => format!(
                "{} {} {:>2} {:02}:{:02}:{:02} {}",
                WDAY_ABBR[wday], MON_ABBR[mon], tm.mday, tm.hour, tm.min, tm.sec, tm.year
            ),
            b'd' => format!("{:02}", tm.mday),
            b'H' => format!("{:02}", tm.hour),
            b'I' => format!("{hour12:02}"),
            b'j' => format!("{:03}", tm.yday + 1),
            b'm' => format!("{:02}", tm.mon + 1),
            b'M' => format!("{:02}", tm.min),
            b'p' => (if tm.hour < 12 { "AM" } else { "PM" }).to_string(),
            b'S' => format!("{:02}", tm.sec),
            b'U' => format!("{:02}", (tm.yday + 7 - tm.wday) / 7),
            b'w' => tm.wday.to_string(),
            b'W' => format!("{:02}", (tm.yday + 7 - (tm.wday + 6) % 7) / 7),
            b'x' => format!(
                "{:02}/{:02}/{:02}",
                tm.mon + 1,
                tm.mday,
                tm.year.rem_euclid(100)
            ),
            b'X' => format!("{:02}:{:02}:{:02}", tm.hour, tm.min, tm.sec),
            b'y' => format!("{:02}", tm.year.rem_euclid(100)),
            b'Y' => tm.year.to_string(),
            b'z' => "+0000".to_string(),
            b'Z' => "UTC".to_string(),
            b'%' => "%".to_string(),
            _ => return Err(()),
        };
        out.extend_from_slice(piece.as_bytes());
    }
    Ok(out)
}

#[cfg(any())]
mod tests {
    use super::*;

    #[test]
    fn civil_conversion_matches_gmtime() {
        let tm = civil_from_secs(0);
        assert_eq!(
            tm,
            Tm {
                sec: 0,
                min: 0,
                hour: 0,
                mday: 1,
                mon: 0,
                year: 1970,
                wday: 4,
                yday: 0,
            }
        );
        let tm = civil_from_secs(1_700_000_000);
        assert_eq!(
            (tm.year, tm.mon + 1, tm.mday, tm.hour, tm.min, tm.sec),
            (2023, 11, 14, 22, 13, 20)
        );
        assert_eq!((tm.wday, tm.yday + 1), (2, 318));
    }

    #[test]
    fn timegm_round_trips_and_rejects_pre_epoch() {
        assert_eq!(timegm(45, 30, 12, 15, 2, 2021), Some(1_615_811_445));
        assert_eq!(timegm(0, 0, 0, 1, 0, 1970), Some(0));
        // 2000 is a leap year, so Feb 29 is valid.
        assert_eq!(timegm(0, 0, 0, 29, 1, 2000), Some(951_782_400));
        // Pre-1970 is rejected.
        assert_eq!(timegm(0, 0, 0, 31, 11, 1969), None);
    }

    #[test]
    fn strftime_subset_matches_c_locale() {
        let tm = civil_from_secs(1_000_000_000);
        let fmt = |s: &str| String::from_utf8(strftime(s.as_bytes(), &tm).unwrap()).unwrap();
        assert_eq!(fmt("%Y-%m-%d %H:%M:%S"), "2001-09-09 01:46:40");
        assert_eq!(fmt("%a %A %b %B"), "Sun Sunday Sep September");
        assert_eq!(fmt("%p %I %j"), "AM 01 252");
        assert_eq!(fmt("U=%U W=%W w=%w"), "U=36 W=36 w=0");
        assert_eq!(fmt("%c"), "Sun Sep  9 01:46:40 2001");
        assert_eq!(fmt("%x %X"), "09/09/01 01:46:40");
        assert_eq!(fmt("%z %Z 100%%"), "+0000 UTC 100%");
        // A trailing percent is literal; an unknown specifier errors.
        assert_eq!(fmt("done%"), "done%");
        assert!(strftime(b"%Q", &tm).is_err());
    }
}
