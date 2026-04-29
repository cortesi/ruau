//! # High-level bindings to Luau
//!
//! The `ruau` crate provides a safe Rust toolkit for embedding the [Luau programming language].
//!
//! # The `Luau` object
//!
//! The main type exported by this library is the [`Luau`] struct. In addition to methods for
//! [executing] Luau chunks or [evaluating] Luau expressions, it provides methods for creating Luau
//! values and accessing the table of [globals].
//!
//! # Converting data
//!
//! The [`IntoLuau`] and [`FromLuau`] traits allow conversion from Rust types to Luau values and vice
//! versa. They are implemented for many data structures found in Rust's standard library.
//!
//! For more general conversions, the [`IntoLuauMulti`] and [`FromLuauMulti`] traits allow converting
//! between Rust types and *any number* of Luau values.
//!
//! Most code in `ruau` is generic over implementors of those traits, so in most places the normal
//! Rust data structures are accepted without having to write any boilerplate.
//!
//! # Custom Userdata
//!
//! The [`UserData`] trait can be implemented by user-defined types to make them available to Luau.
//! Methods and operators to be used from Luau can be added using the [`UserDataMethods`] API.
//! Fields are supported using the [`UserDataFields`] API.
//!
//! # Serde support
//!
//! Inherent methods on [`Luau`] such as [`Luau::to_value`] and [`Luau::from_value`] allow
//! conversion from Rust types to Luau values and vice versa using serde. Any user defined data
//! type that implements [`serde::Serialize`] or [`serde::Deserialize`] can be converted. For
//! convenience, additional functionality to handle `NULL` values and arrays is provided through
//! [`serde::SerializeOptions`] and [`serde::DeserializeOptions`].
//!
//! The [`Value`] enum and other types implement [`serde::Serialize`] trait to support serializing
//! Luau values into Rust values.
//!
//! # Async/await support
//!
//! The [`Luau::create_async_function`] allows creating non-blocking functions that returns
//! [`Future`]. Luau execution APIs return futures and are intended to be driven by Tokio.
//!
//! [`Luau`] is `!Send + !Sync`: the VM is pinned to a single thread for its entire lifetime.
//! Futures produced by the VM borrow local Luau state, so applications driving Luau work must
//! use a current-thread Tokio runtime with [`tokio::task::LocalSet`] (or another local executor).
//!
//! # Host definitions
//!
//! [`HostApi`] keeps a Rust registration and its `.d.luau` declaration next to each other. Add the
//! definitions to an [`analyzer::Checker`] before checking, then install the same host functions
//! into a [`Luau`] VM before execution.
//!
//! # Analysis and checked loading
//!
//! The [`analyzer`] and [`resolver`] modules support checking a module graph before execution. Use
//! [`HostApi`] to keep Rust globals and their `.d.luau` declarations together, then call
//! [`Luau::checked_load`] or [`Luau::checked_load_resolved`] to get a chunk only after analysis
//! succeeds.
//!
//! [`resolver::ResolverSnapshot`] captures the resolved module graph once and feeds the same module
//! sources to the analyzer and runtime `require` implementation used by checked loading.
//!
//! # Luau Runtime
//!
//! `ruau` embeds Luau from the vendored source package. Luau-specific libraries such as `buffer`,
//! `vector`, and `integer` are exposed through [`StdLib`], while [`StdLib::ALL_SAFE`] excludes the
//! isolation-breaking `debug` library.
//!
//! ```no_run
//! # use ruau::{HostApi, Luau, Result, analyzer::Checker, resolver::InMemoryResolver};
//! # async fn run() -> Result<()> {
//! let host = HostApi::new().global_function(
//!     "log",
//!     |_lua, message: String| {
//!         println!("{message}");
//!         Ok(())
//!     },
//!     "declare function log(message: string)",
//! );
//!
//! let mut checker = Checker::new().expect("checker");
//! host.add_definitions_to(&mut checker).expect("definitions");
//!
//! let lua = Luau::new();
//! host.install(&lua)?;
//!
//! let resolver = InMemoryResolver::new()
//!     .with_module("main", "local dep = require('dep')\nlog(dep.message)")
//!     .with_module("dep", "return { message = 'ready' }");
//! lua.checked_load_resolved(&mut checker, &resolver, "main")
//!     .await
//!     .expect("checked load")
//!     .exec()
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! [Luau programming language]: https://luau.org/
//! [executing]: crate::Chunk::exec
//! [evaluating]: crate::Chunk::eval
//! [globals]: crate::Luau::globals
//! [`Future`]: std::future::Future
//! [`serde::Serialize`]: https://docs.serde.rs/serde/ser/trait.Serialize.html
//! [`serde::Deserialize`]: https://docs.serde.rs/serde/de/trait.Deserialize.html
//! [`AsyncThread`]: crate::AsyncThread

// Deny warnings inside doc tests / examples. When this isn't present, rustdoc doesn't show *any*
// warnings at all.
#![cfg_attr(docsrs, feature(doc_cfg))]
#![allow(unsafe_op_in_unsafe_fn)]
// The inherited codebase intentionally keeps split impl blocks for API docs. Keep only these broad
// style exceptions at crate scope; private-doc and owned-handle exceptions are scoped below.
#![allow(
    clippy::absolute_paths,
    clippy::items_after_statements,
    clippy::multiple_inherent_impl
)]

/// Internal assertion and FFI helper macros.
#[macro_use]
#[allow(clippy::missing_docs_in_private_items)]
mod macros;

/// Integrated Luau analysis API.
pub mod analyzer;
/// Buffer handle implementation.
#[allow(clippy::missing_docs_in_private_items)]
mod buffer;
/// Rust/Luau conversion implementations.
#[allow(clippy::missing_docs_in_private_items)]
mod conversion;
/// Host API registration helpers.
mod host;
/// Luau allocator and memory accounting.
mod memory;
/// Multi-value argument and return handling.
#[allow(clippy::missing_docs_in_private_items)]
mod multi;
/// Scoped handle management.
#[allow(clippy::missing_docs_in_private_items)]
mod scope;
/// Standard library flags.
mod stdlib;
/// Conversion and object traits.
#[allow(clippy::missing_docs_in_private_items)]
mod traits;
/// Shared raw-handle and callback support types.
#[allow(clippy::missing_docs_in_private_items)]
mod types;
/// FFI utility helpers.
#[allow(clippy::missing_docs_in_private_items)]
mod util;
/// Dynamic Luau value representation.
#[allow(clippy::missing_docs_in_private_items)]
mod value;
/// Luau vector value representation.
#[allow(clippy::missing_docs_in_private_items)]
mod vector;

#[allow(clippy::missing_docs_in_private_items)]
mod chunk;
/// Compiler configuration levels and constants.
pub mod compiler;
/// Debug inspection API.
#[allow(clippy::missing_docs_in_private_items)]
pub mod debug;
#[allow(clippy::missing_docs_in_private_items)]
mod error;
#[allow(clippy::missing_docs_in_private_items)]
mod function;
pub mod resolver;
#[allow(clippy::missing_docs_in_private_items)]
mod runtime;
#[allow(clippy::missing_docs_in_private_items)]
mod state;
#[allow(clippy::missing_docs_in_private_items)]
mod string;
#[allow(clippy::missing_docs_in_private_items)]
mod table;
#[allow(clippy::missing_docs_in_private_items)]
mod thread;
/// Advanced userdata handles and registries.
#[allow(clippy::missing_docs_in_private_items)]
pub mod userdata;
#[allow(clippy::missing_docs_in_private_items)]
mod userdata_impl;
/// Advanced VM controls and low-level Luau support types.
#[allow(clippy::missing_docs_in_private_items)]
pub mod vm;

// Public exports.
pub use either::Either;

#[doc(inline)]
pub use crate::error::{Error, ErrorContext, ExternalError, ExternalResult, Result};
#[doc(inline)]
pub use crate::function::Function;
#[doc(inline)]
pub use crate::runtime::HeapDump;
#[doc(inline)]
pub use crate::state::Luau;
#[doc(inline)]
pub use crate::string::{BorrowedBytes, BorrowedStr, LuauString};
#[doc(inline)]
pub use crate::table::Table;
#[doc(inline)]
pub use crate::thread::{AsyncThread, Thread, ThreadStatus};
#[doc(inline)]
pub use crate::traits::{FromLuau, FromLuauMulti, IntoLuau, IntoLuauMulti, ObjectLike, StackCtx};
#[doc(inline)]
pub use crate::userdata_impl::{AnyUserData, MetaMethod, UserData, UserDataFields, UserDataMethods};
pub use crate::{
    buffer::Buffer,
    chunk::{AsChunk, Chunk, Compiler},
    host::{HostApi, HostNamespace},
    multi::{MultiValue, Variadic},
    stdlib::StdLib,
    value::{Nil, Value},
    vector::Vector,
};

#[allow(clippy::missing_docs_in_private_items)]
pub mod serde;

#[cfg(feature = "macros")]
#[allow(unused_imports)]
#[macro_use]
extern crate ruau_derive;

/// Derive [`FromLuau`] for a Rust type.
///
/// Current implementation generate code that takes [`UserData`] value, borrow it (of the Rust type)
/// and clone.
#[cfg(feature = "macros")]
#[cfg_attr(docsrs, doc(cfg(feature = "macros")))]
pub use ruau_derive::FromLuau;
/// Create a type that implements [`AsChunk`] and can capture Rust variables.
///
/// This macro allows to write Luau code directly in Rust code.
///
/// Rust variables can be referenced from Luau using `$` prefix, as shown in the example below.
/// User's Rust types needs to implement [`UserData`] or [`IntoLuau`] traits.
///
/// Captured variables are **moved** into the chunk.
///
/// ```
/// use ruau::{Luau, Result, chunk};
///
/// #[tokio::main(flavor = "current_thread")]
/// async fn main() -> Result<()> {
///     let lua = Luau::new();
///     let name = "Rustacean";
///     lua.load(chunk! {
///         print("hello, " .. $name)
///     }).exec().await
/// }
/// ```
///
/// ## Syntax issues
///
/// Since the Rust tokenizer will tokenize Luau code, this imposes some restrictions.
/// The main thing to remember is:
///
/// - Use double quoted strings (`""`) instead of single quoted strings (`''`).
///
///   (Single quoted strings only work if they contain a single character, since in Rust,
///   `'a'` is a character literal).
///
/// - Using Luau comments `--` is not desirable in **stable** Rust and can have bad side effects.
///
///   This is because procedural macros have Line/Column information available only in
///   **nightly** Rust. Instead, Luau chunks represented as a big single line of code in stable Rust.
///
///   As workaround, Rust comments `//` can be used.
///
/// Other minor limitations:
///
/// - Certain escape codes in string literals don't work. (Specifically: `\a`, `\b`, `\f`, `\v`,
///   `\123` (octal escape codes), `\u`, and `\U`).
///
///   These are accepted: : `\\`, `\n`, `\t`, `\r`, `\xAB` (hex escape codes), and `\0`.
///
/// - The `//` (floor division) operator is unusable, as its start a comment.
///
/// Everything else should work.
#[cfg(feature = "macros")]
#[cfg_attr(docsrs, doc(cfg(feature = "macros")))]
pub use ruau_derive::chunk;
