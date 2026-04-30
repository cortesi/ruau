//! Inline chunk source reconstruction.

use std::cmp::Ordering;

use proc_macro::{TokenStream, TokenTree};

use crate::token::{Pos, retokenize};

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
        let mut captures: Vec<TokenTree> = Vec::new();
        let mut prev: Option<Pos> = None;

        for t in retokenize(tokens) {
            if t.is_cap() && !captures.iter().any(|c| c.to_string() == t.to_string()) {
                captures.push(t.tree().clone());
            }

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
            captures,
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
