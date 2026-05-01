//! Walks through common ruau APIs in one executable example.

use std::{f32, iter::FromIterator};

use ruau::{FromLuau, Function, Luau, MetaMethod, Result, UserData, UserDataMethods, Value, Variadic, chunk};

#[derive(Copy, Clone)]
struct Vec2(f32, f32);

// We can implement `FromLuau` trait for our `Vec2` to return a copy.
impl FromLuau for Vec2 {
    fn from_luau(value: Value, _: &Luau) -> Result<Self> {
        match value {
            Value::UserData(ud) => Ok(*ud.borrow::<Self>()?),
            _ => unreachable!(),
        }
    }
}

impl UserData for Vec2 {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("magnitude", |_, vec, ()| {
            let mag_squared = vec.0 * vec.0 + vec.1 * vec.1;
            Ok(mag_squared.sqrt())
        });

        methods.add_meta_function(MetaMethod::Add, |_, (vec1, vec2): (Self, Self)| {
            Ok(Self(vec1.0 + vec2.0, vec1.1 + vec2.1))
        });
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    // You can create a new Luau state with `Luau::new()`. This loads the default standard library
    // *without* the debug library.
    let lua = Luau::new();

    // You can get and set global variables. Notice that the globals table here is a permanent
    // reference to _G, and it is mutated behind the scenes as Luau code is loaded. This API is
    // based heavily around sharing and internal mutation (just like Luau itself).

    let globals = lua.globals();

    globals.set("string_var", "hello")?;
    globals.set("int_var", 42)?;

    assert_eq!(globals.get::<String>("string_var")?, "hello");
    assert_eq!(globals.get::<i64>("int_var")?, 42);

    // You can load and evaluate Luau code. The returned type of `Luau::load` is a builder
    // that allows you to change settings before running Luau code. Here, we are using it to set
    // the name of the loaded chunk to "example code", which will be used when Luau error
    // messages are printed.

    lua.load(
        r#"
            global = 'foo'..'bar'
        "#,
    )
    .name("example code")
    .exec()
    .await?;
    assert_eq!(globals.get::<String>("global")?, "foobar");

    assert_eq!(lua.load("1 + 1").eval::<i32>().await?, 2);
    assert!(lua.load("false == false").eval::<bool>().await?);
    assert_eq!(lua.load("return 1 + 2").eval::<i32>().await?, 3);

    // You can use the `chunk!` macro to use the Rust tokenizer and automatically capture variables.

    let a = 1;
    let b = 2;
    let name = "world";
    lua.load(chunk! {
        print($a + $b)
        print("hello, " .. $name)
    })
    .exec()
    .await?;

    // You can create and manage Luau tables

    let array_table = lua.create_table()?;
    array_table.set(1, "one")?;
    array_table.set(2, "two")?;
    array_table.set(3, "three")?;
    assert_eq!(array_table.len()?, 3);

    let map_table = lua.create_table()?;
    map_table.set("one", 1)?;
    map_table.set("two", 2)?;
    map_table.set("three", 3)?;
    let v: i64 = map_table.get("two")?;
    assert_eq!(v, 2);

    // You can pass values like `Table` back into Luau

    globals.set("array_table", array_table)?;
    globals.set("map_table", map_table)?;

    lua.load(
        r#"
            for k, v in pairs(array_table) do
                print(k, v)
            end

            for k, v in pairs(map_table) do
                print(k, v)
            end
        "#,
    )
    .exec()
    .await?;

    // You can load Luau functions.

    let print: Function = globals.get("print")?;
    print.call::<()>("hello from rust").await?;

    // This API generally handles variadic using tuples. This is one way to call a function with
    // multiple parameters:

    print.call::<()>(("hello", "again", "from", "rust")).await?;

    // But, you can also pass variadic arguments with the `Variadic` type.

    print
        .call::<()>(Variadic::from_iter(
            ["hello", "yet", "again", "from", "rust"].iter().cloned(),
        ))
        .await?;

    // You can bind Rust functions to Luau as well. Callbacks receive the Luau state itself as their
    // first parameter, and the arguments given to the function as the second parameter. The type
    // of the arguments can be anything that is convertible from the parameters given by Luau, in
    // this case, the function expects two string sequences.

    let check_equal = lua.create_function(|_, (list1, list2): (Vec<String>, Vec<String>)| {
        // This function just checks whether two string lists are equal, and in an inefficient way.
        // Luau callbacks return `ruau::Result`, an Ok value is a normal return, and an Err return
        // turns into a Luau error. Again, any type that is convertible to Luau may be returned.
        Ok(list1 == list2)
    })?;
    globals.set("check_equal", check_equal)?;

    // You can also accept runtime variadic arguments to Rust callbacks.

    let join = lua.create_function(|_, strings: Variadic<String>| {
        // (This is quadratic!, it's just an example!)
        Ok(strings.iter().fold("".to_owned(), |a, b| a + b))
    })?;
    globals.set("join", join)?;

    assert!(
        lua.load(r#"check_equal({"a", "b", "c"}, {"a", "b", "c"})"#)
            .eval::<bool>()
            .await?
    );
    assert!(
        !lua.load(r#"check_equal({"a", "b", "c"}, {"d", "e", "f"})"#)
            .eval::<bool>()
            .await?
    );
    assert_eq!(lua.load(r#"join("a", "b", "c")"#).eval::<String>().await?, "abc");

    // Callbacks receive a Luau state as their first parameter so that they can use it to
    // create new Luau values, if necessary.

    let create_table = lua.create_function(|lua, ()| {
        let t = lua.create_table()?;
        t.set(1, 1)?;
        t.set(2, 2)?;
        Ok(t)
    })?;
    globals.set("create_table", create_table)?;

    assert_eq!(lua.load(r#"create_table()[2]"#).eval::<i32>().await?, 2);

    // You can create userdata with methods and metamethods defined on them.
    // Here's a worked example that shows many of the features of this API
    // together

    let vec2_constructor = lua.create_function(|_, (x, y): (f32, f32)| Ok(Vec2(x, y)))?;
    globals.set("vec2", vec2_constructor)?;

    assert!(
        (lua.load("(vec2(1, 2) + vec2(2, 2)):magnitude()")
            .eval::<f32>()
            .await?
            - 5.0)
            .abs()
            < f32::EPSILON
    );

    // Normally, Rust types passed to `Luau` must be `'static`, because there is no way to be
    // sure of their lifetime inside the Luau state. There is, however, a limited way to lift this
    // requirement. You can call `Luau::scope` to create userdata and callbacks types that only live
    // for as long as the call to scope, but do not have to be `'static` (and `Send`).
    //
    // `Luau::scope` is synchronous, so we drive the sketchy callback through a coroutine using
    // `Thread::resume` rather than the async `Function::call`.
    {
        let mut rust_val = 0;

        lua.scope(|scope| {
            // We create a 'sketchy' Luau callback that holds a mutable reference to the variable
            // `rust_val`. Outside of a `Luau::scope` call, this would not be allowed
            // because it could be unsafe.
            let sketchy = scope.create_function_mut(|_, ()| {
                rust_val = 42;
                Ok(())
            })?;
            lua.globals().set("sketchy", &sketchy)?;
            lua.create_thread(sketchy)?.resume::<()>(())?;
            Ok(())
        })?;

        assert_eq!(rust_val, 42);
    }

    // We were able to run our 'sketchy' function inside the scope just fine. However, if we
    // try to run our 'sketchy' function outside of the scope, the function we created will have
    // been invalidated and we will generate an error. If our function wasn't invalidated, we
    // might be able to improperly access the freed `rust_val` which would be unsafe.
    assert!(lua.load("sketchy()").exec().await.is_err());

    Ok(())
}
