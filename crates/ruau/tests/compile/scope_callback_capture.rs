use ruau::{Luau, Table};

fn main() {
    let lua = Luau::new();
    lua.scope(|scope| {
        let mut inner: Option<Table> = None;
        let f = scope.create_function_mut(|_, t: Table| {
            inner = Some(t);
            Ok(())
        })?;
        f.call::<()>(lua.create_table().await?)?;
        Ok(())
    });
}
