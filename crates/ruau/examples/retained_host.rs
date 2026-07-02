//! Checked retained source-eval host with JSON arguments, prints, and JSON results.

use ruau::{
    host::{DEFAULT_TIMEOUT, Evaluator, Options},
    surface::Surface,
};

const CHUNK_NAME: &str = "retained_host.luau";
const SOURCE: &str = r#"
print("running " .. args.name)
return {
    greeting = "hello " .. args.name,
    next = args.visits + 1.0,
}
"#;

fn main() -> Result<(), String> {
    let surface = Surface::builder()
        .declaration_global("args", "{ name: string, visits: number }")
        .build()
        .map_err(|error| format!("surface: {error}"))?;
    let host = Evaluator::new(surface);

    let outcome = host
        .eval_checked_blocking(
            SOURCE,
            Options::default()
                .chunk_name(CHUNK_NAME)
                .timeout(DEFAULT_TIMEOUT)
                .args(serde_json::json!({ "name": "Ada", "visits": 2.0 })),
        )
        .map_err(|error| error.format_pretty())?;

    assert_eq!(outcome.prints, vec!["running Ada"]);
    assert_eq!(
        outcome.value,
        Some(serde_json::json!({
            "greeting": "hello Ada",
            "next": 3.0,
        }))
    );
    println!("prints = {:?}", outcome.prints);
    println!("value = {:?}", outcome.value);
    Ok(())
}
