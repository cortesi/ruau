//! The Lua string-pattern matcher — a port of `lstrlib.cpp`'s `match` engine.
//!
//! The matcher is recursive (greedy `*`/`+`, lazy `-`, optional `?`, character
//! classes, sets, captures, `%b`/`%f`, back-references). Two adversarial bounds
//! keep it bounded for untrusted input: a recursion-depth cap (deep alternation)
//! and a per-call **step budget** (catastrophic backtracking / ReDoS), both
//! surfaced as ordinary runtime errors rather than a hang or a stack overflow.

use crate::call::{Exec, err};

const L_ESC: u8 = b'%';
const CAP_UNFINISHED: isize = -1;
const CAP_POSITION: isize = -2;

#[derive(Clone, Copy)]
pub struct PatternLimits {
    pub max_steps: u32,
    pub max_depth: u32,
    pub max_captures: usize,
}

/// One capture in a match: a byte range of the source, or a position capture.
#[derive(Clone, Copy, Debug)]
pub enum Capture {
    /// A captured substring `src[start..start+len]`.
    Bytes { start: usize, len: usize },
    /// A position capture `()`, the 1-based byte position.
    Position(usize),
}

/// A successful match: the matched byte range and its captures.
pub struct MatchResult {
    pub start: usize,
    pub end: usize,
    pub captures: Vec<Capture>,
}

struct Matcher<'a, 'b> {
    src: &'a [u8],
    pat: &'a [u8],
    /// `(init, len)` per open/closed capture; `len` is `CAP_UNFINISHED`/`CAP_POSITION`.
    caps: Vec<(usize, isize)>,
    /// The step counter, *shared across every scan position* of one library call,
    /// so the budget bounds the whole `find`/`gsub`/`match` call rather than each
    /// attempt (a per-attempt budget would be `O(n × max_steps)`).
    steps: &'b mut u32,
    limits: PatternLimits,
    depth: u32,
}

impl<'a, 'b> Matcher<'a, 'b> {
    fn new(src: &'a [u8], pat: &'a [u8], steps: &'b mut u32, limits: PatternLimits) -> Self {
        Self {
            src,
            pat,
            caps: Vec::new(),
            steps,
            limits,
            depth: 0,
        }
    }

    /// Spends one step of the budget, erroring when it is exhausted (the ReDoS
    /// guard). Called on every backtracking decision, not only at `do_match`.
    fn tick(&mut self) -> Exec<()> {
        *self.steps += 1;
        if *self.steps > self.limits.max_steps {
            return Err(err("pattern match is too complex"));
        }
        Ok(())
    }

    /// The pattern index just past the single class/set/literal starting at `p`.
    fn class_end(&self, p: usize) -> Exec<usize> {
        let mut p = p;
        let c = self.pat[p];
        p += 1;
        match c {
            L_ESC => {
                if p >= self.pat.len() {
                    return Err(err("malformed pattern (ends with '%')"));
                }
                Ok(p + 1)
            }
            b'[' => {
                if self.pat.get(p) == Some(&b'^') {
                    p += 1;
                }
                loop {
                    if p >= self.pat.len() {
                        return Err(err("malformed pattern (missing ']')"));
                    }
                    let cc = self.pat[p];
                    p += 1;
                    if cc == L_ESC && p < self.pat.len() {
                        p += 1;
                    }
                    if p < self.pat.len() && self.pat[p] == b']' {
                        return Ok(p + 1);
                    }
                }
            }
            _ => Ok(p),
        }
    }

    /// Whether byte `c` matches the single-character class `cl` (`%a`/`%d`/...; an
    /// uppercase class is the complement; a non-letter `cl` matches literally).
    fn match_class(c: u8, cl: u8) -> bool {
        let lower = cl.to_ascii_lowercase();
        let res = match lower {
            b'a' => c.is_ascii_alphabetic(),
            b'c' => c.is_ascii_control(),
            b'd' => c.is_ascii_digit(),
            b'g' => c.is_ascii_graphic(),
            b'l' => c.is_ascii_lowercase(),
            b'p' => c.is_ascii_punctuation(),
            b's' => c == b' ' || (0x09..=0x0d).contains(&c),
            b'u' => c.is_ascii_uppercase(),
            b'w' => c.is_ascii_alphanumeric(),
            b'x' => c.is_ascii_hexdigit(),
            // Deprecated Lua 5.1 class kept by Luau: `%z` is the zero byte, so
            // `%Z` (via the uppercase-negation below) is any non-zero byte.
            b'z' => c == 0,
            _ => return cl == c,
        };
        // An uppercase class name negates the test.
        if cl.is_ascii_uppercase() { !res } else { res }
    }

    /// Whether byte `c` is in the set `pat[p..ec]` (`p` at `[`, `ec` at `]`).
    fn match_bracket_class(&self, c: u8, p: usize, ec: usize) -> bool {
        let mut p = p + 1; // past '['
        let mut sig = true;
        if self.pat.get(p) == Some(&b'^') {
            sig = false;
            p += 1;
        }
        while p < ec {
            if self.pat[p] == L_ESC {
                p += 1;
                if Self::match_class(c, self.pat[p]) {
                    return sig;
                }
                p += 1;
            } else if p + 2 < ec && self.pat[p + 1] == b'-' {
                if self.pat[p] <= c && c <= self.pat[p + 2] {
                    return sig;
                }
                p += 3;
            } else {
                if self.pat[p] == c {
                    return sig;
                }
                p += 1;
            }
        }
        !sig
    }

    /// Whether the source byte at `s` matches the single pattern item `pat[p..ep]`.
    fn single_match(&self, s: usize, p: usize, ep: usize) -> bool {
        let Some(&c) = self.src.get(s) else {
            return false;
        };
        match self.pat[p] {
            b'.' => true,
            L_ESC => Self::match_class(c, self.pat[p + 1]),
            b'[' => self.match_bracket_class(c, p, ep - 1),
            other => other == c,
        }
    }

    /// `%b xy`: a balanced run from `x` (at `s`) to its matching `y`.
    fn match_balance(&mut self, s: usize, p: usize) -> Exec<Option<usize>> {
        if p + 1 >= self.pat.len() {
            return Err(err("malformed pattern (missing arguments to '%b')"));
        }
        if self.src.get(s) != Some(&self.pat[p]) {
            return Ok(None);
        }
        let (open, close) = (self.pat[p], self.pat[p + 1]);
        let mut cont = 1i32;
        let mut s = s + 1;
        while s < self.src.len() {
            self.tick()?;
            if self.src[s] == close {
                cont -= 1;
                if cont == 0 {
                    return Ok(Some(s + 1));
                }
            } else if self.src[s] == open {
                cont += 1;
            }
            s += 1;
        }
        Ok(None)
    }

    /// Greedy `*`/`+`: match as many items as possible, then back off.
    fn max_expand(&mut self, s: usize, p: usize, ep: usize) -> Exec<Option<usize>> {
        let mut count = 0usize;
        while self.single_match(s + count, p, ep) {
            self.tick()?; // the linear greedy scan is charged too, so a huge `.*` counts
            count += 1;
        }
        loop {
            if let Some(end) = self.do_match(s + count, ep + 1)? {
                return Ok(Some(end));
            }
            if count == 0 {
                return Ok(None);
            }
            count -= 1;
        }
    }

    /// Lazy `-`: match as few items as possible.
    fn min_expand(&mut self, s: usize, p: usize, ep: usize) -> Exec<Option<usize>> {
        let mut s = s;
        loop {
            if let Some(end) = self.do_match(s, ep + 1)? {
                return Ok(Some(end));
            }
            if self.single_match(s, p, ep) {
                s += 1;
            } else {
                return Ok(None);
            }
        }
    }

    fn start_capture(&mut self, s: usize, p: usize, what: isize) -> Exec<Option<usize>> {
        if self.caps.len() >= self.limits.max_captures {
            return Err(err("too many captures in pattern"));
        }
        self.caps.push((s, what));
        let res = self.do_match(s, p)?;
        if res.is_none() {
            self.caps.pop();
        }
        Ok(res)
    }

    fn end_capture(&mut self, s: usize, p: usize) -> Exec<Option<usize>> {
        let l = self
            .caps
            .iter()
            .rposition(|&(_, len)| len == CAP_UNFINISHED)
            .ok_or_else(|| err("invalid pattern capture"))?;
        self.caps[l].1 = (s - self.caps[l].0) as isize;
        let res = self.do_match(s, p)?;
        if res.is_none() {
            self.caps[l].1 = CAP_UNFINISHED;
        }
        Ok(res)
    }

    /// `%1`..`%9`: match the literal text of a previously closed capture.
    fn match_capture(&self, s: usize, idx: usize) -> Exec<Option<usize>> {
        if idx >= self.caps.len() || self.caps[idx].1 == CAP_UNFINISHED {
            return Err(err("invalid capture index in pattern"));
        }
        let (start, len) = self.caps[idx];
        let len = len as usize;
        if self.src.len() - s >= len && self.src[start..start + len] == self.src[s..s + len] {
            Ok(Some(s + len))
        } else {
            Ok(None)
        }
    }

    fn do_match(&mut self, s: usize, p: usize) -> Exec<Option<usize>> {
        self.tick()?;
        self.depth += 1;
        if self.depth > self.limits.max_depth {
            self.depth -= 1;
            return Err(err("pattern too complex (too deep)"));
        }
        let result = self.match_inner(s, p);
        self.depth -= 1;
        result
    }

    fn match_inner(&mut self, mut s: usize, mut p: usize) -> Exec<Option<usize>> {
        loop {
            if p >= self.pat.len() {
                return Ok(Some(s));
            }
            match self.pat[p] {
                b'(' => {
                    if self.pat.get(p + 1) == Some(&b')') {
                        return self.start_capture(s, p + 2, CAP_POSITION);
                    }
                    return self.start_capture(s, p + 1, CAP_UNFINISHED);
                }
                b')' => return self.end_capture(s, p + 1),
                b'$' if p + 1 == self.pat.len() => {
                    return Ok((s == self.src.len()).then_some(s));
                }
                L_ESC if matches!(self.pat.get(p + 1), Some(b'b')) => {
                    match self.match_balance(s, p + 2)? {
                        Some(news) => {
                            s = news;
                            p += 4;
                            continue;
                        }
                        None => return Ok(None),
                    }
                }
                L_ESC if matches!(self.pat.get(p + 1), Some(b'f')) => {
                    p += 2;
                    if self.pat.get(p) != Some(&b'[') {
                        return Err(err("missing '[' after '%f' in pattern"));
                    }
                    let ep = self.class_end(p)?;
                    let prev = if s == 0 { 0u8 } else { self.src[s - 1] };
                    let cur = self.src.get(s).copied().unwrap_or(0);
                    if !self.match_bracket_class(prev, p, ep - 1)
                        && self.match_bracket_class(cur, p, ep - 1)
                    {
                        p = ep;
                        continue;
                    }
                    return Ok(None);
                }
                L_ESC if matches!(self.pat.get(p + 1), Some(b'0'..=b'9')) => {
                    // Back-references are `%1`..`%9`. `%0` would underflow the
                    // `- b'1'` index; upstream computes the index signed and
                    // rejects it (`check_capture`), so treat it as invalid here.
                    if self.pat[p + 1] == b'0' {
                        return Err(err("invalid capture index in pattern"));
                    }
                    let idx = (self.pat[p + 1] - b'1') as usize;
                    match self.match_capture(s, idx)? {
                        Some(news) => {
                            s = news;
                            p += 2;
                            continue;
                        }
                        None => return Ok(None),
                    }
                }
                _ => {
                    let ep = self.class_end(p)?;
                    if self.single_match(s, p, ep) {
                        match self.pat.get(ep) {
                            Some(b'?') => {
                                if let Some(end) = self.do_match(s + 1, ep + 1)? {
                                    return Ok(Some(end));
                                }
                                p = ep + 1;
                                continue;
                            }
                            Some(b'+') => return self.max_expand(s + 1, p, ep),
                            Some(b'*') => return self.max_expand(s, p, ep),
                            Some(b'-') => return self.min_expand(s, p, ep),
                            _ => {
                                s += 1;
                                p = ep;
                                continue;
                            }
                        }
                    } else if matches!(self.pat.get(ep), Some(b'*' | b'?' | b'-')) {
                        p = ep + 1;
                        continue;
                    } else {
                        return Ok(None);
                    }
                }
            }
        }
    }

    /// The explicit captures of a completed match (empty when the pattern has
    /// none — the caller substitutes the whole match). Errors if a capture is
    /// still open (a `(` with no `)`), like upstream's "unfinished capture".
    fn collect(&self) -> Exec<Vec<Capture>> {
        self.caps
            .iter()
            .map(|&(init, len)| {
                if len == CAP_UNFINISHED {
                    Err(err("unfinished capture in pattern"))
                } else if len == CAP_POSITION {
                    Ok(Capture::Position(init + 1))
                } else {
                    Ok(Capture::Bytes {
                        start: init,
                        len: len.max(0) as usize,
                    })
                }
            })
            .collect()
    }
}

/// Finds the first match of `pat` in `src` at or after byte `init`. A leading `^`
/// anchors the match to `init`. `steps` is the per-call budget the caller carries
/// across positions; `limits` carries the invocation's pattern ceilings.
pub fn find(
    src: &[u8],
    pat: &[u8],
    init: usize,
    steps: &mut u32,
    limits: PatternLimits,
) -> Exec<Option<MatchResult>> {
    let (anchored, pstart) = if pat.first() == Some(&b'^') {
        (true, 1)
    } else {
        (false, 0)
    };
    // One matcher reused across every start position, so the step budget is spent
    // over the whole scan rather than reset per attempt.
    let mut matcher = Matcher::new(src, pat, steps, limits);
    let mut start = init.min(src.len());
    loop {
        matcher.caps.clear();
        matcher.depth = 0;
        if let Some(end) = matcher.do_match(start, pstart)? {
            return Ok(Some(MatchResult {
                start,
                end,
                captures: matcher.collect()?,
            }));
        }
        if anchored || start >= src.len() {
            return Ok(None);
        }
        start += 1;
    }
}

/// Matches `pat` at exactly byte `pos` (no scanning). `pat` must already be
/// de-anchored by the caller (the leading `^` stripped). Used by `gsub`, which
/// carries `steps` across replacements so the budget is per-call.
pub fn match_at(
    src: &[u8],
    pat: &[u8],
    pos: usize,
    steps: &mut u32,
    limits: PatternLimits,
) -> Exec<Option<MatchResult>> {
    let mut matcher = Matcher::new(src, pat, steps, limits);
    match matcher.do_match(pos, 0)? {
        Some(end) => Ok(Some(MatchResult {
            start: pos,
            end,
            captures: matcher.collect()?,
        })),
        None => Ok(None),
    }
}

#[cfg(any())]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const GENEROUS: PatternLimits = PatternLimits {
        max_steps: 1_000_000,
        max_depth: 200,
        max_captures: 32,
    };
    const TINY: PatternLimits = PatternLimits {
        max_steps: 64,
        max_depth: 200,
        max_captures: 32,
    };

    /// Byte alphabet biased toward pattern metacharacters, so generated
    /// patterns exercise classes, sets, quantifiers, anchors, and invalid
    /// shapes alike.
    const PATTERN_BYTES: &[u8] = b"ab0%.*+-?[]()^$wdsc";

    fn comparable(result: &MatchResult) -> (usize, usize, String) {
        (result.start, result.end, format!("{:?}", result.captures))
    }

    proptest! {
        /// Arbitrary (often invalid) patterns never panic, and a successful
        /// match stays inside the subject.
        #[test]
        fn find_never_panics_and_stays_in_bounds(
            src in proptest::collection::vec(0x20u8..0x7f, 0..48),
            pat in proptest::collection::vec(proptest::sample::select(PATTERN_BYTES), 0..12),
        ) {
            let mut steps = 0;
            if let Ok(Some(result)) = find(&src, &pat, 0, &mut steps, GENEROUS) {
                prop_assert!(result.start <= result.end);
                prop_assert!(result.end <= src.len());
            }
        }

        /// A step-limited run agrees with the unlimited run whenever it
        /// completes; the budget may only turn a verdict into a clean error,
        /// never into a different verdict.
        #[test]
        fn step_limited_find_agrees_or_errors(
            src in proptest::collection::vec(0x20u8..0x7f, 0..48),
            pat in proptest::collection::vec(proptest::sample::select(PATTERN_BYTES), 0..12),
        ) {
            let mut unlimited_steps = 0;
            let unlimited = find(&src, &pat, 0, &mut unlimited_steps, GENEROUS);
            let mut limited_steps = 0;
            let limited = find(&src, &pat, 0, &mut limited_steps, TINY);
            match (unlimited, limited) {
                (Ok(a), Ok(b)) => match (a, b) {
                    (Some(a), Some(b)) => prop_assert_eq!(comparable(&a), comparable(&b)),
                    (None, None) => {}
                    (a, b) => prop_assert!(
                        false,
                        "verdicts diverge: unlimited {:?} vs limited {:?}",
                        a.map(|m| comparable(&m)),
                        b.map(|m| comparable(&m)),
                    ),
                },
                // The tiny budget (or an invalid pattern) may error; an
                // unlimited error must not become a limited success.
                (Ok(_) | Err(_), Err(_)) => {}
                (Err(_), Ok(_)) => prop_assert!(
                    false,
                    "limited run succeeded where the unlimited run errored"
                ),
            }
        }

        /// For plain literal patterns the matcher is exactly naive substring
        /// search (the reference for the metacharacter-free subset).
        #[test]
        fn literal_patterns_match_naive_search(
            src in proptest::collection::vec(b'a'..=b'e', 0..48),
            pat in proptest::collection::vec(b'a'..=b'e', 1..6),
        ) {
            let mut steps = 0;
            let found = find(&src, &pat, 0, &mut steps, GENEROUS)
                .expect("literal patterns are valid");
            let naive = src
                .windows(pat.len())
                .position(|window| window == pat.as_slice());
            match (found, naive) {
                (Some(result), Some(index)) => {
                    prop_assert_eq!(result.start, index);
                    prop_assert_eq!(result.end, index + pat.len());
                    prop_assert!(result.captures.is_empty());
                }
                (None, None) => {}
                (found, naive) => prop_assert!(
                    false,
                    "literal find disagrees with naive search: {:?} vs {:?}",
                    found.map(|m| comparable(&m)),
                    naive,
                ),
            }
        }
    }
}
