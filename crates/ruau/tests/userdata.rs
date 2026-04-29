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

use std::{any::TypeId, collections::HashMap, sync::Arc};

use ruau::{
    AnyUserData, Error, ExternalError, Function, Luau, LuauString, MetaMethod, Nil, ObjectLike, Result,
    UserData, UserDataFields, UserDataMethods, Value, Variadic,
    userdata::{UserDataOwned, UserDataRef, UserDataRegistry},
};

#[tokio::test]
async fn test_userdata() -> Result<()> {
    struct UserData1(i64);
    struct UserData2(Box<i64>);

    impl UserData for UserData1 {}
    impl UserData for UserData2 {}

    let lua = Luau::new();
    let userdata1 = lua.create_userdata(UserData1(1))?;
    let userdata2 = lua.create_userdata(UserData2(Box::new(2)))?;

    assert!(userdata1.is::<UserData1>());
    assert!(userdata1.type_id() == Some(TypeId::of::<UserData1>()));
    assert!(!userdata1.is::<UserData2>());
    assert!(userdata2.is::<UserData2>());
    assert!(!userdata2.is::<UserData1>());
    assert!(userdata2.type_id() == Some(TypeId::of::<UserData2>()));

    assert_eq!(userdata1.borrow::<UserData1>()?.0, 1);
    assert_eq!(*userdata2.borrow::<UserData2>()?.0, 2);

    Ok(())
}

#[tokio::test]
async fn test_methods() -> Result<()> {
    #[derive(serde::Serialize)]
    struct MyUserData(i64);

    impl UserData for MyUserData {
        fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
            methods.add_method("get_value", |_, data, ()| Ok(data.0));
            methods.add_method_mut("set_value", |_, data, args| {
                data.0 = args;
                Ok(())
            });
        }
    }

    async fn check_methods(lua: &Luau, userdata: AnyUserData) -> Result<()> {
        let globals = lua.globals();
        globals.set("userdata", &userdata)?;
        lua.load(
            r#"
            function get_it()
                return userdata:get_value()
            end

            function set_it(i)
                return userdata:set_value(i)
            end
        "#,
        )
        .exec()
        .await?;
        let get = globals.get::<Function>("get_it")?;
        let set = globals.get::<Function>("set_it")?;
        assert_eq!(get.call::<i64>(()).await?, 42);
        userdata.borrow_mut::<MyUserData>()?.0 = 64;
        assert_eq!(get.call::<i64>(()).await?, 64);
        set.call::<()>(100).await?;
        assert_eq!(get.call::<i64>(()).await?, 100);
        Ok(())
    }

    let lua = Luau::new();

    check_methods(&lua, lua.create_userdata(MyUserData(42))?).await?;

    // Additionally check serializable userdata

    check_methods(&lua, lua.create_serializable_userdata(MyUserData(42))?).await?;

    Ok(())
}

#[tokio::test]
async fn test_method_variadic() -> Result<()> {
    struct MyUserData(i64);

    impl UserData for MyUserData {
        fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
            methods.add_method("get", |_, data, ()| Ok(data.0));
            methods.add_method_mut("add", |_, data, vals: Variadic<i64>| {
                data.0 += vals.into_iter().sum::<i64>();
                Ok(())
            });
        }
    }

    let lua = Luau::new();
    let globals = lua.globals();
    globals.set("userdata", MyUserData(0))?;
    lua.load("userdata:add(1, 5, -10)").exec().await?;
    let ud: UserDataRef<MyUserData> = globals.get("userdata")?;
    assert_eq!(ud.0, -4);

    Ok(())
}

#[tokio::test]
async fn test_metamethods() -> Result<()> {
    #[derive(Copy, Clone)]
    struct MyUserData(i64);

    impl UserData for MyUserData {
        fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
            methods.add_method("get", |_, data, ()| Ok(data.0));
            methods.add_meta_function(
                MetaMethod::Add,
                |_, (lhs, rhs): (UserDataRef<Self>, UserDataRef<Self>)| Ok(Self(lhs.0 + rhs.0)),
            );
            methods.add_meta_function(
                MetaMethod::Sub,
                |_, (lhs, rhs): (UserDataRef<Self>, UserDataRef<Self>)| Ok(Self(lhs.0 - rhs.0)),
            );
            methods.add_meta_function(
                MetaMethod::Eq,
                |_, (lhs, rhs): (UserDataRef<Self>, UserDataRef<Self>)| Ok(lhs.0 == rhs.0),
            );
            methods.add_meta_method(MetaMethod::Index, |_, data, index: LuauString| {
                if index.to_str()? == "inner" {
                    Ok(data.0)
                } else {
                    Err("no such custom index".into_luau_err())
                }
            });
        }
    }

    let lua = Luau::new();
    let globals = lua.globals();
    globals.set("userdata1", MyUserData(7))?;
    globals.set("userdata2", MyUserData(3))?;
    globals.set("userdata3", MyUserData(3))?;
    assert_eq!(
        lua.load("userdata1 + userdata2")
            .eval::<UserDataRef<MyUserData>>()
            .await?
            .0,
        10
    );

    assert_eq!(
        lua.load("userdata1 - userdata2")
            .eval::<UserDataRef<MyUserData>>()
            .await?
            .0,
        4
    );
    assert_eq!(lua.load("userdata1:get()").eval::<i64>().await?, 7);
    assert_eq!(lua.load("userdata2.inner").eval::<i64>().await?, 3);
    assert!(lua.load("userdata2.nonexist_field").eval::<()>().await.is_err());

    let userdata2: Value = globals.get("userdata2")?;
    let userdata3: Value = globals.get("userdata3")?;

    assert!(lua.load("userdata2 == userdata3").eval::<bool>().await?);
    assert!(userdata2 != userdata3); // because references are differ
    assert!(userdata2.equals(&userdata3)?);

    let userdata1: AnyUserData = globals.get("userdata1")?;
    assert!(userdata1.metatable()?.contains(MetaMethod::Add)?);
    assert!(userdata1.metatable()?.contains(MetaMethod::Sub)?);
    assert!(userdata1.metatable()?.contains(MetaMethod::Index)?);
    assert!(!userdata1.metatable()?.contains(MetaMethod::Pow)?);

    Ok(())
}

#[tokio::test]
async fn test_gc_userdata() -> Result<()> {
    struct MyUserdata {
        id: u8,
    }

    impl UserData for MyUserdata {
        fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
            methods.add_method("access", |_, this, ()| {
                assert_eq!(this.id, 123);
                Ok(())
            });
        }
    }

    let lua = Luau::new();
    lua.globals().set("userdata", MyUserdata { id: 123 })?;

    assert!(
        lua.load(
            r#"
            local tbl = setmetatable({
                userdata = userdata
            }, { __gc = function(self)
                -- resurrect userdata
                hatch = self.userdata
            end })

            tbl = nil
            userdata = nil  -- make table and userdata collectable
            collectgarbage("collect")
            hatch:access()
        "#
        )
        .exec()
        .await
        .is_err()
    );

    Ok(())
}

#[tokio::test]
async fn test_userdata_take() -> Result<()> {
    #[derive(Debug)]
    struct MyUserdata(Arc<i64>);

    impl UserData for MyUserdata {
        fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
            methods.add_method("num", |_, this, ()| Ok(*this.0))
        }
    }

    impl serde::Serialize for MyUserdata {
        fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            serializer.serialize_i64(*self.0)
        }
    }

    async fn check_userdata_take(lua: &Luau, userdata: AnyUserData, rc: Arc<i64>) -> Result<()> {
        lua.globals().set("userdata", &userdata)?;
        assert_eq!(Arc::strong_count(&rc), 3);
        {
            let _value = userdata.borrow::<MyUserdata>()?;
            // We should not be able to take userdata if it's borrowed
            match userdata.take::<MyUserdata>() {
                Err(Error::UserDataBorrowMutError) => {}
                r => panic!("expected `UserDataBorrowMutError` error, got {:?}", r),
            }
        }

        let value = userdata.take::<MyUserdata>()?;
        assert_eq!(*value.0, 18);
        drop(value);
        assert_eq!(Arc::strong_count(&rc), 2);

        match userdata.borrow::<MyUserdata>() {
            Err(Error::UserDataDestructed) => {}
            r => panic!("expected `UserDataDestructed` error, got {:?}", r),
        }
        match lua.load("userdata:num()").exec().await {
            Err(Error::CallbackError { ref cause, .. }) => match cause.as_ref() {
                Error::UserDataDestructed => {}
                err => panic!("expected `UserDataDestructed`, got {:?}", err),
            },
            r => panic!("improper return for destructed userdata: {:?}", r),
        }

        assert!(!userdata.is::<MyUserdata>());

        drop(userdata);
        lua.globals().raw_remove("userdata")?;
        lua.gc_collect()?;
        lua.gc_collect()?;
        assert_eq!(Arc::strong_count(&rc), 1);

        Ok(())
    }

    let lua = Luau::new();

    let rc = Arc::new(18);
    let userdata = lua.create_userdata(MyUserdata(rc.clone()))?;
    userdata.set_nth_user_value(2, MyUserdata(rc.clone()))?;
    check_userdata_take(&lua, userdata, rc).await?;

    // Additionally check serializable userdata

    {
        let rc = Arc::new(18);
        let userdata = lua.create_serializable_userdata(MyUserdata(rc.clone()))?;
        userdata.set_nth_user_value(2, MyUserdata(rc.clone()))?;
        check_userdata_take(&lua, userdata, rc).await?;
    }

    Ok(())
}

#[tokio::test]
async fn test_userdata_destroy() -> Result<()> {
    struct MyUserdata(#[allow(unused)] Arc<()>);

    impl UserData for MyUserdata {
        fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
            methods.add_method("try_destroy", |lua, _this, ()| {
                let ud = lua.globals().get::<AnyUserData>("ud")?;
                match ud.destroy() {
                    Err(Error::UserDataBorrowMutError) => {}
                    r => panic!("expected `UserDataBorrowMutError` error, got {:?}", r),
                }
                Ok(())
            });
        }
    }

    let rc = Arc::new(());

    let lua = Luau::new();
    let ud = lua.create_userdata(MyUserdata(rc.clone()))?;
    ud.set_user_value(MyUserdata(rc.clone()))?;
    lua.globals().set("userdata", ud)?;

    assert_eq!(Arc::strong_count(&rc), 3);

    // Should destroy all objects
    lua.globals().raw_remove("userdata")?;
    lua.gc_collect()?;
    lua.gc_collect()?;

    assert_eq!(Arc::strong_count(&rc), 1);

    let ud = lua.create_userdata(MyUserdata(rc.clone()))?;
    assert_eq!(Arc::strong_count(&rc), 2);
    let ud_ref = ud.borrow::<MyUserdata>()?;
    // With active `UserDataRef` this methods only marks userdata as destructed
    // without running destructor
    ud.destroy().unwrap();
    assert_eq!(Arc::strong_count(&rc), 2);
    drop(ud_ref);
    assert_eq!(Arc::strong_count(&rc), 1);

    // We cannot destroy (internally) borrowed userdata
    let ud = lua.create_userdata(MyUserdata(rc.clone()))?;
    lua.globals().set("ud", &ud)?;
    lua.load("ud:try_destroy()").exec().await.unwrap();
    ud.destroy().unwrap();
    assert_eq!(Arc::strong_count(&rc), 1);

    Ok(())
}

#[tokio::test]
async fn test_userdata_method_once() -> Result<()> {
    struct MyUserdata(Arc<i64>);

    impl UserData for MyUserdata {
        fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
            methods.add_method_once("take_value", |_, this, ()| Ok(*this.0));
        }
    }

    let lua = Luau::new();
    let rc = Arc::new(42);
    let userdata = lua.create_userdata(MyUserdata(rc.clone()))?;
    lua.globals().set("userdata", &userdata)?;

    // Control userdata
    let userdata2 = lua.create_userdata(MyUserdata(rc.clone()))?;
    lua.globals().set("userdata2", userdata2)?;

    assert_eq!(lua.load("userdata:take_value()").eval::<i64>().await?, 42);
    match lua.load("userdata2.take_value(userdata)").eval::<i64>().await {
        Err(Error::CallbackError { cause, .. }) => {
            let err = cause.to_string();
            assert!(err.contains("bad argument `self` to `MyUserdata.take_value`"));
            assert!(err.contains("userdata has been destructed"));
        }
        r => panic!("expected Err(CallbackError), got {r:?}"),
    }
    assert_eq!(Arc::strong_count(&rc), 2);

    Ok(())
}

#[tokio::test]
async fn test_user_values() -> Result<()> {
    struct MyUserData;

    impl UserData for MyUserData {}

    let lua = Luau::new();
    let ud = lua.create_userdata(MyUserData)?;

    ud.set_nth_user_value(1, "hello")?;
    ud.set_nth_user_value(2, "world")?;
    ud.set_nth_user_value(65535, 321)?;
    assert_eq!(ud.nth_user_value::<LuauString>(1)?, "hello");
    assert_eq!(ud.nth_user_value::<LuauString>(2)?, "world");
    assert_eq!(ud.nth_user_value::<Value>(3)?, Value::Nil);
    assert_eq!(ud.nth_user_value::<i32>(65535)?, 321);

    assert!(ud.nth_user_value::<Value>(0).is_err());
    assert!(ud.nth_user_value::<Value>(65536).is_err());

    // Named user values
    let ud = lua.create_userdata(MyUserData)?;
    ud.set_named_user_value("name", "alex")?;
    ud.set_named_user_value("age", 10)?;

    assert_eq!(ud.named_user_value::<String>("name")?, "alex");
    assert_eq!(ud.named_user_value::<i32>("age")?, 10);
    assert_eq!(ud.named_user_value::<Value>("nonexist")?, Value::Nil);

    Ok(())
}

#[tokio::test]
async fn test_functions() -> Result<()> {
    struct MyUserData(i64);

    impl UserData for MyUserData {
        fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
            methods.add_function("get_value", |_, ud: AnyUserData| Ok(ud.borrow::<Self>()?.0));
            methods.add_function_mut("set_value", |_, (ud, value): (AnyUserData, i64)| {
                ud.borrow_mut::<Self>()?.0 = value;
                Ok(())
            });
            methods.add_function("get_constant", |_, ()| Ok(7));
        }
    }

    let lua = Luau::new();
    let globals = lua.globals();
    let userdata = lua.create_userdata(MyUserData(42))?;
    globals.set("userdata", &userdata)?;
    lua.load(
        r#"
        function get_it()
            return userdata:get_value()
        end

        function set_it(i)
            return userdata:set_value(i)
        end

        function get_constant()
            return userdata.get_constant()
        end
    "#,
    )
    .exec()
    .await?;
    let get = globals.get::<Function>("get_it")?;
    let set = globals.get::<Function>("set_it")?;
    let get_constant = globals.get::<Function>("get_constant")?;
    assert_eq!(get.call::<i64>(()).await?, 42);
    userdata.borrow_mut::<MyUserData>()?.0 = 64;
    assert_eq!(get.call::<i64>(()).await?, 64);
    set.call::<()>(100).await?;
    assert_eq!(get.call::<i64>(()).await?, 100);
    assert_eq!(get_constant.call::<i64>(()).await?, 7);

    Ok(())
}

#[tokio::test]
async fn test_fields() -> Result<()> {
    let lua = Luau::new();
    let globals = lua.globals();

    #[derive(Copy, Clone)]
    struct MyUserData(i64);

    impl UserData for MyUserData {
        fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
            fields.add_field("static", "constant");
            fields.add_field_method_get("val", |_, data| Ok(data.0));
            fields.add_field_method_set("val", |_, data, val| {
                data.0 = val;
                Ok(())
            });

            // Field that emulates method
            fields.add_field_function_get("val_fget", |lua, ud| {
                lua.create_function(move |_, ()| Ok(ud.borrow::<Self>()?.0))
            });

            // Use userdata "uservalue" storage
            fields.add_field_function_get("uval", |_, ud| ud.user_value::<Option<LuauString>>());
            fields.add_field_function_set("uval", |_, ud, s: Option<LuauString>| ud.set_user_value(s));

            fields.add_meta_field(MetaMethod::Index, HashMap::from([("f", 321)]));
            fields.add_meta_field_with(MetaMethod::NewIndex, |lua| {
                lua.create_function(|lua, (_, field, val): (AnyUserData, String, Value)| {
                    lua.globals().set(field, val)?;
                    Ok(())
                })
            })
        }

        fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
            methods.add_method("dummy", |_, _, ()| Ok(()));
        }
    }

    globals.set("ud", MyUserData(7))?;
    lua.load(
        r#"
        assert(ud.static == "constant")
        assert(ud.val == 7)
        ud.val = 10
        assert(ud.val == 10)
        assert(ud:val_fget() == 10)

        assert(ud.uval == nil)
        ud.uval = "hello"
        assert(ud.uval == "hello")

        assert(ud.f == 321)

        ud.unknown = 789
        assert(unknown == 789)
    "#,
    )
    .exec()
    .await?;

    // Case: fields + __index metamethod (function)
    struct MyUserData2(i64);

    impl UserData for MyUserData2 {
        fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
            fields.add_field("z", 0);
            fields.add_field_method_get("x", |_, data| Ok(data.0));
        }

        fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
            methods.add_meta_method(MetaMethod::Index, |_, _, name: LuauString| {
                match name.to_str()?.as_ref() {
                    "y" => Ok(Some(-1)),
                    _ => Ok(None),
                }
            });
        }
    }

    globals.set("ud", MyUserData2(1))?;
    lua.load(
        r#"
        assert(ud.x == 1)
        assert(ud.y == -1)
        assert(ud.z == 0)
    "#,
    )
    .exec()
    .await?;

    Ok(())
}

#[tokio::test]
async fn test_metatable() -> Result<()> {
    #[derive(Copy, Clone)]
    struct MyUserData;

    impl UserData for MyUserData {
        fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
            methods.add_function("my_type_name", |_, data: AnyUserData| {
                let metatable = data.metatable()?;
                metatable.get::<LuauString>(MetaMethod::Type)
            });
        }
    }

    let lua = Luau::new();
    let globals = lua.globals();
    globals.set("ud", MyUserData)?;
    lua.load(r#"assert(ud:my_type_name() == "MyUserData")"#)
        .exec()
        .await?;

    lua.load(r#"assert(tostring(ud):sub(1, 11) == "MyUserData:")"#)
        .exec()
        .await?;
    lua.load(r#"assert(typeof(ud) == "MyUserData")"#).exec().await?;

    let ud: AnyUserData = globals.get("ud")?;
    let metatable = ud.metatable()?;

    match metatable.get::<Value>("__gc") {
        Ok(_) => panic!("expected MetaMethodRestricted, got no error"),
        Err(Error::MetaMethodRestricted(_)) => {}
        Err(e) => panic!("expected MetaMethodRestricted, got {:?}", e),
    }

    match metatable.set(MetaMethod::Index, Nil) {
        Ok(_) => panic!("expected MetaMethodRestricted, got no error"),
        Err(Error::MetaMethodRestricted(_)) => {}
        Err(e) => panic!("expected MetaMethodRestricted, got {:?}", e),
    }

    let mut methods = metatable
        .pairs()
        .map(|kv: Result<(_, Value)>| Ok(kv?.0))
        .collect::<Result<Vec<_>>>()?;
    methods.sort();
    assert_eq!(methods, vec!["__index", MetaMethod::Type.name()]);

    #[derive(Copy, Clone)]
    struct MyUserData2;

    impl UserData for MyUserData2 {
        fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
            fields.add_meta_field_with("__index", |_| Ok(1));
        }
    }

    match lua.create_userdata(MyUserData2) {
        Ok(_) => panic!("expected MetaMethodTypeError, got no error"),
        Err(Error::MetaMethodTypeError { .. }) => {}
        Err(e) => panic!("expected MetaMethodTypeError, got {:?}", e),
    }

    #[derive(Copy, Clone)]
    struct MyUserData3;

    impl UserData for MyUserData3 {
        fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
            fields.add_meta_field_with(MetaMethod::Type, |_| Ok("CustomName"));
        }
    }

    let ud = lua.create_userdata(MyUserData3)?;
    let metatable = ud.metatable()?;
    assert_eq!(
        metatable.get::<LuauString>(MetaMethod::Type)?.to_str()?,
        "CustomName"
    );

    Ok(())
}

#[tokio::test]
async fn test_userdata_type_name() -> Result<()> {
    struct MyUserData;
    impl UserData for MyUserData {}

    struct MyUserdataCustom;
    impl UserData for MyUserdataCustom {
        fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
            fields.add_meta_field_with(MetaMethod::Type, |_| Ok("MyCustomName"));
        }
    }

    // ruau always sets __name/__type; override with a non-string to test the "userdata" fallback
    struct MyUserdataInvalid;
    impl UserData for MyUserdataInvalid {
        fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
            fields.add_meta_field_with(MetaMethod::Type, |_| Ok(42_i64));
        }
    }

    let lua = Luau::new();

    // Default is the Rust type name
    let ud = lua.create_userdata(MyUserData)?;
    assert_eq!(ud.type_name()?, "MyUserData");

    // Custom name from metatable
    let ud = lua.create_userdata(MyUserdataCustom)?;
    assert_eq!(ud.type_name()?, "MyCustomName");

    // Invalid type name should fallback to "userdata"
    let ud = lua.create_userdata(MyUserdataInvalid)?;
    assert_eq!(ud.type_name()?.to_str()?, "userdata");

    Ok(())
}

#[tokio::test]
async fn test_userdata_proxy() -> Result<()> {
    struct MyUserData(i64);

    impl UserData for MyUserData {
        fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
            fields.add_field("static_field", 123);
            fields.add_field_method_get("n", |_, this| Ok(this.0));
        }

        fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
            methods.add_function("new", |_, n| Ok(Self(n)));

            methods.add_method("plus", |_, this, n: i64| Ok(this.0 + n));
        }
    }

    let lua = Luau::new();
    let globals = lua.globals();
    globals.set("MyUserData", lua.create_proxy::<MyUserData>()?)?;

    assert!(!globals.get::<AnyUserData>("MyUserData")?.is_proxy::<()>());
    assert!(globals.get::<AnyUserData>("MyUserData")?.is_proxy::<MyUserData>());

    lua.load(
        r#"
        assert(MyUserData.static_field == 123)
        local data = MyUserData.new(321)
        assert(data.static_field == 123)
        assert(data.n == 321)
        assert(data:plus(1) == 322)

        -- Error when accessing the proxy object fields and methods that require instance

        local ok = pcall(function() return MyUserData.n end)
        assert(not ok)

        ok = pcall(function() return MyUserData:plus(1) end)
        assert(not ok)
    "#,
    )
    .exec()
    .await
}

#[tokio::test]
async fn test_any_userdata() -> Result<()> {
    let lua = Luau::new();

    lua.register_userdata_type::<String>(|reg| {
        reg.add_method("get", |_, this, ()| Ok(this.clone()));
        reg.add_method_mut("concat", |_, this, s: LuauString| {
            this.push_str(&s.to_string_lossy());
            Ok(())
        });
    })?;

    let ud = lua.create_opaque_userdata("hello".to_string())?;
    assert_eq!(&*ud.borrow::<String>()?, "hello");

    lua.globals().set("ud", ud)?;
    lua.load(
        r#"
        assert(ud:get() == "hello")
        ud:concat(", world")
        assert(ud:get() == "hello, world")
    "#,
    )
    .exec()
    .await
    .unwrap();

    Ok(())
}

#[tokio::test]
async fn test_any_userdata_wrap() -> Result<()> {
    let lua = Luau::new();

    lua.register_userdata_type::<String>(|reg| {
        reg.add_method("get", |_, this, ()| Ok(this.clone()));
    })?;

    lua.globals().set("s", AnyUserData::wrap("hello".to_string()))?;
    lua.load(
        r#"
        assert(s:get() == "hello")
    "#,
    )
    .exec()
    .await
    .unwrap();

    Ok(())
}

#[tokio::test]
async fn test_userdata_object_like() -> Result<()> {
    let lua = Luau::new();

    #[derive(Clone, Copy)]
    struct MyUserData(u32);

    impl UserData for MyUserData {
        fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
            fields.add_field_method_get("n", |_, this| Ok(this.0));
            fields.add_field_method_set("n", |_, this, val| {
                this.0 = val;
                Ok(())
            });
        }

        fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
            methods.add_meta_method(MetaMethod::Call, |_, _this, ()| Ok("called"));
            methods.add_method_mut("add", |_, this, x: u32| {
                this.0 += x;
                Ok(())
            });
        }
    }

    let ud = lua.create_userdata(MyUserData(123))?;

    assert_eq!(ud.get::<u32>("n")?, 123);
    ud.set("n", 321)?;
    assert_eq!(ud.get::<u32>("n")?, 321);
    assert_eq!(ud.get::<Option<u32>>("non-existent")?, None);
    match ud.set("non-existent", 123) {
        Err(Error::RuntimeError(_)) => {}
        r => panic!("expected RuntimeError, got {r:?}"),
    }

    assert_eq!(ud.call::<LuauString>(()).await?, "called");

    ud.call_method::<()>("add", 2).await?;
    assert_eq!(ud.get::<u32>("n")?, 323);

    match ud.call_method::<()>("non_existent", ()).await {
        Err(Error::RuntimeError(err)) => {
            assert!(err.contains("attempt to call a nil value (function 'non_existent')"))
        }
        r => panic!("expected RuntimeError, got {r:?}"),
    }

    assert!(ud.to_string()?.starts_with("MyUserData"));

    Ok(())
}

#[tokio::test]
async fn test_userdata_method_errors() -> Result<()> {
    struct MyUserData(i64);

    impl UserData for MyUserData {
        fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
            methods.add_method("get_value", |_, data, ()| Ok(data.0));
        }
    }

    let lua = Luau::new();

    let ud = lua.create_userdata(MyUserData(123))?;
    let res = ud.call_function::<()>("get_value", "not a userdata").await;
    match res {
        Err(Error::CallbackError { cause, .. }) => match cause.as_ref() {
            Error::BadArgument {
                to,
                name,
                cause: cause2,
                ..
            } => {
                assert_eq!(to.as_deref(), Some("MyUserData.get_value"));
                assert_eq!(name.as_deref(), Some("self"));
                assert_eq!(
                    cause2.to_string(),
                    "error converting Luau string to userdata (expected userdata of type 'MyUserData')"
                );
            }
            err => panic!("expected BadArgument, got {err:?}"),
        },
        r => panic!("expected CallbackError, got {r:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn test_userdata_pointer() -> Result<()> {
    let lua = Luau::new();

    let ud1 = lua.create_opaque_userdata("hello")?;
    let ud2 = lua.create_opaque_userdata("hello")?;

    assert_eq!(ud1.to_pointer(), ud1.to_pointer());
    // Different userdata objects with the same value should have different pointers
    assert_ne!(ud1.to_pointer(), ud2.to_pointer());

    Ok(())
}

#[cfg(feature = "macros")]
#[tokio::test]
async fn test_userdata_derive() -> Result<()> {
    let lua = Luau::new();

    // Simple struct

    #[derive(Clone, Copy, ruau::FromLuau)]
    struct MyUserData(i32);

    lua.register_userdata_type::<MyUserData>(|reg| {
        reg.add_function("val", |_, this: MyUserData| Ok(this.0));
    })?;

    lua.globals().set("ud", AnyUserData::wrap(MyUserData(123)))?;
    lua.load("assert(ud:val() == 123)").exec().await?;

    // More complex struct where generics and where clause

    #[derive(Clone, Copy, ruau::FromLuau)]
    struct MyUserData2<'a, T: ?Sized>(&'a T)
    where
        T: Copy;

    lua.register_userdata_type::<MyUserData2<'static, i32>>(|reg| {
        reg.add_function("val", |_, this: MyUserData2<'static, i32>| Ok(*this.0));
    })?;

    lua.globals().set("ud", AnyUserData::wrap(MyUserData2(&321)))?;
    lua.load("assert(ud:val() == 321)").exec().await?;

    Ok(())
}

#[tokio::test]
async fn test_nested_userdata_gc() -> Result<()> {
    let lua = Luau::new();

    let counter = Arc::new(());
    let arr = vec![lua.create_opaque_userdata(counter.clone())?];
    let arr_ud = lua.create_opaque_userdata(arr)?;

    assert_eq!(Arc::strong_count(&counter), 2);
    drop(arr_ud);
    // On first iteration Luau will destroy the array, on second - userdata
    lua.gc_collect()?;
    lua.gc_collect()?;
    assert_eq!(Arc::strong_count(&counter), 1);

    Ok(())
}

#[tokio::test]
async fn test_userdata_namecall() -> Result<()> {
    let lua = Luau::new();

    struct MyUserData;

    impl UserData for MyUserData {
        fn register(registry: &mut UserDataRegistry<Self>) {
            registry.add_method("method", |_, _, ()| Ok("method called"));
            registry.add_field_method_get("field", |_, _| Ok("field value"));

            registry.add_meta_method(MetaMethod::Index, |_, _, key: LuauString| Ok(key));

            registry.enable_namecall();
        }
    }

    let ud = lua.create_userdata(MyUserData)?;
    lua.globals().set("ud", &ud)?;
    lua.load(
        r#"
        assert(ud:method() == "method called")
        assert(ud.field == "field value")
        assert(ud.dynamic_field == "dynamic_field")
        local ok, err = pcall(function() return ud:dynamic_field() end)
        assert(tostring(err):find("attempt to call an unknown method 'dynamic_field'") ~= nil)
        "#,
    )
    .exec()
    .await?;

    ud.destroy()?;
    let err = lua.load("ud:method()").exec().await.unwrap_err();
    assert!(err.to_string().contains("userdata has been destructed"));

    Ok(())
}

#[tokio::test]
async fn test_userdata_owned() -> Result<()> {
    #[derive(Debug)]
    struct MyUserdata(Arc<i64>);

    impl UserData for MyUserdata {
        fn register(registry: &mut UserDataRegistry<Self>) {
            registry.add_method("num", |_, this, ()| Ok(*this.0));
        }
    }

    let lua = Luau::new();
    let rc = Arc::new(42);

    // It takes ownership and destructs the Luau userdata
    let ud = lua.create_userdata(MyUserdata(rc.clone()))?;
    assert_eq!(Arc::strong_count(&rc), 2);
    let owned: UserDataOwned<MyUserdata> = lua.convert(&ud)?;
    assert_eq!(*owned.0.0, 42);
    drop(owned);
    assert_eq!(Arc::strong_count(&rc), 1);
    match ud.borrow::<MyUserdata>() {
        Err(Error::UserDataDestructed) => {}
        r => panic!("expected UserDataDestructed, got {:?}", r),
    }

    // Cannot take while borrowed
    let rc = Arc::new(7);
    let ud = lua.create_userdata(MyUserdata(rc))?;
    let borrowed = ud.borrow::<MyUserdata>()?;
    match lua.convert::<UserDataOwned<MyUserdata>>(&ud) {
        Err(Error::UserDataBorrowMutError) => {}
        r => panic!("expected UserDataBorrowMutError, got {:?}", r),
    }
    drop(borrowed);

    // Works as a function parameter
    let f = lua.create_function(|_, owned: UserDataOwned<MyUserdata>| Ok(*owned.0.0))?;
    let rc = Arc::new(55);
    let ud = lua.create_userdata(MyUserdata(rc.clone()))?;
    assert_eq!(f.call::<i64>(ud).await?, 55);
    assert_eq!(Arc::strong_count(&rc), 1); // dropped after call

    Ok(())
}

#[tokio::test]
async fn test_userdata_tag_exhaustion_falls_back() -> Result<()> {
    struct Many<const N: usize>;
    impl<const N: usize> UserData for Many<N> {}

    macro_rules! create_many {
        ($lua:expr, $($n:literal),* $(,)?) => {
            $(
                let _ = $lua.create_userdata(Many::<$n>)?;
            )*
        };
    }

    let lua = Luau::new();
    create_many!(
        lua, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
        26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50,
        51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71, 72, 73, 74, 75,
        76, 77, 78, 79, 80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 94, 95, 96, 97, 98, 99, 100,
        101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112, 113, 114, 115, 116, 117, 118, 119, 120,
        121, 122, 123, 124, 125, 126, 127, 128, 129,
    );

    let fallback = lua.create_userdata(Many::<130>)?;
    assert!(fallback.is::<Many<130>>());

    Ok(())
}
