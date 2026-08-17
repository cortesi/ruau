//! Modularized expression constraint generation.

pub mod call;
pub mod expected;
pub mod expression;
pub mod for_in;
pub mod generic_pack_call;
pub mod literal;
pub mod lower;
pub mod operator;
pub mod refinement;
pub mod state;
pub mod statement;
pub mod string_format;
pub mod type_function_eval;
pub mod uninhabited;

pub use statement::{ModuleReturnTypes, generate_expression_constraints_with_require_returns};

pub use crate::checker::GenerationConfig;
