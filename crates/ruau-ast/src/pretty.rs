//! Luau pretty-printing.
//!
//! This module owns source-to-source Luau emission. The current implementation
//! is a CST-preserving bootstrap: it keeps parseable source text stable and
//! blanks typed-only syntax when callers request untyped output.

use crate::parse::{Error, Options as ParseOptions, SyntaxFlags, parse_file_with};

/// Options for source-to-source pretty printing.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub struct Options {
    /// Parser options used before printing.
    pub parse_options: ParseOptions,
    /// Parser-visible syntax flags used before printing.
    pub syntax_flags: SyntaxFlags,
    /// Preserve typed Luau syntax when true.
    pub with_types: bool,
    /// Return code even when parsing reports recoverable errors.
    pub ignore_parse_errors: bool,
}

/// Pretty-prints Luau source.
///
/// # Errors
/// Returns the parse errors when the source does not parse and
/// `ignore_parse_errors` is unset. Each error renders "line:col: message"
/// via its `Display` impl.
pub fn pretty_print_source(source: &str, mut options: Options) -> Result<String, Vec<Error>> {
    options.parse_options.store_cst_data = true;

    let parsed = parse_file_with(source, options.parse_options, options.syntax_flags);
    if !parsed.errors.is_empty() && !options.ignore_parse_errors {
        return Err(parsed.errors);
    }

    Ok(if options.with_types {
        source.to_owned()
    } else {
        strip_untyped_syntax(source)
    })
}

fn strip_untyped_syntax(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut output = bytes.to_vec();
    let mut index = 0;

    while index < bytes.len() {
        if let Some(next) = skip_comment_or_string(bytes, index) {
            index = next;
        } else if starts_with(bytes, index, b"::") {
            let start = trim_horizontal_space_before(bytes, index);
            let end = type_suffix_end(bytes, index + 2);
            blank_ascii_range(&mut output, start, end);
            index = end;
        } else if bytes.get(index) == Some(&b':') && !starts_with(bytes, index + 1, b":") {
            if let Some(end) = annotation_end_before_assignment(bytes, index + 1) {
                blank_ascii_range(&mut output, index, end);
                index = end;
            } else {
                index += 1;
            }
        } else if starts_with(bytes, index, b"<<") {
            if let Some(end) = explicit_type_instantiation_end(bytes, index + 2) {
                blank_ascii_range(&mut output, index, end);
                index = end;
            } else {
                index += 1;
            }
        } else {
            index += 1;
        }
    }

    String::from_utf8(output).expect("ASCII blanking preserves valid UTF-8")
}

fn blank_ascii_range(output: &mut [u8], start: usize, end: usize) {
    if let Some(slice) = output.get_mut(start..end) {
        for byte in slice {
            if !matches!(*byte, b'\n' | b'\r') {
                *byte = b' ';
            }
        }
    }
}

fn skip_comment_or_string(bytes: &[u8], index: usize) -> Option<usize> {
    match bytes.get(index).copied()? {
        b'\'' | b'"' => Some(quoted_string_end(bytes, index + 1, bytes[index])),
        b'-' if starts_with(bytes, index + 1, b"-") => Some(comment_end(bytes, index + 2)),
        b'[' => long_bracket_end(bytes, index).map(|end| end.max(index + 1)),
        _ => None,
    }
}

fn quoted_string_end(bytes: &[u8], mut index: usize, quote: u8) -> usize {
    while index < bytes.len() {
        match bytes.get(index) {
            Some(&b'\\') => index = (index + 2).min(bytes.len()),
            Some(&q) if q == quote => return index + 1,
            _ => index += 1,
        }
    }
    index
}

fn comment_end(bytes: &[u8], index: usize) -> usize {
    if let Some(end) = long_bracket_end(bytes, index) {
        return end;
    }

    bytes
        .get(index..)
        .and_then(|slice| slice.iter().position(|&byte| byte == b'\n'))
        .map(|offset| index + offset)
        .unwrap_or(bytes.len())
}

fn long_bracket_end(bytes: &[u8], index: usize) -> Option<usize> {
    let equals = long_bracket_equals(bytes, index)?;
    let content_start = index + 2 + equals;
    let mut cursor = content_start;
    while cursor < bytes.len() {
        if bytes.get(cursor) == Some(&b']')
            && bytes
                .get(cursor + 1..cursor + 1 + equals)
                .is_some_and(|slice| slice.iter().all(|&byte| byte == b'='))
            && bytes.get(cursor + 1 + equals) == Some(&b']')
        {
            return Some(cursor + 2 + equals);
        }
        cursor += 1;
    }
    Some(bytes.len())
}

fn long_bracket_equals(bytes: &[u8], index: usize) -> Option<usize> {
    if bytes.get(index) != Some(&b'[') {
        return None;
    }

    let mut cursor = index + 1;
    while bytes.get(cursor) == Some(&b'=') {
        cursor += 1;
    }
    (bytes.get(cursor) == Some(&b'[')).then_some(cursor - index - 1)
}

fn trim_horizontal_space_before(bytes: &[u8], index: usize) -> usize {
    let mut start = index;
    while start > 0
        && bytes
            .get(start - 1)
            .is_some_and(|&b| matches!(b, b' ' | b'\t'))
    {
        start -= 1;
    }
    start
}

fn find_matching_generic_gt(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth = 1usize;
    let mut index = start;
    while index < bytes.len() {
        if let Some(next) = skip_comment_or_string(bytes, index) {
            index = next;
            continue;
        }
        match bytes.get(index).copied() {
            Some(b'(' | b'[' | b'{' | b'<') => depth += 1,
            Some(b')' | b']' | b'}' | b'>') => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            Some(b';') if depth == 1 => return None,
            _ => {}
        }
        if depth == 1
            && (starts_with(bytes, index, b"==")
                || starts_with(bytes, index, b">=")
                || starts_with(bytes, index, b"<=")
                || starts_with(bytes, index, b"~="))
        {
            return None;
        }
        index += 1;
    }
    None
}

fn type_suffix_end(bytes: &[u8], mut index: usize) -> usize {
    let mut depth = 0usize;
    while index < bytes.len() {
        if let Some(next) = skip_comment_or_string(bytes, index) {
            index = next;
            continue;
        }
        if depth == 0 && (bytes.get(index) == Some(&b'<') || bytes.get(index) == Some(&b'>')) {
            let gt_index = (bytes.get(index) == Some(&b'<'))
                .then(|| find_matching_generic_gt(bytes, index + 1))
                .flatten();
            if let Some(gt_index) = gt_index {
                index = gt_index + 1;
                continue;
            }
            break;
        }
        match bytes.get(index).copied() {
            Some(b'(' | b'[' | b'{') => depth += 1,
            Some(b')' | b']' | b'}') if depth > 0 => depth -= 1,
            Some(
                b',' | b';' | b'=' | b'+' | b'-' | b'*' | b'/' | b'%' | b'^' | b')' | b']' | b'}',
            ) if depth == 0 => {
                break;
            }
            _ => {}
        }
        index += 1;
    }
    index
}

fn annotation_end_before_assignment(bytes: &[u8], index: usize) -> Option<usize> {
    let end = type_suffix_end(bytes, index);
    (bytes.get(end) == Some(&b'=')).then_some(end)
}

fn explicit_type_instantiation_end(bytes: &[u8], mut index: usize) -> Option<usize> {
    let mut depth = 1usize;
    while index < bytes.len() {
        if let Some(next) = skip_comment_or_string(bytes, index) {
            index = next;
            continue;
        }
        if starts_with(bytes, index, b"<<") {
            depth += 1;
            index += 2;
        } else if starts_with(bytes, index, b">>") {
            depth -= 1;
            index += 2;
            if depth == 0 {
                return Some(index);
            }
        } else {
            index += 1;
        }
    }
    None
}

fn starts_with(bytes: &[u8], index: usize, needle: &[u8]) -> bool {
    bytes
        .get(index..)
        .is_some_and(|slice| slice.starts_with(needle))
}

#[cfg(any())]
mod tests {
    use super::{Options, pretty_print_source};

    #[test]
    fn preserves_parseable_source_in_bootstrap_slice() {
        let source = " local x = 1 ";

        let printed = pretty_print_source(source, Options::default());

        assert_eq!(printed.expect("parses"), source);
    }

    #[test]
    fn reports_parse_error_when_not_ignored() {
        let printed = pretty_print_source("local x =", Options::default());

        let errors = printed.expect_err("parse error reported");
        assert!(!errors.is_empty());
        // The Display rendering carries location and message.
        assert!(errors[0].to_string().contains(':'));
    }

    #[test]
    fn strips_types_from_untyped_output_while_preserving_columns() {
        let printed = pretty_print_source(
            " local s: string= f<<A, B>>() :: any+ g() :: number ",
            Options::default(),
        );

        assert_eq!(
            printed.expect("parses"),
            " local s        = f        ()       + g()           "
        );
    }

    #[test]
    fn strips_types_with_operators_correctly() {
        let printed = pretty_print_source("local x = y :: A > z", Options::default());
        assert_eq!(printed.expect("parses"), "local x = y      > z");

        let printed = pretty_print_source(
            "local x = y :: A < z",
            Options {
                ignore_parse_errors: true,
                ..Options::default()
            },
        );
        assert_eq!(printed.expect("parses"), "local x = y      < z");
    }

    #[test]
    fn strips_types_with_strings_containing_equals_correctly() {
        let printed = pretty_print_source("local x: \"foo=bar\" = \"foo=bar\"", Options::default());
        assert_eq!(printed.expect("parses"), "local x            = \"foo=bar\"");
    }

    #[test]
    fn strips_types_with_comments_correctly() {
        let printed = pretty_print_source(
            "local s: { a: string } -- comment\n = f()",
            Options::default(),
        );
        assert_eq!(
            printed.expect("parses"),
            "local s                          \n = f()"
        );
    }
}
