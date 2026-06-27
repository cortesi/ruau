//! The `string.pack`/`unpack`/`packsize` binary format engine — a port of
//! upstream `lstrlib.cpp`'s pack machinery (the `Header`, the `getoption`/
//! `getdetails` format parser, and `packint`/`unpackint`). This module is the
//! pure, heap-free core: the builtins in [`crate::builtins`] read the arguments
//! and intern the result.
//!
//! Native endianness is fixed to little-endian (the executor's deterministic
//! target), so the `=` option and the default behave as little-endian regardless
//! of the host.

use crate::call::{Exec, err};

const NB: u32 = 8; // bits per byte
const MC: u64 = 0xFF; // single-byte mask
const SZINT: i32 = 8; // the integer width we pack into (i64)
const MAXINTSIZE: i32 = 16;
const MAXALIGN: i32 = 8;
/// Upstream `MAXSSIZE` — the cap on a single size specifier, and on a `packsize`
/// total (which allocates nothing, unlike `pack`'s runtime output cap).
pub const MAXSSIZE: i64 = 1 << 30;

/// The classified options the format string yields.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum KOption {
    /// Signed integer.
    Int,
    /// Unsigned integer.
    Uint,
    /// Floating point (`f`/`d`/`n`).
    Float,
    /// Fixed-length string (`c<n>`).
    Char,
    /// Length-prefixed string (`s<n>`).
    Str,
    /// Zero-terminated string (`z`).
    Zstr,
    /// Explicit padding (`x`).
    Padding,
    /// Alignment padding (`X`).
    PaddAlign,
    /// Configuration / spaces (no value, no bytes).
    Nop,
}

/// The per-call pack state: the current endianness and maximum alignment.
pub struct Header {
    pub little: bool,
    pub maxalign: i32,
}

impl Header {
    #[must_use]
    pub fn new() -> Self {
        Self {
            little: true,
            maxalign: 1,
        }
    }
}

/// A cursor over the format string's bytes.
pub struct Fmt<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Fmt<'a> {
    #[must_use]
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    #[must_use]
    pub fn at_end(&self) -> bool {
        self.pos >= self.bytes.len()
    }
}

fn is_digit(c: u8) -> bool {
    c.is_ascii_digit()
}

/// Reads a decimal numeral, or `df` if there is none (upstream `getnum`).
fn getnum(fmt: &mut Fmt, df: i32) -> Exec<i32> {
    if !fmt.peek().is_some_and(is_digit) {
        return Ok(df);
    }
    let mut a: i32 = 0;
    loop {
        a = a * 10 + i32::from(fmt.bump().expect("peeked digit before bump") - b'0');
        if !(fmt.peek().is_some_and(is_digit) && a <= (i32::MAX - 9) / 10) {
            break;
        }
    }
    if i64::from(a) > MAXSSIZE || fmt.peek().is_some_and(is_digit) {
        return Err(err("size specifier is too large"));
    }
    Ok(a)
}

/// A numeral bounded to a legal integer width (upstream `getnumlimit`).
fn getnumlimit(fmt: &mut Fmt, df: i32) -> Exec<i32> {
    let sz = getnum(fmt, df)?;
    if sz > MAXINTSIZE || sz <= 0 {
        return Err(err(format!(
            "integral size ({sz}) out of limits [1,{MAXINTSIZE}]"
        )));
    }
    Ok(sz)
}

/// Reads and classifies the next option, filling its size (upstream `getoption`).
fn getoption(h: &mut Header, fmt: &mut Fmt) -> Exec<(KOption, i32)> {
    let opt = fmt.bump().unwrap_or(b' ');
    let mut size = 0i32;
    let kopt = match opt {
        b'b' => {
            size = 1;
            KOption::Int
        }
        b'B' => {
            size = 1;
            KOption::Uint
        }
        b'h' => {
            size = 2;
            KOption::Int
        }
        b'H' => {
            size = 2;
            KOption::Uint
        }
        b'l' => {
            size = 8;
            KOption::Int
        }
        b'L' => {
            size = 8;
            KOption::Uint
        }
        b'j' => {
            size = 4;
            KOption::Int
        }
        b'J' | b'T' => {
            size = 4;
            KOption::Uint
        }
        b'f' => {
            size = 4;
            KOption::Float
        }
        b'd' | b'n' => {
            size = 8;
            KOption::Float
        }
        b'i' => {
            size = getnumlimit(fmt, 4)?;
            KOption::Int
        }
        b'I' => {
            size = getnumlimit(fmt, 4)?;
            KOption::Uint
        }
        b's' => {
            size = getnumlimit(fmt, 4)?;
            KOption::Str
        }
        b'c' => {
            size = getnum(fmt, -1)?;
            if size == -1 {
                return Err(err("missing size for format option 'c'"));
            }
            KOption::Char
        }
        b'z' => KOption::Zstr,
        b'x' => {
            size = 1;
            KOption::Padding
        }
        b'X' => KOption::PaddAlign,
        b' ' => KOption::Nop,
        b'<' | b'=' => {
            h.little = true;
            KOption::Nop
        }
        b'>' => {
            h.little = false;
            KOption::Nop
        }
        b'!' => {
            h.maxalign = getnumlimit(fmt, MAXALIGN)?;
            KOption::Nop
        }
        other => return Err(err(format!("invalid format option '{}'", other as char))),
    };
    Ok((kopt, size))
}

/// Classifies the next option and computes its alignment padding for the running
/// `totalsize` (upstream `getdetails`).
pub fn getdetails(h: &mut Header, totalsize: i64, fmt: &mut Fmt) -> Exec<(KOption, i32, i32)> {
    let (opt, size) = getoption(h, fmt)?;
    let mut align = size;
    if opt == KOption::PaddAlign {
        // 'X' borrows its alignment from the following option.
        let next = if fmt.at_end() {
            None
        } else {
            Some(getoption(h, fmt)?)
        };
        match next {
            Some((KOption::Char, _)) | Some((_, 0)) | None => {
                return Err(err("invalid next option for option 'X'"));
            }
            Some((_, a)) => align = a,
        }
    }
    let ntoalign = if align <= 1 || opt == KOption::Char {
        0
    } else {
        if align > h.maxalign {
            align = h.maxalign;
        }
        if (align & (align - 1)) != 0 {
            return Err(err("format asks for alignment not power of 2"));
        }
        let mask = i64::from(align - 1);
        (i64::from(align) - (totalsize & mask)) as i32 & (align - 1)
    };
    Ok((opt, size, ntoalign))
}

/// Packs `n` as `size` little/big-endian bytes, sign-extending a negative value
/// past the i64 width (upstream `packint`).
pub fn packint(out: &mut Vec<u8>, n: u64, little: bool, size: i32, neg: bool) {
    let size = size as usize;
    let mut buff = [0u8; MAXINTSIZE as usize];
    let mut n = n;
    buff[if little { 0 } else { size - 1 }] = (n & MC) as u8;
    for i in 1..size {
        n >>= NB;
        buff[if little { i } else { size - 1 - i }] = (n & MC) as u8;
    }
    if neg && size > SZINT as usize {
        for i in (SZINT as usize)..size {
            buff[if little { i } else { size - 1 - i }] = MC as u8;
        }
    }
    out.extend_from_slice(&buff[..size]);
}

/// Unpacks a `size`-byte integer, sign-extending (signed) or checking the
/// high bytes do not overflow an i64 (upstream `unpackint`). `data` must hold at
/// least `size` bytes.
///
/// # Errors
/// Errors when a `> 8`-byte value does not fit a Lua integer.
pub fn unpackint(data: &[u8], little: bool, size: i32, signed: bool) -> Exec<i64> {
    let size = size as usize;
    let szint = SZINT as usize;
    let mut res: u64 = 0;
    let limit = size.min(szint);
    for i in (0..limit).rev() {
        res <<= NB;
        res |= u64::from(data[if little { i } else { size - 1 - i }]);
    }
    if size < szint {
        if signed {
            let mask = 1u64 << (size as u32 * NB - 1);
            res = (res ^ mask).wrapping_sub(mask);
        }
    } else if size > szint {
        let fill: u8 = if !signed || (res as i64) >= 0 {
            0
        } else {
            MC as u8
        };
        for i in limit..size {
            if data[if little { i } else { size - 1 - i }] != fill {
                return Err(err(format!(
                    "{size}-byte integer does not fit into Lua Integer"
                )));
            }
        }
    }
    Ok(res as i64)
}

#[cfg(any())]
mod tests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        /// Signed integers that fit `size` bytes round-trip through
        /// `packint`/`unpackint` in both endiannesses.
        #[test]
        fn packint_roundtrips_signed(raw: i64, size in 1i32..=8, little: bool) {
            // Fold the raw value into the size's signed range (arithmetic
            // shift sign-extends), so every generated case is in range.
            let bits = size as u32 * 8;
            let n = if size == 8 {
                raw
            } else {
                (raw << (64 - bits)) >> (64 - bits)
            };
            let mut out = Vec::new();
            packint(&mut out, n as u64, little, size, n < 0);
            prop_assert_eq!(out.len(), size as usize);
            let back = unpackint(&out, little, size, true).expect("fitting value unpacks");
            prop_assert_eq!(back, n);
        }

        /// Sizes wider than the i64 width sign-extend on pack and verify the
        /// fill bytes on unpack.
        #[test]
        fn packint_roundtrips_wide(n: i64, size in 9i32..=16, little: bool) {
            let mut out = Vec::new();
            packint(&mut out, n as u64, little, size, n < 0);
            prop_assert_eq!(out.len(), size as usize);
            let back = unpackint(&out, little, size, true)
                .expect("sign-extended wide value unpacks");
            prop_assert_eq!(back, n);
        }

        /// Unsigned integers that fit `size` bytes round-trip bit-exactly.
        #[test]
        fn packint_roundtrips_unsigned(raw: u64, size in 1i32..=8, little: bool) {
            // Mask the raw value into the size's unsigned range.
            let n = if size == 8 {
                raw
            } else {
                raw & ((1u64 << (size as u32 * 8)) - 1)
            };
            let mut out = Vec::new();
            packint(&mut out, n, little, size, false);
            let back = unpackint(&out, little, size, false).expect("fitting value unpacks");
            prop_assert_eq!(back as u64, n);
        }
    }
}
