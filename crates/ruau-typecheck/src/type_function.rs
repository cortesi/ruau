//! Type-function reduction runtime.
//!
//! `TypeFunctionRuntime` is the collaborator consulted when a
//! `TypeKind::TypeFunctionInstance { name, arguments }` node needs to
//! either reduce or report `pending`. The runtime owns the builtin
//! type-function table (`keyof`, `index`, `mul`, `add`, …).
//!
//! The module ships the runtime carrier, an enum for the possible
//! reduction outcomes, and a small builtin reduction surface. Source
//! lowering can produce `TypeFunctionInstance` nodes, and consumers
//! choose either immutable no-allocation reductions or the mutable
//! allocation-capable path depending on their arena access.

use crate::{
    member_access::{self, MemberFailure},
    types::{Arena, FollowedTypeKind, PrimitiveType, SingletonType, TableType, TypeId, TypeKind},
};

/// Builtin `setmetatable` type-function name.
pub const SETMETATABLE_TYPE_FUNCTION: &str = "setmetatable";

/// Outcome of one type-function reduction attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Reduction {
    /// The runtime reduced the instance to the resulting type.
    Reduced(TypeId),
    /// The instance is irreducible at the moment — typically because
    /// at least one argument is still a free or blocked type that
    /// must be solved first.
    Pending,
}

/// Type-function reduction collaborator.
///
/// The immutable reduction path stays deliberately narrow for
/// read-only consumers. Mutable consumers can additionally reduce
/// builtin operations that allocate fresh union or singleton-derived
/// result types.
#[derive(Clone, Debug, Default)]
pub struct TypeFunctionRuntime;

/// Reduces one type-function instance in place when possible.
pub fn reduce_type_function_instance(arena: &mut Arena, id: TypeId) -> TypeId {
    let id = arena.follow(id);
    let TypeKind::TypeFunctionInstance { name, arguments } = arena.get(id) else {
        return id;
    };
    let (name, arguments) = (name.clone(), arguments.clone());
    match TypeFunctionRuntime::new().reduce_allocating(arena, &name, &arguments) {
        Reduction::Reduced(reduced) if reduced != id => arena.follow(reduced),
        Reduction::Reduced(_) | Reduction::Pending => id,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AddOperand {
    Number,
    Never,
    ConcreteNonNumber,
    Pending,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConcatOperand {
    Valid,
    Dynamic,
    Never,
    ConcreteInvalid,
    Pending,
}

impl TypeFunctionRuntime {
    /// Creates a runtime with the builtin reductions enabled.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Attempts to reduce `name(arguments)` to a concrete type.
    ///
    /// Callers should treat the original instance as the canonical
    /// representative when this returns `Pending`.
    #[must_use]
    pub fn reduce(&self, arena: &Arena, name: &str, arguments: &[TypeId]) -> Reduction {
        match name {
            "add" => self.reduce_add(arena, arguments),
            "concat" => self.reduce_concat(arena, arguments),
            "union" => self.reduce_union(arena, arguments),
            _ => Reduction::Pending,
        }
    }

    /// Attempts to reduce `name(arguments)` and may allocate the result in
    /// `arena`.
    ///
    /// This path is for mutable consumers such as normalization, unification,
    /// and expected-type extraction. Read-only consumers should keep using
    /// [`Self::reduce`].
    pub fn reduce_allocating(
        &self,
        arena: &mut Arena,
        name: &str,
        arguments: &[TypeId],
    ) -> Reduction {
        match self.reduce(arena, name, arguments) {
            Reduction::Pending => match name {
                "union" => self.reduce_union_allocating(arena, arguments),
                "keyof" => self.reduce_keyof(arena, arguments),
                "index" => self.reduce_index(arena, arguments),
                SETMETATABLE_TYPE_FUNCTION => self.reduce_setmetatable(arena, arguments),
                _ => Reduction::Pending,
            },
            reduced => reduced,
        }
    }

    fn reduce_add(&self, arena: &Arena, arguments: &[TypeId]) -> Reduction {
        let (left, right) = match arguments {
            [left] => (*left, *left),
            [left, right] => (*left, *right),
            _ => return Reduction::Pending,
        };

        if let Some(result) = add_metamethod_result(arena, left, right) {
            return Reduction::Reduced(result);
        }

        match (
            classify_add_operand(arena, left),
            classify_add_operand(arena, right),
        ) {
            (AddOperand::Number, AddOperand::Number) => {
                Reduction::Reduced(arena.primitives().number)
            }
            (AddOperand::Never, _) | (_, AddOperand::Never) => {
                Reduction::Reduced(arena.primitives().never)
            }
            (AddOperand::Pending, _) | (_, AddOperand::Pending) => Reduction::Pending,
            (AddOperand::ConcreteNonNumber, _) | (_, AddOperand::ConcreteNonNumber) => {
                Reduction::Reduced(arena.primitives().never)
            }
        }
    }

    fn reduce_concat(&self, arena: &Arena, arguments: &[TypeId]) -> Reduction {
        let (left, right) = match arguments {
            [left] => (*left, *left),
            [left, right] => (*left, *right),
            _ => return Reduction::Pending,
        };

        if let Some(result) = binary_metamethod_result(arena, left, right, "__concat") {
            return Reduction::Reduced(result);
        }

        match (
            classify_concat_operand(arena, left),
            classify_concat_operand(arena, right),
        ) {
            (ConcatOperand::Never, _) | (_, ConcatOperand::Never) => {
                Reduction::Reduced(arena.primitives().never)
            }
            (ConcatOperand::Dynamic, _) | (_, ConcatOperand::Dynamic) => {
                Reduction::Reduced(arena.primitives().any)
            }
            (ConcatOperand::Valid, ConcatOperand::Valid) => {
                Reduction::Reduced(arena.primitives().string)
            }
            (ConcatOperand::Pending, _) | (_, ConcatOperand::Pending) => Reduction::Pending,
            (ConcatOperand::ConcreteInvalid, _) | (_, ConcatOperand::ConcreteInvalid) => {
                Reduction::Reduced(arena.primitives().never)
            }
        }
    }

    fn reduce_union(&self, arena: &Arena, arguments: &[TypeId]) -> Reduction {
        let mut non_never = arguments
            .iter()
            .copied()
            .filter(|argument| !matches!(arena.get(arena.follow(*argument)), TypeKind::Never));
        let Some(first) = non_never.next() else {
            return Reduction::Reduced(arena.primitives().never);
        };
        if non_never.next().is_none() {
            return Reduction::Reduced(first);
        }
        Reduction::Pending
    }

    fn reduce_union_allocating(&self, arena: &mut Arena, arguments: &[TypeId]) -> Reduction {
        Reduction::Reduced(alloc_union_type(arena, arguments.to_vec()))
    }

    fn reduce_keyof(&self, arena: &mut Arena, arguments: &[TypeId]) -> Reduction {
        let [target] = arguments else {
            return Reduction::Pending;
        };
        match arena.followed(*target).1 {
            FollowedTypeKind::Table(table) => {
                let table = table.clone();
                Reduction::Reduced(self.reduce_keyof_table(arena, &table))
            }
            FollowedTypeKind::Never => Reduction::Reduced(arena.primitives().never),
            FollowedTypeKind::Primitive(_)
            | FollowedTypeKind::Singleton(_)
            | FollowedTypeKind::Function(_)
            | FollowedTypeKind::Extern { .. }
            | FollowedTypeKind::Metatable { .. }
            | FollowedTypeKind::TypeFunctionInstance { .. }
            | FollowedTypeKind::Union(_)
            | FollowedTypeKind::Intersection(_)
            | FollowedTypeKind::Negation(_)
            | FollowedTypeKind::Free(_)
            | FollowedTypeKind::Blocked(_)
            | FollowedTypeKind::Generic(_)
            | FollowedTypeKind::Error
            | FollowedTypeKind::Unknown
            | FollowedTypeKind::Any => Reduction::Pending,
        }
    }

    fn reduce_keyof_table(&self, arena: &mut Arena, table: &TableType) -> TypeId {
        let mut keys =
            Vec::with_capacity(table.properties.len() + usize::from(table.indexer.is_some()));
        for name in table.properties.keys() {
            keys.push(arena.alloc(TypeKind::Singleton(SingletonType::String(name.clone()))));
        }
        if let Some(indexer) = &table.indexer {
            keys.push(indexer.key);
        }
        alloc_union_type(arena, keys)
    }

    fn reduce_index(&self, arena: &mut Arena, arguments: &[TypeId]) -> Reduction {
        let [base, key] = arguments else {
            return Reduction::Pending;
        };
        self.reduce_index_pair(arena, *base, *key)
    }

    fn reduce_index_pair(&self, arena: &mut Arena, base: TypeId, key: TypeId) -> Reduction {
        let (base, base_kind) = arena.followed(base);
        let (key, key_kind) = arena.followed(key);
        if matches!(base_kind, FollowedTypeKind::Never)
            || matches!(key_kind, FollowedTypeKind::Never)
        {
            return Reduction::Reduced(arena.primitives().never);
        }

        if let FollowedTypeKind::Union(keys) = key_kind {
            let keys = keys.to_vec();
            let mut values = Vec::new();
            for key in keys {
                match self.reduce_index_pair(arena, base, key) {
                    Reduction::Reduced(value) => values.push(value),
                    Reduction::Pending => return Reduction::Pending,
                }
            }
            return Reduction::Reduced(alloc_union_type(arena, values));
        }

        if let FollowedTypeKind::Union(bases) = base_kind {
            let bases = bases.to_vec();
            let mut values = Vec::new();
            for base in bases {
                match self.reduce_index_pair(arena, base, key) {
                    Reduction::Reduced(value) => values.push(value),
                    Reduction::Pending => return Reduction::Pending,
                }
            }
            return Reduction::Reduced(alloc_union_type(arena, values));
        }

        match base_kind {
            FollowedTypeKind::Table(table) => self.reduce_index_table(arena, table, key),
            FollowedTypeKind::Primitive(_)
            | FollowedTypeKind::Singleton(_)
            | FollowedTypeKind::Function(_)
            | FollowedTypeKind::Extern { .. }
            | FollowedTypeKind::Metatable { .. }
            | FollowedTypeKind::TypeFunctionInstance { .. }
            | FollowedTypeKind::Union(_)
            | FollowedTypeKind::Intersection(_)
            | FollowedTypeKind::Negation(_)
            | FollowedTypeKind::Free(_)
            | FollowedTypeKind::Blocked(_)
            | FollowedTypeKind::Generic(_)
            | FollowedTypeKind::Error
            | FollowedTypeKind::Unknown
            | FollowedTypeKind::Never
            | FollowedTypeKind::Any => Reduction::Pending,
        }
    }

    fn reduce_index_table(&self, arena: &Arena, table: &TableType, key: TypeId) -> Reduction {
        match member_access::table_index_member(arena, table, key) {
            Ok(member) => Reduction::Reduced(member.read_type()),
            Err(MemberFailure::Missing) => Reduction::Reduced(arena.primitives().never),
            Err(MemberFailure::KeyMismatch | MemberFailure::Unsupported) => Reduction::Pending,
        }
    }

    fn reduce_setmetatable(&self, arena: &mut Arena, arguments: &[TypeId]) -> Reduction {
        let [table, metatable] = arguments else {
            return Reduction::Pending;
        };
        let (table, table_kind) = arena.followed(*table);
        let (metatable, metatable_kind) = arena.followed(*metatable);
        if setmetatable_table_operand_is_uninhabited_kind(table_kind)
            || matches!(metatable_kind, FollowedTypeKind::Never)
        {
            return Reduction::Reduced(arena.primitives().never);
        }

        if !is_concrete_setmetatable_table_kind(table_kind)
            || !is_valid_setmetatable_metatable_kind(metatable_kind)
        {
            return Reduction::Pending;
        }

        Reduction::Reduced(arena.alloc(TypeKind::Metatable {
            table,
            metatable,
            name: None,
        }))
    }
}

pub fn is_builtin_type_function(name: &str) -> bool {
    matches!(
        name,
        "add"
            | "sub"
            | "mul"
            | "div"
            | "idiv"
            | "mod"
            | "pow"
            | "unm"
            | "concat"
            | "len"
            | "union"
            | "intersect"
            | "keyof"
            | "index"
            | "rawget"
            | SETMETATABLE_TYPE_FUNCTION
            | "getmetatable"
    )
}

pub fn setmetatable_type_function_arguments(
    name: &str,
    arguments: &[TypeId],
) -> Option<(TypeId, TypeId)> {
    let [table, metatable] = arguments else {
        return None;
    };
    (name == SETMETATABLE_TYPE_FUNCTION).then_some((*table, *metatable))
}

fn add_metamethod_result(arena: &Arena, left: TypeId, right: TypeId) -> Option<TypeId> {
    binary_metamethod_result(arena, left, right, "__add")
}

fn binary_metamethod_result(
    arena: &Arena,
    left: TypeId,
    right: TypeId,
    name: &str,
) -> Option<TypeId> {
    binary_metamethod(arena, left, name)
        .or_else(|| binary_metamethod(arena, right, name))
        .and_then(|callee| first_function_return(arena, callee))
}

fn binary_metamethod(arena: &Arena, ty: TypeId, name: &str) -> Option<TypeId> {
    match arena.followed(ty).1 {
        FollowedTypeKind::Metatable { metatable, .. } => {
            table_property_type(arena, metatable, name)
        }
        FollowedTypeKind::Extern { properties, .. } => {
            properties.get(name).map(|property| property.ty)
        }
        _ => None,
    }
}

fn table_property_type(arena: &Arena, table: TypeId, property: &str) -> Option<TypeId> {
    let TypeKind::Table(table) = arena.get(arena.follow(table)) else {
        return None;
    };
    table.properties.get(property).map(|property| property.ty)
}

fn first_function_return(arena: &Arena, callee: TypeId) -> Option<TypeId> {
    match arena.followed(callee).1 {
        FollowedTypeKind::Function(function) => arena.first_in_pack(function.returns),
        FollowedTypeKind::Any
        | FollowedTypeKind::Unknown
        | FollowedTypeKind::Error
        | FollowedTypeKind::Blocked(_) => Some(arena.primitives().any),
        _ => None,
    }
}

fn classify_add_operand(arena: &Arena, ty: TypeId) -> AddOperand {
    match arena.followed(ty).1 {
        FollowedTypeKind::Primitive(PrimitiveType::Number) => AddOperand::Number,
        FollowedTypeKind::Never => AddOperand::Never,
        FollowedTypeKind::Primitive(_)
        | FollowedTypeKind::Singleton(_)
        | FollowedTypeKind::Function(_)
        | FollowedTypeKind::Table(_)
        | FollowedTypeKind::Extern { .. }
        | FollowedTypeKind::Metatable { .. } => AddOperand::ConcreteNonNumber,
        FollowedTypeKind::Any
        | FollowedTypeKind::Unknown
        | FollowedTypeKind::Error
        | FollowedTypeKind::Free(_)
        | FollowedTypeKind::Blocked(_)
        | FollowedTypeKind::Generic(_)
        | FollowedTypeKind::Union(_)
        | FollowedTypeKind::Intersection(_)
        | FollowedTypeKind::Negation(_)
        | FollowedTypeKind::TypeFunctionInstance { .. } => AddOperand::Pending,
    }
}

fn classify_concat_operand(arena: &Arena, ty: TypeId) -> ConcatOperand {
    match arena.followed(ty).1 {
        FollowedTypeKind::Primitive(PrimitiveType::String | PrimitiveType::Number)
        | FollowedTypeKind::Singleton(SingletonType::String(_)) => ConcatOperand::Valid,
        FollowedTypeKind::Never => ConcatOperand::Never,
        FollowedTypeKind::Any | FollowedTypeKind::Error => ConcatOperand::Dynamic,
        FollowedTypeKind::Free(_)
        | FollowedTypeKind::Blocked(_)
        | FollowedTypeKind::Generic(_)
        | FollowedTypeKind::TypeFunctionInstance { .. }
        | FollowedTypeKind::Union(_)
        | FollowedTypeKind::Intersection(_)
        | FollowedTypeKind::Negation(_) => ConcatOperand::Pending,
        FollowedTypeKind::Unknown
        | FollowedTypeKind::Primitive(_)
        | FollowedTypeKind::Singleton(_)
        | FollowedTypeKind::Function(_)
        | FollowedTypeKind::Table(_)
        | FollowedTypeKind::Extern { .. }
        | FollowedTypeKind::Metatable { .. } => ConcatOperand::ConcreteInvalid,
    }
}

fn alloc_union_type(arena: &mut Arena, types: Vec<TypeId>) -> TypeId {
    let never = arena.primitives().never;
    let mut flattened = Vec::new();
    for ty in types {
        let ty = arena.follow(ty);
        if ty == never {
            continue;
        }
        match arena.get(ty).clone() {
            TypeKind::Any | TypeKind::Unknown => return ty,
            TypeKind::Union(options) => flattened.extend(options),
            TypeKind::Never => {}
            _ => flattened.push(ty),
        }
    }

    flattened.sort_unstable();
    flattened.dedup();
    let primitives = arena.primitives();
    let has_boolean = flattened.contains(&primitives.boolean);
    let has_string = flattened.contains(&primitives.string);
    if has_boolean || has_string {
        flattened.retain(|ty| match arena.get(*ty) {
            TypeKind::Singleton(SingletonType::Boolean(_)) => !has_boolean,
            TypeKind::Singleton(SingletonType::String(_)) => !has_string,
            _ => true,
        });
    }

    match flattened.as_slice() {
        [] => never,
        [only] => *only,
        _ => arena.alloc(TypeKind::Union(flattened)),
    }
}

fn is_concrete_setmetatable_table_kind(kind: FollowedTypeKind<'_>) -> bool {
    matches!(
        kind,
        FollowedTypeKind::Table(_) | FollowedTypeKind::Metatable { .. }
    )
}

fn setmetatable_table_operand_is_uninhabited_kind(kind: FollowedTypeKind<'_>) -> bool {
    matches!(
        kind,
        FollowedTypeKind::Primitive(_)
            | FollowedTypeKind::Singleton(_)
            | FollowedTypeKind::Function(_)
            | FollowedTypeKind::Unknown
            | FollowedTypeKind::Never
    )
}

fn is_valid_setmetatable_metatable_kind(kind: FollowedTypeKind<'_>) -> bool {
    matches!(
        kind,
        FollowedTypeKind::Table(_)
            | FollowedTypeKind::Metatable { .. }
            | FollowedTypeKind::Any
            | FollowedTypeKind::Unknown
            | FollowedTypeKind::Error
    )
}

#[cfg(any())]
mod tests {
    use super::*;
    use crate::types::{Arena, TableIndexer, TableProperty, TableState};

    #[test]
    fn runtime_reduces_builtin_add_when_arguments_are_concrete() {
        let runtime = TypeFunctionRuntime::new();
        let arena = Arena::new();
        let primitives = arena.primitives();

        assert_eq!(
            runtime.reduce(&arena, "add", &[primitives.number, primitives.number]),
            Reduction::Reduced(primitives.number)
        );
        assert_eq!(
            runtime.reduce(&arena, "add", &[primitives.number]),
            Reduction::Reduced(primitives.number)
        );
        assert_eq!(
            runtime.reduce(&arena, "add", &[primitives.string, primitives.boolean]),
            Reduction::Reduced(primitives.never)
        );
        assert_eq!(
            runtime.reduce(&arena, "add", &[primitives.never, primitives.number]),
            Reduction::Reduced(primitives.never)
        );
    }

    #[test]
    fn runtime_reduces_builtin_concat_when_arguments_are_concrete() {
        let runtime = TypeFunctionRuntime::new();
        let arena = Arena::new();
        let primitives = arena.primitives();

        assert_eq!(
            runtime.reduce(&arena, "concat", &[primitives.string, primitives.string]),
            Reduction::Reduced(primitives.string)
        );
        assert_eq!(
            runtime.reduce(&arena, "concat", &[primitives.number, primitives.string]),
            Reduction::Reduced(primitives.string)
        );
        assert_eq!(
            runtime.reduce(&arena, "concat", &[primitives.boolean, primitives.string]),
            Reduction::Reduced(primitives.never)
        );
        assert_eq!(
            runtime.reduce(&arena, "concat", &[primitives.any, primitives.string]),
            Reduction::Reduced(primitives.any)
        );
    }

    #[test]
    fn runtime_reduces_builtin_union_when_no_allocation_is_needed() {
        let runtime = TypeFunctionRuntime::new();
        let arena = Arena::new();
        let primitives = arena.primitives();

        assert_eq!(
            runtime.reduce(&arena, "union", &[primitives.string, primitives.never]),
            Reduction::Reduced(primitives.string)
        );
        assert_eq!(
            runtime.reduce(&arena, "union", &[primitives.never, primitives.never]),
            Reduction::Reduced(primitives.never)
        );
        assert_eq!(
            runtime.reduce(&arena, "union", &[primitives.string, primitives.number]),
            Reduction::Pending
        );
    }

    #[test]
    fn runtime_reduces_builtin_union_when_allocation_is_needed() {
        let runtime = TypeFunctionRuntime::new();
        let mut arena = Arena::new();
        let primitives = arena.primitives();

        let reduced =
            runtime.reduce_allocating(&mut arena, "union", &[primitives.string, primitives.number]);
        let Reduction::Reduced(reduced) = reduced else {
            panic!("expected allocated union reduction");
        };

        assert_eq!(arena.summary(reduced), "number | string");
    }

    #[test]
    fn runtime_reduces_builtin_keyof_for_concrete_tables() {
        let runtime = TypeFunctionRuntime::new();
        let mut arena = Arena::new();
        let primitives = arena.primitives();
        let table = table_with(
            &mut arena,
            &[("a", primitives.number), ("b", primitives.string)],
        );

        let reduced = runtime.reduce_allocating(&mut arena, "keyof", &[table]);
        let Reduction::Reduced(reduced) = reduced else {
            panic!("expected keyof reduction");
        };
        assert_eq!(arena.summary(reduced), "\"a\" | \"b\"");

        let empty = arena.alloc(TypeKind::Table(TableType::new(TableState::Sealed)));
        assert_eq!(
            runtime.reduce_allocating(&mut arena, "keyof", &[empty]),
            Reduction::Reduced(primitives.never)
        );

        let mut indexed = TableType::new(TableState::Sealed);
        indexed.indexer = Some(TableIndexer {
            key: primitives.string,
            value: primitives.boolean,
            read_only: false,
        });
        let indexed = arena.alloc(TypeKind::Table(indexed));
        assert_eq!(
            runtime.reduce_allocating(&mut arena, "keyof", &[indexed]),
            Reduction::Reduced(primitives.string)
        );
    }

    #[test]
    fn runtime_reduces_builtin_index_for_concrete_tables() {
        let runtime = TypeFunctionRuntime::new();
        let mut arena = Arena::new();
        let primitives = arena.primitives();
        let table = table_with(
            &mut arena,
            &[("a", primitives.number), ("b", primitives.string)],
        );
        let a = singleton_string(&mut arena, "a");
        let b = singleton_string(&mut arena, "b");
        let c = singleton_string(&mut arena, "c");

        assert_eq!(
            runtime.reduce_allocating(&mut arena, "index", &[table, a]),
            Reduction::Reduced(primitives.number)
        );
        assert_eq!(
            runtime.reduce_allocating(&mut arena, "index", &[table, c]),
            Reduction::Reduced(primitives.never)
        );

        let key_union = arena.alloc(TypeKind::Union(vec![a, b]));
        let reduced = runtime.reduce_allocating(&mut arena, "index", &[table, key_union]);
        let Reduction::Reduced(reduced) = reduced else {
            panic!("expected indexed key-union reduction");
        };
        assert_eq!(arena.summary(reduced), "number | string");

        let mut indexed = TableType::new(TableState::Sealed);
        indexed.indexer = Some(TableIndexer {
            key: primitives.string,
            value: primitives.boolean,
            read_only: false,
        });
        let indexed = arena.alloc(TypeKind::Table(indexed));
        assert_eq!(
            runtime.reduce_allocating(&mut arena, "index", &[indexed, c]),
            Reduction::Reduced(primitives.boolean)
        );
    }

    #[test]
    fn runtime_reduces_builtin_setmetatable_for_concrete_tables() {
        let runtime = TypeFunctionRuntime::new();
        let mut arena = Arena::new();
        let primitives = arena.primitives();
        let table = table_with(&mut arena, &[("x", primitives.number)]);
        let metatable = table_with(&mut arena, &[("__index", table)]);

        let reduced = runtime.reduce_allocating(&mut arena, "setmetatable", &[table, metatable]);
        let Reduction::Reduced(reduced) = reduced else {
            panic!("expected setmetatable reduction");
        };
        assert_eq!(
            arena.get(reduced),
            &TypeKind::Metatable {
                table,
                metatable,
                name: None
            }
        );

        let dynamic =
            runtime.reduce_allocating(&mut arena, "setmetatable", &[table, primitives.any]);
        let Reduction::Reduced(dynamic) = dynamic else {
            panic!("expected dynamic-metatable reduction");
        };
        assert_eq!(
            arena.get(dynamic),
            &TypeKind::Metatable {
                table,
                metatable: primitives.any,
                name: None
            }
        );

        assert_eq!(
            runtime.reduce_allocating(&mut arena, "setmetatable", &[primitives.unknown, metatable]),
            Reduction::Reduced(primitives.never)
        );
        assert_eq!(
            runtime.reduce_allocating(&mut arena, "setmetatable", &[primitives.string, metatable]),
            Reduction::Reduced(primitives.never)
        );
        assert_eq!(
            runtime.reduce_allocating(&mut arena, "setmetatable", &[table]),
            Reduction::Pending
        );
    }

    #[test]
    fn runtime_keeps_unresolved_or_unknown_requests_pending() {
        let runtime = TypeFunctionRuntime::new();
        let mut arena = Arena::new();
        let primitives = arena.primitives();
        let generic = arena.alloc(TypeKind::Generic(crate::types::GenericType {
            name: "T".to_owned(),
            level: crate::types::TypeLevel(0),
        }));

        assert_eq!(
            runtime.reduce(&arena, "add", &[generic, primitives.number]),
            Reduction::Pending
        );
        assert_eq!(
            runtime.reduce(&arena, "keyof", &[primitives.number]),
            Reduction::Pending
        );
        assert_eq!(
            runtime.reduce(&arena, "unknown_fn", &[]),
            Reduction::Pending
        );
        assert_eq!(
            runtime.reduce_allocating(&mut arena, "setmetatable", &[generic, primitives.unknown],),
            Reduction::Pending
        );
    }

    fn table_with(arena: &mut Arena, properties: &[(&str, TypeId)]) -> TypeId {
        let mut table = TableType::new(TableState::Sealed);
        for (name, ty) in properties {
            table
                .properties
                .insert((*name).to_owned(), TableProperty::new(*ty));
        }
        arena.alloc(TypeKind::Table(table))
    }

    fn singleton_string(arena: &mut Arena, value: &str) -> TypeId {
        arena.alloc(TypeKind::Singleton(SingletonType::String(value.to_owned())))
    }
}
