#![allow(
    missing_docs,
    clippy::absolute_paths,
    clippy::missing_docs_in_private_items,
    clippy::tests_outside_test_module,
    clippy::items_after_statements,
    clippy::cognitive_complexity,
    clippy::let_underscore_must_use,
    clippy::manual_c_str_literals,
    clippy::mutable_key_type,
    clippy::needless_maybe_sized,
    clippy::needless_pass_by_value,
    clippy::redundant_pattern_matching
)]

use std::io::{Read, Seek, SeekFrom, Write};

use ruau::{Luau, Result, Value};

#[tokio::test]
async fn test_buffer() -> Result<()> {
    let lua = Luau::new();

    let buf1 = lua
        .load(
            r#"
        local buf = buffer.fromstring("hello")
        assert(buffer.len(buf) == 5)
        return buf
    "#,
        )
        .eval::<Value>()
        .await?;
    assert!(buf1.is_buffer());
    assert_eq!(buf1.type_name(), "buffer");

    let buf2 = lua.load("buffer.fromstring('hello')").eval::<Value>().await?;
    assert_ne!(buf1, buf2);

    // Check that we can pass buffer type to Luau
    let buf1 = buf1.as_buffer().unwrap();
    let func = lua.create_function(|_, buf: Value| buf.to_string())?;
    assert!(func.call::<String>(buf1).await?.starts_with("buffer:"));

    // Check buffer methods
    assert_eq!(buf1.len(), 5);
    assert_eq!(buf1.to_vec(), b"hello");
    assert_eq!(buf1.try_read_bytes::<3>(1)?, [b'e', b'l', b'l']);
    assert_eq!(buf1.read_bytes::<3>(1), [b'e', b'l', b'l']);
    buf1.try_write_bytes(1, b"i")?;
    assert_eq!(buf1.to_vec(), b"hillo");
    buf1.write_bytes(1, b"i");
    assert_eq!(buf1.to_vec(), b"hillo");

    let buf3 = lua.create_buffer(b"")?;
    assert!(buf3.is_empty());
    assert!(!Value::Buffer(buf3).to_pointer().is_null());

    Ok(())
}

#[tokio::test]
async fn test_buffer_typed_access() -> Result<()> {
    let lua = Luau::new();
    let buf = lua.create_buffer_with_capacity(42)?;

    buf.write_i8(0, -2)?;
    buf.write_u8(1, 254)?;
    buf.write_i16(2, -1234)?;
    buf.write_u16(4, 0xabcd)?;
    buf.write_i32(6, -123_456)?;
    buf.write_u32(10, 0x89ab_cdef)?;
    buf.write_i64(14, -123_456_789)?;
    buf.write_u64(22, 0x0123_4567_89ab_cdef)?;
    buf.write_f32(30, 12.5)?;
    buf.write_f64(34, -0.25)?;

    assert_eq!(buf.read_i8(0)?, -2);
    assert_eq!(buf.read_u8(1)?, 254);
    assert_eq!(buf.read_i16(2)?, -1234);
    assert_eq!(buf.read_u16(4)?, 0xabcd);
    assert_eq!(buf.read_i32(6)?, -123_456);
    assert_eq!(buf.read_u32(10)?, 0x89ab_cdef);
    assert_eq!(buf.read_i64(14)?, -123_456_789);
    assert_eq!(buf.read_u64(22)?, 0x0123_4567_89ab_cdef);
    assert_eq!(buf.read_f32(30)?, 12.5);
    assert_eq!(buf.read_f64(34)?, -0.25);

    lua.globals().set("buf", buf.clone())?;
    lua.load(
        r#"
        assert(buffer.readi8(buf, 0) == -2)
        assert(buffer.readu8(buf, 1) == 254)
        assert(buffer.readi16(buf, 2) == -1234)
        assert(buffer.readu16(buf, 4) == 0xabcd)
        assert(buffer.readi32(buf, 6) == -123456)
        assert(buffer.readu32(buf, 10) == 0x89abcdef)
        assert(buffer.readf32(buf, 30) == 12.5)
        assert(buffer.readf64(buf, 34) == -0.25)
        "#,
    )
    .exec()
    .await?;

    assert!(buf.read_u32(39).is_err());
    assert!(buf.write_u32(39, 1).is_err());
    assert!(buf.try_read_bytes::<4>(39).is_err());
    assert!(buf.try_write_bytes(39, &[1, 2, 3, 4]).is_err());

    Ok(())
}

#[tokio::test]
async fn test_buffer_bit_access() -> Result<()> {
    let lua = Luau::new();
    let buf = lua.create_buffer([0b1011_0101, 0b0100_0011, 0])?;

    assert_eq!(buf.read_bits(0, 4)?, 0b0101);
    assert_eq!(buf.read_bits(4, 8)?, 0b0011_1011);
    assert_eq!(buf.read_bits(7, 10)?, 0b1000_0111);
    assert_eq!(buf.read_bits(24, 0)?, 0);

    buf.write_bits(3, 9, 0b1_0101_0101)?;
    lua.globals().set("buf", buf.clone())?;
    lua.load(
        r#"
        assert(buffer.readbits(buf, 3, 9) == 0b101010101)
        buffer.writebits(buf, 8, 8, 0xaa)
        "#,
    )
    .exec()
    .await?;

    assert_eq!(buf.read_bits(8, 8)?, 0xaa);
    assert!(buf.read_bits(0, 33).is_err());
    assert!(buf.write_bits(25, 1, 1).is_err());

    Ok(())
}

#[tokio::test]
#[should_panic(expected = "buffer access out of bounds")]
async fn test_buffer_out_of_bounds_read() {
    let lua = Luau::new();
    let buf = lua.create_buffer(b"hello, world!").unwrap();
    _ = buf.read_bytes::<1>(13);
}

#[tokio::test]
#[should_panic(expected = "buffer access out of bounds")]
async fn test_buffer_out_of_bounds_write() {
    let lua = Luau::new();
    let buf = lua.create_buffer(b"hello, world!").unwrap();
    buf.write_bytes(14, b"!!");
}

#[tokio::test]
async fn create_large_buffer() {
    let lua = Luau::new();
    let err = lua.create_buffer_with_capacity(1_073_741_824 + 1).unwrap_err(); // 1GB
    assert!(err.to_string().contains("memory allocation error"));

    // Normal buffer is okay
    let buf = lua.create_buffer_with_capacity(1024 * 1024).unwrap();
    assert_eq!(buf.len(), 1024 * 1024);
}

#[tokio::test]
async fn test_buffer_cursor() -> Result<()> {
    let lua = Luau::new();
    let mut cursor = lua.create_buffer(b"hello, world")?.cursor();

    let mut data = Vec::new();
    cursor.read_to_end(&mut data)?;
    assert_eq!(data, b"hello, world");

    // No more data to read
    let mut one = [0u8; 1];
    assert_eq!(cursor.read(&mut one)?, 0);

    // Seek to start
    cursor.seek(SeekFrom::Start(0))?;
    cursor.read_exact(&mut one)?;
    assert_eq!(one, [b'h']);

    // Seek to end -5
    cursor.seek(SeekFrom::End(-5))?;
    let mut five = [0u8; 5];
    cursor.read_exact(&mut five)?;
    assert_eq!(&five, b"world");

    // Seek to current -1
    cursor.seek(SeekFrom::Current(-1))?;
    cursor.read_exact(&mut one)?;
    assert_eq!(one, [b'd']);

    // Invalid seek
    assert!(cursor.seek(SeekFrom::Current(-100)).is_err());
    assert!(cursor.seek(SeekFrom::End(1)).is_err());
    assert!(cursor.seek(SeekFrom::Start(u64::MAX)).is_err());
    assert!(cursor.seek(SeekFrom::Current(i64::MAX)).is_err());
    assert!(cursor.seek(SeekFrom::End(i64::MIN)).is_err());

    // Write data
    let buf = lua.create_buffer_with_capacity(100)?;
    cursor = buf.cursor();

    cursor.write_all(b"hello, ...")?;
    cursor.seek(SeekFrom::Current(-3))?;
    cursor.write_all(b"Rust!")?;

    assert_eq!(&buf.read_bytes::<12>(0), b"hello, Rust!");

    let mut second_cursor = buf.cursor();
    let mut second_data = [0; 5];
    second_cursor.read_exact(&mut second_data)?;
    assert_eq!(&second_data, b"hello");

    // Writing beyond the end of the buffer does nothing
    cursor.seek(SeekFrom::End(0))?;
    assert_eq!(cursor.write(b".")?, 0);

    // Flush is no-op
    cursor.flush()?;

    Ok(())
}
