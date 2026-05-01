//! Project maintenance commands.

use std::{
    env,
    ffi::OsStr,
    fs,
    path::PathBuf,
    process::{Command, exit},
};

use clap::{Parser, Subcommand};

mod unsafe_audit;
mod unsafe_fn_check;

/// Where the unsafe-audit baseline JSON lives, relative to the workspace root.
const BASELINE_REL_PATH: &str = "crates/ruau/audit-baseline.json";

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
    /// Count unsafe sites and compare against the audit baseline.
    UnsafeAudit {
        /// Update the baseline JSON to the current numbers instead of comparing.
        #[arg(long)]
        update_baseline: bool,
        /// Print the per-file hotspot table.
        #[arg(long)]
        verbose: bool,
    },
    /// Find `unsafe fn` declarations whose bodies have no actual unsafe operations.
    ///
    /// Walks `crates/ruau/src`, patches each `unsafe fn` to drop the keyword, runs
    /// `cargo check -p ruau --tests`, and reports the function as a candidate if the build
    /// succeeds. Slow (one cargo check per declaration) but exhaustive.
    UnsafeFnCheck,
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        XtaskCommand::Tidy => tidy(),
        XtaskCommand::Test => test(),
        XtaskCommand::UnsafeAudit {
            update_baseline,
            verbose,
        } => unsafe_audit_cmd(update_baseline, verbose),
        XtaskCommand::UnsafeFnCheck => unsafe_fn_check_cmd(),
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
    )?;
    // Soft audit pass: print the audit but never fail tidy at this stage.
    if let Err(err) = unsafe_audit_cmd(false, false) {
        eprintln!("unsafe-audit (soft check): {err}");
    }
    Ok(())
}

/// Run nextest and doctests with the full feature set.
fn test() -> Result<(), String> {
    run("cargo", ["nextest", "run", "--all", "--all-features"])?;
    run("cargo", ["test", "--workspace", "--doc", "--all-features"])
}

/// Run the unsafe-audit subcommand.
fn unsafe_audit_cmd(update_baseline: bool, verbose: bool) -> Result<(), String> {
    let workspace = workspace_root()?;
    let report = unsafe_audit::run(&workspace)?;

    println!("unsafe-audit summary:");
    println!("{}", unsafe_audit::render_summary(&report));

    if verbose {
        println!("hotspots (top 20 by unsafe weight):");
        println!("{}", unsafe_audit::render_hotspots(&report, 20));
    }

    let baseline_path = workspace.join(BASELINE_REL_PATH);

    if update_baseline {
        let json = unsafe_audit::to_json(&report);
        fs::write(&baseline_path, json)
            .map_err(|err| format!("write {}: {err}", baseline_path.display()))?;
        println!("updated baseline at {}", baseline_path.display());
        return Ok(());
    }

    if baseline_path.exists() {
        let baseline_text = fs::read_to_string(&baseline_path)
            .map_err(|err| format!("read {}: {err}", baseline_path.display()))?;
        let baseline = unsafe_audit::from_json(&baseline_text)?;
        let regressions = unsafe_audit::check_baseline(&report, &baseline);
        if regressions > 0 {
            eprintln!(
                "unsafe-audit: {regressions} metric(s) above baseline. Review before commit; \
re-run with `--update-baseline` to accept once acknowledged."
            );
        } else {
            println!("unsafe-audit: at or below baseline.");
        }
    } else {
        eprintln!(
            "unsafe-audit: no baseline at {}. Run with `--update-baseline` to create one.",
            baseline_path.display()
        );
    }

    Ok(())
}

/// Run the unsafe-fn-check subcommand.
fn unsafe_fn_check_cmd() -> Result<(), String> {
    let workspace = workspace_root()?;
    let candidates = unsafe_fn_check::run(&workspace)?;
    println!("{}", unsafe_fn_check::render(&candidates, &workspace));
    Ok(())
}

/// Locate the workspace root by walking up from `CARGO_MANIFEST_DIR` until a
/// `Cargo.toml` containing `[workspace]` is found.
fn workspace_root() -> Result<PathBuf, String> {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").map_err(|err| format!("{err}"))?;
    let mut path = PathBuf::from(manifest_dir);
    loop {
        let candidate = path.join("Cargo.toml");
        if candidate.exists()
            && let Ok(text) = fs::read_to_string(&candidate)
            && text.contains("[workspace]")
        {
            return Ok(path);
        }
        if !path.pop() {
            return Err("could not locate workspace root".to_string());
        }
    }
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
