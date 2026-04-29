use proc_macro::{TokenStream, TokenTree};

use crate::token::{Pos, retokenize};

#[derive(Debug)]
pub struct Chunk {
    source: String,
    captures: Vec<TokenTree>,
}

impl Chunk {
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

            #[allow(clippy::comparison_chain)]
            if start.line > prev_line {
                source.push('\n');
            } else if start.line == prev_line {
                for _ in 0..start.column.saturating_sub(prev_col) {
                    source.push(' ');
                }
            }
            source.push_str(&t.to_string());

            prev = Some(t.end());
        }

        Self {
            source: source.trim_end().to_string(),
            captures,
        }
    }

    pub(crate) fn source(&self) -> &str {
        &self.source
    }

    pub(crate) fn captures(&self) -> &[TokenTree] {
        &self.captures
    }
}
