//! Static `require(...)` tracing for resolver snapshots.

use std::{result::Result as StdResult, slice};

use super::{ModuleId, ModuleResolveError, SourceSpan};

/// Literal require specifier plus source span returned by Luau tracing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequireSpecifier {
    /// Required module specifier.
    pub specifier: String,
    /// Location of the literal specifier in source.
    pub span: SourceSpan,
}

/// Returns literal require specifiers with source spans.
pub(super) fn require_specifiers(
    module: &ModuleId,
    source: &str,
) -> StdResult<Vec<RequireSpecifier>, ModuleResolveError> {
    let source_len = u32::try_from(source.len()).map_err(|_| ModuleResolveError::Parse {
        module: module.to_string(),
        message: format!("source is too large: {} bytes", source.len()),
    })?;
    // SAFETY: ruau_trace_requires accepts the source pointer and length we just validated;
    // the returned RuauRequireTraceResult owns its allocations until the guard frees them.
    let raw = unsafe { ffi::ruau_trace_requires(source.as_ptr(), source_len) };
    let guard = RequireTraceGuard(raw);
    if raw.error_len != 0 {
        return Err(ModuleResolveError::Parse {
            module: module.to_string(),
            // SAFETY: error/error_len are owned by `raw` for the guard's lifetime.
            message: unsafe { string_from_raw(raw.error, raw.error_len) },
        });
    }

    if raw.specifier_count == 0 {
        return Ok(Vec::new());
    }

    // SAFETY: specifiers/specifier_count are owned by `raw` for the guard's lifetime.
    let rows = unsafe { slice::from_raw_parts(raw.specifiers, raw.specifier_count as usize) };
    let specifiers = rows
        .iter()
        .map(|row| {
            Ok(RequireSpecifier {
                // SAFETY: row.specifier/specifier_len are owned by `raw`.
                specifier: unsafe { string_from_raw(row.specifier, row.specifier_len) },
                span: SourceSpan {
                    line: row.line,
                    column: row.col,
                    end_line: row.end_line,
                    end_column: row.end_col,
                },
            })
        })
        .collect::<StdResult<Vec<_>, ModuleResolveError>>();
    drop(guard);
    specifiers
}

/// Returns direct string-literal `require(...)` specifiers discovered in `source`.
///
/// Comments, strings, and dynamic require expressions are ignored. The returned specifiers are in
/// source order and are not resolved relative to `module`.
pub fn required_specifiers(
    module: &ModuleId,
    source: &str,
) -> StdResult<Vec<String>, ModuleResolveError> {
    require_specifiers(module, source).map(|specifiers| {
        specifiers
            .into_iter()
            .map(|specifier| specifier.specifier)
            .collect()
    })
}

/// Returns direct string-literal `require(...)` specifiers and source spans discovered in `source`.
///
/// Comments, strings, and dynamic require expressions are ignored. The returned specifiers are in
/// source order and are not resolved relative to `module`.
pub fn required_specifiers_with_spans(
    module: &ModuleId,
    source: &str,
) -> StdResult<Vec<RequireSpecifier>, ModuleResolveError> {
    require_specifiers(module, source)
}

/// Frees a raw Luau require tracing result on drop.
struct RequireTraceGuard(ffi::RuauRequireTraceResult);

impl Drop for RequireTraceGuard {
    fn drop(&mut self) {
        // SAFETY: `self.0` originated from `ruau_trace_requires` and must be released exactly
        // once via the matching free function.
        unsafe { ffi::ruau_require_trace_result_free(self.0) };
    }
}

/// Converts a raw UTF-8-ish byte range from Luau tracing into an owned string.
///
/// # Safety
///
/// `data` must point to `len` valid bytes for the call duration.
unsafe fn string_from_raw(data: *const u8, len: u32) -> String {
    String::from_utf8_lossy(slice::from_raw_parts(data, len as usize)).into_owned()
}
