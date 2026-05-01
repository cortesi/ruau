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
//! Inherent methods on [`Luau`] such as [`Luau::to_value`] and [`Luau::deserialize_value`] allow
//! conversion from Rust types to Luau values and vice versa using serde. Any user defined data
//! type that implements [`serde::Serialize`] or [`serde::Deserialize`] can be converted. For
//! convenience, additional functionality to handle `NULL` values and arrays is provided through
//! [`crate::serde::SerializeOptions`] and [`crate::serde::DeserializeOptions`].
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
//! Futures produced by direct VM APIs borrow local Luau state and are not `Send`, so direct mode
//! should use a current-thread Tokio runtime. A [`LocalSet`] is required when spawning
//! local VM futures or mixing `spawn_local` with Luau callbacks.
//!
//! Multi-thread Tokio applications should use [`LuauWorker`]. The worker owns one VM on a
//! dedicated OS thread with a current-thread Tokio runtime and local task lane, while
//! [`LuauWorkerHandle`] is `Clone + Send + Sync` and can be used from ordinary `tokio::spawn`
//! tasks.
//!
//! # Host definitions
//!
//! [`HostApi`] keeps a Rust registration and its `.d.luau` declaration next to each other. Add the
//! definitions to an [`analyzer::Checker`] before checking, then install the same host functions
//! into a [`Luau`] VM before execution.
//!
//! # Debugging
//!
//! The [`debug`] module contains stack inspection, traceback, function metadata, and heap dump
//! helpers. These APIs are grouped separately from [`Luau`] because they are diagnostic tools
//! rather than the ordinary embedding surface.
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
// Stage Four of plans/unsafe.md kept this allow because every `unsafe fn` body still relies on
// implicit unsafe permission for FFI calls. Removing it would require wrapping every op in a
// per-line `unsafe { ... }` block — a separate refactor that is out of scope for the current
// sweep. The allow stays until that work lands.
#![allow(unsafe_op_in_unsafe_fn)]
// Hidden stack-level trait hooks intentionally use crate-private implementation context.
#![allow(private_interfaces)]
// Split impl blocks keep API-specific docs near their modules.
#![allow(clippy::multiple_inherent_impl)]
// Stage Four of plans/unsafe.md added SAFETY comments to every unsafe block in this crate;
// the allow is no longer needed. The workspace-level `clippy::undocumented_unsafe_blocks`
// lint catches future regressions.
// Stage Five of plans/unsafe.md will remove this and add # Safety sections per fn.
#![allow(clippy::missing_safety_doc)]

/// Internal assertion and FFI helper macros.
#[macro_use]
mod macros;

/// Integrated Luau analysis API.
pub mod analyzer;
/// Buffer handle implementation.
mod buffer;
/// Rust/Luau conversion implementations.
mod conversion;
/// Host API registration helpers.
mod host;
/// Luau allocator and memory accounting.
mod memory;
/// Multi-value argument and return handling.
mod multi;
/// Scoped handle management.
mod scope;
/// Standard library flags.
mod stdlib;
/// Conversion and object traits.
mod traits;
/// Shared raw-handle and callback support types.
mod types;
/// FFI utility helpers.
mod util;
/// Dynamic Luau value representation.
mod value;
/// Generic value-boundary traversal helpers.
pub mod value_visit;
/// Luau vector value representation.
mod vector;
mod worker;

mod chunk;
/// Debug inspection API.
pub mod debug;
mod error;
mod function;
pub mod resolver;
mod runtime;
mod state;
mod string;
mod table;
mod thread;
/// Advanced userdata handles and registries.
pub mod userdata;
mod userdata_impl;
#[doc(inline)]
pub use crate::error::{Error, ErrorContext, ExternalError, ExternalResult, Result};
#[doc(inline)]
pub use crate::function::{Function, ProtectedCallError};
#[doc(inline)]
pub use crate::scope::Scope;
#[doc(inline)]
pub use crate::state::{
    GcIncParams, GcMode, Luau, LuauOptions, Registry, ScopedAppData, ScopedInterrupt,
    ThreadCallbacks, ThreadCollectFn, ThreadCreateFn, WeakLuau,
};
#[doc(inline)]
pub use crate::string::{BorrowedBytes, BorrowedStr, LuauString};
#[doc(inline)]
pub use crate::table::Table;
#[doc(inline)]
pub use crate::thread::{AsyncThread, Thread, ThreadStatus};
#[doc(inline)]
pub use crate::traits::{FromLuau, FromLuauMulti, IntoLuau, IntoLuauMulti, ObjectLike};
#[doc(inline)]
pub use crate::types::{
    AppData, AppDataRef, AppDataRefMut, Integer, LightUserData, Number, PrimitiveType, RegistryKey,
    VmState,
};
#[doc(inline)]
pub use crate::userdata_impl::{
    AnyUserData, MetaMethod, UserData, UserDataFields, UserDataMethods,
};
pub use crate::{
    buffer::Buffer,
    chunk::{
        AsChunk, Chunk, CompileConstant, Compiler, CoverageLevel, DebugLevel, OptimizationLevel,
        TypeInfoLevel,
    },
    host::{HostApi, HostNamespace},
    multi::{MultiValue, Variadic},
    stdlib::StdLib,
    value::{Nil, OpaqueValue, Value},
    value_visit::{
        BoundaryAction, DefaultInboundVisitor, HostValue, InboundKind, InboundMapKey,
        InboundSource, InboundVisitor, OutboundVisitor, UnsupportedOutboundValue, ValuePath,
        ValueVisitError, ValueVisitResult, inbound_to_luau, inbound_to_luau_at_path,
        visit_luau_value, visit_luau_value_at_path,
    },
    vector::Vector,
    worker::{
        LuauWorker, LuauWorkerBuilder, LuauWorkerCancellation, LuauWorkerError, LuauWorkerHandle,
        LuauWorkerResult,
    },
};

pub mod serde;

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
