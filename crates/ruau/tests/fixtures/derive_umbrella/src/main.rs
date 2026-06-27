use ruau::{
    vm::{Ambient, FromLua, IntoLua, Limits, Profile, ScopedValue, Vm},
};

#[derive(Debug, PartialEq, IntoLua, FromLua)]
struct Settings {
    name: String,
    retries: i64,
}

fn main() {
    let mut vm = Vm::builder()
        .ambient(Ambient::deterministic(0))
        .limits(Limits::unlimited())
        .profile(Profile::full())
        .build()
        .expect("VM builds");
    vm.step(|scope| {
        let value = Settings {
            name: "umbrella".to_owned(),
            retries: 3,
        }
        .into_lua(scope)?;

        let ScopedValue::Table(table) = value else {
            panic!("derived IntoLua returns a table");
        };

        let settings = Settings::from_lua(ScopedValue::Table(table), scope)?;
        assert_eq!(
            settings,
            Settings {
                name: "umbrella".to_owned(),
                retries: 3,
            }
        );
        Ok(())
    })
    .expect("derive round trip");
}
