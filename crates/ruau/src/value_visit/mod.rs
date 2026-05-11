//! Generic traversal helpers for values crossing a Luau host boundary.

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
