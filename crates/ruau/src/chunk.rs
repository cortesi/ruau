//! Luau chunk loading and execution.
//!
//! This module provides types for loading Luau source code into a [`Chunk`], configuring how it is
//! compiled and executed, and converting it into a callable [`Function`].
//!
//! Chunks can be loaded from strings or byte buffers via the [`AsChunk`] trait.

use std::{borrow::Cow, collections::HashMap, ffi::CString, io::Result as IoResult, panic::Location};

use crate::{
    error::{Error, Result},
    function::Function,
    state::{Luau, WeakLuau},
    table::Table,
    traits::{FromLuauMulti, IntoLuau, IntoLuauMulti},
    value::Value,
};

/// Trait for source inputs loadable by Luau and convertible to a [`Chunk`].
pub trait AsChunk {
    /// Returns optional chunk name
    ///
    /// See [`Chunk::name`] for possible name prefixes.
    fn name(&self) -> Option<String> {
        None
    }

    /// Returns optional chunk environment.
    fn environment(&self, lua: &Luau) -> Result<Option<Table>> {
        let _lua = lua; // suppress warning
        Ok(None)
    }

    /// Returns chunk source data.
    fn source<'a>(&self) -> IoResult<Cow<'a, [u8]>>
    where
        Self: 'a;
}

impl AsChunk for &str {
    fn source<'a>(&self) -> IoResult<Cow<'a, [u8]>>
    where
        Self: 'a,
    {
        Ok(Cow::Borrowed(self.as_bytes()))
    }
}

impl AsChunk for String {
    fn source<'a>(&self) -> IoResult<Cow<'a, [u8]>> {
        Ok(Cow::Owned(self.clone().into_bytes()))
    }
}

impl AsChunk for &String {
    fn source<'a>(&self) -> IoResult<Cow<'a, [u8]>>
    where
        Self: 'a,
    {
        Ok(Cow::Borrowed(self.as_bytes()))
    }
}

impl AsChunk for &[u8] {
    fn source<'a>(&self) -> IoResult<Cow<'a, [u8]>>
    where
        Self: 'a,
    {
        Ok(Cow::Borrowed(self))
    }
}

impl AsChunk for Vec<u8> {
    fn source<'a>(&self) -> IoResult<Cow<'a, [u8]>> {
        Ok(Cow::Owned(self.clone()))
    }
}

impl AsChunk for &Vec<u8> {
    fn source<'a>(&self) -> IoResult<Cow<'a, [u8]>>
    where
        Self: 'a,
    {
        Ok(Cow::Borrowed(self))
    }
}

impl<C: AsChunk + ?Sized> AsChunk for Box<C> {
    fn name(&self) -> Option<String> {
        (**self).name()
    }

    fn environment(&self, lua: &Luau) -> Result<Option<Table>> {
        (**self).environment(lua)
    }

    fn source<'a>(&self) -> IoResult<Cow<'a, [u8]>>
    where
        Self: 'a,
    {
        (**self).source()
    }
}

/// Returned from [`Luau::load`] and is used to finalize loading and executing Luau main chunks.
#[must_use = "`Chunk`s do nothing unless one of `exec`, `eval`, `call`, or `into_function` are called on them"]
pub struct Chunk<'a> {
    pub(crate) lua: WeakLuau,
    pub(crate) name: String,
    pub(crate) env: Result<Option<Table>>,
    pub(crate) mode: ChunkMode,
    pub(crate) source: IoResult<Cow<'a, [u8]>>,
    pub(crate) compiler: Option<Compiler>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChunkMode {
    Text,
    Binary,
}

/// Represents a constant value that can be used by Luau compiler.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub enum CompileConstant {
    /// Luau `nil`.
    Nil,
    /// Boolean constant.
    Boolean(bool),
    /// Numeric constant.
    Number(f64),
    /// Vector constant.
    Vector(crate::Vector),
    /// String constant.
    String(String),
}

impl From<bool> for CompileConstant {
    fn from(b: bool) -> Self {
        Self::Boolean(b)
    }
}

impl From<f64> for CompileConstant {
    fn from(n: f64) -> Self {
        Self::Number(n)
    }
}

impl From<crate::Vector> for CompileConstant {
    fn from(v: crate::Vector) -> Self {
        Self::Vector(v)
    }
}

impl From<&str> for CompileConstant {
    fn from(s: &str) -> Self {
        Self::String(s.to_owned())
    }
}

type LibraryMemberConstantMap = HashMap<(String, String), CompileConstant>;

/// Luau compiler optimization level.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum OptimizationLevel {
    /// No optimization.
    None = 0,
    /// Baseline optimization that preserves debuggability.
    Debug = 1,
    /// Additional optimizations such as inlining that can reduce debuggability.
    Release = 2,
}

/// Luau compiler debug information level.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum DebugLevel {
    /// No debugging support.
    None = 0,
    /// Line info and function names for backtraces.
    LineInfo = 1,
    /// Full debug info with local and upvalue names.
    Full = 2,
}

/// Luau type information level used to guide native code generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum TypeInfoLevel {
    /// Generate type information for native modules.
    NativeModules = 0,
    /// Generate type information for all modules.
    AllModules = 1,
}

/// Luau compiler code coverage instrumentation level.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CoverageLevel {
    /// No code coverage support.
    None = 0,
    /// Statement coverage.
    Statement = 1,
    /// Statement and expression coverage.
    StatementAndExpression = 2,
}

/// Luau compiler
#[derive(Clone, Debug)]
pub struct Compiler {
    optimization_level: OptimizationLevel,
    debug_level: DebugLevel,
    type_info_level: TypeInfoLevel,
    coverage_level: CoverageLevel,
    mutable_globals: Vec<String>,
    userdata_types: Vec<String>,
    libraries_with_known_members: Vec<String>,
    library_constants: Option<LibraryMemberConstantMap>,
    disabled_builtins: Vec<String>,
}

impl Default for Compiler {
    fn default() -> Self {
        const { Self::new() }
    }
}

impl Compiler {
    /// Creates Luau compiler instance with default options
    pub const fn new() -> Self {
        // Defaults are taken from luacode.h
        Self {
            optimization_level: OptimizationLevel::Debug,
            debug_level: DebugLevel::LineInfo,
            type_info_level: TypeInfoLevel::NativeModules,
            coverage_level: CoverageLevel::None,
            mutable_globals: Vec::new(),
            userdata_types: Vec::new(),
            libraries_with_known_members: Vec::new(),
            library_constants: None,
            disabled_builtins: Vec::new(),
        }
    }

    /// Sets Luau compiler optimization level.
    #[must_use]
    pub const fn optimization_level(mut self, level: OptimizationLevel) -> Self {
        self.optimization_level = level;
        self
    }

    /// Sets Luau compiler debug level.
    #[must_use]
    pub const fn debug_level(mut self, level: DebugLevel) -> Self {
        self.debug_level = level;
        self
    }

    /// Sets Luau type information level used to guide native code generation decisions.
    #[must_use]
    pub const fn type_info_level(mut self, level: TypeInfoLevel) -> Self {
        self.type_info_level = level;
        self
    }

    /// Sets Luau compiler code coverage level.
    #[must_use]
    pub const fn coverage_level(mut self, level: CoverageLevel) -> Self {
        self.coverage_level = level;
        self
    }

    /// Sets a list of globals that are mutable.
    ///
    /// It disables the import optimization for fields accessed through these.
    #[must_use]
    pub fn mutable_globals<S: Into<String>>(mut self, globals: impl IntoIterator<Item = S>) -> Self {
        self.mutable_globals = globals.into_iter().map(|s| s.into()).collect();
        self
    }

    /// Sets a list of userdata types that will be included in the type information.
    #[must_use]
    pub fn userdata_types<S: Into<String>>(mut self, types: impl IntoIterator<Item = S>) -> Self {
        self.userdata_types = types.into_iter().map(|s| s.into()).collect();
        self
    }

    /// Adds a constant for a known library member.
    ///
    /// The constants are used by the compiler to optimize the generated bytecode.
    /// Optimization level must be at least 2 for this to have any effect.
    ///
    /// The `name` is a string in the format `lib.member`, where `lib` is the library name
    /// and `member` is the member (constant) name.
    #[must_use]
    pub fn add_library_constant(
        mut self,
        name: impl AsRef<str>,
        constant: impl Into<CompileConstant>,
    ) -> Self {
        let Some((lib, member)) = name.as_ref().split_once('.') else {
            return self;
        };
        let (lib, member) = (lib.to_owned(), member.to_owned());

        if !self.libraries_with_known_members.contains(&lib) {
            self.libraries_with_known_members.push(lib.clone());
        }
        self.library_constants
            .get_or_insert_default()
            .insert((lib, member), constant.into());
        self
    }

    /// Adds a compile-time constant under the built-in `vector` library.
    ///
    /// This is a convenience wrapper for constants like `vector.zero` or `vector.one` when the
    /// embedding wants Luau's optimizer to fold them without exposing a custom library name.
    #[must_use]
    pub fn add_vector_constant(self, member: impl AsRef<str>, vector: impl Into<crate::Vector>) -> Self {
        self.add_library_constant(format!("vector.{}", member.as_ref()), vector.into())
    }

    /// Sets a list of builtins that should be disabled.
    #[must_use]
    pub fn disabled_builtins<S: Into<String>>(mut self, builtins: impl IntoIterator<Item = S>) -> Self {
        self.disabled_builtins = builtins.into_iter().map(|s| s.into()).collect();
        self
    }

    /// Compiles the `source` into bytecode.
    ///
    /// Returns [`Error::SyntaxError`] if the source code is invalid.
    pub fn compile(&self, source: impl AsRef<[u8]>) -> Result<Vec<u8>> {
        use std::{
            cell::RefCell,
            ffi::CStr,
            os::raw::{c_char, c_int},
            ptr,
        };

        macro_rules! vec2cstring_ptr {
            ($name:ident, $name_ptr:ident) => {
                let $name = self
                    .$name
                    .iter()
                    .map(|name| CString::new(name.clone()).ok())
                    .collect::<Option<Vec<_>>>()
                    .unwrap_or_default();
                let mut $name = $name.iter().map(|s| s.as_ptr()).collect::<Vec<_>>();
                let mut $name_ptr = ptr::null();
                if !$name.is_empty() {
                    $name.push(ptr::null());
                    $name_ptr = $name.as_ptr();
                }
            };
        }

        vec2cstring_ptr!(mutable_globals, mutable_globals_ptr);
        vec2cstring_ptr!(userdata_types, userdata_types_ptr);
        vec2cstring_ptr!(libraries_with_known_members, libraries_with_known_members_ptr);
        vec2cstring_ptr!(disabled_builtins, disabled_builtins_ptr);

        thread_local! {
            static LIBRARY_MEMBER_CONSTANT_MAP: RefCell<LibraryMemberConstantMap> = Default::default();
        }
        unsafe extern "C-unwind" fn library_member_constant_callback(
            library: *const c_char,
            member: *const c_char,
            constant: *mut ffi::lua_CompileConstant,
        ) {
            let library = CStr::from_ptr(library).to_string_lossy();
            let member = CStr::from_ptr(member).to_string_lossy();
            LIBRARY_MEMBER_CONSTANT_MAP.with_borrow(|map| {
                if let Some(cons) = map.get(&(library.to_string(), member.to_string())) {
                    match cons {
                        CompileConstant::Nil => ffi::luau_set_compile_constant_nil(constant),
                        CompileConstant::Boolean(b) => {
                            ffi::luau_set_compile_constant_boolean(constant, *b as c_int)
                        }
                        CompileConstant::Number(n) => ffi::luau_set_compile_constant_number(constant, *n),
                        CompileConstant::Vector(v) => {
                            ffi::luau_set_compile_constant_vector(constant, v.x(), v.y(), v.z(), 0.0);
                        }
                        CompileConstant::String(s) => ffi::luau_set_compile_constant_string(
                            constant,
                            s.as_ptr() as *const c_char,
                            s.len(),
                        ),
                    }
                }
            })
        }

        let bytecode = unsafe {
            let mut options = ffi::lua_CompileOptions::default();
            options.optimizationLevel = self.optimization_level as c_int;
            options.debugLevel = self.debug_level as c_int;
            options.typeInfoLevel = self.type_info_level as c_int;
            options.coverageLevel = self.coverage_level as c_int;
            options.mutableGlobals = mutable_globals_ptr;
            options.userdataTypes = userdata_types_ptr;
            options.librariesWithKnownMembers = libraries_with_known_members_ptr;
            if let Some(map) = self.library_constants.as_ref()
                && !self.libraries_with_known_members.is_empty()
            {
                LIBRARY_MEMBER_CONSTANT_MAP.with_borrow_mut(|gmap| *gmap = map.clone());
                options.libraryMemberConstantCallback = Some(library_member_constant_callback);
            }
            options.disabledBuiltins = disabled_builtins_ptr;
            ffi::luau_compile(source.as_ref(), options)
        };

        if bytecode.first() == Some(&0) {
            // The rest of the bytecode is the error message starting with `:`
            // See https://github.com/luau-lang/luau/blob/0.640/Compiler/src/Compiler.cpp#L4336
            let message = String::from_utf8_lossy(&bytecode[2..]).into_owned();
            return Err(Error::SyntaxError {
                incomplete_input: message.ends_with("<eof>"),
                message,
            });
        }

        Ok(bytecode)
    }
}

impl Chunk<'_> {
    /// Sets the name of this chunk, which results in more informative error traces.
    ///
    /// Possible name prefixes:
    /// - `@` - file path (when truncation is needed, the end of the file path is kept, as this is
    ///   more useful for identifying the file)
    /// - `=` - custom chunk name (when truncation is needed, the beginning of the name is kept)
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Sets the environment of the loaded chunk to the given value.
    ///
    /// Luau chunks use an environment table for global variable resolution. By default this is set
    /// to the global environment.
    ///
    /// Calling this method changes the environment used by the chunk, and global variables inside
    /// the chunk will refer to the given table rather than the global one.
    ///
    /// All global variables (including the standard library!) are looked up in `_ENV`, so it may be
    /// necessary to populate the environment in order for scripts using custom environments to be
    /// useful.
    pub fn environment(mut self, env: Table) -> Self {
        self.env = Ok(Some(env));
        self
    }

    /// Sets or overwrites a Luau compiler used for this chunk.
    ///
    /// See [`Compiler`] for details and possible options.
    pub fn compiler(mut self, compiler: Compiler) -> Self {
        self.compiler = Some(compiler);
        self
    }

    /// Execute this chunk of code.
    ///
    /// This is equivalent to calling the chunk function with no arguments and no return values.
    /// The returned future is local to the VM and is not `Send`; spawn it only on a local executor
    /// such as [`tokio::task::LocalSet`].
    pub async fn exec(self) -> Result<()> {
        self.call(()).await
    }

    /// Evaluate the chunk as either an expression or block.
    ///
    /// If the chunk can be parsed as an expression, this loads and executes the chunk and returns
    /// the value that it evaluates to. Otherwise, the chunk is interpreted as a block as normal,
    /// and this is equivalent to calling `exec`.
    ///
    /// The returned future is local to the VM and is not `Send`; spawn it only on a local executor
    /// such as [`tokio::task::LocalSet`].
    pub async fn eval<R: FromLuauMulti>(self) -> Result<R> {
        // First try interpreting the source as an expression by adding
        // "return", then as a statement. This is the same thing the
        // actual Luau REPL does.
        if let Ok(function) = self.to_expression() {
            function.call(()).await
        } else {
            self.call(()).await
        }
    }

    /// Load the chunk function and call it with the given arguments.
    ///
    /// This is equivalent to `into_function` and calling the resulting function.
    /// The returned future is local to the VM and is not `Send`; spawn it only on a local executor
    /// such as [`tokio::task::LocalSet`].
    pub async fn call<R>(self, args: impl IntoLuauMulti) -> Result<R>
    where
        R: FromLuauMulti,
    {
        self.into_function()?.call(args).await
    }

    pub(crate) fn call_sync<R: FromLuauMulti>(self, args: impl IntoLuauMulti) -> Result<R> {
        self.into_function()?.call_sync(args)
    }

    /// Load this chunk into a regular [`Function`].
    ///
    /// This simply compiles the chunk without actually executing it.
    pub fn into_function(mut self) -> Result<Function> {
        if self.compiler.is_some() {
            // We don't need to compile source if no compiler set
            self.compile();
        }

        let name = Self::convert_name(self.name)?;
        self.lua
            .raw()
            .load_chunk(Some(&name), self.env?.as_ref(), self.mode, self.source?.as_ref())
    }

    /// Compiles the chunk and changes mode to binary.
    ///
    /// It does nothing if the chunk is already binary or invalid.
    fn compile(&mut self) {
        if let Ok(ref source) = self.source
            && self.mode == ChunkMode::Text
            && let Ok(data) = self.compiler.get_or_insert_default().compile(source)
        {
            self.source = Ok(Cow::Owned(data));
            self.mode = ChunkMode::Binary;
        }
    }

    /// Fetches compiled bytecode of this chunk from the cache.
    ///
    /// If not found, compiles the source code and stores it on the cache.
    pub(crate) fn try_cache(mut self) -> Self {
        struct ChunksCache(HashMap<Vec<u8>, Vec<u8>>);

        // Try to fetch compiled chunk from cache
        let mut text_source = None;
        if let Ok(ref source) = self.source
            && self.mode == ChunkMode::Text
        {
            let cached = {
                let lua = self.lua.raw();
                lua.priv_app_data_ref::<ChunksCache>()
                    .and_then(|cache| cache.0.get(source.as_ref()).cloned())
            };
            if let Some(data) = cached {
                self.source = Ok(Cow::Owned(data));
                self.mode = ChunkMode::Binary;
                return self;
            }
            text_source = Some(source.as_ref().to_vec());
        }

        // Compile and cache the chunk
        if let Some(text_source) = text_source {
            self.compile();
            if let Ok(ref binary_source) = self.source
                && self.mode == ChunkMode::Binary
            {
                let lua = self.lua.raw();
                if let Some(mut cache) = lua.priv_app_data_mut::<ChunksCache>() {
                    cache.0.insert(text_source, binary_source.to_vec());
                } else {
                    let mut cache = ChunksCache(HashMap::new());
                    cache.0.insert(text_source, binary_source.to_vec());
                    lua.set_priv_app_data(cache);
                }
            }
        }

        self
    }

    fn to_expression(&self) -> Result<Function> {
        // We assume that mode is Text
        let source = self.source.as_ref();
        let source = source.map_err(Error::runtime)?;
        let source = Self::expression_source(source);
        // We don't need to compile source if no compiler options set
        let source = self
            .compiler
            .as_ref()
            .map(|c| c.compile(&source))
            .transpose()?
            .unwrap_or(source);

        let name = Self::convert_name(self.name.clone())?;
        let env = match &self.env {
            Ok(Some(env)) => Some(env),
            Ok(None) => None,
            Err(err) => return Err(err.clone()),
        };
        self.lua
            .raw()
            .load_chunk(Some(&name), env, ChunkMode::Text, &source)
    }

    pub(crate) fn convert_name(name: String) -> Result<CString> {
        CString::new(name).map_err(|err| Error::runtime(format!("invalid name: {err}")))
    }

    fn expression_source(source: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(b"return ".len() + source.len());
        buf.extend(b"return ");
        buf.extend(source);
        buf
    }
}

struct WrappedChunk<T: AsChunk> {
    chunk: T,
    caller: &'static Location<'static>,
}

impl Chunk<'_> {
    /// Returns a deferred conversion adapter for a chunk of Luau code.
    ///
    /// The resulting [`IntoLuau`] implementation compiles the chunk into a Luau function without
    /// executing it when conversion runs. Use [`Luau::load`](crate::Luau::load) when the chunk
    /// should be loaded eagerly.
    ///
    /// See also [`LuauString::wrap`](crate::LuauString::wrap) and
    /// [`AnyUserData::wrap`](crate::AnyUserData::wrap).
    #[track_caller]
    pub fn wrap(chunk: impl AsChunk) -> impl IntoLuau {
        WrappedChunk {
            chunk,
            caller: Location::caller(),
        }
    }
}

impl<T: AsChunk> IntoLuau for WrappedChunk<T> {
    fn into_luau(self, lua: &Luau) -> Result<Value> {
        lua.load_with_location(self.chunk, self.caller)
            .into_function()
            .map(Value::Function)
    }
}
