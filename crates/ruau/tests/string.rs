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

use std::{borrow::Cow, collections::HashSet};

use ruau::{Luau, LuauString, Result};

#[tokio::test]
async fn test_string_compare() {
    let lua = Luau::new();

    fn with_str<F: FnOnce(LuauString)>(lua: &Luau, s: &str, f: F) {
        f(lua.create_string(s).unwrap());
    }

    // Tests that all comparisons we want to have are usable
    with_str(&lua, "teststring", |t| assert_eq!(t, "teststring")); // &str
    with_str(&lua, "teststring", |t| assert_eq!(t, b"teststring")); // &[u8]
    with_str(&lua, "teststring", |t| {
        assert_eq!(t, b"teststring".to_vec())
    }); // Vec<u8>
    with_str(&lua, "teststring", |t| {
        assert_eq!(t, "teststring".to_string())
    }); // String
    with_str(&lua, "teststring", |t| assert_eq!(t, t)); // ruau::String
    with_str(&lua, "teststring", |t| {
        assert_eq!(t, Cow::from(b"teststring".as_ref())) // Cow (borrowed)
    });
    with_str(&lua, "bla", |t| assert_eq!(t, Cow::from(b"bla".to_vec()))); // Cow (owned)

    // Test ordering
    with_str(&lua, "a", |a| {
        assert!((a >= a));
        assert!((a <= a));
    });
    with_str(&lua, "a", |a| assert!(a < "b"));
    with_str(&lua, "a", |a| assert!(a < b"b"));
    with_str(&lua, "a", |a| with_str(&lua, "b", |b| assert!(a < b)));

    // Long strings (not interned by Luau)
    let long_str = "abc".repeat(100);
    with_str(&lua, &long_str, |s1| {
        with_str(&lua, &long_str, |s2| assert_eq!(s1, s2))
    });
}

#[tokio::test]
async fn test_string_views() -> Result<()> {
    let lua = Luau::new();

    lua.load(
        r#"
        ok = "null bytes are valid utf-8, wh\0 knew?"
        err = "but \255 isn't :("
        empty = ""
    "#,
    )
    .exec()
    .await?;

    let globals = lua.globals();
    let ok: LuauString = globals.get("ok")?;
    let err: LuauString = globals.get("err")?;
    let empty: LuauString = globals.get("empty")?;

    assert_eq!(ok.to_str()?, "null bytes are valid utf-8, wh\0 knew?");
    assert_eq!(
        ok.to_string_lossy(),
        "null bytes are valid utf-8, wh\0 knew?"
    );
    assert_eq!(
        ok.as_bytes(),
        &b"null bytes are valid utf-8, wh\0 knew?"[..]
    );

    assert!(err.to_str().is_err());
    assert_eq!(err.as_bytes(), &b"but \xff isn't :("[..]);

    assert_eq!(empty.to_str()?, "");
    assert_eq!(empty.as_bytes_with_nul(), &[0]);
    assert_eq!(empty.as_bytes(), &[]);

    Ok(())
}

#[tokio::test]
async fn test_string_from_bytes() -> Result<()> {
    let lua = Luau::new();

    let rs = lua.create_string([0, 1, 2, 3, 0, 1, 2, 3])?;
    assert_eq!(rs.as_bytes(), &[0, 1, 2, 3, 0, 1, 2, 3]);

    Ok(())
}

#[tokio::test]
async fn test_string_hash() -> Result<()> {
    let lua = Luau::new();

    let set: HashSet<LuauString> = lua.load(r#"{"hello", "world", "abc", 321}"#).eval().await?;
    assert_eq!(set.len(), 4);
    assert!(set.contains(&lua.create_string("hello")?));
    assert!(set.contains(&lua.create_string("world")?));
    assert!(set.contains(&lua.create_string("abc")?));
    assert!(set.contains(&lua.create_string("321")?));
    assert!(!set.contains(&lua.create_string("Hello")?));

    Ok(())
}

#[tokio::test]
async fn test_string_fmt_debug() -> Result<()> {
    let lua = Luau::new();

    // Valid utf8
    let s = lua.create_string("hello")?;
    assert_eq!(format!("{s:?}"), r#""hello""#);
    assert_eq!(format!("{:?}", s.to_str()?), r#""hello""#);
    assert_eq!(format!("{:?}", s.as_bytes()), "[104, 101, 108, 108, 111]");

    // Invalid utf8
    let s = lua.create_string(b"hello\0world\r\n\t\xf0\x90\x80")?;
    assert_eq!(format!("{s:?}"), r#"b"hello\0world\r\n\t\xf0\x90\x80""#);

    Ok(())
}

#[tokio::test]
async fn test_string_pointer() -> Result<()> {
    let lua = Luau::new();

    let str1 = lua.create_string("hello")?;
    let str2 = lua.create_string("hello")?;

    // Luau uses string interning, so these should be the same
    assert_eq!(str1.to_pointer(), str2.to_pointer());

    Ok(())
}

#[tokio::test]
async fn test_string_display() -> Result<()> {
    let lua = Luau::new();

    let s = lua.create_string("hello")?;
    assert_eq!(format!("{}", s.display()), "hello");

    // With invalid utf8
    let s = lua.create_string(b"hello\0world\xFF")?;
    assert_eq!(format!("{}", s.display()), "hello\0world�");

    Ok(())
}

#[tokio::test]
async fn test_string_wrap() -> Result<()> {
    let lua = Luau::new();

    let s = LuauString::wrap("hello, world");
    lua.globals().set("s", s)?;
    assert_eq!(lua.globals().get::<LuauString>("s")?, "hello, world");

    let s2 = LuauString::wrap("hello, world (owned)".to_string());
    lua.globals().set("s2", s2)?;
    assert_eq!(
        lua.globals().get::<LuauString>("s2")?,
        "hello, world (owned)"
    );

    Ok(())
}

#[tokio::test]
async fn test_bytes_into_iter() -> Result<()> {
    let lua = Luau::new();

    let s = lua.create_string("hello")?;
    let bytes = s.as_bytes();

    for (i, &b) in bytes.into_iter().enumerate() {
        assert_eq!(b, s.as_bytes()[i]);
    }

    Ok(())
}
