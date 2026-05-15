//! Workspace-owned build helper for the vendored Luau source tree.

use std::{
    env, fs,
    path::{Path, PathBuf},
    result::Result as StdResult,
};

/// Luau release version vendored by this crate.
pub const LUAU_VERSION: &str = "0.716";

/// Number of components in Luau's vector value.
pub const VECTOR_SIZE: usize = 3;

/// Build configuration for the vendored Luau runtime libraries.
pub struct Build {
    /// Directory where native build artifacts are written.
    out_dir: Option<PathBuf>,
    /// Cargo target triple.
    target: Option<String>,
    /// Cargo host triple.
    host: Option<String>,
    /// Maximum C stack slots allowed by the Luau VM.
    max_cstack_size: usize,
}

/// Description of one Luau C++ component/library to compile.
#[derive(Clone, Copy)]
struct Component {
    name: &'static str,
    source: ComponentSource,
    extra_includes: &'static [&'static str],
    native_api_defines: &'static [&'static str],
    emscripten_error: Option<&'static str>,
}

/// Location of a component source directory.
#[derive(Clone, Copy)]
enum ComponentSource {
    /// Relative to the vendored Luau source root.
    Luau(&'static str),
    /// Relative to this crate's manifest directory.
    Crate(&'static str),
}

/// All Luau components we build, in compile order.
const COMPONENTS: &[Component] = &[
    Component {
        name: "luauast",
        source: ComponentSource::Luau("Ast/src"),
        extra_includes: &["Ast/include"],
        native_api_defines: &[],
        emscripten_error: None,
    },
    Component {
        name: "luauanalysis",
        source: ComponentSource::Luau("Analysis/src"),
        extra_includes: &[
            "Analysis/include",
            "Ast/include",
            "VM/include",
            "Compiler/include",
            "Config/include",
        ],
        native_api_defines: &[],
        emscripten_error: None,
    },
    Component {
        name: "luaucodegen",
        source: ComponentSource::Luau("CodeGen/src"),
        extra_includes: &["CodeGen/include", "VM/include", "VM/src"],
        native_api_defines: &["LUACODEGEN_API"],
        emscripten_error: Some("CodeGen is not supported on emscripten"),
    },
    Component {
        name: "luaucommon",
        source: ComponentSource::Luau("Common/src"),
        extra_includes: &["Common/include"],
        native_api_defines: &[],
        emscripten_error: None,
    },
    Component {
        name: "luaucompiler",
        source: ComponentSource::Luau("Compiler/src"),
        extra_includes: &["Compiler/include", "Ast/include"],
        native_api_defines: &["LUACODE_API"],
        emscripten_error: None,
    },
    Component {
        name: "luauconfig",
        source: ComponentSource::Luau("Config/src"),
        extra_includes: &[
            "Config/include",
            "Ast/include",
            "Compiler/include",
            "VM/include",
        ],
        native_api_defines: &[],
        emscripten_error: None,
    },
    Component {
        name: "luaucustom",
        source: ComponentSource::Crate("custom/src"),
        extra_includes: &["VM/include", "VM/src"],
        native_api_defines: &[],
        emscripten_error: None,
    },
    Component {
        name: "luaurequire",
        source: ComponentSource::Luau("Require/src"),
        extra_includes: &[
            "Require/include",
            "Ast/include",
            "Config/include",
            "VM/include",
        ],
        native_api_defines: &[],
        emscripten_error: None,
    },
    Component {
        name: "luauvm",
        source: ComponentSource::Luau("VM/src"),
        extra_includes: &["VM/include"],
        native_api_defines: &[],
        emscripten_error: None,
    },
];

/// Static Luau libraries in the link order expected by downstream build scripts.
const LINK_LIBS: &[&str] = &[
    "luauvm",
    "luauanalysis",
    "luaucompiler",
    "luauast",
    "luaucommon",
    "luauconfig",
    "luaucustom",
    "luaurequire",
    "luaucodegen",
];

/// Native artifacts produced by [`Build`].
pub struct Artifacts {
    /// Native library output directory.
    lib_dir: PathBuf,
    /// Static Luau libraries in link order.
    libs: Vec<String>,
    /// C++ standard library to link, if one is needed.
    cpp_stdlib: Option<String>,
    /// Root of the vendored Luau source tree.
    source_root: PathBuf,
    /// Include paths required by downstream native shims.
    include_paths: Vec<PathBuf>,
}

impl Default for Build {
    fn default() -> Self {
        Self {
            out_dir: env::var_os("OUT_DIR").map(PathBuf::from),
            target: env::var("TARGET").ok(),
            host: env::var("HOST").ok(),
            max_cstack_size: 1_000_000,
        }
    }
}

impl Build {
    /// Creates a new build configuration.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the output directory for build artifacts.
    ///
    /// Defaults to the `OUT_DIR` environment variable.
    pub fn out_dir<P: AsRef<Path>>(&mut self, path: P) -> &mut Self {
        self.out_dir = Some(path.as_ref().to_path_buf());
        self
    }

    /// Sets the target triple.
    ///
    /// Defaults to the `TARGET` environment variable.
    pub fn target(&mut self, target: &str) -> &mut Self {
        self.target = Some(target.to_string());
        self
    }

    /// Sets the host triple.
    ///
    /// Defaults to the `HOST` environment variable.
    pub fn host(&mut self, host: &str) -> &mut Self {
        self.host = Some(host.to_string());
        self
    }

    /// Sets the maximum number of Luau stack slots a C function can use.
    pub fn set_max_cstack_size(&mut self, size: usize) -> &mut Self {
        self.max_cstack_size = size;
        self
    }

    /// Builds the vendored Luau runtime libraries.
    ///
    /// CodeGen is always compiled. Luau vectors are always configured as 3-wide.
    #[must_use]
    pub fn build(&mut self) -> Artifacts {
        let target = self.target.as_deref().expect("TARGET is not set");
        let host = self.host.as_deref().expect("HOST is not set");
        let out_dir = self.out_dir.as_ref().expect("OUT_DIR is not set");
        let build_dir = out_dir.join("luau-build");

        let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("luau");
        emit_rerun_if_changed(&source_root);

        if build_dir.exists() {
            fs::remove_dir_all(&build_dir).unwrap();
        }

        let mut base = self.base_config(target);
        base.include(source_root.join("Common").join("include"));

        for comp in COMPONENTS {
            if target.ends_with("emscripten")
                && let Some(message) = comp.emscripten_error
            {
                panic!("{message}");
            }

            let src_dir = match comp.source {
                ComponentSource::Luau(path) => source_root.join(path),
                ComponentSource::Crate(path) => Path::new(env!("CARGO_MANIFEST_DIR")).join(path),
            };

            if matches!(comp.source, ComponentSource::Crate(_)) {
                emit_rerun_if_changed(&src_dir);
            }

            let mut cfg = base.clone();

            for inc in comp.extra_includes {
                cfg.include(source_root.join(inc));
            }

            for define in comp.native_api_defines {
                cfg.define(define, native_api_define());
            }

            cfg.add_files_by_ext_sorted(&src_dir, "cpp")
                .out_dir(&build_dir)
                .compile(comp.name);
        }

        // The include_paths returned to downstream (mainly for the analysis shim)
        // must remain stable. We keep them explicit here.
        let common_include_dir = source_root.join("Common").join("include");
        let vm_source_dir = source_root.join("VM").join("src");
        let vm_include_dir = source_root.join("VM").join("include");
        let ast_include_dir = source_root.join("Ast").join("include");
        let analysis_include_dir = source_root.join("Analysis").join("include");
        let compiler_include_dir = source_root.join("Compiler").join("include");
        let config_include_dir = source_root.join("Config").join("include");
        let codegen_include_dir = source_root.join("CodeGen").join("include");
        let require_include_dir = source_root.join("Require").join("include");

        Artifacts {
            lib_dir: build_dir,
            libs: LINK_LIBS.iter().map(ToString::to_string).collect(),
            cpp_stdlib: Self::cpp_link_stdlib(target, host),
            source_root,
            include_paths: vec![
                common_include_dir,
                analysis_include_dir,
                ast_include_dir,
                codegen_include_dir,
                compiler_include_dir,
                config_include_dir,
                require_include_dir,
                vm_include_dir,
                vm_source_dir,
            ],
        }
    }

    /// Creates the base C++ compiler configuration shared by all Luau libraries.
    fn base_config(&self, target: &str) -> cc::Build {
        let mut config = cc::Build::new();
        config
            .warnings(false)
            .cargo_metadata(false)
            .std("c++17")
            .cpp(true);

        if target.ends_with("emscripten") {
            config.flag_if_supported("-fexceptions");
            config.flag_if_supported("-fwasm-exceptions");
        }

        if !target.contains("msvc") {
            config.flag_if_supported("-fvisibility=hidden");
        }

        config.define("LUAI_MAXCSTACK", self.max_cstack_size.to_string().as_str());
        config.define("LUA_VECTOR_SIZE", VECTOR_SIZE.to_string().as_str());
        config.define("LUA_API", native_api_define());

        if cfg!(debug_assertions) {
            config.define("LUAU_ENABLE_ASSERT", None);
        } else {
            config.flag_if_supported("-fno-math-errno");
        }

        config
    }

    /// Determines the C++ standard library to link for a target.
    fn cpp_link_stdlib(target: &str, host: &str) -> Option<String> {
        let kind = if host == target { "HOST" } else { "TARGET" };
        let explicit = env::var(format!("CXXSTDLIB_{target}"))
            .or_else(|_| env::var(format!("CXXSTDLIB_{}", target.replace('-', "_"))))
            .or_else(|_| env::var(format!("{kind}_CXXSTDLIB")))
            .or_else(|_| env::var("CXXSTDLIB"))
            .ok();
        if explicit.is_some() {
            return explicit;
        }

        if target.contains("msvc") {
            None
        } else if target.contains("apple")
            || target.contains("freebsd")
            || target.contains("openbsd")
        {
            Some("c++".to_string())
        } else if target.contains("android") {
            Some("c++_shared".to_string())
        } else {
            Some("stdc++".to_string())
        }
    }
}

impl Artifacts {
    /// Returns the native library output directory.
    #[must_use]
    pub fn lib_dir(&self) -> &Path {
        &self.lib_dir
    }

    /// Returns the static Luau libraries in link order.
    #[must_use]
    pub fn libs(&self) -> &[String] {
        &self.libs
    }

    /// Returns the C++ standard library that downstream build scripts should link.
    #[must_use]
    pub fn cpp_stdlib(&self) -> Option<&str> {
        self.cpp_stdlib.as_deref()
    }

    /// Returns the vendored Luau source root.
    #[must_use]
    pub fn source_root(&self) -> &Path {
        &self.source_root
    }

    /// Returns include paths needed by native shims that compile against Luau.
    #[must_use]
    pub fn include_paths(&self) -> &[PathBuf] {
        &self.include_paths
    }

    /// Emits Cargo metadata needed by downstream build scripts.
    pub fn print_cargo_metadata(&self) {
        println!("cargo:rustc-link-search=native={}", self.lib_dir.display());
        for lib in &self.libs {
            println!("cargo:rustc-link-lib=static={lib}");
        }
        if let Some(ref cpp_stdlib) = self.cpp_stdlib {
            println!("cargo:rustc-link-lib={cpp_stdlib}");
        }

        println!("cargo:rustc-env=LUAU_VERSION={LUAU_VERSION}");
        println!(
            "cargo:rustc-env=RUAU_LUAU_SOURCE_ROOT={}",
            self.source_root.display()
        );
        for include_path in &self.include_paths {
            println!(
                "cargo:rustc-env=RUAU_LUAU_INCLUDE_PATH={}",
                include_path.display()
            );
        }
    }

    /// Returns the vendored Luau version.
    #[must_use]
    pub const fn version(&self) -> &'static str {
        LUAU_VERSION
    }
}

/// Returns the native API visibility define used for C++ builds.
fn native_api_define() -> &'static str {
    "extern \"C\""
}

/// Emits a Cargo rerun-if-changed directive.
///
/// Cargo automatically watches the directory and all its contents recursively,
/// so emitting one entry per file is redundant and produces very noisy build output.
fn emit_rerun_if_changed(path: &Path) {
    println!("cargo:rerun-if-changed={}", path.display());
}

/// Adds sorted source files to a C++ build by file extension.
trait AddFilesByExt {
    /// Adds files under `dir` with extension `ext`, sorted by path.
    fn add_files_by_ext_sorted(&mut self, dir: &Path, ext: &str) -> &mut Self;
}

impl AddFilesByExt for cc::Build {
    fn add_files_by_ext_sorted(&mut self, dir: &Path, ext: &str) -> &mut Self {
        let mut sources: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .filter_map(StdResult::ok)
            .filter(|entry| entry.path().extension() == Some(ext.as_ref()))
            .map(|entry| entry.path())
            .collect();

        sources.sort();

        for source in sources {
            self.file(source);
        }

        self
    }
}
