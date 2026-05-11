//! Native analyzer input packing and result decoding.

use std::{fs, marker::PhantomData, path::Path, ptr, slice, time::Duration};

use super::{AnalysisError, CheckOptions, CheckerOptions, Diagnostic, EntrypointParam, Severity};
use crate::{
    resolver::{ModuleId, SourceSpan},
    util::shim::{FfiResource, RawGuard},
};

/// Loads Luau definition source through the native checker with a chosen module label.
pub(super) fn add_definitions_raw(
    checker: ffi::RuauCheckerHandle,
    defs: &str,
    module_name: &str,
) -> Result<(), AnalysisError> {
    let defs = FfiStr::new(defs, "definitions")?;
    let module_name = FfiStr::new(module_name, "definition module name")?;

    // SAFETY: Pointers are valid for call duration and checker handle is live.
    let raw = RawGuard::new(unsafe {
        ffi::ruau_checker_add_definitions(
            checker,
            defs.ptr(),
            defs.len(),
            module_name.ptr(),
            module_name.len(),
        )
    });

    let string = raw.as_ref();
    if string.len == 0 {
        Ok(())
    } else {
        Err(AnalysisError::Definitions(string_from_raw(
            string.data,
            string.len,
        )))
    }
}

/// UTF-8 checker input loaded from disk together with its display label.
pub(super) struct LoadedInput {
    /// Display label used for diagnostics and module names.
    pub(super) label: String,
    /// UTF-8 contents loaded from disk.
    pub(super) contents: String,
}

impl LoadedInput {
    /// Reads one UTF-8 file used as checker input.
    pub(super) fn read(path: &Path, kind: &'static str) -> Result<Self, AnalysisError> {
        let label = path.display().to_string();
        let contents = fs::read_to_string(path).map_err(|error| AnalysisError::ReadFile {
            kind,
            path: label.clone(),
            message: error.to_string(),
        })?;
        Ok(Self { label, contents })
    }
}

/// Borrowed UTF-8 input prepared for a C ABI call.
#[derive(Clone, Copy)]
pub(super) struct FfiStr<'a> {
    /// Pointer to the UTF-8 bytes, or null for empty strings.
    ptr: *const u8,
    /// Length of the UTF-8 payload in bytes.
    len: u32,
    /// Ties the raw pointer to the borrowed Rust string lifetime.
    _marker: PhantomData<&'a str>,
}

impl<'a> FfiStr<'a> {
    /// Converts a Rust string to a pointer-length pair accepted by the C ABI.
    pub(super) fn new(value: &'a str, kind: &'static str) -> Result<Self, AnalysisError> {
        let len = u32::try_from(value.len()).map_err(|_| AnalysisError::InputTooLarge {
            kind,
            len: value.len(),
        })?;

        Ok(Self {
            ptr: if len == 0 {
                ptr::null()
            } else {
                value.as_ptr()
            },
            len,
            _marker: PhantomData,
        })
    }

    /// Returns the UTF-8 pointer for the C ABI.
    pub(super) fn ptr(self) -> *const u8 {
        self.ptr
    }

    /// Returns the UTF-8 byte length for the C ABI.
    pub(super) fn len(self) -> u32 {
        self.len
    }
}

/// One owned virtual module's stable backing storage.
///
/// `name` and `source` are heap-allocated `Box<[u8]>`, so their pointers stay stable across
/// moves of the enclosing `OwnedCheckInputs`.
struct OwnedVirtualModule {
    name: Box<[u8]>,
    name_len: u32,
    source: Box<[u8]>,
    source_len: u32,
}

/// Owned, `Send + 'static` package of inputs for one `ruau_checker_check` call.
///
/// Built on the runtime thread before `spawn_blocking` is invoked. All input
/// pointers passed to the C ABI are derived from boxed slices owned by this struct, so the
/// data outlives any caller borrows for the duration of the blocking work.
///
/// `virtual_module_entries` contains raw pointers into the boxed slices in retained
/// virtual module storage; both fields move together with the struct since the heap
/// allocations are pointed-at by stable addresses.
pub(super) struct OwnedCheckInputs {
    pub(super) module_id: ModuleId,
    source: Box<[u8]>,
    source_len: u32,
    module_name: Box<[u8]>,
    module_name_len: u32,
    timeout: Option<Duration>,
    /// Stable backing storage; pointers in `virtual_module_entries` reference these boxes.
    _virtual_module_storage: Vec<OwnedVirtualModule>,
    virtual_module_entries: Vec<ffi::RuauVirtualModule>,
}

// SAFETY: All pointer fields point into heap-allocated `Box<[u8]>` storage that is kept alive
// by the same struct. Moving the struct does not invalidate those pointers.
unsafe impl Send for OwnedCheckInputs {}

impl OwnedCheckInputs {
    pub(super) fn from_borrowed(
        source: &str,
        options: &CheckOptions<'_>,
        defaults: &CheckerOptions,
    ) -> Result<Self, AnalysisError> {
        let source_len = u32::try_from(source.len()).map_err(|_| AnalysisError::InputTooLarge {
            kind: "source",
            len: source.len(),
        })?;
        let module_name = options
            .module_name
            .unwrap_or(defaults.default_module_name.as_str());
        let module_name_len =
            u32::try_from(module_name.len()).map_err(|_| AnalysisError::InputTooLarge {
                kind: "module name",
                len: module_name.len(),
            })?;

        let mut virtual_module_storage = Vec::with_capacity(options.virtual_modules.len());
        for module in options.virtual_modules {
            let name_len =
                u32::try_from(module.name.len()).map_err(|_| AnalysisError::InputTooLarge {
                    kind: "virtual module name",
                    len: module.name.len(),
                })?;
            let source_len =
                u32::try_from(module.source.len()).map_err(|_| AnalysisError::InputTooLarge {
                    kind: "virtual module source",
                    len: module.source.len(),
                })?;
            virtual_module_storage.push(OwnedVirtualModule {
                name: module.name.as_bytes().to_vec().into_boxed_slice(),
                name_len,
                source: module.source.as_bytes().to_vec().into_boxed_slice(),
                source_len,
            });
        }
        let _: u32 = u32::try_from(virtual_module_storage.len()).map_err(|_| {
            AnalysisError::InputTooLarge {
                kind: "virtual modules",
                len: virtual_module_storage.len(),
            }
        })?;

        // Build the FFI entry array with pointers into the heap-stable storage above.
        let virtual_module_entries: Vec<ffi::RuauVirtualModule> = virtual_module_storage
            .iter()
            .map(|m| ffi::RuauVirtualModule {
                name: if m.name.is_empty() {
                    ptr::null()
                } else {
                    m.name.as_ptr()
                },
                name_len: m.name_len,
                source: if m.source.is_empty() {
                    ptr::null()
                } else {
                    m.source.as_ptr()
                },
                source_len: m.source_len,
            })
            .collect();

        Ok(Self {
            module_id: ModuleId::new(module_name),
            source: source.as_bytes().to_vec().into_boxed_slice(),
            source_len,
            module_name: module_name.as_bytes().to_vec().into_boxed_slice(),
            module_name_len,
            timeout: options.timeout.or(defaults.default_timeout),
            _virtual_module_storage: virtual_module_storage,
            virtual_module_entries,
        })
    }

    pub(super) fn source_ptr(&self) -> *const u8 {
        if self.source.is_empty() {
            ptr::null()
        } else {
            self.source.as_ptr()
        }
    }

    pub(super) fn source_len(&self) -> u32 {
        self.source_len
    }

    /// Builds the raw `RuauCheckOptions` value pointing into this struct's owned data.
    ///
    /// The returned struct borrows from `self`; it must not outlive `self`. The
    /// `cancellation_token` argument is the C handle obtained from a live `CancellationToken`
    /// kept alive by the caller for at least the same duration.
    pub(super) fn as_ffi(&self, cancellation_token: ffi::RuauTokenHandle) -> ffi::RuauCheckOptions {
        ffi::RuauCheckOptions {
            module_name: if self.module_name.is_empty() {
                ptr::null()
            } else {
                self.module_name.as_ptr()
            },
            module_name_len: self.module_name_len,
            has_timeout: u32::from(self.timeout.is_some()),
            timeout_seconds: self.timeout.map_or(0.0, |duration| duration.as_secs_f64()),
            cancellation_token,
            virtual_modules: if self.virtual_module_entries.is_empty() {
                ptr::null()
            } else {
                self.virtual_module_entries.as_ptr()
            },
            virtual_module_count: self.virtual_module_entries.len() as u32,
        }
    }
}

impl FfiResource for ffi::RuauCheckResult {
    unsafe fn release(self) {
        // SAFETY: Caller guarantees this value came from `ruau_checker_check`.
        unsafe { ffi::ruau_check_result_free(self) };
    }
}

impl FfiResource for ffi::RuauString {
    unsafe fn release(self) {
        // SAFETY: Caller guarantees this value came from a shim entrypoint that returns `LuauString`.
        unsafe { ffi::ruau_string_free(self) };
    }
}

impl FfiResource for ffi::RuauEntrypointSchemaResult {
    unsafe fn release(self) {
        // SAFETY: Caller guarantees this value came from `ruau_extract_entrypoint_schema`.
        unsafe { ffi::ruau_entrypoint_schema_result_free(self) };
    }
}

/// Adds the file label to definitions failures produced by the native layer.
pub(super) fn prefix_definitions_error(
    label: &str,
    result: Result<(), AnalysisError>,
) -> Result<(), AnalysisError> {
    match result {
        Err(AnalysisError::Definitions(message)) => {
            Err(AnalysisError::Definitions(format!("{label}: {message}")))
        }
        other => other,
    }
}

/// Converts raw UTF-8 bytes from C into a Rust `String`.
pub(super) fn string_from_raw(ptr: *const u8, len: u32) -> String {
    if ptr.is_null() || len == 0 {
        return String::new();
    }

    // SAFETY: `ptr` points to `len` bytes provided by the shim for this call scope.
    let bytes = unsafe { slice::from_raw_parts(ptr, len as usize) };
    String::from_utf8_lossy(bytes).into_owned()
}

/// Converts diagnostic rows owned by the shim into Rust values.
pub(super) fn collect_diagnostics(
    raw: &ffi::RuauCheckResult,
    module: &ModuleId,
) -> Vec<Diagnostic> {
    // SAFETY: `raw.diagnostics` points to `diagnostic_count` entries owned by `raw`.
    unsafe { raw_slice(raw.diagnostics, raw.diagnostic_count) }
        .iter()
        .map(|diagnostic| Diagnostic {
            module: module.clone(),
            span: SourceSpan {
                line: diagnostic.line,
                column: diagnostic.col,
                end_line: diagnostic.end_line,
                end_column: diagnostic.end_col,
            },
            severity: Severity::from_ffi(diagnostic.severity),
            message: string_from_raw(diagnostic.message, diagnostic.message_len),
        })
        .collect()
}

/// Converts entrypoint parameter rows owned by the shim into Rust values.
pub(super) fn collect_entrypoint_params(
    raw: &ffi::RuauEntrypointSchemaResult,
) -> Vec<EntrypointParam> {
    // SAFETY: `raw.params` points to `param_count` entries owned by `raw`.
    unsafe { raw_slice(raw.params, raw.param_count) }
        .iter()
        .map(|param| EntrypointParam {
            name: string_from_raw(param.name, param.name_len),
            annotation: string_from_raw(param.annotation, param.annotation_len),
            optional: param.optional != 0,
        })
        .collect()
}

/// Forms a borrowed slice from a non-owning C pointer and element count.
unsafe fn raw_slice<'a, T>(ptr: *const T, len: u32) -> &'a [T] {
    if len == 0 {
        &[]
    } else {
        debug_assert!(!ptr.is_null(), "non-empty shim slice must not be null");
        // SAFETY: The caller guarantees `ptr` is valid for `len` elements.
        unsafe { slice::from_raw_parts(ptr, len as usize) }
    }
}
