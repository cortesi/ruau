//! Generic type-alias support: syntactic argument-shape helpers plus
//! root-alias validation and materialization diagnostics.
//!
//! Generation-time alias *lowering* lives in `generation::lower`, which needs
//! the constraint generator's mutable state; this module owns the parts that
//! depend only on the syntax tree and the scope/type arenas.

mod shape;
mod validate;

pub use shape::{
    arguments_are_out_of_order, type_argument_can_follow_pack, type_reference_has_parameter_list,
};
pub use validate::{
    generic_pack_used_as_type_diagnostic, generic_type_used_as_pack_diagnostic,
    materialize_root_type_aliases, recursive_type_alias_diagnostic, validate_root_type_aliases,
};
