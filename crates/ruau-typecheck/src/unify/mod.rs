//! Structural unification over arena-owned Luau types.

use std::collections::BTreeSet;

use crate::{
    member_access,
    type_function::{Reduction, TypeFunctionRuntime, setmetatable_type_function_arguments},
    types::{
        Arena, FlattenedListPack, FunctionType, PackField, TableIndexer, TableProperty, TableState,
        TableType, TypeField, TypeId, TypeKind, TypePackId, TypePackKind, TypePath,
        TypePathComponent, same_alias_identity_table_arity,
    },
};

fn tail_path(path: &TypePath) -> TypePath {
    path.push(TypePathComponent::PackField(PackField::Tail))
}

/// Target that failed during unification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnifyTarget {
    /// Type node.
    Type(TypeId),
    /// Type-pack node.
    Pack(TypePackId),
}

/// Structured unification failure category.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UnifyErrorKind {
    /// The two type shapes are incompatible.
    Mismatch,
    /// Binding a free variable would create a recursive type.
    OccursCheck,
    /// Two packs or type argument lists have different lengths.
    ArityMismatch,
    /// Two table shapes do not expose the same property names.
    PropertySetMismatch,
    /// Matching table properties differ in metadata.
    PropertyMetadataMismatch,
    /// Unification was aborted early because an internal complexity or
    /// iteration budget was exceeded (mirrors upstream UnificationTooComplex).
    ComplexityExceeded,
}

/// Structured unification failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnifyError {
    /// Category.
    pub kind: UnifyErrorKind,
    /// Path within the compared type.
    pub path: TypePath,
    /// Left-hand failed node.
    pub left: UnifyTarget,
    /// Right-hand failed node.
    pub right: UnifyTarget,
}

impl UnifyError {
    fn type_error(kind: UnifyErrorKind, path: TypePath, left: TypeId, right: TypeId) -> Self {
        Self {
            kind,
            path,
            left: UnifyTarget::Type(left),
            right: UnifyTarget::Type(right),
        }
    }

    fn pack_error(
        kind: UnifyErrorKind,
        path: TypePath,
        left: TypePackId,
        right: TypePackId,
    ) -> Self {
        Self {
            kind,
            path,
            left: UnifyTarget::Pack(left),
            right: UnifyTarget::Pack(right),
        }
    }
}

/// Arena-mutating unifier.
pub struct Unifier<'a> {
    arena: &'a mut Arena,
    seen_unify_types: BTreeSet<(TypeId, TypeId)>,
    seen_unify_packs: BTreeSet<(TypePackId, TypePackId)>,
    seen_constraint_types: BTreeSet<(TypeId, TypeId)>,
    seen_constraint_packs: BTreeSet<(TypePackId, TypePackId)>,
    /// Remaining unification complexity budget (number of type nodes visited).
    /// None = unlimited (production default). Low values used by tests to
    /// force early bail matching upstream low-limit fuzzer cases.
    remaining_complexity: Option<usize>,
}

impl<'a> Unifier<'a> {
    /// Creates a unifier over a mutable type arena (unlimited complexity budget).
    pub fn new(arena: &'a mut Arena) -> Self {
        Self::with_complexity_budget(arena, None)
    }

    /// Creates a unifier with an explicit remaining complexity budget.
    /// When the budget is exhausted during recursive unification, a
    /// `ComplexityExceeded` error is surfaced instead of continuing (or hanging).
    pub fn with_complexity_budget(arena: &'a mut Arena, budget: Option<usize>) -> Self {
        Self {
            arena,
            seen_unify_types: BTreeSet::new(),
            seen_unify_packs: BTreeSet::new(),
            seen_constraint_types: BTreeSet::new(),
            seen_constraint_packs: BTreeSet::new(),
            remaining_complexity: budget,
        }
    }

    /// Unifies two type handles, binding free variables in the arena.
    pub fn unify(&mut self, left: TypeId, right: TypeId) -> Result<(), UnifyError> {
        self.unify_type(left, right, TypePath::new())
    }

    /// Unifies two type-pack handles, binding free pack variables in the arena.
    pub fn unify_pack(&mut self, left: TypePackId, right: TypePackId) -> Result<(), UnifyError> {
        self.consume_complexity()?;
        self.unify_type_pack(left, right, TypePath::new())
    }

    /// Constrains `left` to be a subtype of `right`, updating free-variable
    /// bounds instead of eagerly binding them to concrete nodes.
    pub fn constrain_subtype(&mut self, left: TypeId, right: TypeId) -> Result<(), UnifyError> {
        self.constrain_type(left, right, TypePath::new())
    }

    /// Constrains `left` to be a subtype of `right` as a type pack.
    pub fn constrain_pack_subtype(
        &mut self,
        left: TypePackId,
        right: TypePackId,
    ) -> Result<(), UnifyError> {
        self.consume_complexity()?;
        self.constrain_type_pack(left, right, TypePath::new())
    }

    fn consume_complexity(&mut self) -> Result<(), UnifyError> {
        if let Some(rem) = &mut self.remaining_complexity {
            if *rem == 0 {
                return Err(UnifyError::type_error(
                    UnifyErrorKind::ComplexityExceeded,
                    TypePath::new(),
                    // The concrete ids are not known at the guard point; the
                    // caller context will still have useful location info via
                    // the surrounding constraint.
                    self.arena.primitives().error,
                    self.arena.primitives().error,
                ));
            }
            *rem -= 1;
        }
        Ok(())
    }

    fn unify_type(
        &mut self,
        left: TypeId,
        right: TypeId,
        path: TypePath,
    ) -> Result<(), UnifyError> {
        self.consume_complexity()?;
        let left = self.reduce_type_function_instance(left);
        let right = self.reduce_type_function_instance(right);
        if left == right {
            return Ok(());
        }
        if !self.seen_unify_types.insert((left, right)) {
            return Ok(());
        }

        let left_kind = self.arena.get(left).clone();
        let right_kind = self.arena.get(right).clone();

        match (left_kind, right_kind) {
            (TypeKind::Error | TypeKind::Any | TypeKind::Unknown | TypeKind::Blocked(_), _)
            | (_, TypeKind::Error | TypeKind::Any | TypeKind::Unknown | TypeKind::Blocked(_)) => {
                Ok(())
            }
            (TypeKind::Free(_), _) => self.bind_free_type(left, right, path),
            (_, TypeKind::Free(_)) => self.bind_free_type(right, left, path),
            (TypeKind::Primitive(left), TypeKind::Primitive(right)) if left == right => Ok(()),
            (TypeKind::Singleton(left), TypeKind::Singleton(right)) if left == right => Ok(()),
            (TypeKind::Singleton(singleton), TypeKind::Primitive(primitive))
            | (TypeKind::Primitive(primitive), TypeKind::Singleton(singleton))
                if singleton.primitive() == primitive =>
            {
                Ok(())
            }
            (TypeKind::Never, TypeKind::Never) => Ok(()),
            (TypeKind::Generic(left), TypeKind::Generic(right)) if left == right => Ok(()),
            (TypeKind::Extern { name: left, .. }, TypeKind::Extern { name: right, .. })
                if left == right =>
            {
                Ok(())
            }
            (TypeKind::Function(left), TypeKind::Function(right)) => {
                self.unify_function(&left, &right, path)
            }
            (TypeKind::Table(left_kind), TypeKind::Table(right_kind)) => {
                self.unify_table(left, right, left_kind, right_kind, path)
            }
            (
                TypeKind::Metatable {
                    table: left_table,
                    metatable: left_metatable,
                    name: _,
                },
                TypeKind::Metatable {
                    table: right_table,
                    metatable: right_metatable,
                    name: _,
                },
            ) => self.unify_metatable_parts(
                (left_table, left_metatable),
                (right_table, right_metatable),
                &path,
            ),
            (
                TypeKind::Metatable {
                    table: left_table,
                    metatable: left_metatable,
                    name: _,
                },
                TypeKind::TypeFunctionInstance {
                    name: right_name,
                    arguments: right_arguments,
                },
            ) if let Some((right_table, right_metatable)) =
                setmetatable_type_function_arguments(&right_name, &right_arguments) =>
            {
                self.unify_metatable_parts(
                    (left_table, left_metatable),
                    (right_table, right_metatable),
                    &path,
                )
            }
            (
                TypeKind::TypeFunctionInstance {
                    name: left_name,
                    arguments: left_arguments,
                },
                TypeKind::Metatable {
                    table: right_table,
                    metatable: right_metatable,
                    name: _,
                },
            ) if let Some((left_table, left_metatable)) =
                setmetatable_type_function_arguments(&left_name, &left_arguments) =>
            {
                self.unify_metatable_parts(
                    (left_table, left_metatable),
                    (right_table, right_metatable),
                    &path,
                )
            }
            (
                TypeKind::TypeFunctionInstance {
                    name: left_name,
                    arguments: left_arguments,
                },
                TypeKind::TypeFunctionInstance {
                    name: right_name,
                    arguments: right_arguments,
                },
            ) if left_name == right_name => {
                self.unify_type_list(left_arguments, right_arguments, path, left, right)
            }
            (TypeKind::Union(options), _)
                if matches!(self.arena.get(right), TypeKind::Metatable { .. })
                    && options
                        .iter()
                        .any(|option| self.arena.follow(*option) == right) =>
            {
                Ok(())
            }
            (_, TypeKind::Union(options))
                if matches!(self.arena.get(left), TypeKind::Metatable { .. })
                    && options
                        .iter()
                        .any(|option| self.arena.follow(*option) == left) =>
            {
                Ok(())
            }
            (TypeKind::Union(lefts), TypeKind::Union(rights))
            | (TypeKind::Intersection(lefts), TypeKind::Intersection(rights)) => {
                self.unify_type_list(lefts, rights, path, left, right)
            }
            (TypeKind::Negation(left), TypeKind::Negation(right)) => self.unify_type(
                left,
                right,
                path.push(TypePathComponent::TypeField(TypeField::Negated)),
            ),
            _ => Err(UnifyError::type_error(
                UnifyErrorKind::Mismatch,
                path,
                left,
                right,
            )),
        }
    }

    fn constrain_type(
        &mut self,
        left: TypeId,
        right: TypeId,
        path: TypePath,
    ) -> Result<(), UnifyError> {
        self.consume_complexity()?;
        let left = self.reduce_type_function_instance(left);
        let right = self.reduce_type_function_instance(right);
        if left == right {
            return Ok(());
        }
        if !self.seen_constraint_types.insert((left, right)) {
            return Ok(());
        }

        let left_kind = self.arena.get(left).clone();
        let right_kind = self.arena.get(right).clone();

        match (left_kind, right_kind) {
            (TypeKind::Error | TypeKind::Any | TypeKind::Blocked(_), _)
            | (_, TypeKind::Error | TypeKind::Any | TypeKind::Unknown | TypeKind::Blocked(_))
            | (TypeKind::Never, _) => Ok(()),
            (TypeKind::Free(_), TypeKind::Free(_)) => {
                self.add_free_upper_bound(left, right, path.clone())?;
                self.add_free_lower_bound(right, left, path)
            }
            (TypeKind::Free(_), _) => self.add_free_upper_bound(left, right, path),
            (_, TypeKind::Free(_)) => self.add_free_lower_bound(right, left, path),
            (TypeKind::Primitive(left), TypeKind::Primitive(right)) if left == right => Ok(()),
            (TypeKind::Singleton(left), TypeKind::Singleton(right)) if left == right => Ok(()),
            (TypeKind::Singleton(singleton), TypeKind::Primitive(primitive))
                if singleton.primitive() == primitive =>
            {
                Ok(())
            }
            (TypeKind::Generic(left), TypeKind::Generic(right)) if left == right => Ok(()),
            (TypeKind::Generic(_), _) | (_, TypeKind::Generic(_)) => Ok(()),
            (TypeKind::Extern { name: left, .. }, TypeKind::Extern { name: right, .. })
                if left == right =>
            {
                Ok(())
            }
            (TypeKind::Function(left), TypeKind::Function(right)) => {
                self.constrain_type_pack(
                    right.arguments,
                    left.arguments,
                    path.push(TypePathComponent::PackField(PackField::Arguments)),
                )?;
                self.constrain_type_pack(
                    left.returns,
                    right.returns,
                    path.push(TypePathComponent::PackField(PackField::Returns)),
                )
            }
            (TypeKind::Union(options), _) => {
                for (index, option) in options.into_iter().enumerate() {
                    self.constrain_type(
                        option,
                        right,
                        path.push(TypePathComponent::Index { index }),
                    )?;
                }
                Ok(())
            }
            (_, TypeKind::Union(options)) => {
                if let Some(free) = options.iter().copied().find(|option| {
                    matches!(
                        self.arena.get(self.arena.follow(*option)),
                        TypeKind::Free(_)
                    )
                }) {
                    return self.add_free_lower_bound(free, left, path);
                }
                options
                    .into_iter()
                    .enumerate()
                    .find_map(|(index, option)| {
                        self.constrain_type(
                            left,
                            option,
                            path.push(TypePathComponent::Index { index }),
                        )
                        .ok()
                    })
                    .map_or_else(
                        || {
                            Err(UnifyError::type_error(
                                UnifyErrorKind::Mismatch,
                                path,
                                left,
                                right,
                            ))
                        },
                        |_| Ok(()),
                    )
            }
            // An uninhabited intersection (e.g. `string & number`) is `never`,
            // which is a subtype of everything, so it constrains to any target.
            (TypeKind::Intersection(_), _)
                if crate::subtype::definitely_uninhabited_type(self.arena, left) =>
            {
                Ok(())
            }
            (TypeKind::Intersection(options), _) => {
                if let Some(free) = options.iter().copied().find(|option| {
                    matches!(
                        self.arena.get(self.arena.follow(*option)),
                        TypeKind::Free(_)
                    )
                }) {
                    return self.add_free_upper_bound(free, right, path);
                }
                options
                    .into_iter()
                    .enumerate()
                    .find_map(|(index, option)| {
                        self.constrain_type(
                            option,
                            right,
                            path.push(TypePathComponent::Index { index }),
                        )
                        .ok()
                    })
                    .map_or_else(
                        || {
                            Err(UnifyError::type_error(
                                UnifyErrorKind::Mismatch,
                                path,
                                left,
                                right,
                            ))
                        },
                        |_| Ok(()),
                    )
            }
            (_, TypeKind::Intersection(options)) => {
                for (index, option) in options.into_iter().enumerate() {
                    self.constrain_type(
                        left,
                        option,
                        path.push(TypePathComponent::Index { index }),
                    )?;
                }
                Ok(())
            }
            (TypeKind::Table(left_kind), TypeKind::Table(right_kind)) => {
                self.constrain_table(left, right, left_kind, right_kind, path)
            }
            (
                TypeKind::Metatable {
                    table: left_table,
                    metatable: left_metatable,
                    name: _,
                },
                TypeKind::Metatable {
                    table: right_table,
                    metatable: right_metatable,
                    name: _,
                },
            ) => {
                self.constrain_type(
                    left_table,
                    right_table,
                    path.push(TypePathComponent::TypeField(TypeField::Table)),
                )?;
                self.constrain_type(
                    left_metatable,
                    right_metatable,
                    path.push(TypePathComponent::TypeField(TypeField::Metatable)),
                )
            }
            (TypeKind::Negation(left), TypeKind::Negation(right)) => self.constrain_type(
                right,
                left,
                path.push(TypePathComponent::TypeField(TypeField::Negated)),
            ),
            _ => Err(UnifyError::type_error(
                UnifyErrorKind::Mismatch,
                path,
                left,
                right,
            )),
        }
    }

    fn reduce_type_function_instance(&mut self, id: TypeId) -> TypeId {
        let id = self.arena.follow(id);
        let TypeKind::TypeFunctionInstance { name, arguments } = self.arena.get(id).clone() else {
            return id;
        };
        match TypeFunctionRuntime::new().reduce_allocating(self.arena, &name, &arguments) {
            Reduction::Reduced(reduced) if reduced != id => self.arena.follow(reduced),
            Reduction::Reduced(_) | Reduction::Pending => id,
        }
    }

    fn constrain_type_pack(
        &mut self,
        left: TypePackId,
        right: TypePackId,
        path: TypePath,
    ) -> Result<(), UnifyError> {
        let left = self.arena.follow_pack(left);
        let right = self.arena.follow_pack(right);
        if left == right {
            return Ok(());
        }
        if !self.seen_constraint_packs.insert((left, right)) {
            return Ok(());
        }

        let left_kind = self.arena.get_pack(left).clone();
        let right_kind = self.arena.get_pack(right).clone();

        match (left_kind, right_kind) {
            (TypePackKind::Error, _) | (_, TypePackKind::Error) => Ok(()),
            (TypePackKind::Free { .. }, _) => self.bind_free_pack(left, right, path),
            (_, TypePackKind::Free { .. }) => self.bind_free_pack(right, left, path),
            (TypePackKind::Generic(left), TypePackKind::Generic(right)) if left == right => Ok(()),
            (TypePackKind::Generic(_), _) | (_, TypePackKind::Generic(_)) => Ok(()),
            (TypePackKind::Variadic { ty: left }, TypePackKind::Variadic { ty: right }) => self
                .constrain_type(
                    left,
                    right,
                    path.push(TypePathComponent::TypeField(TypeField::Variadic)),
                ),
            (
                TypePackKind::List {
                    types: left_types,
                    tail: left_tail,
                },
                TypePackKind::List {
                    types: right_types,
                    tail: right_tail,
                },
            ) => self.constrain_list_pack(
                &self
                    .arena
                    .flatten_list_pack_from_parts(left, left_types, left_tail),
                &self
                    .arena
                    .flatten_list_pack_from_parts(right, right_types, right_tail),
                path,
            ),
            (
                TypePackKind::List {
                    types: left_types,
                    tail: None,
                },
                TypePackKind::Variadic { ty: right_ty },
            ) => {
                for (index, left_ty) in left_types.into_iter().enumerate() {
                    self.constrain_type(
                        left_ty,
                        right_ty,
                        path.push(TypePathComponent::Index { index }),
                    )?;
                }
                Ok(())
            }
            (
                TypePackKind::Variadic { .. },
                TypePackKind::List {
                    types: right_types,
                    tail: right_tail,
                },
            ) if right_types.is_empty() && right_tail.is_none() => Ok(()),
            (TypePackKind::Bound(_), _) | (_, TypePackKind::Bound(_)) => {
                unreachable!("follow_pack removes bound packs")
            }
            _ => Err(UnifyError::pack_error(
                UnifyErrorKind::Mismatch,
                path,
                left,
                right,
            )),
        }
    }

    fn constrain_list_pack(
        &mut self,
        left: &FlattenedListPack,
        right: &FlattenedListPack,
        path: TypePath,
    ) -> Result<(), UnifyError> {
        let common_len = left.types.len().min(right.types.len());
        for index in 0..common_len {
            self.constrain_type(
                left.types[index],
                right.types[index],
                path.push(TypePathComponent::Index { index }),
            )?;
        }

        match left.types.len().cmp(&right.types.len()) {
            std::cmp::Ordering::Equal => match (left.tail, right.tail) {
                (Some(left_tail), Some(right_tail)) => {
                    self.constrain_type_pack(left_tail, right_tail, tail_path(&path))
                }
                (Some(left_tail), None) => {
                    self.constrain_tail_to_empty(left_tail, path, left.id, right.id)
                }
                (None, Some(right_tail)) => self.constrain_empty_to_tail(right_tail, path),
                (None, None) => Ok(()),
            },
            std::cmp::Ordering::Greater => {
                let Some(right_tail) = right.tail else {
                    if path.ends_in_function_arguments() {
                        return Ok(());
                    }
                    return Err(UnifyError::pack_error(
                        UnifyErrorKind::ArityMismatch,
                        path,
                        left.id,
                        right.id,
                    ));
                };
                self.constrain_extra_types_to_tail(
                    &left.types[common_len..],
                    left.tail,
                    right_tail,
                    path,
                    left.id,
                    right.id,
                )
            }
            std::cmp::Ordering::Less => {
                let Some(left_tail) = left.tail else {
                    return Err(UnifyError::pack_error(
                        UnifyErrorKind::ArityMismatch,
                        path,
                        left.id,
                        right.id,
                    ));
                };
                self.constrain_tail_to_extra_types(
                    left_tail,
                    &right.types[common_len..],
                    right.tail,
                    path,
                    left.id,
                    right.id,
                )
            }
        }
    }

    fn constrain_tail_to_empty(
        &mut self,
        tail: TypePackId,
        path: TypePath,
        left: TypePackId,
        right: TypePackId,
    ) -> Result<(), UnifyError> {
        let tail = self.arena.follow_pack(tail);
        match self.arena.get_pack(tail).clone() {
            TypePackKind::Free { .. } => {
                self.bind_free_pack(tail, self.arena.empty_pack(), tail_path(&path))
            }
            TypePackKind::Generic(_) | TypePackKind::Error => Ok(()),
            TypePackKind::Variadic { ty }
                if self.arena.follow(ty) == self.arena.primitives().any
                    && path.ends_in_function_arguments() =>
            {
                Ok(())
            }
            TypePackKind::Bound(_) => unreachable!("follow_pack removes bound packs"),
            TypePackKind::Variadic { .. } | TypePackKind::List { .. } => Err(
                UnifyError::pack_error(UnifyErrorKind::ArityMismatch, path, left, right),
            ),
        }
    }

    fn constrain_empty_to_tail(
        &mut self,
        tail: TypePackId,
        path: TypePath,
    ) -> Result<(), UnifyError> {
        let tail = self.arena.follow_pack(tail);
        match self.arena.get_pack(tail).clone() {
            TypePackKind::Free { .. } => {
                self.bind_free_pack(tail, self.arena.empty_pack(), tail_path(&path))
            }
            TypePackKind::Generic(_) | TypePackKind::Error => Ok(()),
            TypePackKind::Variadic { ty }
                if self.arena.follow(ty) == self.arena.primitives().any =>
            {
                Ok(())
            }
            TypePackKind::Bound(_) => unreachable!("follow_pack removes bound packs"),
            TypePackKind::Variadic { .. } | TypePackKind::List { .. } => {
                Err(UnifyError::pack_error(
                    UnifyErrorKind::ArityMismatch,
                    path,
                    self.arena.empty_pack(),
                    tail,
                ))
            }
        }
    }

    fn constrain_extra_types_to_tail(
        &mut self,
        extra_types: &[TypeId],
        extra_tail: Option<TypePackId>,
        target_tail: TypePackId,
        path: TypePath,
        left: TypePackId,
        right: TypePackId,
    ) -> Result<(), UnifyError> {
        let target_tail = self.arena.follow_pack(target_tail);
        match self.arena.get_pack(target_tail).clone() {
            TypePackKind::Free { .. } => {
                let target = if extra_types.is_empty() {
                    extra_tail.unwrap_or_else(|| self.arena.empty_pack())
                } else {
                    self.arena.alloc_pack(TypePackKind::List {
                        types: extra_types.to_vec(),
                        tail: extra_tail,
                    })
                };
                self.bind_free_pack(target_tail, target, tail_path(&path))
            }
            TypePackKind::Generic(_) | TypePackKind::Error => Ok(()),
            TypePackKind::Variadic { ty } => {
                for (offset, extra) in extra_types.iter().copied().enumerate() {
                    self.constrain_type(
                        extra,
                        ty,
                        path.push(TypePathComponent::Index { index: offset }),
                    )?;
                }
                if let Some(extra_tail) = extra_tail {
                    self.constrain_type_pack(extra_tail, target_tail, tail_path(&path))?;
                }
                Ok(())
            }
            TypePackKind::Bound(_) => unreachable!("follow_pack removes bound packs"),
            TypePackKind::List { .. } => Err(UnifyError::pack_error(
                UnifyErrorKind::ArityMismatch,
                path,
                left,
                right,
            )),
        }
    }

    fn constrain_tail_to_extra_types(
        &mut self,
        tail: TypePackId,
        extra_types: &[TypeId],
        extra_tail: Option<TypePackId>,
        path: TypePath,
        left: TypePackId,
        right: TypePackId,
    ) -> Result<(), UnifyError> {
        let tail = self.arena.follow_pack(tail);
        match self.arena.get_pack(tail).clone() {
            TypePackKind::Free { .. } => {
                let target = self.arena.alloc_pack(TypePackKind::List {
                    types: extra_types.to_vec(),
                    tail: extra_tail,
                });
                self.bind_free_pack(tail, target, tail_path(&path))
            }
            TypePackKind::Generic(_) | TypePackKind::Error => Ok(()),
            TypePackKind::Bound(_) => unreachable!("follow_pack removes bound packs"),
            TypePackKind::Variadic { .. } | TypePackKind::List { .. } => Err(
                UnifyError::pack_error(UnifyErrorKind::ArityMismatch, path, left, right),
            ),
        }
    }

    fn unify_function(
        &mut self,
        left: &FunctionType,
        right: &FunctionType,
        path: TypePath,
    ) -> Result<(), UnifyError> {
        // Argument-name metadata is intentionally not compared here:
        // upstream and the subtyper treat names as display-only, so a
        // function like `(x: number) -> ()` unifies with
        // `(a: number) -> ()`. Only the type-level shape (generics,
        // packs, self-binding, checked-mode flag) is load-bearing.
        if left.generics != right.generics
            || left.generic_packs != right.generic_packs
            || left.has_self != right.has_self
            || left.is_checked != right.is_checked
        {
            return Err(UnifyError {
                kind: UnifyErrorKind::Mismatch,
                path,
                left: UnifyTarget::Pack(left.arguments),
                right: UnifyTarget::Pack(right.arguments),
            });
        }
        self.unify_type_pack(
            left.arguments,
            right.arguments,
            path.push(TypePathComponent::PackField(PackField::Arguments)),
        )?;
        self.unify_type_pack(
            left.returns,
            right.returns,
            path.push(TypePathComponent::PackField(PackField::Returns)),
        )
    }

    fn unify_table(
        &mut self,
        left_id: TypeId,
        right_id: TypeId,
        left: TableType,
        right: TableType,
        path: TypePath,
    ) -> Result<(), UnifyError> {
        if left.state != right.state || left.indexer.is_some() != right.indexer.is_some() {
            return Err(UnifyError::type_error(
                UnifyErrorKind::PropertySetMismatch,
                path,
                left_id,
                right_id,
            ));
        }

        // Two instances of the same named generic alias are equal exactly when
        // their type arguments are: the alias body is a function of those
        // arguments. Unifying just the arguments settles the instances and binds
        // any shared free variables their bodies reference, without re-unifying
        // the (recursive, method-bearing) body — which can otherwise diverge
        // across separately-lowered copies whose receiver/self types differ only
        // structurally.
        if same_alias_identity_table_arity(&left, &right)
            && (!left.instantiated_type_params.is_empty()
                || !left.instantiated_type_pack_params.is_empty())
        {
            self.unify_type_list(
                left.instantiated_type_params,
                right.instantiated_type_params,
                path,
                left_id,
                right_id,
            )?;
            for (left_pack, right_pack) in left
                .instantiated_type_pack_params
                .into_iter()
                .zip(right.instantiated_type_pack_params)
            {
                self.unify_pack(left_pack, right_pack)?;
            }
            return Ok(());
        }

        if !left.instantiated_type_params.is_empty() && !right.instantiated_type_params.is_empty() {
            self.unify_type_list(
                left.instantiated_type_params,
                right.instantiated_type_params,
                path.clone(),
                left_id,
                right_id,
            )?;
        }

        // Two free tables are open type variables being inferred; unifying them
        // merges their property sets so each observes the union, rather than
        // requiring the sets to already match. A property present on only one
        // side is therefore not a mismatch.
        let both_free = left.state == TableState::Free && right.state == TableState::Free;
        for (name, left_property) in &left.properties {
            if both_free
                || right.properties.contains_key(name)
                || self.missing_property_can_read_nil(left_property)
            {
                continue;
            }
            return Err(UnifyError::type_error(
                UnifyErrorKind::PropertySetMismatch,
                path.push(TypePathComponent::read_property(name.clone())),
                left_id,
                right_id,
            ));
        }
        for (name, right_property) in &right.properties {
            if both_free
                || left.properties.contains_key(name)
                || self.missing_property_can_read_nil(right_property)
            {
                continue;
            }
            return Err(UnifyError::type_error(
                UnifyErrorKind::PropertySetMismatch,
                path.push(TypePathComponent::read_property(name.clone())),
                left_id,
                right_id,
            ));
        }

        for (name, left_property) in left.properties {
            if let Some(right_property) = right.properties.get(&name) {
                self.unify_property(
                    &left_property,
                    right_property,
                    path.push(TypePathComponent::property(name)),
                )?;
            }
        }

        if let (Some(left_indexer), Some(right_indexer)) = (left.indexer, right.indexer) {
            self.unify_indexer(&left_indexer, &right_indexer, &path)?;
        }
        Ok(())
    }

    fn missing_property_can_read_nil(&self, property: &TableProperty) -> bool {
        member_access::missing_read_can_be_nil(self.arena, property)
    }

    fn constrain_table(
        &mut self,
        sub_id: TypeId,
        sup_id: TypeId,
        sub: TableType,
        sup: TableType,
        path: TypePath,
    ) -> Result<(), UnifyError> {
        if !compatible_table_state(sub.state, sup.state)
            || (sup.indexer.is_some() && sub.indexer.is_none())
        {
            return Err(UnifyError::type_error(
                UnifyErrorKind::Mismatch,
                path,
                sub_id,
                sup_id,
            ));
        }

        if !sub.instantiated_type_params.is_empty() && !sup.instantiated_type_params.is_empty() {
            self.unify_type_list(
                sub.instantiated_type_params,
                sup.instantiated_type_params,
                path.clone(),
                sub_id,
                sup_id,
            )?;
        }

        for (name, sup_property) in sup.properties {
            let sub_property = if let Some(sub_property) = sub.properties.get(&name) {
                sub_property.clone()
            } else if let Some(sub_indexer) = &sub.indexer
                && member_access::property_name_matches_key(self.arena, &name, sub_indexer.key)
            {
                TableProperty {
                    ty: sub_indexer.value,
                    write_ty: None,
                    location: None,
                    documentation_symbol: None,
                    read_only: sub_indexer.read_only,
                    write_only: false,
                    deprecated: false,
                }
            } else {
                if self.missing_property_can_read_nil(&sup_property) {
                    continue;
                }
                return Err(UnifyError::type_error(
                    UnifyErrorKind::PropertySetMismatch,
                    path.push(TypePathComponent::read_property(name)),
                    sub_id,
                    sup_id,
                ));
            };
            self.constrain_property(
                &sub_property,
                &sup_property,
                path.push(TypePathComponent::property(name)),
            )?;
        }

        if let (Some(sub_indexer), Some(sup_indexer)) = (sub.indexer, sup.indexer) {
            self.constrain_indexer(&sub_indexer, &sup_indexer, &path)?;
        }
        Ok(())
    }

    fn unify_property(
        &mut self,
        left: &TableProperty,
        right: &TableProperty,
        path: TypePath,
    ) -> Result<(), UnifyError> {
        if left.read_only != right.read_only
            || left.write_only != right.write_only
            || left.deprecated != right.deprecated
        {
            return Err(UnifyError {
                kind: UnifyErrorKind::PropertyMetadataMismatch,
                path,
                left: UnifyTarget::Type(left.ty),
                right: UnifyTarget::Type(right.ty),
            });
        }
        self.unify_type(left.ty, right.ty, path)
    }

    fn constrain_property(
        &mut self,
        sub: &TableProperty,
        sup: &TableProperty,
        path: TypePath,
    ) -> Result<(), UnifyError> {
        if sub.deprecated != sup.deprecated {
            return Err(UnifyError {
                kind: UnifyErrorKind::PropertyMetadataMismatch,
                path,
                left: UnifyTarget::Type(sub.ty),
                right: UnifyTarget::Type(sup.ty),
            });
        }
        if sub.read_only || sup.read_only || sub.write_only || sup.write_only {
            self.constrain_type(sub.ty, sup.ty, path)
        } else {
            self.unify_type(sub.ty, sup.ty, path)
        }
    }

    fn unify_indexer(
        &mut self,
        left: &TableIndexer,
        right: &TableIndexer,
        path: &TypePath,
    ) -> Result<(), UnifyError> {
        self.unify_type(
            left.key,
            right.key,
            path.push(TypePathComponent::TypeField(TypeField::IndexLookup)),
        )?;
        self.unify_type(
            left.value,
            right.value,
            path.push(TypePathComponent::TypeField(TypeField::IndexResult)),
        )
    }

    fn constrain_indexer(
        &mut self,
        sub: &TableIndexer,
        sup: &TableIndexer,
        path: &TypePath,
    ) -> Result<(), UnifyError> {
        self.unify_type(
            sub.key,
            sup.key,
            path.push(TypePathComponent::TypeField(TypeField::IndexLookup)),
        )?;
        self.constrain_type(
            sub.value,
            sup.value,
            path.push(TypePathComponent::TypeField(TypeField::IndexResult)),
        )
    }

    /// Unifies a metatable-shaped pair part-wise: table with table under the
    /// `Table` path component, then metatable with metatable under
    /// `Metatable`. Shared by the Metatable/Metatable arm and the two
    /// `setmetatable` type-function bridging arms.
    fn unify_metatable_parts(
        &mut self,
        (left_table, left_metatable): (TypeId, TypeId),
        (right_table, right_metatable): (TypeId, TypeId),
        path: &TypePath,
    ) -> Result<(), UnifyError> {
        self.unify_type(
            left_table,
            right_table,
            path.push(TypePathComponent::TypeField(TypeField::Table)),
        )?;
        self.unify_type(
            left_metatable,
            right_metatable,
            path.push(TypePathComponent::TypeField(TypeField::Metatable)),
        )
    }

    fn unify_type_list(
        &mut self,
        lefts: Vec<TypeId>,
        rights: Vec<TypeId>,
        path: TypePath,
        left: TypeId,
        right: TypeId,
    ) -> Result<(), UnifyError> {
        if lefts.len() != rights.len() {
            return Err(UnifyError::type_error(
                UnifyErrorKind::ArityMismatch,
                path,
                left,
                right,
            ));
        }
        for (index, (left, right)) in lefts.into_iter().zip(rights).enumerate() {
            self.unify_type(left, right, path.push(TypePathComponent::Index { index }))?;
        }
        Ok(())
    }

    fn unify_type_pack(
        &mut self,
        left: TypePackId,
        right: TypePackId,
        path: TypePath,
    ) -> Result<(), UnifyError> {
        let left = self.arena.follow_pack(left);
        let right = self.arena.follow_pack(right);
        if left == right {
            return Ok(());
        }
        if !self.seen_unify_packs.insert((left, right)) {
            return Ok(());
        }

        let left_kind = self.arena.get_pack(left).clone();
        let right_kind = self.arena.get_pack(right).clone();

        match (left_kind, right_kind) {
            (TypePackKind::Error, _) | (_, TypePackKind::Error) => Ok(()),
            (TypePackKind::Free { .. }, _) => self.bind_free_pack(left, right, path),
            (_, TypePackKind::Free { .. }) => self.bind_free_pack(right, left, path),
            (TypePackKind::Generic(left), TypePackKind::Generic(right)) if left == right => Ok(()),
            (TypePackKind::Bound(_), _) | (_, TypePackKind::Bound(_)) => {
                unreachable!("follow_pack removes bound packs")
            }
            (TypePackKind::Variadic { ty: left }, TypePackKind::Variadic { ty: right }) => self
                .unify_type(
                    left,
                    right,
                    path.push(TypePathComponent::TypeField(TypeField::Variadic)),
                ),
            (
                TypePackKind::List {
                    types: left_types,
                    tail: left_tail,
                },
                TypePackKind::List {
                    types: right_types,
                    tail: right_tail,
                },
            ) => self.unify_list_pack(
                &self
                    .arena
                    .flatten_list_pack_from_parts(left, left_types, left_tail),
                &self
                    .arena
                    .flatten_list_pack_from_parts(right, right_types, right_tail),
                path,
            ),
            _ => Err(UnifyError::pack_error(
                UnifyErrorKind::Mismatch,
                path,
                left,
                right,
            )),
        }
    }

    fn unify_list_pack(
        &mut self,
        left: &FlattenedListPack,
        right: &FlattenedListPack,
        path: TypePath,
    ) -> Result<(), UnifyError> {
        let common_len = left.types.len().min(right.types.len());
        for index in 0..common_len {
            self.unify_type(
                left.types[index],
                right.types[index],
                path.push(TypePathComponent::Index { index }),
            )?;
        }

        match left.types.len().cmp(&right.types.len()) {
            std::cmp::Ordering::Equal => match (left.tail, right.tail) {
                (Some(left_tail), Some(right_tail)) => self.unify_type_pack(
                    left_tail,
                    right_tail,
                    path.push(TypePathComponent::PackField(PackField::Tail)),
                ),
                (Some(left_tail), None) => self.unify_tail_with_list(
                    left_tail,
                    Vec::new(),
                    None,
                    path.push(TypePathComponent::PackField(PackField::Tail)),
                    left.id,
                    right.id,
                ),
                (None, Some(right_tail)) => self.unify_tail_with_list(
                    right_tail,
                    Vec::new(),
                    None,
                    path.push(TypePathComponent::PackField(PackField::Tail)),
                    right.id,
                    left.id,
                ),
                (None, None) => Ok(()),
            },
            std::cmp::Ordering::Greater => {
                let Some(right_tail) = right.tail else {
                    return Err(UnifyError::pack_error(
                        UnifyErrorKind::ArityMismatch,
                        path,
                        left.id,
                        right.id,
                    ));
                };
                self.unify_tail_with_list(
                    right_tail,
                    left.types[common_len..].to_vec(),
                    left.tail,
                    path.push(TypePathComponent::PackField(PackField::Tail)),
                    right.id,
                    left.id,
                )
            }
            std::cmp::Ordering::Less => {
                let Some(left_tail) = left.tail else {
                    return Err(UnifyError::pack_error(
                        UnifyErrorKind::ArityMismatch,
                        path,
                        left.id,
                        right.id,
                    ));
                };
                self.unify_tail_with_list(
                    left_tail,
                    right.types[common_len..].to_vec(),
                    right.tail,
                    path.push(TypePathComponent::PackField(PackField::Tail)),
                    left.id,
                    right.id,
                )
            }
        }
    }

    fn unify_tail_with_list(
        &mut self,
        tail: TypePackId,
        remaining_types: Vec<TypeId>,
        remaining_tail: Option<TypePackId>,
        path: TypePath,
        tail_owner: TypePackId,
        list_owner: TypePackId,
    ) -> Result<(), UnifyError> {
        let tail = self.arena.follow_pack(tail);
        match self.arena.get_pack(tail).clone() {
            TypePackKind::Free { .. } => {
                let target = if remaining_types.is_empty() {
                    remaining_tail.unwrap_or_else(|| self.arena.empty_pack())
                } else {
                    self.arena.alloc_pack(TypePackKind::List {
                        types: remaining_types,
                        tail: remaining_tail,
                    })
                };
                self.bind_free_pack(tail, target, path)
            }
            TypePackKind::Variadic { ty } => {
                for (index, remaining) in remaining_types.into_iter().enumerate() {
                    self.unify_type(ty, remaining, path.push(TypePathComponent::Index { index }))?;
                }
                if let Some(remaining_tail) = remaining_tail {
                    self.unify_type_pack(remaining_tail, tail, path)
                } else {
                    Ok(())
                }
            }
            TypePackKind::Bound(_) => unreachable!("follow_pack removes bound packs"),
            TypePackKind::Error => Ok(()),
            TypePackKind::List { .. } | TypePackKind::Generic(_) => Err(UnifyError::pack_error(
                UnifyErrorKind::ArityMismatch,
                path,
                tail_owner,
                list_owner,
            )),
        }
    }

    fn bind_free_type(
        &mut self,
        free: TypeId,
        target: TypeId,
        path: TypePath,
    ) -> Result<(), UnifyError> {
        if self.type_occurs_in_type(free, target, &mut BTreeSet::new()) {
            return Err(UnifyError::type_error(
                UnifyErrorKind::OccursCheck,
                path,
                free,
                target,
            ));
        }
        self.arena.replace(free, TypeKind::Bound(target));
        Ok(())
    }

    fn add_free_upper_bound(
        &mut self,
        free: TypeId,
        upper: TypeId,
        path: TypePath,
    ) -> Result<(), UnifyError> {
        if !matches!(self.arena.get(self.arena.follow(upper)), TypeKind::Free(_))
            && self.type_occurs_in_type(free, upper, &mut BTreeSet::new())
        {
            return Err(UnifyError::type_error(
                UnifyErrorKind::OccursCheck,
                path,
                free,
                upper,
            ));
        }
        let TypeKind::Free(mut variable) = self.arena.get(free).clone() else {
            return self.constrain_type(free, upper, path);
        };
        variable.upper_bound = Some(match variable.upper_bound {
            Some(existing) if existing != upper => self
                .arena
                .alloc(TypeKind::Intersection(vec![existing, upper])),
            Some(existing) => existing,
            None => upper,
        });
        self.arena.replace(free, TypeKind::Free(variable));
        Ok(())
    }

    fn add_free_lower_bound(
        &mut self,
        free: TypeId,
        lower: TypeId,
        path: TypePath,
    ) -> Result<(), UnifyError> {
        if !matches!(self.arena.get(self.arena.follow(lower)), TypeKind::Free(_))
            && self.type_occurs_in_type(free, lower, &mut BTreeSet::new())
        {
            return Err(UnifyError::type_error(
                UnifyErrorKind::OccursCheck,
                path,
                lower,
                free,
            ));
        }
        let TypeKind::Free(mut variable) = self.arena.get(free).clone() else {
            return self.constrain_type(lower, free, path);
        };
        variable.lower_bound = Some(match variable.lower_bound {
            Some(existing) if existing != lower => {
                self.arena.alloc(TypeKind::Union(vec![existing, lower]))
            }
            Some(existing) => existing,
            None => lower,
        });
        self.arena.replace(free, TypeKind::Free(variable));
        Ok(())
    }

    fn bind_free_pack(
        &mut self,
        free: TypePackId,
        target: TypePackId,
        path: TypePath,
    ) -> Result<(), UnifyError> {
        if self.pack_occurs_in_pack(free, target, &mut BTreeSet::new()) {
            return Err(UnifyError::pack_error(
                UnifyErrorKind::OccursCheck,
                path,
                free,
                target,
            ));
        }
        self.arena.replace_pack(free, TypePackKind::Bound(target));
        Ok(())
    }

    fn type_occurs_in_type(
        &self,
        needle: TypeId,
        haystack: TypeId,
        seen: &mut BTreeSet<TypeId>,
    ) -> bool {
        self.type_occurs_in_type_guarded(needle, haystack, seen, &mut BTreeSet::new(), false)
    }

    fn type_occurs_in_type_guarded(
        &self,
        needle: TypeId,
        haystack: TypeId,
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
        guarded: bool,
    ) -> bool {
        let haystack = self.arena.follow(haystack);
        if needle == haystack {
            return !guarded;
        }
        if !seen_types.insert(haystack) {
            return false;
        }
        match self.arena.get(haystack).clone() {
            TypeKind::Function(function) => {
                self.type_occurs_in_pack(
                    needle,
                    function.arguments,
                    seen_types,
                    seen_packs,
                    guarded,
                ) || self.type_occurs_in_pack(
                    needle,
                    function.returns,
                    seen_types,
                    seen_packs,
                    true,
                )
            }
            TypeKind::Table(table) => {
                self.type_occurs_in_table(needle, table, seen_types, seen_packs, true)
            }
            TypeKind::Metatable {
                table, metatable, ..
            } => {
                self.type_occurs_in_type_guarded(needle, table, seen_types, seen_packs, true)
                    || self.type_occurs_in_type_guarded(
                        needle, metatable, seen_types, seen_packs, true,
                    )
            }
            TypeKind::TypeFunctionInstance { arguments, .. }
            | TypeKind::Union(arguments)
            | TypeKind::Intersection(arguments) => arguments.into_iter().any(|ty| {
                self.type_occurs_in_type_guarded(needle, ty, seen_types, seen_packs, guarded)
            }),
            TypeKind::Negation(ty) => {
                self.type_occurs_in_type_guarded(needle, ty, seen_types, seen_packs, guarded)
            }
            TypeKind::Free(variable) => variable
                .lower_bound
                .into_iter()
                .chain(variable.upper_bound)
                .any(|ty| {
                    self.type_occurs_in_type_guarded(needle, ty, seen_types, seen_packs, guarded)
                }),
            TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Extern { .. }
            | TypeKind::Bound(_)
            | TypeKind::Generic(_)
            | TypeKind::Blocked(_)
            | TypeKind::Error
            | TypeKind::Unknown
            | TypeKind::Never
            | TypeKind::Any => false,
        }
    }

    fn type_occurs_in_table(
        &self,
        needle: TypeId,
        table: TableType,
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
        guarded: bool,
    ) -> bool {
        table
            .instantiated_type_params
            .into_iter()
            .any(|ty| self.type_occurs_in_type_guarded(needle, ty, seen_types, seen_packs, guarded))
            || table.properties.into_values().any(|property| {
                self.type_occurs_in_type_guarded(
                    needle,
                    property.ty,
                    seen_types,
                    seen_packs,
                    guarded,
                )
            })
            || table.indexer.is_some_and(|indexer| {
                self.type_occurs_in_type_guarded(
                    needle,
                    indexer.key,
                    seen_types,
                    seen_packs,
                    guarded,
                ) || self.type_occurs_in_type_guarded(
                    needle,
                    indexer.value,
                    seen_types,
                    seen_packs,
                    guarded,
                )
            })
    }

    fn type_occurs_in_pack(
        &self,
        needle: TypeId,
        haystack: TypePackId,
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
        guarded: bool,
    ) -> bool {
        let haystack = self.arena.follow_pack(haystack);
        if !seen_packs.insert(haystack) {
            return false;
        }
        match self.arena.get_pack(haystack).clone() {
            TypePackKind::List { types, tail } => {
                types.into_iter().any(|ty| {
                    self.type_occurs_in_type_guarded(needle, ty, seen_types, seen_packs, guarded)
                }) || tail.is_some_and(|tail| {
                    self.type_occurs_in_pack(needle, tail, seen_types, seen_packs, guarded)
                })
            }
            TypePackKind::Variadic { ty } => {
                self.type_occurs_in_type_guarded(needle, ty, seen_types, seen_packs, guarded)
            }
            TypePackKind::Bound(_)
            | TypePackKind::Free { .. }
            | TypePackKind::Generic(_)
            | TypePackKind::Error => false,
        }
    }

    fn pack_occurs_in_pack(
        &self,
        needle: TypePackId,
        haystack: TypePackId,
        seen: &mut BTreeSet<TypePackId>,
    ) -> bool {
        let haystack = self.arena.follow_pack(haystack);
        if needle == haystack {
            return true;
        }
        if !seen.insert(haystack) {
            return false;
        }
        match self.arena.get_pack(haystack).clone() {
            TypePackKind::List { tail, .. } => {
                tail.is_some_and(|tail| self.pack_occurs_in_pack(needle, tail, seen))
            }
            TypePackKind::Bound(_)
            | TypePackKind::Variadic { .. }
            | TypePackKind::Free { .. }
            | TypePackKind::Generic(_)
            | TypePackKind::Error => false,
        }
    }
}

fn compatible_table_state(sub: TableState, sup: TableState) -> bool {
    sub == sup
        || matches!(
            (sub, sup),
            (TableState::Unsealed, TableState::Sealed)
                | (TableState::Sealed, TableState::Unsealed)
                | (TableState::Free, TableState::Unsealed | TableState::Sealed)
                | (TableState::Unsealed | TableState::Sealed, TableState::Free)
        )
}

#[cfg(any())]
mod tests;
