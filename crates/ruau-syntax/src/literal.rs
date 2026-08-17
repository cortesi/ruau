//! Canonical Luau literal rendering.

/// Renders one UTF-8 string as a quoted Luau string literal.
///
/// The renderer uses short escapes for common control characters and Luau
/// Unicode escapes for the remaining control code points and non-ASCII text.
#[must_use]
pub fn render_string_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            character if character.is_control() || !character.is_ascii() => {
                use std::fmt::Write as _;
                write!(out, "\\u{{{:x}}}", character as u32)
                    .expect("writing to a string cannot fail");
            }
            character => out.push(character),
        }
    }
    out.push('"');
    out
}

#[cfg(any())]
mod tests {
    use super::render_string_literal;
    use crate::{Type, parse};

    #[test]
    fn renders_controls_quotes_slashes_and_unicode() {
        let value = (0_u8..=0x1f).map(char::from).collect::<String>() + "\"\\\t\n\rλ";
        let rendered = render_string_literal(&value);
        let parsed = parse::parse_type(&rendered);
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        let Type::SingletonString { value: actual, .. } = parsed.root else {
            panic!("expected one string singleton");
        };
        assert_eq!(actual, value);
    }
}
