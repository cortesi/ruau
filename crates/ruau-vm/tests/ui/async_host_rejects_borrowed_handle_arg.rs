use ruau_vm::{RuntimeError as Error, Table, async_host_fn};
use ruau_vm::HostReturn;

fn main() {
    let _host = async_host_fn(|_ctx, table: Table<'_>| async move {
        let _ = table;
        Ok::<_, Error>(HostReturn::default())
    });
}
