//! Project maintenance commands.

use std::{
    ffi::OsStr,
    process::{Command, exit},
};

use clap::{Parser, Subcommand};

/// Run project maintenance tasks.
#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Cli {
    /// Command to run.
    #[command(subcommand)]
    command: XtaskCommand,
}

/// Supported maintenance commands.
#[derive(Debug, Subcommand)]
enum XtaskCommand {
    /// Format and lint the workspace.
    Tidy,
    /// Run the standard test suite.
    Test,
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        XtaskCommand::Tidy => tidy(),
        XtaskCommand::Test => test(),
    };

    if let Err(error) = result {
        eprintln!("{error}");
        exit(1);
    }
}

/// Run formatting and clippy fix.
fn tidy() -> Result<(), String> {
    run(
        "cargo",
        [
            "+nightly",
            "fmt",
            "--all",
            "--",
            "--config-path",
            "./rustfmt-nightly.toml",
        ],
    )?;
    run(
        "cargo",
        [
            "clippy",
            "-q",
            "--fix",
            "--all",
            "--all-targets",
            "--all-features",
            "--allow-dirty",
            "--tests",
            "--examples",
        ],
    )
}

/// Run nextest and doctests with the full feature set.
fn test() -> Result<(), String> {
    run("cargo", ["nextest", "run", "--all", "--all-features"])?;
    run("cargo", ["test", "--workspace", "--doc", "--all-features"])
}

/// Run a command and propagate its exit status as an error.
fn run<I, S>(program: &str, args: I) -> Result<(), String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args = args.into_iter().collect::<Vec<_>>();
    eprintln!(
        "$ {program} {}",
        args.iter()
            .map(|arg| arg.as_ref().to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ")
    );

    let status = Command::new(program)
        .args(args.iter().map(AsRef::as_ref))
        .status()
        .map_err(|error| format!("failed to run {program}: {error}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} exited with {status}"))
    }
}
