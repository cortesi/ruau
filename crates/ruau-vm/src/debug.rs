//! Runtime error locations (port `ldebug.cpp`).
//!
//! A runtime error reports where it was raised as a `source:line:` prefix, the
//! way `luaG_runerror` does: the chunk name the module was loaded under and the
//! source line of the failing instruction. The line comes from the prototype's
//! decoded line table (built at load from the chunk's line info); the chunk name
//! is the prototype's shared `source`. The prefix is attached once, at the
//! innermost frame, by the enclosing protected boundary — [`locate`].

use crate::{
    api::{RawGc, marker},
    call::{ErrorPayload, RaisedError},
    heap::Heap,
    state::{CallStackEntry, Thread},
};

/// The chunk-name fallback when a frame's prototype carries none (it always does
/// after a normal load; this guards a hand-built or native frame). Already in
/// display form (no `chunk_id` needed), matching upstream's `[C]` short source.
const UNKNOWN_CHUNK: &[u8] = b"[C]";

/// The marker class carried by a Luau chunk name.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ChunkNameKind {
    /// `@path`: a file/path-like chunk name. Its display form strips `@`.
    File,
    /// `=name`: an explicit display name. Its display form strips `=`.
    Named,
    /// Bare source bytes. Its display form is `[string "..."]`.
    Source,
}

/// An owned Luau chunk name.
///
/// Luau chunk names are raw bytes with an optional leading marker:
///
/// - `@path` displays as `path`
/// - `=name` displays as `name`
/// - bare bytes display as `[string "..."]`
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ChunkName {
    raw: Vec<u8>,
}

impl ChunkName {
    /// Wraps already-markerized raw chunk-name bytes.
    #[must_use]
    pub fn from_raw(raw: impl AsRef<[u8]>) -> Self {
        Self {
            raw: raw.as_ref().to_vec(),
        }
    }

    /// Constructs an `@path` chunk name.
    #[must_use]
    pub fn file(path: impl AsRef<[u8]>) -> Self {
        let path = path.as_ref();
        let mut raw = Vec::with_capacity(path.len() + 1);
        raw.push(b'@');
        raw.extend_from_slice(path);
        Self { raw }
    }

    /// Constructs a `=name` chunk name.
    #[must_use]
    pub fn named(name: impl AsRef<[u8]>) -> Self {
        let name = name.as_ref();
        let mut raw = Vec::with_capacity(name.len() + 1);
        raw.push(b'=');
        raw.extend_from_slice(name);
        Self { raw }
    }

    /// Constructs a bare-source chunk name.
    #[must_use]
    pub fn source(source: impl AsRef<[u8]>) -> Self {
        Self {
            raw: source.as_ref().to_vec(),
        }
    }

    /// Borrows this chunk name as a typed view.
    #[must_use]
    pub fn as_chunk_name_ref(&self) -> ChunkNameRef<'_> {
        ChunkNameRef::from_raw(&self.raw)
    }

    /// Raw chunk-name bytes, including any leading marker.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.raw
    }

    /// Consumes this chunk name into its raw bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.raw
    }

    /// The marker class of this chunk name.
    #[must_use]
    pub fn kind(&self) -> ChunkNameKind {
        self.as_chunk_name_ref().kind()
    }

    /// The bytes after the marker for `@`/`=` names; bare source bytes unchanged.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        self.as_chunk_name_ref().payload()
    }

    /// The display bytes produced by Luau's `luaO_chunkid` convention.
    #[must_use]
    pub fn display_bytes(&self) -> Vec<u8> {
        self.as_chunk_name_ref().display_bytes()
    }

    /// The display string produced by Luau's `luaO_chunkid` convention.
    #[must_use]
    pub fn display_string(&self) -> String {
        self.as_chunk_name_ref().display_string()
    }
}

impl AsRef<[u8]> for ChunkName {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

/// A borrowed Luau chunk name.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ChunkNameRef<'a> {
    raw: &'a [u8],
}

impl<'a> ChunkNameRef<'a> {
    /// Parses already-markerized raw chunk-name bytes.
    #[must_use]
    pub fn from_raw(raw: &'a [u8]) -> Self {
        Self { raw }
    }

    /// Parses already-markerized raw chunk-name bytes.
    #[must_use]
    pub fn parse(raw: &'a [u8]) -> Self {
        Self::from_raw(raw)
    }

    /// Raw chunk-name bytes, including any leading marker.
    #[must_use]
    pub fn as_bytes(self) -> &'a [u8] {
        self.raw
    }

    /// The marker class of this chunk name.
    #[must_use]
    pub fn kind(self) -> ChunkNameKind {
        match self.raw.first() {
            Some(b'@') => ChunkNameKind::File,
            Some(b'=') => ChunkNameKind::Named,
            _ => ChunkNameKind::Source,
        }
    }

    /// The bytes after the marker for `@`/`=` names; bare source bytes unchanged.
    #[must_use]
    pub fn payload(self) -> &'a [u8] {
        match self.kind() {
            ChunkNameKind::File | ChunkNameKind::Named => &self.raw[1..],
            ChunkNameKind::Source => self.raw,
        }
    }

    /// The UTF-8 payload after the marker for `@`/`=` names; bare source bytes
    /// unchanged. Returns `None` for non-UTF-8 chunk names.
    #[must_use]
    pub fn payload_str(self) -> Option<&'a str> {
        std::str::from_utf8(self.payload()).ok()
    }

    /// The display bytes produced by Luau's `luaO_chunkid` convention.
    #[must_use]
    pub fn display_bytes(self) -> Vec<u8> {
        chunk_id(self.raw)
    }

    /// The display string produced by Luau's `luaO_chunkid` convention.
    #[must_use]
    pub fn display_string(self) -> String {
        String::from_utf8_lossy(&self.display_bytes()).into_owned()
    }
}

impl AsRef<[u8]> for ChunkNameRef<'_> {
    fn as_ref(&self) -> &[u8] {
        self.raw
    }
}

/// Formats a raw chunk name for display in an error location or a `debug` query,
/// like upstream `luaO_chunkid` (`lobject.cpp`): a `=name`/`@name` shows `name`,
/// while a bare source string shows `[string "<first line>"]` (truncated with an
/// ellipsis when it runs long or spans more than one line). The raw name is what a
/// module is loaded under and what the prototype stores; the formatted form is
/// what the location prefix and `debug.info`'s short source report.
#[must_use]
pub fn chunk_id(source: &[u8]) -> Vec<u8> {
    // The `[string "…"]` wrapper occupies a fixed number of bytes; the first line is
    // truncated so the whole id stays within upstream's `LUA_IDSIZE` budget.
    const LUA_IDSIZE: usize = 256;
    const MAX_MARKED: usize = LUA_IDSIZE - 1;
    const MAX_PATH_TAIL: usize = LUA_IDSIZE - 4;
    const MAX_BODY: usize = LUA_IDSIZE - b"[string \"...\"]".len() - 1;
    match source.first() {
        Some(b'=') => {
            if source.len() <= LUA_IDSIZE {
                source[1..].to_vec()
            } else {
                source[1..][..MAX_MARKED].to_vec()
            }
        }
        Some(b'@') => {
            if source.len() <= LUA_IDSIZE {
                source[1..].to_vec()
            } else {
                let mut out = b"...".to_vec();
                out.extend_from_slice(&source[source.len() - MAX_PATH_TAIL..]);
                out
            }
        }
        _ => {
            let first_line = source
                .iter()
                .position(|&b| b == b'\n' || b == b'\r')
                .unwrap_or(source.len());
            let truncated = first_line > MAX_BODY;
            let body = &source[..first_line.min(MAX_BODY)];
            let mut out = b"[string \"".to_vec();
            out.extend_from_slice(body);
            // Upstream appends "..." when the source was cut short — either the first
            // line was truncated or there is more text after it.
            if truncated || first_line < source.len() {
                out.extend_from_slice(b"...");
            }
            out.extend_from_slice(b"\"]");
            out
        }
    }
}

/// A script source position, surfaced to the host: the chunk name a frame's
/// module was loaded under (in `luaO_chunkid` display form — a `=name`/`@name`
/// marker shows `name`, a bare source shows `[string "…"]`) and the 1-based
/// source line. Chunk names are decoded lossily; a non-UTF-8 name shows
/// replacement characters.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SourceLocation {
    /// The chunk name in display form, matching error locations and
    /// `debug.info`'s short source.
    pub chunk_name: String,
    /// The 1-based source line.
    pub line: u32,
}

/// Resolves the `level`-th Lua frame of `thread`, innermost first, to its
/// current source position. Only executable Lua frames count as levels —
/// `pcall`/`require` boundaries and native (builtin/host) activations occupy no
/// Lua frame and are skipped. Returns `None` past the stack top or when the
/// frame's prototype carries no line info.
pub fn caller_location(heap: &Heap, thread: &Thread, level: usize) -> Option<SourceLocation> {
    let frame = thread
        .call_stack
        .iter()
        .rev()
        .filter_map(CallStackEntry::frame)
        .nth(level)?;
    let proto = heap.proto(heap.closure(frame.closure)?.proto)?;
    // `savedpc` is the resume pc — one past the executing call instruction — so
    // the frame's current line is at `savedpc - 1` (the `debug.info` convention).
    let line = proto.line(frame.savedpc.saturating_sub(1))?;
    let chunk_name = proto
        .source
        .and_then(|source| heap.string(source))
        .map_or_else(|| UNKNOWN_CHUNK.to_vec(), |string| chunk_id(string.bytes()));
    Some(SourceLocation {
        chunk_name: String::from_utf8_lossy(&chunk_name).into_owned(),
        line,
    })
}

/// Prefixes `error` with its requested frame's `source:line:` location, unless it
/// is already located or no line can be resolved (a frame with no line info, or a
/// native builtin frame). Marks the error located so an outer boundary does not
/// prefix it a second time.
#[must_use]
pub fn locate(heap: &Heap, thread: &Thread, mut error: RaisedError) -> RaisedError {
    if error.located {
        return error;
    }
    // Only a message payload is prefixed; a thrown value surfaces unchanged.
    let ErrorPayload::Message(message) = &error.payload else {
        return error;
    };
    let frame_offset = error.location_level - 1;
    // Count every call-stack entry as an activation level, including `pcall`/
    // `xpcall` protected boundaries. Upstream's `luaL_where` counts C activations
    // too and yields an empty location for them, so a level that lands on a
    // protected boundary resolves to no location (its `frame()` is `None`) rather
    // than skipping outward to the next Lua frame.
    if let Some(location) = thread
        .call_stack
        .iter()
        .rev()
        .nth(frame_offset)
        .and_then(CallStackEntry::frame)
        .and_then(|frame| {
            let pc = if frame_offset == 0 {
                frame.savedpc
            } else {
                frame.savedpc.saturating_sub(1)
            };
            frame_location(heap, frame.closure, pc)
        })
    {
        error.payload = ErrorPayload::Message(format!("{location}: {message}"));
    }
    error.located = true;
    error
}

/// One frame of a captured stack traceback: the structured form of one line of
/// the rendered traceback text. Frames carry the same display-form values the
/// text shows — the chunk name in `luaO_chunkid` display form (as on
/// [`SourceLocation`]), the 1-based source line when the prototype has line
/// info, and the function's debug name when it has one — so an embedder can
/// map them to its own location types without parsing the text.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct TracebackFrame {
    /// The chunk name in display form, matching error locations and
    /// `debug.info`'s short source.
    pub chunk_name: String,
    /// The 1-based source line of the frame's current instruction, when the
    /// prototype carries line info.
    pub line: Option<u32>,
    /// The function's debug name, when the prototype carries one (the main
    /// chunk and anonymous functions do not).
    pub function_name: Option<String>,
}

impl TracebackFrame {
    /// The chunk name in display form.
    #[must_use]
    pub fn chunk_name(&self) -> &str {
        &self.chunk_name
    }

    /// The 1-based source line of the frame's current instruction, when known.
    #[must_use]
    pub fn line_number(&self) -> Option<u32> {
        self.line
    }

    /// The function's debug name, when known.
    #[must_use]
    pub fn function_name(&self) -> Option<&str> {
        self.function_name.as_deref()
    }

    /// Renders this frame as one line of traceback text:
    /// `chunk_name[:line][ function name]`.
    fn render(&self) -> String {
        let mut out = self.chunk_name.clone();
        if let Some(line) = self.line {
            out.push(':');
            out.push_str(&line.to_string());
        }
        if let Some(name) = &self.function_name {
            out.push_str(" function ");
            out.push_str(name);
        }
        out
    }
}

/// Returns the frame most embedders should attribute a script failure to.
///
/// Tracebacks are innermost-first. Prefer the first frame with source-line
/// information, falling back to the first collected frame for chunks without
/// line tables.
#[must_use]
pub fn primary_user_frame(frames: &[TracebackFrame]) -> Option<&TracebackFrame> {
    frames
        .iter()
        .find(|frame| frame.line.is_some())
        .or_else(|| frames.first())
}

/// A captured stack traceback: the structured frames and the text rendered
/// from them, both produced by [`traceback`] under one byte budget. The text
/// is the embedder-visible traceback string; the frames are its structured
/// form, and the text is derived from them line by line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Traceback {
    frames: Vec<TracebackFrame>,
    truncated: bool,
    text: String,
}

impl Traceback {
    /// The rendered traceback text, one line per frame, innermost first.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Consumes the capture into its structured frames and whether the byte
    /// budget cut frame collection short.
    pub fn into_frames(self) -> (Vec<TracebackFrame>, bool) {
        (self.frames, self.truncated)
    }
}

/// Re-pairs a protected failure's rendered traceback text with the structured
/// capture stashed by the same unwind.
///
/// A stale capture from another protected boundary is ignored unless its
/// rendered text exactly matches the failure text.
pub fn frames_for_traceback(
    traceback: Option<&str>,
    capture: Option<Traceback>,
) -> (Vec<TracebackFrame>, bool) {
    match capture {
        Some(capture) if traceback == Some(capture.text()) => capture.into_frames(),
        _ => (Vec::new(), false),
    }
}

/// Captures a compact stack traceback for the current thread, before a protected
/// boundary unwinds its failing frames.
///
/// `max_bytes` budgets the rendered text and the structured frames at once: a
/// frame is collected only when its fully rendered line (plus its newline
/// separator) still fits the budget. When a frame's line does not fit, the
/// text keeps the byte-budgeted prefix of that line — the historical rendered
/// form — while the partially rendered frame is dropped from the structured
/// frames and the capture is marked truncated, so the frame list never holds a
/// frame whose data was cut mid-line.
#[must_use]
pub fn traceback(heap: &Heap, thread: &Thread, max_bytes: usize) -> Option<Traceback> {
    if max_bytes == 0 {
        return None;
    }
    let mut frames = Vec::new();
    let mut truncated = false;
    let mut text = String::new();
    for frame in thread
        .call_stack
        .iter()
        .rev()
        .filter_map(CallStackEntry::frame)
    {
        let Some(tb_frame) = traceback_frame(heap, frame.closure, frame.savedpc) else {
            continue;
        };
        let mut line = tb_frame.render();
        let separator = usize::from(!text.is_empty());
        if text
            .len()
            .saturating_add(separator)
            .saturating_add(line.len())
            > max_bytes
        {
            truncated = true;
            let mut remaining = max_bytes.saturating_sub(text.len());
            if remaining == 0 {
                break;
            }
            if !text.is_empty() {
                text.push('\n');
                remaining = remaining.saturating_sub(1);
            }
            while remaining > 0 && !line.is_char_boundary(remaining) {
                remaining -= 1;
            }
            line.truncate(remaining);
            text.push_str(&line);
            break;
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&line);
        frames.push(tb_frame);
    }
    if text.is_empty() {
        None
    } else {
        Some(Traceback {
            frames,
            truncated,
            text,
        })
    }
}

fn traceback_frame(
    heap: &Heap,
    closure: RawGc<marker::Closure>,
    savedpc: usize,
) -> Option<TracebackFrame> {
    let proto_handle = heap.closure(closure)?.proto;
    let proto = heap.proto(proto_handle)?;
    let source = proto
        .source
        .and_then(|source| heap.string(source).map(|source| chunk_id(source.bytes())))
        .unwrap_or_else(|| UNKNOWN_CHUNK.to_vec());
    let line = if proto.has_line_info() {
        proto.line(savedpc.saturating_sub(1))
    } else {
        None
    };
    let function_name = proto
        .debug_name
        .and_then(|name| heap.string(name))
        .map(|name| String::from_utf8_lossy(name.bytes()).into_owned());
    Some(TracebackFrame {
        chunk_name: String::from_utf8_lossy(&source).into_owned(),
        line,
        function_name,
    })
}

/// The `source:line` location of program counter `pc` in `closure`'s prototype,
/// or `None` when the prototype has no line for that pc (no line info, or a
/// native builtin with no code).
fn frame_location(heap: &Heap, closure: RawGc<marker::Closure>, pc: usize) -> Option<String> {
    let proto = heap.closure(closure)?.proto;
    let proto = heap.proto(proto)?;
    let line = proto.line(pc)?;
    let name = proto
        .source
        .and_then(|source| heap.string(source))
        .map_or_else(|| UNKNOWN_CHUNK.to_vec(), |string| chunk_id(string.bytes()));
    Some(format!("{}:{line}", String::from_utf8_lossy(&name)))
}

#[cfg(any())]
mod tests {
    use super::{ChunkName, ChunkNameKind, ChunkNameRef, chunk_id};

    #[test]
    fn chunk_name_constructors_and_parser_cover_marker_forms() {
        let file = ChunkName::file("src/main.luau");
        assert_eq!(file.as_bytes(), b"@src/main.luau");
        assert_eq!(file.kind(), ChunkNameKind::File);
        assert_eq!(file.payload(), b"src/main.luau");
        assert_eq!(file.display_string(), "src/main.luau");

        let named = ChunkName::named("setup");
        assert_eq!(named.as_bytes(), b"=setup");
        assert_eq!(named.kind(), ChunkNameKind::Named);
        assert_eq!(named.payload(), b"setup");
        assert_eq!(named.display_string(), "setup");

        let source = ChunkName::source("line one\nline two");
        assert_eq!(source.kind(), ChunkNameKind::Source);
        assert_eq!(source.payload(), b"line one\nline two");
        assert_eq!(source.display_string(), "[string \"line one...\"]");

        let parsed = ChunkNameRef::parse(b"@already/raw");
        assert_eq!(parsed.kind(), ChunkNameKind::File);
        assert_eq!(parsed.payload_str(), Some("already/raw"));
        assert_eq!(parsed.display_string(), "already/raw");
    }

    #[test]
    fn chunk_id_matches_luao_chunkid() {
        // `=`/`@` markers are stripped; the placeholder `=?` shows `?`.
        assert_eq!(chunk_id(b"=basic.luau"), b"basic.luau");
        assert_eq!(chunk_id(b"@/path/to/file.luau"), b"/path/to/file.luau");
        assert_eq!(chunk_id(b"=?"), b"?");
        let repeated =
            b"thisisaverylongstringitssolongthatitwontfitintotheinternalbufferprovidedtovariousdebugfacilities"
                .repeat(10);
        let mut file_source = b"@".to_vec();
        file_source.extend_from_slice(&repeated);
        let mut expected_file = b"...".to_vec();
        expected_file.extend_from_slice(&repeated[repeated.len() - (256 - 4)..]);
        assert_eq!(chunk_id(&file_source), expected_file);
        let mut custom_source = b"=".to_vec();
        custom_source.extend_from_slice(&repeated);
        assert_eq!(chunk_id(&custom_source), repeated[..255]);
        // A bare source is wrapped; a single short line gets no ellipsis.
        assert_eq!(chunk_id(b"hello world"), b"[string \"hello world\"]");
        // More than one line truncates at the first newline with an ellipsis.
        assert_eq!(chunk_id(b"line one\nline two"), b"[string \"line one...\"]");
        // An over-long first line is cut to the id budget with an ellipsis.
        let long = vec![b'x'; 500];
        let id = chunk_id(&long);
        assert!(id.starts_with(b"[string \"") && id.ends_with(b"...\"]"));
        assert!(id.len() < 300);
    }
}
