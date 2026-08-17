//! Status-only compile-fail test support for the Ruau workspace.

use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

/// One path dependency made available to compile-fail fixtures.
#[derive(Clone, Copy)]
pub struct Dependency<'a> {
    name: &'a str,
    path: &'a str,
    default_features: bool,
    features: &'a [&'a str],
}

impl<'a> Dependency<'a> {
    /// Creates a dependency with its default features enabled.
    #[must_use]
    pub const fn new(name: &'a str, path: &'a str) -> Self {
        Self {
            name,
            path,
            default_features: true,
            features: &[],
        }
    }

    /// Disables the dependency's default features.
    #[must_use]
    pub const fn without_default_features(mut self) -> Self {
        self.default_features = false;
        self
    }

    /// Enables the named dependency features.
    #[must_use]
    pub const fn with_features(mut self, features: &'a [&'a str]) -> Self {
        self.features = features;
        self
    }
}

struct Case {
    name: String,
    source: PathBuf,
}

fn append_cases(directory: &Path, cases: &mut Vec<Case>) {
    let mut sources = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", directory.display()))
        .map(|entry| {
            entry
                .expect("compile-fail directory entry is readable")
                .path()
        })
        .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
        .collect::<Vec<_>>();
    sources.sort();
    for source in sources {
        let name = source
            .file_stem()
            .and_then(|stem| stem.to_str())
            .expect("compile-fail fixture has a UTF-8 file stem")
            .to_owned();
        cases.push(Case { name, source });
    }
}

fn workspace_root(manifest_dir: &Path) -> &Path {
    manifest_dir
        .ancestors()
        .find(|ancestor| {
            ancestor.join("Cargo.toml").is_file() && ancestor.join("Cargo.lock").is_file()
        })
        .expect("compile-fail subject belongs to a locked Cargo workspace")
}

fn workspace_target_dir(workspace: &Path) -> PathBuf {
    let Some(configured) = env::var_os("CARGO_TARGET_DIR") else {
        return workspace.join("target");
    };
    let configured = PathBuf::from(configured);
    if configured.is_absolute() {
        configured
    } else {
        workspace.join(configured)
    }
}

fn append_dependency(manifest: &mut String, manifest_dir: &Path, dependency: Dependency<'_>) {
    assert!(
        dependency
            .name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-'),
        "compile-fail dependency has a Cargo package name"
    );
    let path = manifest_dir
        .join(dependency.path)
        .canonicalize()
        .unwrap_or_else(|error| {
            panic!(
                "failed to resolve compile-fail dependency {}: {error}",
                dependency.path
            )
        });
    manifest.push_str("\n[dependencies.");
    manifest.push_str(dependency.name);
    manifest.push_str("]\npath = ");
    manifest.push_str(&serde_json::to_string(&path.to_string_lossy()).expect("path serializes"));
    manifest.push_str("\ndefault-features = ");
    manifest.push_str(if dependency.default_features {
        "true"
    } else {
        "false"
    });
    manifest.push('\n');
    if !dependency.features.is_empty() {
        manifest.push_str("features = ");
        manifest.push_str(
            &serde_json::to_string(dependency.features).expect("feature names serialize"),
        );
        manifest.push('\n');
    }
}

/// Compiles every Rust fixture in `fixture_dirs` and requires each target to fail.
///
/// The runner checks only that rustc emits an error for every fixture target. It
/// deliberately does not compare or store rendered compiler diagnostics.
/// Paths in `fixture_dirs` and [`Dependency`] are relative to `manifest_dir`.
pub fn run(
    manifest_dir: &str,
    profile: &str,
    fixture_dirs: &[&str],
    dependencies: &[Dependency<'_>],
) {
    assert!(
        profile.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
        }),
        "compile-fail profile is safe to use as a Cargo package and directory name"
    );
    let manifest_dir = Path::new(manifest_dir);
    let workspace = workspace_root(manifest_dir);
    let mut cases = Vec::new();
    for directory in fixture_dirs {
        append_cases(&manifest_dir.join(directory), &mut cases);
    }
    assert!(!cases.is_empty(), "compile-fail fixture set is not empty");

    let compile_fail_dir = workspace_target_dir(workspace).join("compile-fail");
    let fixture_dir = compile_fail_dir.join(format!("fixture-{profile}"));
    let bin_dir = fixture_dir.join("src/bin");
    fs::create_dir_all(&bin_dir).expect("compile-fail fixture directory is writable");

    let mut manifest = format!(
        "[package]\nname = \"compile-fail-{profile}\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\nautobins = false\n\n[workspace]\nresolver = \"2\"\n"
    );
    for dependency in dependencies {
        append_dependency(&mut manifest, manifest_dir, *dependency);
    }

    let mut expected = BTreeSet::new();
    for case in &cases {
        assert!(
            expected.insert(case.name.clone()),
            "compile-fail fixture names are unique: {}",
            case.name
        );
        let fixture = bin_dir.join(format!("{}.rs", case.name));
        fs::copy(&case.source, &fixture).unwrap_or_else(|error| {
            panic!(
                "failed to copy {} to {}: {error}",
                case.source.display(),
                fixture.display()
            )
        });
        manifest.push_str("\n[[bin]]\nname = ");
        manifest.push_str(&serde_json::to_string(&case.name).expect("target name serializes"));
        manifest.push_str("\npath = ");
        manifest.push_str(
            &serde_json::to_string(&format!("src/bin/{}.rs", case.name))
                .expect("target path serializes"),
        );
        manifest.push('\n');
    }

    fs::write(fixture_dir.join("Cargo.toml"), manifest)
        .expect("compile-fail fixture manifest is writable");
    fs::copy(workspace.join("Cargo.lock"), fixture_dir.join("Cargo.lock"))
        .expect("workspace lockfile copies into the compile-fail fixture");

    let output = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .arg("check")
        .arg("--manifest-path")
        .arg(fixture_dir.join("Cargo.toml"))
        .arg("--bins")
        .arg("--keep-going")
        .arg("--message-format=json")
        .arg("--offline")
        .arg("--target-dir")
        .arg(compile_fail_dir.join("build"))
        .env("CARGO_TERM_COLOR", "never")
        .output()
        .expect("cargo runs the compile-fail fixtures");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let failed = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|message| message["reason"] == "compiler-message")
        .filter(|message| message["message"]["level"] == "error")
        .filter_map(|message| message["target"]["name"].as_str().map(str::to_owned))
        .filter(|name| expected.contains(name))
        .collect::<BTreeSet<_>>();
    let missing = expected.difference(&failed).collect::<Vec<_>>();

    assert!(
        !output.status.success(),
        "compile-fail fixtures unexpectedly compiled successfully"
    );
    assert!(
        missing.is_empty(),
        "fixtures without a compiler error: {missing:?}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
