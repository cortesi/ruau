//! Finds `unsafe fn` declarations whose bodies have no actual unsafe operations.
//!
//! For each `unsafe fn` declaration in `crates/ruau/src`, the tool patches the file to remove
//! the `unsafe` keyword, runs `cargo check -p ruau --tests`, and reports the function as a
//! "could be safe" candidate if the build succeeds. The original file is always restored.
//!
//! Limitations:
//! - Skips `unsafe extern` declarations (those are FFI signatures and can't be made safe).
//! - Skips functions that are part of a trait implementation where the trait method is
//!   `unsafe fn` — removing `unsafe` from the impl breaks the trait contract, but the build
//!   error makes this obvious.
//! - Each candidate is checked independently; cascading effects (where making one safe
//!   enables others) are not modelled.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use regex::Regex;

/// A redundant `unsafe fn` candidate.
#[derive(Debug)]
pub struct Candidate {
    pub file: PathBuf,
    pub line: usize,
    pub signature: String,
}

/// Walks `crates/ruau/src` and reports `unsafe fn` declarations whose bodies could be safe.
pub fn run(workspace_root: &Path) -> Result<Vec<Candidate>, String> {
    let src_root = workspace_root.join("crates/ruau/src");
    if !src_root.exists() {
        return Err(format!("missing source dir: {}", src_root.display()));
    }

    let mut files = Vec::new();
    collect_rs_files(&src_root, &mut files)
        .map_err(|err| format!("walk {}: {err}", src_root.display()))?;
    files.sort();

    // Match `unsafe fn` declarations at the start of a line, ignoring `unsafe extern` and
    // `unsafe impl` (which are not function declarations). Captures: (1) leading indent and
    // visibility prefix, (2) the `fn ...` portion that should remain.
    let pattern = Regex::new(r"(?m)^(?P<prefix>\s*(?:pub(?:\([^)]+\))?\s+)?)unsafe\s+(?P<rest>fn\s+\w+)")
        .map_err(|err| format!("regex: {err}"))?;

    let mut candidates = Vec::new();

    for file in &files {
        let original = fs::read_to_string(file)
            .map_err(|err| format!("read {}: {err}", file.display()))?;

        // Collect match offsets in reverse so that patching one doesn't shift the others.
        let matches: Vec<(usize, usize, String, String)> = pattern
            .captures_iter(&original)
            .map(|cap| {
                let m = cap.get(0).expect("regex must match");
                let prefix = cap.name("prefix").map(|m| m.as_str()).unwrap_or("");
                let rest = cap.name("rest").expect("rest required").as_str();
                (m.start(), m.end(), prefix.to_string(), rest.to_string())
            })
            .collect();

        if matches.is_empty() {
            continue;
        }

        eprintln!(
            "checking {} ({} declarations)",
            file.strip_prefix(workspace_root).unwrap_or(file).display(),
            matches.len()
        );

        for (start, end, prefix, rest) in matches {
            let line = line_of(&original, start);
            let signature = signature_at(&original, start);
            let replacement = format!("{prefix}{rest}");
            let mut patched = String::with_capacity(original.len());
            patched.push_str(&original[..start]);
            patched.push_str(&replacement);
            patched.push_str(&original[end..]);

            fs::write(file, &patched)
                .map_err(|err| format!("write {}: {err}", file.display()))?;

            let ok = run_cargo_check(workspace_root)?;

            // Always restore before the next iteration.
            fs::write(file, &original)
                .map_err(|err| format!("restore {}: {err}", file.display()))?;

            if ok {
                eprintln!("  CANDIDATE  {}:{}  {}", file.display(), line, signature);
                candidates.push(Candidate {
                    file: file.clone(),
                    line,
                    signature,
                });
            }
        }
    }

    Ok(candidates)
}

fn run_cargo_check(workspace_root: &Path) -> Result<bool, String> {
    let status = Command::new("cargo")
        .current_dir(workspace_root)
        .args(["check", "-p", "ruau", "--tests", "--quiet"])
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .status()
        .map_err(|err| format!("cargo check: {err}"))?;
    Ok(status.success())
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
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

fn line_of(text: &str, byte_offset: usize) -> usize {
    text[..byte_offset].bytes().filter(|b| *b == b'\n').count() + 1
}

fn signature_at(text: &str, byte_offset: usize) -> String {
    let line_end = text[byte_offset..]
        .find('\n')
        .map(|i| byte_offset + i)
        .unwrap_or(text.len());
    text[byte_offset..line_end].trim().to_string()
}

/// Renders the candidate report.
pub fn render(candidates: &[Candidate], workspace_root: &Path) -> String {
    if candidates.is_empty() {
        return "No redundant `unsafe fn` declarations found.\n".to_string();
    }

    let mut out = String::new();
    out.push_str(&format!(
        "{} `unsafe fn` declarations could become safe `fn`:\n\n",
        candidates.len()
    ));
    for c in candidates {
        let rel = c.file.strip_prefix(workspace_root).unwrap_or(&c.file);
        out.push_str(&format!("  {}:{}  {}\n", rel.display(), c.line, c.signature));
    }
    out
}
