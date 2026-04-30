//! Token stream flattening with span-aware source positions.

use std::{
    fmt::{self, Display, Formatter},
    sync::LazyLock,
};

use proc_macro::{Delimiter, Span, TokenStream, TokenTree};
use proc_macro2::Span as Span2;
use regex::Regex;

#[derive(Clone, Copy, Debug)]
/// Source position in line and column coordinates.
pub struct Pos {
    /// One-indexed source line when available.
    pub(crate) line: usize,
    /// Zero-indexed source column.
    pub(crate) column: usize,
}

impl Pos {
    /// Construct a source position.
    fn new(line: usize, column: usize) -> Self {
        Self { line, column }
    }
}

/// Return the start and end positions for a span.
fn span_pos(span: &Span) -> (Pos, Pos) {
    let span2: Span2 = (*span).into();
    let start = span2.start();
    let end = span2.end();

    // Stable rust does not provide line/column information; both fields are 0
    // (lines are otherwise 1-indexed). Fall back to parsing the Debug output.
    if start.line == 0 || end.line == 0 {
        return fallback_span_pos(span);
    }

    (Pos::new(start.line, start.column), Pos::new(end.line, end.column))
}

/// Recover span positions from debug output when stable spans omit them.
fn fallback_span_pos(span: &Span) -> (Pos, Pos) {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"bytes\(([0-9]+)\.\.([0-9]+)\)").unwrap());

    let debug = format!("{span:?}");
    let parsed = RE.captures(&debug).and_then(|c| {
        let start = c.get(1)?.as_str().parse().ok()?;
        let end = c.get(2)?.as_str().parse().ok()?;
        Some((start, end))
    });
    let Some((start, end)) = parsed else {
        proc_macro_error2::abort_call_site!("Cannot retrieve span information; please use nightly");
    };
    (Pos::new(1, start), Pos::new(1, end))
}

/// Attribute of token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TokenAttr {
    /// No attribute.
    None,
    /// Starts with `$`.
    Cap,
}

#[derive(Clone, Debug)]
/// Token with source text, original tree, span positions, and capture metadata.
pub struct Token {
    /// Source text for this token.
    source: String,
    /// Original token tree.
    tree: TokenTree,
    /// Start position.
    start: Pos,
    /// End position.
    end: Pos,
    /// Capture marker state.
    attr: TokenAttr,
}

impl Token {
    /// Construct a token from a token tree.
    fn new(tree: TokenTree) -> Self {
        let (start, end) = span_pos(&tree.span());
        Self {
            source: tree.to_string(),
            start,
            end,
            tree,
            attr: TokenAttr::None,
        }
    }

    /// Construct a synthetic delimiter token around a group.
    fn new_delim(source: String, tree: TokenTree, open: bool) -> Self {
        let (start, end) = span_pos(&tree.span());
        let (start, end) = if open {
            (
                start,
                Pos {
                    column: start.column.saturating_add(1),
                    ..start
                },
            )
        } else {
            (
                Pos {
                    column: end.column.saturating_sub(1),
                    ..end
                },
                end,
            )
        };

        Self {
            source,
            tree,
            start,
            end,
            attr: TokenAttr::None,
        }
    }

    /// Original token tree.
    pub(crate) fn tree(&self) -> &TokenTree {
        &self.tree
    }

    /// Whether this token is a `$ident` capture.
    pub(crate) fn is_cap(&self) -> bool {
        self.attr == TokenAttr::Cap
    }

    /// Start position.
    pub(crate) fn start(&self) -> Pos {
        self.start
    }

    /// End position.
    pub(crate) fn end(&self) -> Pos {
        self.end
    }
}

/// Flatten grouped tokens and mark `$ident` capture tokens.
pub fn retokenize(tt: TokenStream) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::new();
    let mut iter = tt.into_iter().flat_map(flatten);
    while let Some(t) = iter.next() {
        if t.source == "$" {
            out.push(capture_token(&t, iter.next()));
        } else {
            out.push(t);
        }
    }
    out
}

/// Convert `$` followed by an identifier into a capture token.
fn capture_token(dollar: &Token, token: Option<Token>) -> Token {
    match token {
        Some(mut t) if matches!(t.tree, TokenTree::Ident(_)) => {
            t.attr = TokenAttr::Cap;
            t
        }
        Some(t) => {
            proc_macro_error2::abort!(t.tree.span(), "expected an identifier after `$` in chunk capture")
        }
        None => proc_macro_error2::abort!(
            dollar.tree.span(),
            "expected an identifier after `$` in chunk capture"
        ),
    }
}

/// Flatten groups into explicit delimiter tokens.
fn flatten(tt: TokenTree) -> Vec<Token> {
    match tt.clone() {
        TokenTree::Group(g) => {
            let (open, close) = match g.delimiter() {
                Delimiter::Parenthesis => ("(", ")"),
                Delimiter::Brace => ("{", "}"),
                Delimiter::Bracket => ("[", "]"),
                Delimiter::None => ("", ""),
            };
            let mut out = vec![Token::new_delim(open.into(), tt.clone(), true)];
            out.extend(g.stream().into_iter().flat_map(flatten));
            out.push(Token::new_delim(close.into(), tt, false));
            out
        }
        _ => vec![Token::new(tt)],
    }
}

impl Display for Token {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "{}", self.source)
    }
}
