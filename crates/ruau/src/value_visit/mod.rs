//! Generic traversal helpers for values crossing a Luau host boundary.
//!
//! This module is for embedders that need to inspect or constrain arbitrary values at the host
//! boundary before conversion finishes. Typical uses include rejecting unsupported outbound values
//! before serializing them to JSON, attaching path-aware diagnostics to nested data, or applying a
//! policy to inbound host data before pushing it into a VM. Ordinary typed function arguments and
//! returns should use [`crate::FromLuau`], [`crate::FromLuauMulti`], [`crate::IntoLuau`], and
//! [`crate::IntoLuauMulti`] instead.

mod error;
mod inbound;
mod outbound;
mod path;

#[cfg(test)]
mod tests;

pub use error::{ValueVisitError, ValueVisitResult};
pub use inbound::{
    DefaultInboundVisitor, InboundKind, InboundMapKey, InboundSource, InboundVisitor,
    inbound_to_luau, inbound_to_luau_at_path,
};
pub use outbound::{
    BoundaryAction, HostValue, OutboundVisitor, UnsupportedOutboundValue, visit_luau_value,
    visit_luau_value_at_path,
};
pub use path::ValuePath;
