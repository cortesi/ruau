//! memory integration tests.

use std::sync::Arc;

use ruau::{Error, GcIncParams, GcMode, Luau, Result, UserData};

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_memory_limit() -> Result<()> {
        let lua = Luau::new();

        let initial_memory = lua.used_memory();
        assert!(
            initial_memory > 0,
            "used_memory reporting is wrong, lua uses memory for stdlib"
        );

        let f = lua
            .load("local t = {}; for i = 1,10000 do t[i] = i end")
            .into_function()?;
        f.call::<()>(()).await.expect("should trigger no memory limit");

        lua.set_memory_limit(initial_memory + 10000)?;
        match f.call::<()>(()).await {
            Err(Error::MemoryError(_)) => {}
            something_else => panic!("did not trigger memory error: {:?}", something_else),
        };

        lua.set_memory_limit(0)?;
        f.call::<()>(()).await.expect("should trigger no memory limit");

        // Test memory limit during chunk loading
        lua.set_memory_limit(1024)?;
        match lua
            .load("local t = {}; for i = 1,10000 do t[i] = i end")
            .into_function()
        {
            Err(Error::MemoryError(_)) => {}
            _ => panic!("did not trigger memory error"),
        };

        Ok(())
    }

    #[tokio::test]
    async fn test_memory_limit_thread() -> Result<()> {
        let lua = Luau::new();

        let f = lua
            .load("local t = {}; for i = 1,10000 do t[i] = i end")
            .into_function()?;

        let thread = lua.create_thread(f)?;
        lua.set_memory_limit(lua.used_memory() + 10000)?;
        match thread.resume::<()>(()) {
            Err(Error::MemoryError(_)) => {}
            something_else => panic!("did not trigger memory error: {:?}", something_else),
        };

        Ok(())
    }

    #[tokio::test]
    async fn test_gc_control() -> Result<()> {
        struct MyUserdata {
            _rc: Arc<()>,
        }
        impl UserData for MyUserdata {}

        let lua = Luau::new();
        let globals = lua.globals();

        lua.gc_set_mode(GcMode::Incremental({
            let p = GcIncParams::default().step_multiplier(100);
            p.goal(200)
        }));

        let rc = Arc::new(());
        globals.set("userdata", lua.create_userdata(MyUserdata { _rc: rc.clone() })?)?;
        globals.raw_remove("userdata")?;

        assert_eq!(Arc::strong_count(&rc), 2);
        lua.gc_collect()?;
        lua.gc_collect()?;
        assert_eq!(Arc::strong_count(&rc), 1);

        Ok(())
    }
}
