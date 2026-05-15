//! Inline chunk source reconstruction.

use std::{cmp::Ordering, collections::HashSet};

use proc_macro::{TokenStream, TokenTree};

use crate::token::{Pos, Token, retokenize};

#[derive(Debug)]
/// Parsed inline Luau chunk source plus captured Rust identifiers.
pub struct Chunk {
    /// Reconstructed Luau source.
    source: String,
    /// Rust identifiers captured with `$ident` syntax.
    captures: Vec<TokenTree>,
}

impl Chunk {
    /// Build a chunk from macro input tokens.
    pub(crate) fn new(tokens: TokenStream) -> Self {
        let mut source = String::new();
        let mut captures = Captures::default();
        let mut prev: Option<Pos> = None;

        for t in retokenize(tokens) {
            captures.insert(&t);

            let start = t.start();
            let (prev_line, prev_col) = prev
                .take()
                .map_or((start.line, start.column), |p| (p.line, p.column));

            match start.line.cmp(&prev_line) {
                Ordering::Greater => source.push('\n'),
                Ordering::Equal => {
                    for _ in 0..start.column.saturating_sub(prev_col) {
                        source.push(' ');
                    }
                }
                Ordering::Less => {}
            }
            source.push_str(&t.to_string());

            prev = Some(t.end());
        }

        Self {
            source: source.trim_end().to_string(),
            captures: captures.into_vec(),
        }
    }

    /// Reconstructed Luau source text.
    pub(crate) fn source(&self) -> &str {
        &self.source
    }

    /// Captured Rust identifiers.
    pub(crate) fn captures(&self) -> &[TokenTree] {
        &self.captures
    }
}

#[derive(Default)]
/// Unique captured Rust identifiers, preserving first-use order.
struct Captures {
    /// Captured identifier names already seen.
    names: HashSet<String>,
    /// Captured identifier tokens in source order.
    tokens: Vec<TokenTree>,
}

impl Captures {
    /// Insert a capture token if it has not already appeared.
    fn insert(&mut self, token: &Token) {
        if !token.is_cap() {
            return;
        }

        let name = token.to_string();
        if self.names.insert(name) {
            self.tokens.push(token.tree().clone());
        }
    }

    /// Return captured identifiers in first-use order.
    fn into_vec(self) -> Vec<TokenTree> {
        self.tokens
    }
}
