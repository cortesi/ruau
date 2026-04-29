//! Records Luau statement coverage for an embedded chunk.

use ruau::{Compiler, Luau, Result, compiler::CoverageLevel};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let lua = Luau::new();
    lua.set_compiler(Compiler::new().coverage_level(CoverageLevel::Statement));

    let function = lua
        .load(
            r#"
            local total = 0

            for index = 1, 4 do
                total += index
            end

            if total > 5 then
                total *= 2
            end

            return total
            "#,
        )
        .name("@coverage_example.luau")
        .into_function()?;

    assert_eq!(function.call::<i32>(()).await?, 20);

    let mut hit_lines = 0;
    function.coverage(|coverage| {
        hit_lines += coverage.hits.iter().filter(|hits| **hits > 0).count();
        println!(
            "function={:?} line={} depth={} hits={:?}",
            coverage.function, coverage.line_defined, coverage.depth, coverage.hits
        );
    });

    assert!(hit_lines > 0);
    Ok(())
}
