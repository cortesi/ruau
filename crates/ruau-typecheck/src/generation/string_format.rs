//! Static `string.format` format-string analysis.

/// Expected type category for one `string.format` substitution argument.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatArgument {
    /// `%s` requires a string.
    String,
    /// Numeric specifiers require a number.
    Number,
    /// Luau's tostring-like `%*` accepts any value.
    Any,
}

/// Returns the expected argument categories described by a literal format
/// string.
pub fn expected_arguments(format: &str) -> Vec<FormatArgument> {
    let mut expected = Vec::new();
    let mut chars = format.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            continue;
        }
        if chars.next_if_eq(&'%').is_some() {
            continue;
        }
        while chars
            .peek()
            .is_some_and(|flag| matches!(flag, '-' | '+' | ' ' | '#' | '0'))
        {
            chars.next();
        }
        while chars.peek().is_some_and(|width| width.is_ascii_digit()) {
            chars.next();
        }
        if chars.next_if_eq(&'.').is_some() {
            while chars
                .peek()
                .is_some_and(|precision| precision.is_ascii_digit())
            {
                chars.next();
            }
        }
        let Some(specifier) = chars.next() else {
            break;
        };
        expected.push(match specifier {
            's' => FormatArgument::String,
            'd' | 'i' | 'o' | 'u' | 'x' | 'X' | 'f' | 'e' | 'E' | 'g' | 'G' | 'c' => {
                FormatArgument::Number
            }
            '*' => FormatArgument::Any,
            _ => FormatArgument::Any,
        });
    }
    expected
}
