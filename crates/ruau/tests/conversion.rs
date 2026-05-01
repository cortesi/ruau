//! conversion integration tests.

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    ffi::{CString, OsString},
    path::PathBuf,
};

use bstr::BString;
use either::Either;
use maplit::{btreemap, btreeset, hashmap, hashset};
use ruau::{
    AnyUserData, BorrowedBytes, BorrowedStr, Error, FromLuau, Function, IntoLuau, Luau, RegistryKey, Result,
    Table, Thread, Value, userdata::UserDataRef,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_value_into_luau() -> Result<()> {
        let lua = Luau::new();

        // Direct conversion
        let v = Value::Boolean(true);
        let v2 = (&v).into_luau(&lua)?;
        assert_eq!(v, v2);

        // Push into stack
        let table = lua.create_table()?;
        table.set("v", &v)?;
        assert_eq!(v, table.get::<Value>("v")?);

        Ok(())
    }

    #[tokio::test]
    async fn test_string_into_luau() -> Result<()> {
        let lua = Luau::new();

        // Direct conversion
        let s = lua.create_string("hello, world!")?;
        let s2 = (&s).into_luau(&lua)?;
        assert_eq!(s, *s2.as_string().unwrap());

        // Push into stack
        let table = lua.create_table()?;
        table.set("s", &s)?;
        assert_eq!(s, table.get::<String>("s")?);

        Ok(())
    }

    #[tokio::test]
    async fn test_string_from_luau() -> Result<()> {
        let lua = Luau::new();

        // From stack
        let f = lua.create_function(|_, s: ruau::LuauString| Ok(s))?;
        let s = f.call::<String>("hello, world!").await?;
        assert_eq!(s, "hello, world!");

        // Should fallback to default conversion
        let s = f.call::<String>(42).await?;
        assert_eq!(s, "42");

        Ok(())
    }

    #[tokio::test]
    async fn test_borrowedstr_into_luau() -> Result<()> {
        let lua = Luau::new();

        // Direct conversion
        let s = lua.create_string("hello, world!")?;
        let bs = s.to_str()?;
        let bs2 = (&bs).into_luau(&lua)?;
        assert_eq!(bs2.as_string().unwrap(), "hello, world!");

        // Push into stack
        let table = lua.create_table()?;
        table.set("bs", &bs)?;
        assert_eq!(bs, table.get::<String>("bs")?);

        Ok(())
    }

    #[tokio::test]
    async fn test_borrowedstr_from_luau() -> Result<()> {
        let lua = Luau::new();

        // From stack
        let f = lua.create_function(|_, s: BorrowedStr| Ok(s))?;
        let s = f.call::<String>("hello, world!").await?;
        assert_eq!(s, "hello, world!");

        Ok(())
    }

    #[tokio::test]
    async fn test_borrowedbytes_into_luau() -> Result<()> {
        let lua = Luau::new();

        // Direct conversion
        let s = lua.create_string("hello, world!")?;
        let bb = s.as_bytes();
        let bb2 = (&bb).into_luau(&lua)?;
        assert_eq!(bb2.as_string().unwrap(), "hello, world!");

        // Push into stack
        let table = lua.create_table()?;
        table.set("bb", &bb)?;
        assert_eq!(bb, table.get::<String>("bb")?.as_bytes());

        Ok(())
    }

    #[tokio::test]
    async fn test_borrowedbytes_from_luau() -> Result<()> {
        let lua = Luau::new();

        // From stack
        let f = lua.create_function(|_, s: BorrowedBytes| Ok(s))?;
        let s = f.call::<String>("hello, world!").await?;
        assert_eq!(s, "hello, world!");

        Ok(())
    }

    #[tokio::test]
    async fn test_table_into_luau() -> Result<()> {
        let lua = Luau::new();

        // Direct conversion
        let t = lua.create_table()?;
        let t2 = (&t).into_luau(&lua)?;
        assert_eq!(&t, t2.as_table().unwrap());

        // Push into stack
        let f = lua.create_function(|_, (t, s): (Table, String)| t.set("s", s))?;
        f.call::<()>((&t, "hello")).await?;
        assert_eq!("hello", t.get::<String>("s")?);

        Ok(())
    }

    #[tokio::test]
    async fn test_function_into_luau() -> Result<()> {
        let lua = Luau::new();

        // Direct conversion
        let f = lua.create_function(|_, ()| Ok::<_, Error>(()))?;
        let f2 = (&f).into_luau(&lua)?;
        assert_eq!(&f, f2.as_function().unwrap());

        // Push into stack
        let table = lua.create_table()?;
        table.set("f", &f)?;
        assert_eq!(f, table.get::<Function>("f")?);

        Ok(())
    }

    #[tokio::test]
    async fn test_function_from_luau() -> Result<()> {
        let lua = Luau::new();

        assert!(lua.globals().get::<Function>("print").is_ok());
        match lua.globals().get::<Function>("math") {
            Err(err @ Error::FromLuauConversionError { .. }) => {
                assert_eq!(err.to_string(), "error converting Luau table to function");
            }
            _ => panic!("expected `Error::FromLuauConversionError`"),
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_thread_into_luau() -> Result<()> {
        let lua = Luau::new();

        // Direct conversion
        let f = lua.create_function(|_, ()| Ok::<_, Error>(()))?;
        let th = lua.create_thread(f)?;
        let th2 = (&th).into_luau(&lua)?;
        assert_eq!(&th, th2.as_thread().unwrap());

        // Push into stack
        let table = lua.create_table()?;
        table.set("th", &th)?;
        assert_eq!(th, table.get::<Thread>("th")?);

        Ok(())
    }

    #[tokio::test]
    async fn test_thread_from_luau() -> Result<()> {
        let lua = Luau::new();

        match lua.globals().get::<Thread>("print") {
            Err(err @ Error::FromLuauConversionError { .. }) => {
                assert_eq!(err.to_string(), "error converting Luau function to thread");
            }
            _ => panic!("expected `Error::FromLuauConversionError`"),
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_anyuserdata_into_luau() -> Result<()> {
        let lua = Luau::new();

        // Direct conversion
        let ud = lua.create_opaque_userdata(String::from("hello"))?;
        let ud2 = (&ud).into_luau(&lua)?;
        assert_eq!(&ud, ud2.as_userdata().unwrap());

        // Push into stack
        let table = lua.create_table()?;
        table.set("ud", &ud)?;
        assert_eq!(ud, table.get::<AnyUserData>("ud")?);
        assert_eq!("hello", *table.get::<UserDataRef<String>>("ud")?);

        Ok(())
    }

    #[tokio::test]
    async fn test_anyuserdata_from_luau() -> Result<()> {
        let lua = Luau::new();

        match lua.globals().get::<AnyUserData>("print") {
            Err(err @ Error::FromLuauConversionError { .. }) => {
                assert_eq!(err.to_string(), "error converting Luau function to userdata");
            }
            _ => panic!("expected `Error::FromLuauConversionError`"),
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_error_conversion() -> Result<()> {
        let lua = Luau::new();

        // Any Luau value can be converted to `Error`
        match Error::from_luau(Error::external("external error").into_luau(&lua)?, &lua) {
            Ok(Error::ExternalError(msg)) => assert_eq!(msg.to_string(), "external error"),
            res => panic!("expected `Error::ExternalError`, got {res:?}"),
        }
        match Error::from_luau("abc".into_luau(&lua)?, &lua) {
            Ok(Error::RuntimeError(msg)) => assert_eq!(msg, "abc"),
            res => panic!("expected `Error::RuntimeError`, got {res:?}"),
        }
        match Error::from_luau(true.into_luau(&lua)?, &lua) {
            Ok(Error::RuntimeError(msg)) => assert_eq!(msg, "true"),
            res => panic!("expected `Error::RuntimeError`, got {res:?}"),
        }
        match Error::from_luau(lua.globals().into_luau(&lua)?, &lua) {
            Ok(Error::RuntimeError(msg)) => assert!(msg.starts_with("table:")),
            res => panic!("expected `Error::RuntimeError`, got {res:?}"),
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_registry_value_into_luau() -> Result<()> {
        let lua = Luau::new();

        // Direct conversion
        let s = lua.create_string("hello, world")?;
        let r = lua.registry().insert(&s)?;
        let value1 = (&r).into_luau(&lua)?;
        let value2 = r.into_luau(&lua)?;
        assert_eq!(value1.to_string()?, "hello, world");
        assert_eq!(value1.to_pointer(), value2.to_pointer());

        // Push into stack
        let t = lua.create_table()?;
        let r = lua.registry().insert(&t)?;
        let f = lua.create_function(|_, (t, k, v): (Table, Value, Value)| t.set(k, v))?;
        f.call::<()>((&r, "hello", "world")).await?;
        f.call::<()>((r, "welcome", "to the jungle")).await?;
        assert_eq!(t.get::<String>("hello")?, "world");
        assert_eq!(t.get::<String>("welcome")?, "to the jungle");

        // Try to set nil registry key
        let r_nil = lua.registry().insert(Value::Nil)?;
        t.set("hello", &r_nil)?;
        assert_eq!(t.get::<Value>("hello")?, Value::Nil);

        // Check non-owned registry key
        let lua2 = Luau::new();
        let r2 = lua2.registry().insert("abc")?;
        assert!(matches!(
            f.call::<()>(&r2).await,
            Err(Error::MismatchedRegistryKey)
        ));

        Ok(())
    }

    #[tokio::test]
    async fn test_registry_key_from_luau() -> Result<()> {
        let lua = Luau::new();

        let fkey = lua.load("function() return 1 end").eval::<RegistryKey>().await?;
        let f = lua.registry().get::<Function>(&fkey)?;
        assert_eq!(f.call::<i32>(()).await?, 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_bool_into_luau() -> Result<()> {
        let lua = Luau::new();

        // Direct conversion
        assert!(true.into_luau(&lua)?.is_boolean());

        // Push into stack
        let table = lua.create_table()?;
        table.set("b", true)?;
        assert!(table.get::<bool>("b")?);

        Ok(())
    }

    #[tokio::test]
    async fn test_bool_from_luau() -> Result<()> {
        let lua = Luau::new();

        assert!(lua.globals().get::<bool>("print")?);
        assert!(bool::from_luau(123.into_luau(&lua)?, &lua)?);
        assert!(!bool::from_luau(Value::Nil, &lua)?);

        Ok(())
    }

    #[tokio::test]
    async fn test_integer_from_luau() -> Result<()> {
        let lua = Luau::new();

        // From stack
        let f = lua.create_function(|_, i: i32| Ok(i))?;
        assert_eq!(f.call::<i32>(42).await?, 42);

        // Out of range
        match f.call::<i32>(i64::MAX).await.err() {
            Some(Error::CallbackError { cause, .. }) => match cause.as_ref() {
                Error::BadArgument { cause, .. } => match cause.as_ref() {
                    Error::FromLuauConversionError { message, .. } => {
                        assert_eq!(message.as_ref().unwrap(), "out of range");
                    }
                    err => panic!("expected Error::FromLuauConversionError, got {err:?}"),
                },
                err => panic!("expected Error::BadArgument, got {err:?}"),
            },
            err => panic!("expected Error::CallbackError, got {err:?}"),
        }

        // Should fallback to default conversion
        assert_eq!(f.call::<i32>("42").await?, 42);

        Ok(())
    }

    #[tokio::test]
    async fn test_float_from_luau() -> Result<()> {
        let lua = Luau::new();

        // From stack
        let f = lua.create_function(|_, f: f32| Ok(f))?;
        assert_eq!(f.call::<f32>(42.0).await?, 42.0);

        // Out of range (but never fails)
        let val = f.call::<f32>(f64::MAX).await?;
        assert!(val.is_infinite());

        // Should fallback to default conversion
        assert_eq!(f.call::<f32>("42.0").await?, 42.0);

        Ok(())
    }

    #[tokio::test]
    async fn test_conv_vec() -> Result<()> {
        let lua = Luau::new();

        let v = vec![1, 2, 3];
        lua.globals().set("v", v.clone())?;
        let v2: Vec<i32> = lua.globals().get("v")?;
        assert_eq!(v, v2);

        Ok(())
    }

    #[tokio::test]
    async fn test_conv_hashmap() -> Result<()> {
        let lua = Luau::new();

        let map = hashmap! {"hello".to_string() => "world".to_string()};
        lua.globals().set("map", map.clone())?;
        let map2: HashMap<String, String> = lua.globals().get("map")?;
        assert_eq!(map, map2);

        Ok(())
    }

    #[tokio::test]
    async fn test_conv_hashset() -> Result<()> {
        let lua = Luau::new();

        let set = hashset! {"hello".to_string(), "world".to_string()};
        lua.globals().set("set", set.clone())?;
        let set2: HashSet<String> = lua.globals().get("set")?;
        assert_eq!(set, set2);

        let set3 = lua.load(r#"{"a", "b", "c"}"#).eval::<HashSet<String>>().await?;
        assert_eq!(set3, hashset! { "a".into(), "b".into(), "c".into() });

        Ok(())
    }

    #[tokio::test]
    async fn test_conv_btreemap() -> Result<()> {
        let lua = Luau::new();

        let map = btreemap! {"hello".to_string() => "world".to_string()};
        lua.globals().set("map", map.clone())?;
        let map2: BTreeMap<String, String> = lua.globals().get("map")?;
        assert_eq!(map, map2);

        Ok(())
    }

    #[tokio::test]
    async fn test_conv_btreeset() -> Result<()> {
        let lua = Luau::new();

        let set = btreeset! {"hello".to_string(), "world".to_string()};
        lua.globals().set("set", set.clone())?;
        let set2: BTreeSet<String> = lua.globals().get("set")?;
        assert_eq!(set, set2);

        let set3 = lua.load(r#"{"a", "b", "c"}"#).eval::<BTreeSet<String>>().await?;
        assert_eq!(set3, btreeset! { "a".into(), "b".into(), "c".into() });

        Ok(())
    }

    #[tokio::test]
    async fn test_conv_cstring() -> Result<()> {
        let lua = Luau::new();

        let s = CString::new(b"hello".to_vec()).unwrap();
        lua.globals().set("s", s.clone())?;
        let s2: CString = lua.globals().get("s")?;
        assert_eq!(s, s2);

        let cs = c"hello";
        lua.globals().set("cs", c"hello")?;
        let cs2: CString = lua.globals().get("cs")?;
        assert_eq!(cs, cs2.as_c_str());

        Ok(())
    }

    #[tokio::test]
    async fn test_conv_cow() -> Result<()> {
        let lua = Luau::new();

        let s = Cow::from("hello");
        lua.globals().set("s", s.clone())?;
        let s2: String = lua.globals().get("s")?;
        assert_eq!(s, s2);

        Ok(())
    }

    #[tokio::test]
    async fn test_conv_boxed_str() -> Result<()> {
        let lua = Luau::new();

        let s = String::from("hello").into_boxed_str();
        lua.globals().set("s", s.clone())?;
        let s2: Box<str> = lua.globals().get("s")?;
        assert_eq!(s, s2);

        Ok(())
    }

    #[tokio::test]
    async fn test_conv_boxed_slice() -> Result<()> {
        let lua = Luau::new();

        let v = vec![1, 2, 3].into_boxed_slice();
        lua.globals().set("v", v.clone())?;
        let v2: Box<[i32]> = lua.globals().get("v")?;
        assert_eq!(v, v2);

        Ok(())
    }

    #[tokio::test]
    async fn test_conv_array() -> Result<()> {
        let lua = Luau::new();

        let v = [1, 2, 3];
        lua.globals().set("v", v)?;
        let v2: [i32; 3] = lua.globals().get("v")?;
        assert_eq!(v, v2);

        let v2 = lua.globals().get::<[i32; 4]>("v");
        assert!(matches!(v2, Err(Error::FromLuauConversionError { .. })));

        Ok(())
    }

    #[tokio::test]
    async fn test_bstring_from_luau() -> Result<()> {
        let lua = Luau::new();

        let s = lua.create_string("hello, world")?;
        let bstr = BString::from_luau(Value::String(s), &lua)?;
        assert_eq!(bstr, "hello, world");

        let bstr = BString::from_luau(Value::Integer(123), &lua)?;
        assert_eq!(bstr, "123");

        let bstr = BString::from_luau(Value::Number(-123.55), &lua)?;
        assert_eq!(bstr, "-123.55");

        // Test from stack
        let f = lua.create_function(|_, bstr: BString| Ok(bstr))?;
        let bstr = f.call::<BString>("hello, world").await?;
        assert_eq!(bstr, "hello, world");

        let bstr = f.call::<BString>(-43.22).await?;
        assert_eq!(bstr, "-43.22");

        Ok(())
    }
    #[tokio::test]
    async fn test_bstring_from_luau_buffer() -> Result<()> {
        let lua = Luau::new();

        let buf = lua.create_buffer("hello, world")?;
        let bstr = BString::from_luau(buf.into_luau(&lua)?, &lua)?;
        assert_eq!(bstr, "hello, world");

        // Test from stack
        let f = lua.create_function(|_, bstr: BString| Ok(bstr))?;
        let buf = lua.create_buffer("hello, world")?;
        let bstr = f.call::<BString>(buf).await?;
        assert_eq!(bstr, "hello, world");

        Ok(())
    }

    #[tokio::test]
    async fn test_osstring_into_from_luau() -> Result<()> {
        let lua = Luau::new();

        let s = OsString::from("hello, world");

        let v = s.as_os_str().into_luau(&lua)?;
        assert!(v.is_string());
        assert_eq!(v.as_string().unwrap(), "hello, world");

        let v = s.into_luau(&lua)?;
        assert!(v.is_string());
        assert_eq!(v.as_string().unwrap(), "hello, world");

        let s = lua.create_string("hello, world")?;
        let bstr = OsString::from_luau(Value::String(s), &lua)?;
        assert_eq!(bstr, "hello, world");

        let bstr = OsString::from_luau(Value::Integer(123), &lua)?;
        assert_eq!(bstr, "123");

        let bstr = OsString::from_luau(Value::Number(-123.55), &lua)?;
        assert_eq!(bstr, "-123.55");

        Ok(())
    }

    #[tokio::test]
    async fn test_pathbuf_into_from_luau() -> Result<()> {
        let lua = Luau::new();

        let pb = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        let pb_str = pb.to_str().unwrap();

        let v = pb.as_path().into_luau(&lua)?;
        assert!(v.is_string());
        assert_eq!(v.to_string().unwrap(), pb_str);

        let v = pb.clone().into_luau(&lua)?;
        assert!(v.is_string());
        assert_eq!(v.to_string().unwrap(), pb_str);

        let s = lua.create_string(pb_str)?;
        let bstr = PathBuf::from_luau(Value::String(s), &lua)?;
        assert_eq!(bstr, pb);

        Ok(())
    }

    #[tokio::test]
    async fn test_option_into_from_luau() -> Result<()> {
        let lua = Luau::new();

        // Direct conversion
        let v = Some(42);
        let v2 = v.into_luau(&lua)?;
        assert_eq!(v, v2.as_i32());

        // Push into stack / get from stack
        let f = lua.create_function(|_, v: Option<i32>| Ok(v))?;
        assert_eq!(f.call::<Option<i32>>(Some(42)).await?, Some(42));
        assert_eq!(f.call::<Option<i32>>(Option::<i32>::None).await?, None);
        assert_eq!(f.call::<Option<i32>>(()).await?, None);

        Ok(())
    }

    #[tokio::test]
    async fn test_either_enum() -> Result<()> {
        // Left
        let mut either = Either::<_, String>::Left(42);
        assert!(either.is_left());
        assert_eq!(*either.as_ref().left().unwrap(), 42);
        *either.as_mut().left().unwrap() = 44;
        assert_eq!(*either.as_ref().left().unwrap(), 44);
        assert_eq!(format!("{either}"), "44");
        assert_eq!(either.right(), None);

        // Right
        either = Either::Right("hello".to_string());
        assert!(either.is_right());
        assert_eq!(*either.as_ref().right().unwrap(), "hello");
        *either.as_mut().right().unwrap() = "world".to_string();
        assert_eq!(*either.as_ref().right().unwrap(), "world");
        assert_eq!(format!("{either}"), "world");
        assert_eq!(either.left(), None);

        Ok(())
    }

    #[tokio::test]
    async fn test_either_into_luau() -> Result<()> {
        let lua = Luau::new();

        // Direct conversion
        let mut either = Either::<i32, &Table>::Left(42);
        assert_eq!(either.into_luau(&lua)?, Value::Integer(42));
        let t = lua.create_table()?;
        either = Either::Right(&t);
        assert!(matches!(either.into_luau(&lua)?, Value::Table(_)));

        // Push into stack
        let f = lua
            .create_function(|_, either: Either<i32, Table>| either.right().unwrap().set("hello", "world"))?;
        let t = lua.create_table()?;
        either = Either::Right(&t);
        f.call::<()>(either).await?;
        assert_eq!(t.get::<String>("hello")?, "world");

        let f = lua.create_function(|_, either: Either<i32, Table>| Ok(either.left().unwrap() + 1))?;
        either = Either::Left(42);
        assert_eq!(f.call::<i32>(either).await?, 43);

        Ok(())
    }

    #[tokio::test]
    async fn test_either_from_luau() -> Result<()> {
        let lua = Luau::new();

        // From value
        let mut either = Either::<i32, Table>::from_luau(Value::Integer(42), &lua)?;
        assert!(either.is_left());
        assert_eq!(*either.as_ref().left().unwrap(), 42);
        let t = lua.create_table()?;
        either = Either::<i32, Table>::from_luau(Value::Table(t.clone()), &lua)?;
        assert!(either.is_right());
        assert_eq!(either.as_ref().right().unwrap(), &t);
        match Either::<i32, Table>::from_luau(Value::String(lua.create_string("abc")?), &lua) {
            Err(Error::FromLuauConversionError { to, .. }) => assert_eq!(to, "Either<i32, Table>"),
            _ => panic!("expected `Error::FromLuauConversionError`"),
        }

        // From stack
        let f = lua.create_function(|_, either: Either<i32, Table>| Ok(either))?;
        let either = f.call::<Either<i32, Table>>(42).await?;
        assert!(either.is_left());
        assert_eq!(*either.as_ref().left().unwrap(), 42);

        let either = f.call::<Either<i32, Table>>([5; 5]).await?;
        assert!(either.is_right());
        assert_eq!(either.as_ref().right().unwrap(), &[5; 5]);

        // Check error message
        match f.call::<Value>("hello").await {
            Ok(_) => panic!("expected error, got Ok"),
            Err(ref err @ Error::CallbackError { ref cause, .. }) => {
                match cause.as_ref() {
                    Error::BadArgument { cause, .. } => match cause.as_ref() {
                        Error::FromLuauConversionError { to, .. } => {
                            assert_eq!(to, "Either<i32, Table>")
                        }
                        err => panic!("expected `Error::FromLuauConversionError`, got {err:?}"),
                    },
                    err => panic!("expected `Error::BadArgument`, got {err:?}"),
                }
                assert!(
                    err.to_string()
                        .starts_with("bad argument #1: error converting Luau string to Either<i32, Table>"),
                );
            }
            err => panic!("expected `Error::CallbackError`, got {err:?}"),
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_char_into_luau() -> Result<()> {
        let lua = Luau::new();

        let v = '🦀';
        let v2 = v.into_luau(&lua)?;
        assert_eq!(*v2.as_string().unwrap(), v.to_string());

        Ok(())
    }

    #[tokio::test]
    async fn test_char_from_luau() -> Result<()> {
        let lua = Luau::new();

        assert_eq!(char::from_luau("A".into_luau(&lua)?, &lua)?, 'A');
        assert_eq!(char::from_luau(65.into_luau(&lua)?, &lua)?, 'A');
        assert_eq!(char::from_luau(128175.into_luau(&lua)?, &lua)?, '💯');
        assert!(
            char::from_luau(5456324.into_luau(&lua)?, &lua)
                .is_err_and(|e| e.to_string().contains("integer out of range"))
        );
        assert!(
            char::from_luau("hello".into_luau(&lua)?, &lua)
                .is_err_and(|e| { e.to_string().contains("expected string to have exactly one char") })
        );
        assert!(
            char::from_luau(HashMap::<String, String>::new().into_luau(&lua)?, &lua)
                .is_err_and(|e| e.to_string().contains("expected string or integer"))
        );

        Ok(())
    }
}
