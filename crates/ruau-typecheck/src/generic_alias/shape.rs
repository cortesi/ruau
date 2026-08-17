//! Shared generic type-alias argument-shape helpers.

use ruau_syntax::{Location, Type, TypeParameter};

/// Returns true when a type reference syntactically included a parameter list.
pub fn type_reference_has_parameter_list(
    reference_location: Option<Location>,
    name_location: Option<Location>,
) -> bool {
    match (reference_location, name_location) {
        (Some(reference), Some(name)) => reference.end > name.end,
        _ => false,
    }
}

/// Returns true when a `TypeParameter::Type` came from parenthesized syntax
/// that can stand for a one-element type pack in a generic alias argument list.
pub fn type_argument_can_follow_pack(ty: &Type) -> bool {
    matches!(ty, Type::Group { .. })
}

/// Returns true when a bare type argument appears after a type-pack argument.
pub fn arguments_are_out_of_order(
    parameters: &[TypeParameter],
    type_parameter_count: usize,
) -> bool {
    let mut saw_pack = false;
    for (index, parameter) in parameters.iter().enumerate() {
        match parameter {
            TypeParameter::Pack(_) => saw_pack = true,
            TypeParameter::Type(ty)
                if (saw_pack || index >= type_parameter_count)
                    && type_argument_can_follow_pack(ty) =>
            {
                saw_pack = true;
            }
            TypeParameter::Type(_) if saw_pack => return true,
            TypeParameter::Type(_) => {}
        }
    }
    false
}

/// Counts type and type-pack arguments, treating parenthesized singleton pack
/// arguments after the type-parameter prefix as type-pack arguments.
pub fn argument_counts(
    parameters: &[TypeParameter],
    type_parameter_count: usize,
) -> (usize, usize) {
    let mut actual_types = 0;
    let mut actual_packs = 0;
    let mut saw_pack = false;
    for (index, parameter) in parameters.iter().enumerate() {
        match parameter {
            TypeParameter::Pack(_) => {
                actual_packs += 1;
                saw_pack = true;
            }
            TypeParameter::Type(ty)
                if (saw_pack || index >= type_parameter_count)
                    && type_argument_can_follow_pack(ty) =>
            {
                actual_packs += 1;
                saw_pack = true;
            }
            TypeParameter::Type(_) => actual_types += 1,
        }
    }
    (actual_types, actual_packs)
}
