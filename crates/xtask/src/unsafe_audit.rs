//! Counts unsafe sites across the workspace.
//!
//! See `plans/unsafe.md` for the audit policy. The exit codes are:
//! - `0` on a successful run, even when the current numbers exceed the baseline
//!   (the audit is a soft check at this stage).
//! - non-zero only when the audit cannot read the source tree.

use std::{
    cmp::Ordering,
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use regex::Regex;

/// Pulls one named metric out of a [`Counts`] row.
type MetricFn = fn(&Counts) -> usize;

const CRATES: &[&str] = &["ruau", "ruau-sys"];

/// One row of the audit table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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
#[derive(Default, Debug)]
pub struct Report {
    pub crates: BTreeMap<String, Counts>,
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
/// Returns the number of regressing rows. The caller decides whether to treat
/// regressions as a hard failure; Stage One uses a soft check.
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

/// Serialises a report to a JSON-shaped string without pulling in `serde_json`.
pub fn to_json(report: &Report) -> String {
    let mut out = String::from("{\n  \"crates\": {\n");
    let crate_entries: Vec<String> = report
        .crates
        .iter()
        .map(|(name, counts)| format!("    {}", encode_entry(name, counts)))
        .collect();
    out.push_str(&crate_entries.join(",\n"));
    out.push_str("\n  }\n}\n");
    out
}

/// Parses the JSON shape produced by [`to_json`]. Tolerant of whitespace, but
/// expects the exact key set and integer values.
pub fn from_json(text: &str) -> Result<Report, String> {
    let mut report = Report::default();
    let crate_re = Regex::new(
        r#"(?ms)"([A-Za-z0-9_-]+)"\s*:\s*\{\s*"unsafe_fn"\s*:\s*(\d+)\s*,\s*"pub_unsafe_fn"\s*:\s*(\d+)\s*,\s*"unsafe_block"\s*:\s*(\d+)\s*,\s*"unsafe_impl"\s*:\s*(\d+)\s*,\s*"unsafe_extern"\s*:\s*(\d+)\s*,\s*"safety_comments"\s*:\s*(\d+)\s*\}"#,
    )
    .expect("baseline parser");

    for cap in crate_re.captures_iter(text) {
        let name = cap[1].to_string();
        if name == "crates" {
            continue;
        }
        let counts = Counts {
            unsafe_fn: cap[2].parse().map_err(|e| format!("parse: {e}"))?,
            pub_unsafe_fn: cap[3].parse().map_err(|e| format!("parse: {e}"))?,
            unsafe_block: cap[4].parse().map_err(|e| format!("parse: {e}"))?,
            unsafe_impl: cap[5].parse().map_err(|e| format!("parse: {e}"))?,
            unsafe_extern: cap[6].parse().map_err(|e| format!("parse: {e}"))?,
            safety_comments: cap[7].parse().map_err(|e| format!("parse: {e}"))?,
        };
        report.crates.insert(name, counts);
    }

    if report.crates.is_empty() {
        return Err("baseline file did not contain any crate entries".to_string());
    }
    Ok(report)
}

fn encode_entry(name: &str, counts: &Counts) -> String {
    format!(
        "\"{name}\": {{ \
\"unsafe_fn\": {}, \
\"pub_unsafe_fn\": {}, \
\"unsafe_block\": {}, \
\"unsafe_impl\": {}, \
\"unsafe_extern\": {}, \
\"safety_comments\": {} }}",
        counts.unsafe_fn,
        counts.pub_unsafe_fn,
        counts.unsafe_block,
        counts.unsafe_impl,
        counts.unsafe_extern,
        counts.safety_comments,
    )
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
