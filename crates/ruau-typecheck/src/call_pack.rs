//! Shared call argument and parameter-pack adjustment helpers.

use crate::types::{Arena, TypeId, TypeKind, TypePackId, TypePackKind, TypePackTail};

/// Whether the first function parameter is still matched as an explicit
/// argument, or has already been supplied by the call form/callable value.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub enum ReceiverParameter {
    /// Keep the first parameter in explicit argument matching.
    #[default]
    Explicit,
    /// Remove the first parameter before matching explicit arguments.
    Supplied,
}

/// Parameter pack shape used by call solving before pack normalization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallParameterPack {
    /// Fixed parameters after any call-site adjustment.
    pub(crate) types: Vec<TypeId>,
    /// Remaining parameter tail, preserved as the original pack handle.
    pub(crate) tail: Option<TypePackId>,
}

impl CallParameterPack {
    /// Builds a call parameter view from a list pack.
    pub(crate) fn from_list(arena: &Arena, pack: TypePackId) -> Option<Self> {
        let list = arena.flatten_list_pack(pack)?;
        Some(Self {
            types: list.types,
            tail: list.tail,
        })
    }

    /// Adjusts the pack to the parameters still matched by explicit arguments.
    #[must_use]
    pub(crate) fn for_explicit_arguments(mut self, receiver: ReceiverParameter) -> Self {
        match receiver {
            ReceiverParameter::Explicit => {}
            ReceiverParameter::Supplied => {
                if !self.types.is_empty() {
                    self.types.remove(0);
                }
            }
        }
        self
    }
}

/// Expected parameter roles for source-level call argument generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExpectedCallParameterPack {
    /// Implicit receiver parameter supplied by a method call, if any.
    receiver: Option<TypeId>,
    /// Explicit call parameters after any receiver adjustment.
    parameters: CallParameterPack,
}

impl ExpectedCallParameterPack {
    /// Builds expected source-call parameters from a function type.
    pub(crate) fn from_callee(arena: &Arena, callee: TypeId, receiver: ReceiverParameter) -> Self {
        let TypeKind::Function(function) = arena.get(arena.follow(callee)) else {
            return Self::empty();
        };
        CallParameterPack::from_list(arena, function.arguments)
            .map(|parameters| Self::from_parameters(parameters, receiver))
            .unwrap_or_else(Self::empty)
    }

    /// Returns the expected receiver type supplied by a method call.
    pub(crate) fn receiver(&self) -> Option<TypeId> {
        self.receiver
    }

    /// Returns the expected type for a fixed explicit call argument.
    pub(crate) fn fixed_parameter(&self, index: usize) -> Option<TypeId> {
        self.parameters.types.get(index).copied()
    }

    /// Returns the expected type for an explicit argument, including a
    /// variadic tail when the fixed prefix is exhausted.
    pub(crate) fn parameter_at(&self, arena: &Arena, index: usize) -> Option<TypeId> {
        if let Some(parameter) = self.fixed_parameter(index) {
            return Some(parameter);
        }

        let _remaining = index.checked_sub(self.parameters.types.len())?;
        let tail = self.parameters.tail?;
        match arena.get_pack(arena.follow_pack(tail)).clone() {
            TypePackKind::Variadic { ty } => Some(ty),
            TypePackKind::Error => Some(arena.primitives().error),
            TypePackKind::Bound(_) => unreachable!("follow_pack removes bound packs"),
            TypePackKind::List { .. } | TypePackKind::Free { .. } | TypePackKind::Generic(_) => {
                None
            }
        }
    }

    /// Returns the fixed explicit parameters after receiver adjustment.
    pub(crate) fn fixed_parameters(&self) -> &[TypeId] {
        &self.parameters.types
    }

    fn from_parameters(mut parameters: CallParameterPack, receiver: ReceiverParameter) -> Self {
        let receiver = match receiver {
            ReceiverParameter::Explicit => None,
            ReceiverParameter::Supplied if !parameters.types.is_empty() => {
                Some(parameters.types.remove(0))
            }
            ReceiverParameter::Supplied => None,
        };
        Self {
            receiver,
            parameters,
        }
    }

    fn empty() -> Self {
        Self {
            receiver: None,
            parameters: CallParameterPack {
                types: Vec::new(),
                tail: None,
            },
        }
    }
}

/// Expected return-pack role for contextual function expressions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExpectedFunctionReturnPack {
    returns: TypePackId,
}

impl ExpectedFunctionReturnPack {
    /// Builds a contextual return-pack view from an expected function type.
    pub(crate) fn from_expected_type(arena: &Arena, expected: TypeId) -> Option<Self> {
        let TypeKind::Function(function) = arena.get(arena.follow(expected)) else {
            return None;
        };
        Some(Self {
            returns: function.returns,
        })
    }

    /// Returns the contextual function return pack.
    pub(crate) fn returns(self) -> TypePackId {
        self.returns
    }

    /// Returns true when an empty body is compatible with this contextual pack.
    ///
    /// A tail-only pack (`...T`/free/generic tail) trivially accepts zero
    /// returns. A single unresolved free or generic return type is also
    /// compatible: it is an inference target (e.g. `R` in `(fn: (txn) -> R) ->
    /// R`), so a body that returns nothing simply settles it to no value rather
    /// than demanding a returned value.
    pub(crate) fn allows_empty_body(self, arena: &Arena) -> bool {
        self.is_tail_only(arena) || self.is_single_inferable_return(arena)
    }

    /// Returns true when the pack is a single not-yet-resolved free or generic
    /// type with no tail — an inference target rather than a demanded value.
    pub(crate) fn is_single_inferable_return(self, arena: &Arena) -> bool {
        let normalized = arena.normalize_pack(self.returns);
        normalized.tail.is_none()
            && matches!(
                normalized.types.as_slice(),
                [ty] if matches!(
                    arena.get(arena.follow(*ty)),
                    crate::types::TypeKind::Free(_) | crate::types::TypeKind::Generic(_)
                )
            )
    }

    /// Returns true when this contextual pack is represented only by a tail.
    pub(crate) fn is_tail_only(self, arena: &Arena) -> bool {
        let normalized = arena.normalize_pack(self.returns);
        normalized.types.is_empty()
            && matches!(
                normalized.tail,
                Some(
                    TypePackTail::Free { .. }
                        | TypePackTail::Generic(_)
                        | TypePackTail::Variadic(_)
                )
            )
    }
}

/// Normalized parameter pack shape used by overload checking.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NormalizedCallParameterPack {
    /// Fixed parameters after any call-site adjustment.
    pub(crate) types: Vec<TypeId>,
    /// Remaining normalized parameter tail.
    pub(crate) tail: Option<TypePackTail>,
}

impl NormalizedCallParameterPack {
    /// Builds a normalized call parameter view.
    pub(crate) fn from_pack(arena: &Arena, pack: TypePackId) -> Self {
        let normalized = arena.normalize_pack(pack);
        Self {
            types: normalized.types,
            tail: normalized.tail,
        }
    }

    /// Drops a required implicit receiver parameter.
    #[must_use]
    fn without_required_receiver(mut self) -> Option<Self> {
        if self.types.is_empty() {
            return None;
        }
        self.types.remove(0);
        Some(self)
    }

    /// Adjusts the pack to the parameters still matched by explicit arguments.
    pub(crate) fn for_explicit_arguments(self, receiver: ReceiverParameter) -> Option<Self> {
        match receiver {
            ReceiverParameter::Explicit => Some(self),
            ReceiverParameter::Supplied => self.without_required_receiver(),
        }
    }
}

#[cfg(any())]
mod tests {
    use super::{
        CallParameterPack, ExpectedCallParameterPack, ExpectedFunctionReturnPack,
        NormalizedCallParameterPack, ReceiverParameter,
    };
    use crate::types::{
        Arena, FunctionType, GenericTypePack, TypeKind, TypeLevel, TypePackKind, TypePackTail,
    };

    #[test]
    fn list_parameter_pack_drops_receiver_without_losing_tail() {
        let mut arena = Arena::new();
        let primitives = arena.primitives();
        let tail = arena.alloc_pack(TypePackKind::Variadic {
            ty: primitives.string,
        });
        let pack = arena.alloc_pack(TypePackKind::List {
            types: vec![primitives.number],
            tail: Some(tail),
        });

        let adjusted = CallParameterPack::from_list(&arena, pack)
            .unwrap()
            .for_explicit_arguments(ReceiverParameter::Supplied);

        assert_eq!(adjusted.types, Vec::new());
        assert_eq!(adjusted.tail, Some(tail));
    }

    #[test]
    fn normalized_parameter_pack_requires_receiver_to_skip() {
        let mut arena = Arena::new();
        let empty = arena.alloc_pack(TypePackKind::List {
            types: Vec::new(),
            tail: None,
        });

        let adjusted = NormalizedCallParameterPack::from_pack(&arena, empty)
            .for_explicit_arguments(ReceiverParameter::Supplied);

        assert!(adjusted.is_none());
    }

    #[test]
    fn normalized_parameter_pack_drops_receiver_without_losing_tail() {
        let mut arena = Arena::new();
        let primitives = arena.primitives();
        let tail = arena.alloc_pack(TypePackKind::Variadic {
            ty: primitives.string,
        });
        let pack = arena.alloc_pack(TypePackKind::List {
            types: vec![primitives.number],
            tail: Some(tail),
        });

        let adjusted = NormalizedCallParameterPack::from_pack(&arena, pack)
            .for_explicit_arguments(ReceiverParameter::Supplied)
            .unwrap();

        assert_eq!(adjusted.types, Vec::new());
        assert_eq!(
            adjusted.tail,
            Some(TypePackTail::Variadic(primitives.string))
        );
    }

    #[test]
    fn normalized_parameter_pack_keeps_explicit_receiver() {
        let mut arena = Arena::new();
        let primitives = arena.primitives();
        let pack = arena.alloc_pack(TypePackKind::List {
            types: vec![primitives.number],
            tail: None,
        });

        let adjusted = NormalizedCallParameterPack::from_pack(&arena, pack)
            .for_explicit_arguments(ReceiverParameter::Explicit)
            .unwrap();

        assert_eq!(adjusted.types, vec![primitives.number]);
        assert_eq!(adjusted.tail, None);
    }

    #[test]
    fn expected_source_call_parameters_split_receiver_without_losing_tail() {
        let mut arena = Arena::new();
        let primitives = arena.primitives();
        let tail = arena.alloc_pack(TypePackKind::Variadic {
            ty: primitives.boolean,
        });
        let arguments = arena.alloc_pack(TypePackKind::List {
            types: vec![primitives.string, primitives.number],
            tail: Some(tail),
        });
        let returns = arena.alloc_pack(TypePackKind::List {
            types: Vec::new(),
            tail: None,
        });
        let function = arena.alloc(TypeKind::Function(FunctionType::new(arguments, returns)));

        let expected =
            ExpectedCallParameterPack::from_callee(&arena, function, ReceiverParameter::Supplied);

        assert_eq!(expected.receiver(), Some(primitives.string));
        assert_eq!(expected.fixed_parameter(0), Some(primitives.number));
        assert_eq!(expected.fixed_parameters(), &[primitives.number]);
        assert_eq!(expected.parameters.tail, Some(tail));
    }

    #[test]
    fn expected_source_call_parameters_keep_first_parameter_without_receiver() {
        let mut arena = Arena::new();
        let primitives = arena.primitives();
        let arguments = arena.alloc_pack(TypePackKind::List {
            types: vec![primitives.string, primitives.number],
            tail: None,
        });
        let returns = arena.alloc_pack(TypePackKind::List {
            types: Vec::new(),
            tail: None,
        });
        let function = arena.alloc(TypeKind::Function(FunctionType::new(arguments, returns)));

        let expected =
            ExpectedCallParameterPack::from_callee(&arena, function, ReceiverParameter::Explicit);

        assert_eq!(expected.receiver(), None);
        assert_eq!(
            expected.fixed_parameters(),
            &[primitives.string, primitives.number]
        );
    }

    #[test]
    fn expected_source_call_parameters_read_variadic_tail() {
        let mut arena = Arena::new();
        let primitives = arena.primitives();
        let tail = arena.alloc_pack(TypePackKind::Variadic {
            ty: primitives.string,
        });
        let arguments = arena.alloc_pack(TypePackKind::List {
            types: vec![primitives.number],
            tail: Some(tail),
        });
        let returns = arena.alloc_pack(TypePackKind::List {
            types: Vec::new(),
            tail: None,
        });
        let function = arena.alloc(TypeKind::Function(FunctionType::new(arguments, returns)));

        let expected =
            ExpectedCallParameterPack::from_callee(&arena, function, ReceiverParameter::Explicit);

        assert_eq!(expected.parameter_at(&arena, 0), Some(primitives.number));
        assert_eq!(expected.parameter_at(&arena, 1), Some(primitives.string));
        assert_eq!(expected.parameter_at(&arena, 2), Some(primitives.string));
    }

    #[test]
    fn expected_function_return_pack_allows_empty_generic_pack_body() {
        let mut arena = Arena::new();
        let arguments = arena.alloc_pack(TypePackKind::List {
            types: Vec::new(),
            tail: None,
        });
        let returns = arena.alloc_pack(TypePackKind::Generic(GenericTypePack {
            name: "R".to_owned(),
            level: TypeLevel(0),
        }));
        let function = arena.alloc(TypeKind::Function(FunctionType::new(arguments, returns)));

        let expected = ExpectedFunctionReturnPack::from_expected_type(&arena, function).unwrap();

        assert_eq!(expected.returns(), returns);
        assert!(expected.allows_empty_body(&arena));
    }

    #[test]
    fn expected_function_return_pack_requires_fixed_return_body() {
        let mut arena = Arena::new();
        let primitives = arena.primitives();
        let arguments = arena.alloc_pack(TypePackKind::List {
            types: Vec::new(),
            tail: None,
        });
        let returns = arena.alloc_pack(TypePackKind::List {
            types: vec![primitives.number],
            tail: None,
        });
        let function = arena.alloc(TypeKind::Function(FunctionType::new(arguments, returns)));

        let expected = ExpectedFunctionReturnPack::from_expected_type(&arena, function).unwrap();

        assert!(!expected.allows_empty_body(&arena));
    }
}
