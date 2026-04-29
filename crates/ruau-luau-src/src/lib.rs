//! Workspace-owned build helper for the vendored Luau source tree.

#![allow(
    clippy::absolute_paths,
    clippy::missing_docs_in_private_items,
    clippy::needless_pass_by_value
)]

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
    out_dir: Option<PathBuf>,
    target: Option<String>,
    host: Option<String>,
    max_cstack_size: usize,
    use_longjmp: bool,
}

/// Native artifacts produced by [`Build`].
pub struct Artifacts {
    lib_dir: PathBuf,
    libs: Vec<String>,
    cpp_stdlib: Option<String>,
    source_root: PathBuf,
    include_paths: Vec<PathBuf>,
}

impl Default for Build {
    fn default() -> Self {
        Self {
            out_dir: env::var_os("OUT_DIR").map(PathBuf::from),
            target: env::var("TARGET").ok(),
            host: env::var("HOST").ok(),
            max_cstack_size: 1_000_000,
            use_longjmp: false,
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

    /// Uses `longjmp` instead of C++ exceptions in Luau error handling.
    pub fn use_longjmp(&mut self, use_longjmp: bool) -> &mut Self {
        self.use_longjmp = use_longjmp;
        self
    }

    /// Builds the vendored Luau runtime libraries.
    ///
    /// CodeGen is always compiled. Luau vectors are always configured as 3-wide.
    #[must_use]
    pub fn build(&mut self) -> Artifacts {
        let target = &self.target.as_ref().expect("TARGET is not set")[..];
        let host = &self.host.as_ref().expect("HOST is not set")[..];
        let out_dir = self.out_dir.as_ref().expect("OUT_DIR is not set");
        let build_dir = out_dir.join("luau-build");

        let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("luau");
        emit_rerun_if_changed(&source_root);

        let common_include_dir = source_root.join("Common").join("include");
        let vm_source_dir = source_root.join("VM").join("src");
        let vm_include_dir = source_root.join("VM").join("include");

        if build_dir.exists() {
            fs::remove_dir_all(&build_dir).unwrap();
        }

        let mut config = self.base_config(target);
        config.include(&common_include_dir);

        let ast_source_dir = source_root.join("Ast").join("src");
        let ast_include_dir = source_root.join("Ast").join("include");
        let ast_lib_name = "luauast";
        config
            .clone()
            .include(&ast_include_dir)
            .add_files_by_ext_sorted(&ast_source_dir, "cpp")
            .out_dir(&build_dir)
            .compile(ast_lib_name);

        let codegen_source_dir = source_root.join("CodeGen").join("src");
        let codegen_include_dir = source_root.join("CodeGen").join("include");
        let codegen_lib_name = "luaucodegen";
        if target.ends_with("emscripten") {
            panic!("CodeGen is not supported on emscripten");
        }
        config
            .clone()
            .include(&codegen_include_dir)
            .include(&vm_include_dir)
            .include(&vm_source_dir)
            .define("LUACODEGEN_API", native_api_define())
            .add_files_by_ext_sorted(&codegen_source_dir, "cpp")
            .out_dir(&build_dir)
            .compile(codegen_lib_name);

        let common_source_dir = source_root.join("Common").join("src");
        let common_lib_name = "luaucommon";
        config
            .clone()
            .include(&common_include_dir)
            .add_files_by_ext_sorted(&common_source_dir, "cpp")
            .out_dir(&build_dir)
            .compile(common_lib_name);

        let compiler_source_dir = source_root.join("Compiler").join("src");
        let compiler_include_dir = source_root.join("Compiler").join("include");
        let compiler_lib_name = "luaucompiler";
        config
            .clone()
            .include(&compiler_include_dir)
            .include(&ast_include_dir)
            .define("LUACODE_API", native_api_define())
            .add_files_by_ext_sorted(&compiler_source_dir, "cpp")
            .out_dir(&build_dir)
            .compile(compiler_lib_name);

        let config_source_dir = source_root.join("Config").join("src");
        let config_include_dir = source_root.join("Config").join("include");
        let config_lib_name = "luauconfig";
        config
            .clone()
            .include(&config_include_dir)
            .include(&ast_include_dir)
            .include(&compiler_include_dir)
            .include(&vm_include_dir)
            .add_files_by_ext_sorted(&config_source_dir, "cpp")
            .out_dir(&build_dir)
            .compile(config_lib_name);

        let custom_source_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("custom")
            .join("src");
        emit_rerun_if_changed(&custom_source_dir);

        let custom_lib_name = "luaucustom";
        config
            .clone()
            .include(&vm_include_dir)
            .include(&vm_source_dir)
            .add_files_by_ext_sorted(&custom_source_dir, "cpp")
            .out_dir(&build_dir)
            .compile(custom_lib_name);

        let require_source_dir = source_root.join("Require").join("src");
        let require_include_dir = source_root.join("Require").join("include");
        let require_lib_name = "luaurequire";
        config
            .clone()
            .include(&require_include_dir)
            .include(&ast_include_dir)
            .include(&config_include_dir)
            .include(&vm_include_dir)
            .add_files_by_ext_sorted(&require_source_dir, "cpp")
            .out_dir(&build_dir)
            .compile(require_lib_name);

        let vm_lib_name = "luauvm";
        config
            .clone()
            .include(&vm_include_dir)
            .add_files_by_ext_sorted(&vm_source_dir, "cpp")
            .out_dir(&build_dir)
            .compile(vm_lib_name);

        Artifacts {
            lib_dir: build_dir,
            libs: vec![
                vm_lib_name.to_string(),
                compiler_lib_name.to_string(),
                ast_lib_name.to_string(),
                common_lib_name.to_string(),
                config_lib_name.to_string(),
                custom_lib_name.to_string(),
                require_lib_name.to_string(),
                codegen_lib_name.to_string(),
            ],
            cpp_stdlib: Self::cpp_link_stdlib(target, host),
            source_root,
            include_paths: vec![
                common_include_dir,
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

        if self.use_longjmp {
            config.define("LUA_USE_LONGJMP", "1");
        }

        if cfg!(debug_assertions) {
            config.define("LUAU_ENABLE_ASSERT", None);
        } else {
            config.flag_if_supported("-fno-math-errno");
        }

        config
    }

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

fn native_api_define() -> &'static str {
    "extern \"C\""
}

fn emit_rerun_if_changed(path: &Path) {
    println!("cargo:rerun-if-changed={}", path.display());

    let mut entries: Vec<_> = fs::read_dir(path)
        .unwrap()
        .filter_map(StdResult::ok)
        .map(|entry| entry.path())
        .collect();
    entries.sort();

    for entry in entries {
        if entry.is_dir() {
            emit_rerun_if_changed(&entry);
        } else {
            println!("cargo:rerun-if-changed={}", entry.display());
        }
    }
}

trait AddFilesByExt {
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
