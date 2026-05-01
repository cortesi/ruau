use std::{ffi::c_void, ptr};

/// C ABI diagnostic structure emitted by the analysis shim.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct RuauDiagnostic {
    pub line: u32,
    pub col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub severity: u32,
    pub message: *const u8,
    pub message_len: u32,
}

/// C ABI result object containing diagnostic storage.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct RuauCheckResult {
    pub _internal: *mut c_void,
    pub diagnostics: *const RuauDiagnostic,
    pub diagnostic_count: u32,
    pub timed_out: u32,
    pub cancelled: u32,
}

/// C ABI string result used for definition-load failures.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct RuauString {
    pub _internal: *mut c_void,
    pub data: *const u8,
    pub len: u32,
}

/// C ABI entrypoint parameter row emitted by the shim.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct RuauEntrypointParam {
    pub name: *const u8,
    pub name_len: u32,
    pub annotation: *const u8,
    pub annotation_len: u32,
    pub optional: u32,
}

/// C ABI entrypoint schema extraction result.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct RuauEntrypointSchemaResult {
    pub _internal: *mut c_void,
    pub params: *const RuauEntrypointParam,
    pub param_count: u32,
    pub error: *const u8,
    pub error_len: u32,
}

/// C ABI require specifier row emitted by the shim.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct RuauRequireSpecifier {
    pub specifier: *const u8,
    pub specifier_len: u32,
    pub line: u32,
    pub col: u32,
    pub end_line: u32,
    pub end_col: u32,
}

/// C ABI require tracing result.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct RuauRequireTraceResult {
    pub _internal: *mut c_void,
    pub specifiers: *const RuauRequireSpecifier,
    pub specifier_count: u32,
    pub error: *const u8,
    pub error_len: u32,
}

/// C ABI check options passed into a single checker invocation.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct RuauCheckOptions {
    pub module_name: *const u8,
    pub module_name_len: u32,
    pub has_timeout: u32,
    pub timeout_seconds: f64,
    pub cancellation_token: RuauTokenHandle,
    pub virtual_modules: *const RuauVirtualModule,
    pub virtual_module_count: u32,
}

/// C ABI virtual module entry passed into one checker invocation.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct RuauVirtualModule {
    pub name: *const u8,
    pub name_len: u32,
    pub source: *const u8,
    pub source_len: u32,
}

/// Opaque checker handle returned by the native shim.
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct RuauCheckerHandle(pub *mut c_void);

impl RuauCheckerHandle {
    /// Returns whether the handle is null.
    #[must_use]
    pub fn is_null(self) -> bool {
        self.0.is_null()
    }
}

/// Opaque cancellation token handle returned by the native shim.
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct RuauTokenHandle(pub *mut c_void);

impl RuauTokenHandle {
    /// Returns a null token handle.
    #[must_use]
    pub const fn null() -> Self {
        Self(ptr::null_mut())
    }

    /// Returns whether the handle is null.
    #[must_use]
    pub fn is_null(self) -> bool {
        self.0.is_null()
    }
}

unsafe extern "C" {
    pub fn ruau_checker_new() -> RuauCheckerHandle;
    pub fn ruau_checker_free(checker: RuauCheckerHandle);

    pub fn ruau_cancellation_token_new() -> RuauTokenHandle;
    pub fn ruau_cancellation_token_free(token: RuauTokenHandle);
    pub fn ruau_cancellation_token_cancel(token: RuauTokenHandle);
    pub fn ruau_cancellation_token_reset(token: RuauTokenHandle);

    pub fn ruau_checker_add_definitions(
        checker: RuauCheckerHandle,
        defs: *const u8,
        defs_len: u32,
        module_name: *const u8,
        module_name_len: u32,
    ) -> RuauString;

    pub fn ruau_checker_check(
        checker: RuauCheckerHandle,
        source: *const u8,
        source_len: u32,
        options: *const RuauCheckOptions,
    ) -> RuauCheckResult;

    pub fn ruau_extract_entrypoint_schema(source: *const u8, source_len: u32) -> RuauEntrypointSchemaResult;

    pub fn ruau_trace_requires(source: *const u8, source_len: u32) -> RuauRequireTraceResult;

    pub fn ruau_check_result_free(result: RuauCheckResult);
    pub fn ruau_entrypoint_schema_result_free(result: RuauEntrypointSchemaResult);
    pub fn ruau_require_trace_result_free(result: RuauRequireTraceResult);
    pub fn ruau_string_free(value: RuauString);
}
