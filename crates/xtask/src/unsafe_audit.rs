//! Counts unsafe sites across the workspace.
//!
//! See `crates/ruau/SAFETY.md` for the audit anchor and current numbers. Exit codes:
//! - `0` on a successful run when the current numbers are at or below the baseline.
//! - non-zero when the current numbers exceed the baseline.
//! - non-zero when the audit cannot read the source tree.

use std::{cmp::Ordering, collections::BTreeMap, fs, path::Path};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::collect_rs_files;

/// Pulls one named metric out of a [`Counts`] row.
type MetricFn = fn(&Counts) -> usize;

const CRATES: &[&str] = &["ruau", "ruau-sys"];

/// One row of the audit table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    pub unsafe_fn: usize,
    pub pub_unsafe_fn: usize,
    pub unsafe_block: usize,
    pub unsafe_impl: usize,
    pub unsafe_extern: usize,
    pub safety_comments: usize,
}

impl Counts {
    fn add_assign(&mut self, other: &Self) {
        self.unsafe_fn += other.unsafe_fn;
        self.pub_unsafe_fn += other.pub_unsafe_fn;
        self.unsafe_block += other.unsafe_block;
        self.unsafe_impl += other.unsafe_impl;
        self.unsafe_extern += other.unsafe_extern;
        self.safety_comments += other.safety_comments;
    }
}

/// Patterns used to count unsafe sites. Held together so they are compiled
/// once per audit run.
struct Patterns {
    unsafe_fn: Regex,
    pub_unsafe_fn: Regex,
    unsafe_block: Regex,
    unsafe_impl: Regex,
    unsafe_extern: Regex,
    safety_comments: Regex,
}

impl Patterns {
    fn new() -> Self {
        let line = |s: &str| Regex::new(&format!(r"(?m)^\s*{s}")).expect("audit regex");
        Self {
            unsafe_fn: line(r"(pub(\([^)]+\))?\s+)?unsafe\s+fn\b"),
            pub_unsafe_fn: line(r"pub\s+unsafe\s+fn\b"),
            unsafe_block: Regex::new(r"\bunsafe\s*\{").expect("audit regex"),
            unsafe_impl: line(r"unsafe\s+impl\b"),
            unsafe_extern: line(r"(pub(\([^)]+\))?\s+)?unsafe\s+extern\b"),
            // `// SAFETY:` block comments and `///`/`//!` rustdoc `# Safety` headers.
            safety_comments: Regex::new(r"(?m)//.*\bSAFETY:|^\s*///?!?\s*#\s*Safety\b")
                .expect("audit regex"),
        }
    }

    fn count(&self, content: &str) -> Counts {
        Counts {
            unsafe_fn: self.unsafe_fn.find_iter(content).count(),
            pub_unsafe_fn: self.pub_unsafe_fn.find_iter(content).count(),
            unsafe_block: self.unsafe_block.find_iter(content).count(),
            unsafe_impl: self.unsafe_impl.find_iter(content).count(),
            unsafe_extern: self.unsafe_extern.find_iter(content).count(),
            safety_comments: self.safety_comments.find_iter(content).count(),
        }
    }
}

/// Aggregated audit results.
///
/// The `files` map is populated at runtime for hotspot reporting and is not persisted
/// in the baseline JSON (only `crates` is stored for reproducibility across machines).
#[derive(Default, Debug, Serialize, Deserialize)]
pub struct Report {
    pub crates: BTreeMap<String, Counts>,
    #[serde(skip)]
    pub files: BTreeMap<String, Counts>,
}

/// Walks the workspace and counts unsafe sites.
pub fn run(workspace_root: &Path) -> Result<Report, String> {
    let patterns = Patterns::new();
    let mut report = Report::default();

    for name in CRATES {
        let src_root = workspace_root.join("crates").join(name).join("src");
        if !src_root.exists() {
            return Err(format!("missing source dir: {}", src_root.display()));
        }

        let mut crate_total = Counts::default();
        let mut files = Vec::new();
        collect_rs_files(&src_root, &mut files)
            .map_err(|err| format!("walk {}: {err}", src_root.display()))?;

        for file in files {
            let content = fs::read_to_string(&file)
                .map_err(|err| format!("read {}: {err}", file.display()))?;
            let counts = patterns.count(&content);
            crate_total.add_assign(&counts);
            let rel = file
                .strip_prefix(workspace_root)
                .unwrap_or(&file)
                .display()
                .to_string();
            report.files.insert(rel, counts);
        }

        report.crates.insert((*name).to_string(), crate_total);
    }

    Ok(report)
}

/// Renders the per-crate summary as a fixed-width table.
pub fn render_summary(report: &Report) -> String {
    let mut out = String::new();
    let names: Vec<&str> = report.crates.keys().map(String::as_str).collect();

    let header_width = 22;
    let col_width = 12;

    out.push_str(&pad("metric", header_width));
    for name in &names {
        out.push_str(&pad(name, col_width));
    }
    out.push('\n');
    out.push_str(&"-".repeat(header_width + col_width * names.len()));
    out.push('\n');

    let rows: &[(&str, MetricFn)] = &[
        ("unsafe fn (total)", |c| c.unsafe_fn),
        ("pub unsafe fn", |c| c.pub_unsafe_fn),
        ("unsafe { ... } blocks", |c| c.unsafe_block),
        ("unsafe impl", |c| c.unsafe_impl),
        ("unsafe extern", |c| c.unsafe_extern),
        ("SAFETY: comments", |c| c.safety_comments),
    ];

    for (label, get) in rows {
        out.push_str(&pad(label, header_width));
        for name in &names {
            let counts = &report.crates[*name];
            out.push_str(&pad(&get(counts).to_string(), col_width));
        }
        out.push('\n');
    }

    out
}

/// Renders the per-file hotspot table for files that contain unsafe at all,
/// sorted by total unsafe weight descending.
pub fn render_hotspots(report: &Report, top_n: usize) -> String {
    let mut rows: Vec<(&str, &Counts, usize)> = report
        .files
        .iter()
        .map(|(name, counts)| {
            let weight = counts.unsafe_fn + counts.unsafe_block + counts.unsafe_impl;
            (name.as_str(), counts, weight)
        })
        .filter(|(_, _, w)| *w > 0)
        .collect();
    rows.sort_by(|a, b| match b.2.cmp(&a.2) {
        Ordering::Equal => a.0.cmp(b.0),
        other => other,
    });

    let mut out = String::new();
    let path_width = 48;
    let col_width = 10;

    out.push_str(&pad("file", path_width));
    for label in ["fn", "pubfn", "block", "impl", "extern", "SAFETY"] {
        out.push_str(&pad(label, col_width));
    }
    out.push('\n');
    out.push_str(&"-".repeat(path_width + col_width * 6));
    out.push('\n');

    for (path, counts, _) in rows.into_iter().take(top_n) {
        out.push_str(&pad(path, path_width));
        for value in [
            counts.unsafe_fn,
            counts.pub_unsafe_fn,
            counts.unsafe_block,
            counts.unsafe_impl,
            counts.unsafe_extern,
            counts.safety_comments,
        ] {
            out.push_str(&pad(&value.to_string(), col_width));
        }
        out.push('\n');
    }

    out
}

/// Compares the live report against a baseline and prints any regressions.
///
/// Returns the number of regressing rows. The caller decides whether to treat regressions
/// as a hard failure; the default `cargo xtask unsafe-audit` treats them as a soft check.
pub fn check_baseline(report: &Report, baseline: &Report) -> usize {
    let mut regressions = 0;
    let metrics: &[(&str, MetricFn)] = &[
        ("unsafe fn", |c| c.unsafe_fn),
        ("pub unsafe fn", |c| c.pub_unsafe_fn),
        ("unsafe block", |c| c.unsafe_block),
        ("unsafe impl", |c| c.unsafe_impl),
        ("unsafe extern", |c| c.unsafe_extern),
    ];

    for (crate_name, current) in &report.crates {
        let Some(prev) = baseline.crates.get(crate_name) else {
            continue;
        };
        for (label, get) in metrics {
            let now = get(current);
            let then = get(prev);
            if now > then {
                regressions += 1;
                eprintln!(
                    "  regression: {crate_name}: {label} {then} -> {now} (+{})",
                    now - then
                );
            }
        }
    }
    regressions
}

/// Serializes a report to JSON for the baseline file.
///
/// Only the `crates` map is persisted (the per-file data is regenerated on each run and
/// is marked `#[serde(skip)]`).
pub fn to_json(report: &Report) -> String {
    serde_json::to_string_pretty(report).expect("baseline serialization cannot fail")
}

/// Parses a baseline JSON file previously written by [`to_json`].
pub fn from_json(text: &str) -> Result<Report, String> {
    let report: Report = serde_json::from_str(text)
        .map_err(|e| format!("failed to parse baseline JSON: {e}"))?;
    if report.crates.is_empty() {
        return Err("baseline file did not contain any crate entries".to_string());
    }
    Ok(report)
}

fn pad(s: &str, width: usize) -> String {
    let mut out = s.to_string();
    if out.len() < width {
        out.push_str(&" ".repeat(width - out.len()));
    } else {
        out.push(' ');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_basic_unsafe_shapes() {
        let src = r#"
pub unsafe fn alpha() {}
unsafe fn beta() {}
pub(crate) unsafe fn gamma() {}
pub unsafe fn delta() {}
unsafe impl Send for X {}
unsafe extern "C-unwind" fn cb(_: *mut i32) -> i32 { 0 }

fn outer() {
    unsafe { dangerous() }
    let _ = unsafe { stuff() };
}

// SAFETY: this comment counts.
/// # Safety
/// Documented contract.
"#;
        let p = Patterns::new();
        let c = p.count(src);
        // `unsafe extern fn` is intentionally counted only on the `unsafe extern` axis.
        assert_eq!(c.unsafe_fn, 4, "unsafe_fn");
        assert_eq!(c.pub_unsafe_fn, 2, "pub_unsafe_fn");
        assert_eq!(c.unsafe_block, 2, "unsafe_block");
        assert_eq!(c.unsafe_impl, 1, "unsafe_impl");
        assert_eq!(c.unsafe_extern, 1, "unsafe_extern");
        assert_eq!(c.safety_comments, 2, "safety_comments");
    }

    #[test]
    fn json_roundtrip() {
        let mut report = Report::default();
        report.crates.insert(
            "ruau".to_string(),
            Counts {
                unsafe_fn: 1,
                pub_unsafe_fn: 2,
                unsafe_block: 3,
                unsafe_impl: 4,
                unsafe_extern: 5,
                safety_comments: 6,
            },
        );
        let json = to_json(&report);
        let parsed = from_json(&json).expect("roundtrip");
        assert_eq!(parsed.crates["ruau"], report.crates["ruau"]);
    }

    #[test]
    fn check_baseline_reports_only_regressions() {
        let mut baseline = Report::default();
        baseline.crates.insert(
            "ruau".to_string(),
            Counts {
                unsafe_fn: 10,
                pub_unsafe_fn: 5,
                unsafe_block: 20,
                ..Counts::default()
            },
        );
        let mut current = Report::default();
        current.crates.insert(
            "ruau".to_string(),
            Counts {
                unsafe_fn: 12, // regressed
                pub_unsafe_fn: 4,
                unsafe_block: 20,
                ..Counts::default()
            },
        );
        let regressions = check_baseline(&current, &baseline);
        assert_eq!(regressions, 1);
    }
}
