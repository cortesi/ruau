//! Project maintenance commands.

use std::{
    env,
    ffi::OsStr,
    fs, io, iter,
    path::{Path, PathBuf},
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
    /// Run the documentation build used by docs.rs and GitHub Pages.
    Docs,
    /// Run the non-mutating CI preflight expected before publishing.
    Ci,
    /// Format and lint-fix the workspace.
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
        XtaskCommand::Docs => docs(),
        XtaskCommand::Ci => ci(),
        XtaskCommand::Tidy => tidy(),
        XtaskCommand::Test => test(),
        XtaskCommand::UnsafeAudit {
            update_baseline,
            verbose,
        } => unsafe_audit_cmd(update_baseline, verbose, true),
        XtaskCommand::UnsafeFnCheck => unsafe_fn_check_cmd(),
    };

    if let Err(error) = result {
        eprintln!("{error}");
        exit(1);
    }
}

/// Run the docs build from package metadata so CI and docs.rs stay aligned.
fn docs() -> Result<(), String> {
    let features = docs_features()?;
    let mut args = vec!["+nightly", "doc", "-p", "ruau", "--no-deps"];
    let feature_arg;
    if !features.is_empty() {
        feature_arg = features.join(",");
        args.extend(["--features", feature_arg.as_str()]);
    }
    run_with_env("cargo", args, [("RUSTDOCFLAGS", "-D warnings")])
}

/// Run the standard local CI preflight without modifying the workspace.
fn ci() -> Result<(), String> {
    fmt_check()?;
    clippy_check()?;
    test()?;
    docs()?;
    unsafe_audit_cmd(false, false, true)
}

/// Run formatting and clippy fix.
fn tidy() -> Result<(), String> {
    fmt_write()?;
    clippy_fix()?;
    // Soft audit pass: print the audit but never fail tidy at this stage.
    if let Err(err) = unsafe_audit_cmd(false, false, false) {
        eprintln!("unsafe-audit (soft check): {err}");
    }
    Ok(())
}

/// Format the workspace with the project rustfmt configuration.
fn fmt_write() -> Result<(), String> {
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
    )
}

/// Check formatting with the project rustfmt configuration.
fn fmt_check() -> Result<(), String> {
    run(
        "cargo",
        [
            "+nightly",
            "fmt",
            "--all",
            "--",
            "--check",
            "--config-path",
            "./rustfmt-nightly.toml",
        ],
    )
}

/// Run clippy fixes across the workspace.
fn clippy_fix() -> Result<(), String> {
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

/// Check clippy across the workspace without applying fixes.
fn clippy_check() -> Result<(), String> {
    run(
        "cargo",
        [
            "clippy",
            "-q",
            "--all",
            "--all-targets",
            "--all-features",
            "--tests",
            "--examples",
            "--",
            "-D",
            "warnings",
        ],
    )
}

/// Run nextest and doctests with the full feature set.
fn test() -> Result<(), String> {
    run("cargo", ["nextest", "run", "--all", "--all-features"])?;
    run("cargo", ["test", "--workspace", "--doc", "--all-features"])
}

/// Run the unsafe-audit subcommand.
fn unsafe_audit_cmd(
    update_baseline: bool,
    verbose: bool,
    fail_on_regression: bool,
) -> Result<(), String> {
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
            let message = format!(
                "unsafe-audit: {regressions} metric(s) above baseline. Review before commit; \
re-run with `--update-baseline` to accept once acknowledged."
            );
            if fail_on_regression {
                return Err(message);
            }
            eprintln!("{message}");
        } else {
            println!("unsafe-audit: at or below baseline.");
        }
    } else {
        let message = format!(
            "unsafe-audit: no baseline at {}. Run with `--update-baseline` to create one.",
            baseline_path.display()
        );
        if fail_on_regression {
            return Err(message);
        }
        eprintln!("{message}");
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

/// Reads docs.rs feature metadata from the ruau crate manifest.
fn docs_features() -> Result<Vec<String>, String> {
    let manifest = workspace_root()?.join("crates/ruau/Cargo.toml");
    let text = fs::read_to_string(&manifest)
        .map_err(|err| format!("read {}: {err}", manifest.display()))?;
    let mut in_docsrs = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_docsrs = trimmed == "[package.metadata.docs.rs]";
            continue;
        }
        if in_docsrs && let Some(value) = trimmed.strip_prefix("features = ") {
            return parse_string_array(value)
                .map_err(|err| format!("parse docs.rs features in {}: {err}", manifest.display()));
        }
    }
    Ok(Vec::new())
}

/// Parses a simple TOML string array like `["macros"]`.
fn parse_string_array(value: &str) -> Result<Vec<String>, String> {
    let value = value.trim();
    let Some(inner) = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    else {
        return Err(format!("expected string array, got {value:?}"));
    };
    let inner = inner.trim();
    if inner.is_empty() {
        return Ok(Vec::new());
    }
    inner
        .split(',')
        .map(|item| {
            let item = item.trim();
            item.strip_prefix('"')
                .and_then(|item| item.strip_suffix('"'))
                .map(str::to_owned)
                .ok_or_else(|| format!("expected quoted string, got {item:?}"))
        })
        .collect()
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

/// Recursively collect Rust source files under `dir`.
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let ty = entry.file_type()?;
        if ty.is_dir() {
            collect_rs_files(&path, out)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// Run a command and propagate its exit status as an error.
fn run<I, S>(program: &str, args: I) -> Result<(), String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_with_env(program, args, iter::empty::<(&str, &str)>())
}

/// Run a command with additional environment variables and propagate failures.
fn run_with_env<I, S, E, K, V>(program: &str, args: I, envs: E) -> Result<(), String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
    E: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    let args = args.into_iter().collect::<Vec<_>>();
    let envs = envs.into_iter().collect::<Vec<_>>();
    eprintln!(
        "$ {program} {}",
        args.iter()
            .map(|arg| arg.as_ref().to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ")
    );

    let mut command = Command::new(program);
    command.args(args.iter().map(AsRef::as_ref));
    for (key, value) in envs {
        command.env(key, value);
    }

    let status = command
        .status()
        .map_err(|error| format!("failed to run {program}: {error}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} exited with {status}"))
    }
}
