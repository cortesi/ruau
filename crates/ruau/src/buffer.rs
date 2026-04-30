use std::{io, ops::Range, result::Result as StdResult, slice};

use serde::ser::{Serialize, Serializer};

use crate::{Error, error::Result, state::RawLuau, types::ValueRef};

/// A Luau buffer type.
///
/// See the buffer [documentation] for more information.
///
/// [documentation]: https://luau.org/library#buffer-library
#[derive(Clone, Debug, PartialEq)]
pub struct Buffer(pub(crate) ValueRef);

impl Buffer {
    /// Copies the buffer data into a new `Vec<u8>`.
    pub fn to_vec(&self) -> Vec<u8> {
        let lua = self.0.lua.raw();
        self.as_slice(lua).to_vec()
    }

    /// Returns the length of the buffer.
    pub fn len(&self) -> usize {
        let lua = self.0.lua.raw();
        self.as_slice(lua).len()
    }

    /// Returns `true` if the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Reads given number of bytes from the buffer at the given offset.
    ///
    /// Offset is 0-based.
    #[track_caller]
    pub fn read_bytes<const N: usize>(&self, offset: usize) -> [u8; N] {
        self.try_read_bytes(offset).expect("buffer access out of bounds")
    }

    /// Writes given bytes to the buffer at the given offset.
    ///
    /// Offset is 0-based.
    #[track_caller]
    pub fn write_bytes(&self, offset: usize, bytes: &[u8]) {
        self.try_write_bytes(offset, bytes)
            .expect("buffer access out of bounds");
    }

    /// Reads given number of bytes from the buffer at the given offset.
    ///
    /// Offset is 0-based. Returns an error when the requested range is outside the buffer.
    pub fn try_read_bytes<const N: usize>(&self, offset: usize) -> Result<[u8; N]> {
        let lua = self.0.lua.raw();
        let data = self.as_slice(lua);
        let range = checked_byte_range(data.len(), offset, N)?;
        let mut bytes = [0u8; N];
        bytes.copy_from_slice(&data[range]);
        Ok(bytes)
    }

    /// Writes given bytes to the buffer at the given offset.
    ///
    /// Offset is 0-based. Returns an error when the requested range is outside the buffer.
    pub fn try_write_bytes(&self, offset: usize, bytes: &[u8]) -> Result<()> {
        let lua = self.0.lua.raw();
        self.with_slice_mut(lua, |data| {
            let range = checked_byte_range(data.len(), offset, bytes.len())?;
            data[range].copy_from_slice(bytes);
            Ok(())
        })
    }

    /// Reads a signed 8-bit integer from `offset`.
    pub fn read_i8(&self, offset: usize) -> Result<i8> {
        self.read_number(offset, i8::from_le_bytes)
    }

    /// Reads an unsigned 8-bit integer from `offset`.
    pub fn read_u8(&self, offset: usize) -> Result<u8> {
        self.read_number(offset, u8::from_le_bytes)
    }

    /// Reads a signed 16-bit integer from `offset`.
    pub fn read_i16(&self, offset: usize) -> Result<i16> {
        self.read_number(offset, i16::from_le_bytes)
    }

    /// Reads an unsigned 16-bit integer from `offset`.
    pub fn read_u16(&self, offset: usize) -> Result<u16> {
        self.read_number(offset, u16::from_le_bytes)
    }

    /// Reads a signed 32-bit integer from `offset`.
    pub fn read_i32(&self, offset: usize) -> Result<i32> {
        self.read_number(offset, i32::from_le_bytes)
    }

    /// Reads an unsigned 32-bit integer from `offset`.
    pub fn read_u32(&self, offset: usize) -> Result<u32> {
        self.read_number(offset, u32::from_le_bytes)
    }

    /// Reads a signed 64-bit integer from `offset`.
    pub fn read_i64(&self, offset: usize) -> Result<i64> {
        self.read_number(offset, i64::from_le_bytes)
    }

    /// Reads an unsigned 64-bit integer from `offset`.
    pub fn read_u64(&self, offset: usize) -> Result<u64> {
        self.read_number(offset, u64::from_le_bytes)
    }

    /// Reads a 32-bit floating-point number from `offset`.
    pub fn read_f32(&self, offset: usize) -> Result<f32> {
        self.read_number(offset, f32::from_le_bytes)
    }

    /// Reads a 64-bit floating-point number from `offset`.
    pub fn read_f64(&self, offset: usize) -> Result<f64> {
        self.read_number(offset, f64::from_le_bytes)
    }

    /// Writes a signed 8-bit integer at `offset`.
    pub fn write_i8(&self, offset: usize, value: i8) -> Result<()> {
        self.write_number(offset, value.to_le_bytes())
    }

    /// Writes an unsigned 8-bit integer at `offset`.
    pub fn write_u8(&self, offset: usize, value: u8) -> Result<()> {
        self.write_number(offset, value.to_le_bytes())
    }

    /// Writes a signed 16-bit integer at `offset`.
    pub fn write_i16(&self, offset: usize, value: i16) -> Result<()> {
        self.write_number(offset, value.to_le_bytes())
    }

    /// Writes an unsigned 16-bit integer at `offset`.
    pub fn write_u16(&self, offset: usize, value: u16) -> Result<()> {
        self.write_number(offset, value.to_le_bytes())
    }

    /// Writes a signed 32-bit integer at `offset`.
    pub fn write_i32(&self, offset: usize, value: i32) -> Result<()> {
        self.write_number(offset, value.to_le_bytes())
    }

    /// Writes an unsigned 32-bit integer at `offset`.
    pub fn write_u32(&self, offset: usize, value: u32) -> Result<()> {
        self.write_number(offset, value.to_le_bytes())
    }

    /// Writes a signed 64-bit integer at `offset`.
    pub fn write_i64(&self, offset: usize, value: i64) -> Result<()> {
        self.write_number(offset, value.to_le_bytes())
    }

    /// Writes an unsigned 64-bit integer at `offset`.
    pub fn write_u64(&self, offset: usize, value: u64) -> Result<()> {
        self.write_number(offset, value.to_le_bytes())
    }

    /// Writes a 32-bit floating-point number at `offset`.
    pub fn write_f32(&self, offset: usize, value: f32) -> Result<()> {
        self.write_number(offset, value.to_le_bytes())
    }

    /// Writes a 64-bit floating-point number at `offset`.
    pub fn write_f64(&self, offset: usize, value: f64) -> Result<()> {
        self.write_number(offset, value.to_le_bytes())
    }

    /// Reads up to 32 bits starting at `bit_offset`, matching Luau's `buffer.readbits`.
    ///
    /// `bit_count` must be in `0..=32`. Bits are read in little-endian byte order and returned in
    /// the least significant bits of the result.
    pub fn read_bits(&self, bit_offset: usize, bit_count: u8) -> Result<u32> {
        let lua = self.0.lua.raw();
        let data = self.as_slice(lua);
        let range = checked_bit_range(data.len(), bit_offset, bit_count)?;
        let mut word = 0u64;

        for (shift, byte) in data[range].iter().enumerate() {
            word |= u64::from(*byte) << (shift * 8);
        }

        let mask = bit_mask(bit_count);
        Ok(((word >> (bit_offset & 0x7)) & mask) as u32)
    }

    /// Writes up to 32 bits starting at `bit_offset`, matching Luau's `buffer.writebits`.
    ///
    /// `bit_count` must be in `0..=32`. Bits are written from the least significant bits of
    /// `value`; high bits outside `bit_count` are ignored.
    pub fn write_bits(&self, bit_offset: usize, bit_count: u8, value: u32) -> Result<()> {
        let lua = self.0.lua.raw();
        self.with_slice_mut(lua, |data| {
            let range = checked_bit_range(data.len(), bit_offset, bit_count)?;
            let mut word = 0u64;

            for (shift, byte) in data[range.clone()].iter().enumerate() {
                word |= u64::from(*byte) << (shift * 8);
            }

            let subbyte_offset = bit_offset & 0x7;
            let mask = bit_mask(bit_count) << subbyte_offset;
            word = (word & !mask) | ((u64::from(value) << subbyte_offset) & mask);

            for byte in &mut data[range] {
                *byte = word as u8;
                word >>= 8;
            }

            Ok(())
        })
    }

    /// Returns an adaptor implementing [`io::Read`], [`io::Write`] and [`io::Seek`] over the
    /// buffer.
    ///
    /// Cursors created from the same [`Buffer`] share the same underlying Luau buffer. Writes made
    /// through one cursor are visible through the original buffer and through other cursors.
    ///
    /// Buffer operations are infallible, none of the read/write functions will return an Err.
    pub fn cursor(&self) -> impl io::Read + io::Write + io::Seek + use<> {
        BufferCursor(self.clone(), 0)
    }

    pub(crate) fn as_slice(&self, lua: &RawLuau) -> &[u8] {
        unsafe {
            let (buf, size) = self.as_raw_parts(lua);
            slice::from_raw_parts(buf, size)
        }
    }

    fn with_slice_mut<R>(&self, lua: &RawLuau, f: impl FnOnce(&mut [u8]) -> R) -> R {
        unsafe {
            let (buf, size) = self.as_raw_parts(lua);
            f(slice::from_raw_parts_mut(buf, size))
        }
    }

    fn read_number<T, const N: usize>(
        &self,
        offset: usize,
        from_le_bytes: impl FnOnce([u8; N]) -> T,
    ) -> Result<T> {
        let lua = self.0.lua.raw();
        let data = self.as_slice(lua);
        let range = checked_byte_range(data.len(), offset, N)?;
        let mut bytes = [0u8; N];
        bytes.copy_from_slice(&data[range]);
        Ok(from_le_bytes(bytes))
    }

    fn write_number<const N: usize>(&self, offset: usize, bytes: [u8; N]) -> Result<()> {
        let lua = self.0.lua.raw();
        self.with_slice_mut(lua, |data| {
            let range = checked_byte_range(data.len(), offset, N)?;
            data[range].copy_from_slice(&bytes);
            Ok(())
        })
    }

    unsafe fn as_raw_parts(&self, lua: &RawLuau) -> (*mut u8, usize) {
        let mut size = 0usize;
        let buf = ffi::lua_tobuffer(lua.ref_thread(), self.0.index, &mut size);
        ruau_assert!(!buf.is_null(), "invalid Luau buffer");
        (buf as *mut u8, size)
    }
}

fn checked_byte_range(len: usize, offset: usize, count: usize) -> Result<Range<usize>> {
    let end = offset
        .checked_add(count)
        .ok_or_else(buffer_access_out_of_bounds)?;
    if end > len {
        return Err(buffer_access_out_of_bounds());
    }
    Ok(offset..end)
}

fn checked_bit_range(len: usize, bit_offset: usize, bit_count: u8) -> Result<Range<usize>> {
    if bit_count > 32 {
        return Err(Error::runtime("bit count is out of range of [0; 32]"));
    }

    let bit_end = bit_offset
        .checked_add(usize::from(bit_count))
        .ok_or_else(buffer_access_out_of_bounds)?;
    let bit_len = len.checked_mul(8).ok_or_else(buffer_access_out_of_bounds)?;
    if bit_end > bit_len {
        return Err(buffer_access_out_of_bounds());
    }

    let start = bit_offset / 8;
    let end = bit_end.saturating_add(7) / 8;
    Ok(start..end)
}

fn bit_mask(bit_count: u8) -> u64 {
    if bit_count == 32 {
        u64::from(u32::MAX)
    } else {
        (1u64 << bit_count) - 1
    }
}

fn buffer_access_out_of_bounds() -> Error {
    Error::runtime("buffer access out of bounds")
}

struct BufferCursor(Buffer, usize);

impl io::Read for BufferCursor {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let lua = self.0.0.lua.raw();
        let data = self.0.as_slice(lua);
        if self.1 == data.len() {
            return Ok(0);
        }
        let len = buf.len().min(data.len() - self.1);
        buf[..len].copy_from_slice(&data[self.1..self.1 + len]);
        self.1 += len;
        Ok(len)
    }
}

impl io::Write for BufferCursor {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let lua = self.0.0.lua.raw();
        self.0.with_slice_mut(lua, |data| {
            if self.1 == data.len() {
                return Ok(0);
            }
            let len = buf.len().min(data.len() - self.1);
            data[self.1..self.1 + len].copy_from_slice(&buf[..len]);
            self.1 += len;
            Ok(len)
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl io::Seek for BufferCursor {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        let lua = self.0.0.lua.raw();
        let data = self.0.as_slice(lua);
        let new_offset = match pos {
            io::SeekFrom::Start(offset) => usize::try_from(offset).ok(),
            io::SeekFrom::End(offset) => checked_add_i64(data.len(), offset),
            io::SeekFrom::Current(offset) => checked_add_i64(self.1, offset),
        };
        let Some(new_offset) = new_offset else {
            return Err(invalid_seek("invalid seek position"));
        };
        if new_offset > data.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid seek to a position beyond the end of the buffer",
            ));
        }
        self.1 = new_offset;
        Ok(self.1 as u64)
    }
}

fn checked_add_i64(base: usize, offset: i64) -> Option<usize> {
    if offset >= 0 {
        base.checked_add(usize::try_from(offset).ok()?)
    } else {
        base.checked_sub(usize::try_from(offset.unsigned_abs()).ok()?)
    }
}

fn invalid_seek(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

impl Serialize for Buffer {
    fn serialize<S: Serializer>(&self, serializer: S) -> StdResult<S::Ok, S::Error> {
        let lua = self.0.lua.raw();
        serializer.serialize_bytes(self.as_slice(lua))
    }
}
