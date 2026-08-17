//! Regression coverage for the `ruau-vm` owned derive macro feature.

#![cfg(feature = "derive")]

#[cfg(test)]
mod tests {
    use ruau_vm::{Ambient, FromLua, IntoLua, Limits, RuntimeCapabilities, ScopedValue, Vm};

    #[derive(Debug, PartialEq, IntoLua, FromLua)]
    struct Widget {
        name: String,
        count: i64,
    }

    fn vm() -> Vm {
        Vm::builder()
            .ambient(Ambient::deterministic(0))
            .limits(Limits::unlimited())
            .runtime_capabilities(RuntimeCapabilities::default().enable_runtime_compilation())
            .trusted_host()
            .build()
            .expect("test VM builds")
    }

    #[test]
    fn derive_macros_are_owned_by_vm_crate() {
        let mut vm = vm();
        vm.step(|scope| {
            let value = Widget {
                name: "gadget".to_owned(),
                count: 7,
            }
            .into_lua(scope)?;

            let ScopedValue::Table(table) = value else {
                panic!("derived IntoLua materializes a table");
            };

            assert_eq!(table.get::<_, String>(scope, "name")?, "gadget");
            assert_eq!(table.get::<_, i64>(scope, "count")?, 7);

            let round_trip = Widget::from_lua(ScopedValue::Table(table), scope)?;
            assert_eq!(
                round_trip,
                Widget {
                    name: "gadget".to_owned(),
                    count: 7
                }
            );
            Ok(())
        })
        .expect("derive round trip");
    }
}
