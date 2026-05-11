//! This example shows a simple read-evaluate-print-loop (REPL).

use std::error::Error;

use ruau::{Error as LuauError, Luau, MultiValue};
use rustyline::DefaultEditor;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let lua = Luau::new();
    let mut editor = DefaultEditor::new()?;

    loop {
        let mut prompt = "> ";
        let mut line = String::new();

        loop {
            match editor.readline(prompt) {
                Ok(input) => line.push_str(&input),
                Err(_) => return Ok(()),
            }

            match lua.load(&line).eval::<MultiValue>().await {
                Ok(values) => {
                    editor.add_history_entry(line.as_str())?;
                    if !values.is_empty() {
                        println!(
                            "{}",
                            values
                                .iter()
                                .map(|value| format!("{:#?}", value))
                                .collect::<Vec<_>>()
                                .join("\t")
                        );
                    }
                    break;
                }
                Err(LuauError::SyntaxError {
                    incomplete_input: true,
                    ..
                }) => {
                    // continue reading input and append it to `line`
                    line.push('\n'); // separate input lines
                    prompt = ">> ";
                }
                Err(e) => {
                    eprintln!("error: {}", e);
                    break;
                }
            }
        }
    }
}
