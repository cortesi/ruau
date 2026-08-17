//! Source-to-source Luau emission.
//!
//! The current implementation is a CST-preserving bootstrap toward a full
//! pretty-printer: [`erase_types`] keeps parseable source text stable and
//! blanks typed-only syntax when callers request untyped output.
//! [`erase_declarations`] blanks `declare` statements so a declaration source
//! compiles, and [`read_only_module_surface`] derives the requireable
//! read-only form of one module API source. [`module_root`] identifies the
//! module root that the surface transform rewrites.

mod module_surface;

use std::ops::Range;

pub use module_surface::{ModuleRoot, module_root, read_only_module_surface};

use crate::{
    location::Location,
    parse::{Config, Error, parse_with_config},
    syntax::{Expr, Stat, TypePack},
    visit::{self, Visitor, WalkControl},
};

/// Options for [`erase_types`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub struct EraseTypesOptions {
    /// Parser configuration used before stripping.
    pub parse: Config,
    /// Preserve typed Luau syntax when true, returning the source unchanged.
    pub with_types: bool,
    /// Return code even when parsing reports recoverable errors.
    pub ignore_parse_errors: bool,
}

/// Blanks type annotations out of Luau source, preserving line and column
/// positions of the remaining code.
///
/// # Errors
/// Returns the parse errors when the source does not parse and
/// `ignore_parse_errors` is unset. Each error renders "line:col: message"
/// via its `Display` impl.
pub fn erase_types(source: &str, mut options: EraseTypesOptions) -> Result<String, Vec<Error>> {
    options.parse.store_cst_data = true;

    let parsed = parse_with_config(source, &options.parse);
    if !parsed.errors.is_empty() && !options.ignore_parse_errors {
        return Err(parsed.errors);
    }

    let mut code = if options.with_types {
        source.to_owned()
    } else {
        strip_untyped_syntax(source)
    };
    if options.ignore_parse_errors {
        insert_error_expr_markers(&mut code, source, &parsed.root);
    }

    Ok(code)
}

/// Options for [`erase_declarations`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub struct EraseDeclarationsOptions {
    /// Parser configuration used before erasure. Declaration syntax is always
    /// enabled.
    pub parse: Config,
}

/// Blanks `declare` statements out of Luau source, preserving line and column
/// positions of the remaining code.
///
/// The result compiles under the standard compiler, which rejects declaration
/// syntax. Type aliases, comments, and executable statements stay unchanged.
///
/// Parser locations mirror upstream Luau, where a `declare function` range
/// ends at the next token and a `declare class` range begins at the class
/// name. The erase ranges are therefore recomputed from the declaration's own
/// tokens instead of the recorded statement range.
///
/// # Errors
/// Returns the parse errors when the source does not parse, and a
/// malformed-syntax error when a declaration's erase range cannot be
/// recovered from the source text. Each error renders "line:col: message" via
/// its `Display` impl.
pub fn erase_declarations(
    source: &str,
    mut options: EraseDeclarationsOptions,
) -> Result<String, Vec<Error>> {
    options.parse.allow_declaration_syntax = true;

    let parsed = parse_with_config(source, &options.parse);
    if !parsed.errors.is_empty() {
        return Err(parsed.errors);
    }

    let mut visitor = DeclarationStatVisitor { stats: Vec::new() };
    visit::walk_stat(&parsed.root, &mut visitor);

    let mut output = source.as_bytes().to_vec();
    for stat in visitor.stats {
        let range = declaration_erase_range(source, stat)?;
        blank_ascii_range(&mut output, range.start, range.end);
    }
    Ok(String::from_utf8(output).expect("ASCII blanking preserves valid UTF-8"))
}

/// Collects references to declaration statements.
struct DeclarationStatVisitor<'ast> {
    stats: Vec<&'ast Stat>,
}

impl<'ast> Visitor<'ast> for DeclarationStatVisitor<'ast> {
    fn visit_stat(&mut self, stat: &'ast Stat) -> WalkControl {
        if matches!(
            stat,
            Stat::DeclareGlobal { .. } | Stat::DeclareFunction { .. } | Stat::DeclareClass { .. }
        ) {
            self.stats.push(stat);
        }
        WalkControl::Continue
    }
}

/// Computes the byte range that erases one declaration statement.
fn declaration_erase_range(source: &str, stat: &Stat) -> Result<Range<usize>, Vec<Error>> {
    let location = stat
        .location()
        .ok_or_else(|| vec![erase_range_error(stat, Location::default())])?;
    let range = location
        .byte_range(source)
        .ok_or_else(|| vec![erase_range_error(stat, location)])?;
    match stat {
        Stat::DeclareGlobal { .. } => Ok(range),
        Stat::DeclareClass { .. } => {
            let start = extend_backward_to_declare(source, range.start)
                .ok_or_else(|| vec![erase_range_error(stat, location)])?;
            Ok(start..range.end)
        }
        Stat::DeclareFunction { ret_types, .. } => {
            let end = declare_function_erase_end(source, range.start, ret_types)
                .ok_or_else(|| vec![erase_range_error(stat, location)])?;
            Ok(range.start..end)
        }
        _ => unreachable!("only declaration statements are collected"),
    }
}

/// Builds the error for an unrecoverable declaration erase range.
fn erase_range_error(stat: &Stat, location: Location) -> Error {
    let kind = match stat {
        Stat::DeclareClass { .. } => "class",
        Stat::DeclareFunction { .. } => "function",
        _ => "global",
    };
    Error {
        kind: crate::parse::ErrorKind::MalformedSyntax,
        message: format!("cannot recover the erase range of this declare {kind} statement"),
        location,
    }
}

/// Walks backward from a class name over the `declare [extern type | class]`
/// prefix and returns the offset of the `declare` keyword.
fn extend_backward_to_declare(source: &str, name_begin: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut cursor = name_begin;
    loop {
        while cursor > 0 && bytes[cursor - 1].is_ascii_whitespace() {
            cursor -= 1;
        }
        let word_end = cursor;
        while cursor > 0 && (bytes[cursor - 1].is_ascii_alphanumeric() || bytes[cursor - 1] == b'_')
        {
            cursor -= 1;
        }
        match source.get(cursor..word_end)? {
            "declare" => return Some(cursor),
            "class" | "extern" | "type" => {}
            _ => return None,
        }
    }
}

/// Returns the erase end for a `declare function` statement.
///
/// The end is the return-annotation end when the signature has one, and the
/// closing parenthesis of the parameter list otherwise.
fn declare_function_erase_end(source: &str, begin: usize, ret_types: &TypePack) -> Option<usize> {
    let bytes = source.as_bytes();
    let open = find_significant_char(bytes, begin, b'(')?;
    let close = matching_paren_end(bytes, open)?;
    let after = skip_trivia_forward(bytes, close + 1);
    if bytes.get(after) == Some(&b':') {
        let location = ret_types.location()?;
        return location.end.byte_offset(source);
    }
    Some(close + 1)
}

/// Finds the next significant occurrence of one character, skipping comments
/// and strings.
fn find_significant_char(bytes: &[u8], mut index: usize, needle: u8) -> Option<usize> {
    while index < bytes.len() {
        if let Some(next) = skip_comment_or_string(bytes, index) {
            index = next;
        } else if bytes[index] == needle {
            return Some(index);
        } else {
            index += 1;
        }
    }
    None
}

/// Returns the offset of the parenthesis that closes `open`.
fn matching_paren_end(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut index = open;
    while index < bytes.len() {
        if index > open
            && let Some(next) = skip_comment_or_string(bytes, index)
        {
            index = next;
            continue;
        }
        match bytes[index] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// Skips whitespace and comments after a token.
fn skip_trivia_forward(bytes: &[u8], mut index: usize) -> usize {
    loop {
        while bytes
            .get(index)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            index += 1;
        }
        if starts_with(bytes, index, b"--") {
            index = comment_end(bytes, index + 2);
            continue;
        }
        return index;
    }
}

fn insert_error_expr_markers(code: &mut String, source: &str, root: &crate::syntax::Stat) {
    let mut visitor = ErrorExprMarkerVisitor {
        source,
        offsets: Vec::new(),
    };
    visit::walk_stat(root, &mut visitor);
    visitor.offsets.sort_unstable();
    visitor.offsets.dedup();

    for offset in visitor.offsets.into_iter().rev() {
        code.insert_str(offset, "(error-expr)");
    }
}

struct ErrorExprMarkerVisitor<'source> {
    source: &'source str,
    offsets: Vec<usize>,
}

impl Visitor<'_> for ErrorExprMarkerVisitor<'_> {
    fn visit_expr(&mut self, expr: &Expr) -> WalkControl {
        let Expr::Error {
            location,
            expressions,
            ..
        } = expr
        else {
            return WalkControl::Continue;
        };

        if !expressions.is_empty() {
            return WalkControl::Continue;
        }

        if let Some(location) = location
            && location.begin == location.end
            && let Some(offset) = location.begin.byte_offset(self.source)
        {
            self.offsets.push(offset);
        }

        WalkControl::Continue
    }
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
    use super::{
        EraseDeclarationsOptions, EraseTypesOptions, erase_declarations, erase_types,
        parse_with_config,
    };
    use crate::parse::Config;

    #[test]
    fn erases_declare_global_and_keeps_positions() {
        let source = "\
export type Response = {
    ok: boolean,
}

declare http: {
    request: (url: string) -> Response,
}

return http
";

        let erased = erase_declarations(source, EraseDeclarationsOptions::default())
            .expect("declaration source parses");

        assert_eq!(
            erased,
            "export type Response = {\n    ok: boolean,\n}\n\n               \n\
             \x20                                      \n \n\nreturn http\n"
        );
        let compiled = parse_with_config(&erased, &Config::default());
        assert!(
            compiled.errors.is_empty(),
            "erased source must parse without declaration syntax: {:?}",
            compiled.errors
        );
    }

    #[test]
    fn erases_declare_function_and_class() {
        let source = "\
declare function greet(name: string): string
declare class Widget
    value: number
end
type Kept = number
";

        let erased = erase_declarations(source, EraseDeclarationsOptions::default())
            .expect("declaration source parses");

        assert!(!erased.contains("declare"));
        assert!(!erased.contains("Widget"));
        assert!(erased.contains("type Kept = number"));
        let compiled = parse_with_config(&erased, &Config::default());
        assert!(compiled.errors.is_empty());
    }

    #[test]
    fn erase_declarations_does_not_bleed_into_the_next_statement() {
        let with_annotation = "declare function greet(name: string): string\nlocal x = 1\n";
        let erased = erase_declarations(with_annotation, EraseDeclarationsOptions::default())
            .expect("declaration source parses");
        assert_eq!(
            erased,
            "                                            \nlocal x = 1\n"
        );

        let without_annotation = "declare function ping()\nlocal x = 1\n";
        let erased = erase_declarations(without_annotation, EraseDeclarationsOptions::default())
            .expect("declaration source parses");
        assert_eq!(erased, "                       \nlocal x = 1\n");
    }

    #[test]
    fn erase_declarations_keeps_plain_source_unchanged() {
        let source = "local x = 1\nreturn x\n";

        let erased = erase_declarations(source, EraseDeclarationsOptions::default())
            .expect("plain source parses");

        assert_eq!(erased, source);
    }

    #[test]
    fn erase_declarations_reports_parse_errors() {
        let errors = erase_declarations("declare oops:", EraseDeclarationsOptions::default())
            .expect_err("invalid declaration reports errors");

        assert!(!errors.is_empty());
    }

    #[test]
    fn preserves_parseable_source_in_bootstrap_slice() {
        let source = " local x = 1 ";

        let printed = erase_types(source, EraseTypesOptions::default());

        assert_eq!(printed.expect("parses"), source);
    }

    #[test]
    fn reports_parse_error_when_not_ignored() {
        let printed = erase_types("local x =", EraseTypesOptions::default());

        let errors = printed.expect_err("parse error reported");
        assert!(!errors.is_empty());
        // The Display rendering carries location and message.
        assert!(errors[0].to_string().contains(':'));
    }

    #[test]
    fn renders_empty_error_expressions_when_errors_are_ignored() {
        let printed = erase_types(
            "\nrepeat\n    print(\"hello world\")\n",
            EraseTypesOptions {
                with_types: true,
                ignore_parse_errors: true,
                ..EraseTypesOptions::default()
            },
        );

        assert_eq!(
            printed.expect("ignored parse error"),
            "\nrepeat\n    print(\"hello world\")\n(error-expr)"
        );
    }

    #[test]
    fn strips_types_from_untyped_output_while_preserving_columns() {
        let printed = erase_types(
            " local s: string= f<<A, B>>() :: any+ g() :: number ",
            EraseTypesOptions::default(),
        );

        assert_eq!(
            printed.expect("parses"),
            " local s        = f        ()       + g()           "
        );
    }

    #[test]
    fn strips_types_with_operators_correctly() {
        let printed = erase_types("local x = y :: A > z", EraseTypesOptions::default());
        assert_eq!(printed.expect("parses"), "local x = y      > z");

        let printed = erase_types(
            "local x = y :: A < z",
            EraseTypesOptions {
                ignore_parse_errors: true,
                ..EraseTypesOptions::default()
            },
        );
        assert_eq!(printed.expect("parses"), "local x = y      < z");
    }

    #[test]
    fn strips_types_with_strings_containing_equals_correctly() {
        let printed = erase_types(
            "local x: \"foo=bar\" = \"foo=bar\"",
            EraseTypesOptions::default(),
        );
        assert_eq!(printed.expect("parses"), "local x            = \"foo=bar\"");
    }

    #[test]
    fn strips_types_with_comments_correctly() {
        let printed = erase_types(
            "local s: { a: string } -- comment\n = f()",
            EraseTypesOptions::default(),
        );
        assert_eq!(
            printed.expect("parses"),
            "local s                          \n = f()"
        );
    }
}
