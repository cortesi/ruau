//! Luau AST and parser-facing syntax structures.
//!
//! Includes parsing, AST JSON, source transforms, locations, and visitors. The
//! raw lexer is exposed only by the `fixtures` feature.
//!
//! # Parsing
//!
//! Use [`parse::parse`] for ordinary source text and
//! [`parse::parse_bytes_with_config`] when byte-exact string contents and
//! byte-column locations matter. Whole-file parsing always returns a root
//! [`Stat`]: recovery inserts `Stat::Error` nodes and records flat
//! diagnostics in [`parse::Result::errors`] rather than dropping the root.
//! Type entry points such as [`parse::parse_type`] return the same shape around
//! a single syntax node.
//!
//! Error recovery nodes carry a `message_index` when they correspond directly
//! to one parse diagnostic. The index refers into the surrounding parse
//! result's `errors` vector and is emitted for upstream AST JSON compatibility.

pub mod json;
#[cfg(any())]
#[doc(hidden)]
pub mod lexer;
#[cfg(not(any()))]
pub(crate) mod lexer;
mod literal;
mod location;
pub mod parse;
mod parser;
mod syntax;
pub mod transform;
pub use syntax::*;
pub mod visit;

pub use literal::render_string_literal;
pub use location::{Location, Position};
