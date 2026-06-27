//! Generic type-pack inference checks for source call arguments.

use std::collections::BTreeMap;

use crate::{
    subtype::Subtyper,
    types::{
        Arena, GenericTypePack, TypeId, TypeKind, TypeLevel, TypePackId, TypePackKind, TypePackTail,
    },
};

type GenericPackKey = (String, TypeLevel);
type GenericPackBindings = BTreeMap<GenericPackKey, TypePackId>;

/// How a generic-pack call argument list fails to match the callee.
///
/// The distinction is load-bearing for diagnostic identity: an arity failure is
/// a type-pack mismatch, while a scalar element that fails its expected type is
/// an ordinary type mismatch and must not be reported as a pack mismatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GenericPackCallMismatch {
    /// The argument count does not satisfy the expanded generic pack.
    Arity,
    /// A scalar argument (at this zero-based call position) is not a subtype of
    /// its expected type. The index lets the diagnostic point at the argument.
    ScalarType { argument_index: usize },
}

pub fn generic_pack_call_argument_mismatch(
    arena: &mut Arena,
    callee: TypeId,
    arg_types: &[TypeId],
    arg_tail: Option<TypePackId>,
) -> Option<GenericPackCallMismatch> {
    GenericPackCallChecker::new(arena).call_argument_mismatch(callee, arg_types, arg_tail)
}

struct GenericPackCallChecker<'a> {
    arena: &'a mut Arena,
}

impl<'a> GenericPackCallChecker<'a> {
    fn new(arena: &'a mut Arena) -> Self {
        Self { arena }
    }

    fn call_argument_mismatch(
        &mut self,
        callee: TypeId,
        arg_types: &[TypeId],
        arg_tail: Option<TypePackId>,
    ) -> Option<GenericPackCallMismatch> {
        if arg_tail.is_some() {
            return None;
        }
        let callee = self.arena.follow(callee);
        let TypeKind::Function(function) = self.arena.get(callee).clone() else {
            return None;
        };
        if function.generic_packs.is_empty() {
            return None;
        }
        let arguments = self.arena.normalize_pack(function.arguments);
        let Some(TypePackTail::Generic(tail_generic)) = arguments.tail else {
            return None;
        };
        if !function
            .generic_packs
            .iter()
            .any(|generic| generic == &tail_generic)
        {
            return None;
        }
        if arg_types.len() < arguments.types.len() {
            return None;
        }

        let mut bindings = GenericPackBindings::new();
        for (expected, actual) in arguments
            .types
            .iter()
            .copied()
            .zip(arg_types.iter().copied())
        {
            self.infer_bindings_from_type(expected, actual, &mut bindings);
        }

        let tail_pack = self.arena.alloc_pack(TypePackKind::Generic(tail_generic));
        self.arguments_mismatch(
            tail_pack,
            &arg_types[arguments.types.len()..],
            &mut bindings,
            arguments.types.len(),
        )
    }

    fn infer_bindings_from_type(
        &mut self,
        expected: TypeId,
        actual: TypeId,
        bindings: &mut GenericPackBindings,
    ) {
        let expected = self.arena.follow(expected);
        let actual = self.arena.follow(actual);
        let (TypeKind::Function(expected), TypeKind::Function(actual)) = (
            self.arena.get(expected).clone(),
            self.arena.get(actual).clone(),
        ) else {
            return;
        };
        self.infer_bindings_from_pack(expected.arguments, actual.arguments, bindings);
        self.infer_bindings_from_pack(expected.returns, actual.returns, bindings);
    }

    fn infer_bindings_from_pack(
        &mut self,
        expected: TypePackId,
        actual: TypePackId,
        bindings: &mut GenericPackBindings,
    ) {
        let expected = self.resolve_binding(expected, bindings);
        let expected = self.arena.follow_pack(expected);
        let actual = self.arena.follow_pack(actual);
        match (
            self.arena.get_pack(expected).clone(),
            self.arena.get_pack(actual).clone(),
        ) {
            (TypePackKind::Generic(generic), _) => {
                bindings.entry(generic_pack_key(&generic)).or_insert(actual);
            }
            (
                TypePackKind::List {
                    types: expected_types,
                    tail: expected_tail,
                },
                TypePackKind::List {
                    types: actual_types,
                    tail: actual_tail,
                },
            ) => {
                for (expected, actual) in expected_types
                    .iter()
                    .copied()
                    .zip(actual_types.iter().copied())
                {
                    self.infer_bindings_from_type(expected, actual, bindings);
                }
                if let Some(expected_tail) = expected_tail {
                    let rest = actual_types
                        .get(expected_types.len()..)
                        .map_or_else(Vec::new, |types| types.to_vec());
                    let actual_tail = self.pack_with_tail(rest, actual_tail);
                    self.infer_bindings_from_pack(expected_tail, actual_tail, bindings);
                }
            }
            (TypePackKind::Bound(_), _) | (_, TypePackKind::Bound(_)) => {
                unreachable!("follow_pack removes bound packs")
            }
            _ => {}
        }
    }

    fn arguments_mismatch(
        &mut self,
        expected: TypePackId,
        actual_types: &[TypeId],
        bindings: &mut GenericPackBindings,
        // Zero-based call position of `actual_types[0]`, so a scalar mismatch can
        // report the offending argument's index even through nested pack levels.
        offset: usize,
    ) -> Option<GenericPackCallMismatch> {
        let expected = self.resolve_binding(expected, bindings);
        let expected = self.arena.follow_pack(expected);
        match self.arena.get_pack(expected).clone() {
            TypePackKind::Generic(generic) => {
                let actual = self.pack(actual_types.to_vec());
                bindings.entry(generic_pack_key(&generic)).or_insert(actual);
                None
            }
            TypePackKind::List { types, tail } => {
                let common = types.len().min(actual_types.len());
                for index in 0..common {
                    let expected = types[index];
                    let actual = actual_types[index];
                    self.infer_bindings_from_type(expected, actual, bindings);
                    if self.expected_type_mismatch(expected, actual) {
                        return Some(GenericPackCallMismatch::ScalarType {
                            argument_index: offset + index,
                        });
                    }
                }
                match actual_types.len().cmp(&types.len()) {
                    std::cmp::Ordering::Equal => tail.and_then(|tail| {
                        self.arguments_mismatch(tail, &[], bindings, offset + types.len())
                    }),
                    std::cmp::Ordering::Less => Some(GenericPackCallMismatch::Arity),
                    std::cmp::Ordering::Greater => {
                        let Some(tail) = tail else {
                            return Some(GenericPackCallMismatch::Arity);
                        };
                        self.arguments_mismatch(
                            tail,
                            &actual_types[types.len()..],
                            bindings,
                            offset + types.len(),
                        )
                    }
                }
            }
            TypePackKind::Variadic { ty } => actual_types
                .iter()
                .copied()
                .position(|actual| self.expected_type_mismatch(ty, actual))
                .map(|index| GenericPackCallMismatch::ScalarType {
                    argument_index: offset + index,
                }),
            TypePackKind::Free { .. } | TypePackKind::Error => None,
            TypePackKind::Bound(_) => unreachable!("follow_pack removes bound packs"),
        }
    }

    fn resolve_binding(&self, pack: TypePackId, bindings: &GenericPackBindings) -> TypePackId {
        let pack = self.arena.follow_pack(pack);
        let TypePackKind::Generic(generic) = self.arena.get_pack(pack) else {
            return pack;
        };
        bindings
            .get(&generic_pack_key(generic))
            .copied()
            .unwrap_or(pack)
    }

    fn expected_type_mismatch(&self, expected: TypeId, actual: TypeId) -> bool {
        let expected = self.arena.follow(expected);
        if matches!(
            self.arena.get(expected),
            TypeKind::Any
                | TypeKind::Unknown
                | TypeKind::Error
                | TypeKind::Free(_)
                | TypeKind::Generic(_)
                | TypeKind::Function(_)
        ) {
            return false;
        }
        Subtyper::new(self.arena)
            .is_subtype(actual, expected)
            .is_err()
    }

    fn pack(&mut self, types: Vec<TypeId>) -> TypePackId {
        self.arena
            .alloc_pack(TypePackKind::List { types, tail: None })
    }

    fn pack_with_tail(&mut self, types: Vec<TypeId>, tail: Option<TypePackId>) -> TypePackId {
        self.arena.alloc_pack(TypePackKind::List { types, tail })
    }
}

fn generic_pack_key(generic: &GenericTypePack) -> GenericPackKey {
    (generic.name.clone(), generic.level)
}
