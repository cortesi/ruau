//! Luau AST and parser-facing syntax structures.
//!
//! Includes parsing, AST JSON, pretty-printing, locations, and visitors. The
//! raw lexer is exposed only by the `fixtures` feature.

pub mod json;
#[cfg(any())]
#[doc(hidden)]
pub mod lexer;
#[cfg(not(any()))]
pub(crate) mod lexer;
mod location;
pub mod parse;
mod parser;
pub mod pretty;
pub mod syntax;
pub mod visit;

pub use location::{Location, Position};
