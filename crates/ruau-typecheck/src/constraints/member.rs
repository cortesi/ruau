use std::collections::BTreeMap;

use super::{ConstraintSolveError, ConstraintSolver, SubtypeFailureSet};
use crate::{
    member_access::{self, MemberKey},
    subtype::{SubtypeError, SubtypeErrorKind, SubtypeTarget, Subtyper},
    types::{
        Arena, PrimitiveType, PropertyAccess, SingletonType, TableIndexer, TableProperty,
        TableState, TableType, TypeField, TypeId, TypeKind, TypePackTail, TypePath,
        TypePathComponent,
    },
};

struct MemberAccessPlan {
    key: MemberKey,
    index_key: Option<TypeId>,
}

impl MemberAccessPlan {
    fn property(name: String) -> Self {
        Self {
            key: MemberKey::Property(name),
            index_key: None,
        }
    }

    fn indexer(arena: &Arena, key: TypeId) -> Self {
        Self {
            key: MemberKey::from_index(arena, key),
            index_key: Some(key),
        }
    }

    fn property_name(&self) -> Option<&str> {
        match &self.key {
            MemberKey::Property(name) => Some(name.as_str()),
            MemberKey::Index(_) => None,
        }
    }

    fn property_name_owned(&self) -> Option<String> {
        self.property_name().map(str::to_owned)
    }

    fn index_key(&self, arena: &mut Arena) -> TypeId {
        self.index_key.unwrap_or_else(|| {
            let name = self
                .property_name()
                .expect("property member access can be converted to an index key");
            member_access::property_name_key(arena, name)
        })
    }
}

struct MemberPlan {
    table: TypeId,
    access: MemberAccessPlan,
    value: TypeId,
}

impl MemberPlan {
    fn property(table: TypeId, name: String, value: TypeId) -> Self {
        Self {
            table,
            access: MemberAccessPlan::property(name),
            value,
        }
    }

    fn indexer(arena: &Arena, table: TypeId, key: TypeId, value: TypeId) -> Self {
        Self {
            table,
            access: MemberAccessPlan::indexer(arena, key),
            value,
        }
    }
}

impl<'a> ConstraintSolver<'a> {
    pub(super) fn read_property(
        &mut self,
        table: TypeId,
        name: String,
        value: TypeId,
    ) -> Result<(), ConstraintSolveError> {
        self.solve_read_property(MemberPlan::property(table, name, value))
    }

    pub(super) fn read_indexer(
        &mut self,
        table: TypeId,
        key: TypeId,
        value: TypeId,
    ) -> Result<(), ConstraintSolveError> {
        self.solve_read_indexer(MemberPlan::indexer(self.arena, table, key, value))
    }

    pub(super) fn write_property(
        &mut self,
        table: TypeId,
        name: String,
        value: TypeId,
    ) -> Result<(), ConstraintSolveError> {
        self.solve_write_property(MemberPlan::property(table, name, value))
    }

    pub(super) fn write_indexer(
        &mut self,
        table: TypeId,
        key: TypeId,
        value: TypeId,
    ) -> Result<(), ConstraintSolveError> {
        self.solve_write_indexer(MemberPlan::indexer(self.arena, table, key, value))
    }

    fn solve_write_indexer(&mut self, plan: MemberPlan) -> Result<(), ConstraintSolveError> {
        let MemberPlan {
            table,
            access,
            value,
        } = plan;
        let key = access.index_key(self.arena);
        let table = self.arena.follow(table);
        match self.arena.get(table).clone() {
            TypeKind::Table(mut table_type) => {
                if let Some(name) = access.property_name_owned() {
                    if let Some(result) =
                        self.write_existing_table_property(table, &mut table_type, &name, value)
                    {
                        return result;
                    }
                    if member_access::table_state_allows_member_extension(table_type.state) {
                        table_type
                            .properties
                            .insert(name, TableProperty::new(value));
                        self.arena.replace(table, TypeKind::Table(table_type));
                        return Ok(());
                    }
                }
                if let Some(indexer) = table_type.indexer {
                    let reject_read_only = indexer.read_only
                        && !member_access::table_state_allows_member_extension(table_type.state);
                    return self.write_indexer_value(key, value, &indexer, reject_read_only);
                }
                if member_access::table_state_allows_member_extension(table_type.state) {
                    table_type.indexer = Some(TableIndexer {
                        key: self.arena.scoped_unsealed_indexer_key(key),
                        value,
                        read_only: false,
                    });
                    self.arena.replace(table, TypeKind::Table(table_type));
                    Ok(())
                } else {
                    let expected = self.expected_indexer_table(key, value);
                    self.require_subtype(table, expected)
                }
            }
            TypeKind::Metatable {
                table: base_table, ..
            } => self.write_indexer(base_table, key, value),
            TypeKind::Extern {
                properties,
                indexer,
                ..
            } => {
                if let Some(name) = access.property_name_owned()
                    && let Some(result) =
                        self.write_existing_extern_property(&properties, &name, value)
                {
                    return result;
                }
                if let Some(indexer) = indexer {
                    let reject_read_only = indexer.read_only;
                    return self.write_indexer_value(key, value, &indexer, reject_read_only);
                }
                let expected = self.expected_indexer_table(key, value);
                self.require_subtype(table, expected)
            }
            TypeKind::Intersection(types) => self.write_intersection_indexer(&types, key, value),
            TypeKind::Free(_) => {
                if let Some(name) = access.property_name_owned() {
                    self.replace_with_property_table(
                        table,
                        TableState::Unsealed,
                        name,
                        TableProperty::new(value),
                    );
                } else {
                    self.replace_with_indexer_table(
                        table,
                        TableState::Unsealed,
                        TableIndexer {
                            key,
                            value,
                            read_only: false,
                        },
                    );
                }
                Ok(())
            }
            TypeKind::Any | TypeKind::Unknown | TypeKind::Error | TypeKind::Blocked(_) => Ok(()),
            _ => {
                let expected = self.expected_indexer_table(key, value);
                self.require_subtype(table, expected)
            }
        }
    }

    fn solve_write_property(&mut self, plan: MemberPlan) -> Result<(), ConstraintSolveError> {
        let MemberPlan {
            table,
            access,
            value,
        } = plan;
        let name = access
            .property_name_owned()
            .expect("property write plan carries a property name");
        let table = self.arena.follow(table);
        match self.arena.get(table).clone() {
            TypeKind::Table(mut table_type) => {
                if let Some(result) =
                    self.write_existing_table_property(table, &mut table_type, &name, value)
                {
                    return result;
                }
                if member_access::table_state_allows_member_extension(table_type.state) {
                    table_type
                        .properties
                        .insert(name, TableProperty::new(value));
                    self.arena.replace(table, TypeKind::Table(table_type));
                    Ok(())
                } else if let Some(indexer) = table_type.indexer {
                    if self.arena.is_nil(value) {
                        return Ok(());
                    }
                    let string = access.index_key(self.arena);
                    self.write_indexer_value(string, value, &indexer, false)
                } else {
                    Err(self.missing_property_write_error(table, &name, value))
                }
            }
            TypeKind::Metatable {
                table: base_table, ..
            } => self.write_property(base_table, name, value),
            TypeKind::Extern {
                properties,
                indexer,
                ..
            } => {
                if let Some(result) = self.write_existing_extern_property(&properties, &name, value)
                {
                    return result;
                }
                if let Some(indexer) = indexer {
                    let string = access.index_key(self.arena);
                    let reject_read_only = indexer.read_only;
                    return self.write_indexer_value(string, value, &indexer, reject_read_only);
                }
                Err(self.missing_property_write_error(table, &name, value))
            }
            TypeKind::Intersection(types) => {
                self.write_intersection_property(table, &types, &name, value)
            }
            TypeKind::Union(_) => self.write_union_property(table, &name, value),
            TypeKind::Free(_) => {
                self.replace_with_property_table(
                    table,
                    TableState::Unsealed,
                    name,
                    TableProperty::new(value),
                );
                Ok(())
            }
            TypeKind::Any | TypeKind::Unknown | TypeKind::Error | TypeKind::Blocked(_) => Ok(()),
            _ => {
                let expected = self.expected_property_table(name, TableProperty::new(value));
                self.require_subtype(table, expected)
            }
        }
    }

    fn write_existing_table_property(
        &mut self,
        table: TypeId,
        table_type: &mut TableType,
        name: &str,
        value: TypeId,
    ) -> Option<Result<(), ConstraintSolveError>> {
        let property = table_type.properties.get(name).cloned()?;
        if !member_access::table_property_allows_write(&property, table_type.state) {
            return Some(Err(ConstraintSolveError::PropertyAccessViolation {
                property: name.to_owned(),
                access: PropertyAccess::Write,
            }));
        }
        if member_access::table_property_promotes_on_write(&property, table_type.state) {
            let mut promoted = property.clone();
            promoted.ty = value;
            promoted.read_only = false;
            table_type.properties.insert(name.to_owned(), promoted);
            if table_type.state == TableState::Free {
                table_type.state = TableState::Unsealed;
            }
            self.arena
                .replace(table, TypeKind::Table(table_type.clone()));
        }
        let write_ty = property.write_type();
        self.bind_free_write_value(value, write_ty);
        Some(self.require_write_value(value, write_ty))
    }

    fn write_existing_extern_property(
        &mut self,
        properties: &BTreeMap<String, TableProperty>,
        name: &str,
        value: TypeId,
    ) -> Option<Result<(), ConstraintSolveError>> {
        let property = properties.get(name)?;
        if !member_access::extern_property_allows_write(property) {
            return Some(Err(ConstraintSolveError::PropertyAccessViolation {
                property: name.to_owned(),
                access: PropertyAccess::Write,
            }));
        }
        let write_ty = property.write_type();
        self.bind_free_write_value(value, write_ty);
        Some(self.require_write_value(value, write_ty))
    }

    fn write_indexer_value(
        &mut self,
        key: TypeId,
        value: TypeId,
        indexer: &TableIndexer,
        reject_read_only: bool,
    ) -> Result<(), ConstraintSolveError> {
        if self.arena.is_nil(value) {
            return Ok(());
        }
        if reject_read_only {
            return self.require_subtype(value, self.arena.primitives().never);
        }
        // Property writes pass a freshly allocated string singleton key, so
        // this bind only matters for indexer writes whose key is still free.
        self.bind_free_write_value(key, indexer.key);
        self.require_indexer_key(key, indexer.key)?;
        self.bind_free_write_value(value, indexer.value);
        self.require_write_value(value, indexer.value)
    }

    /// Requires a written value to fit the write target type. A dynamic `any`
    /// or `error` value is accepted, matching Luau's error suppression for
    /// dynamic assignments.
    fn require_write_value(
        &mut self,
        value: TypeId,
        write_ty: TypeId,
    ) -> Result<(), ConstraintSolveError> {
        if matches!(
            self.arena.get(self.arena.follow(value)),
            TypeKind::Any | TypeKind::Error
        ) {
            return Ok(());
        }
        self.require_subtype(value, write_ty)
    }

    /// Discharges a sibling `~nil` negation from one intersection member, so
    /// refinement leftovers such as `(nil | T) & ~nil` expose T's members.
    fn member_without_negated_nil(&mut self, member: TypeId) -> TypeId {
        let member = self.arena.follow(member);
        let TypeKind::Union(_) = self.arena.get(member) else {
            return member;
        };
        let options: Vec<TypeId> = self
            .arena
            .union_options(member)
            .into_iter()
            .filter(|option| !self.arena.is_nil(*option))
            .collect();
        match options.as_slice() {
            [] => member,
            [single] => self.arena.follow(*single),
            _ => self.union_type(options),
        }
    }

    /// Returns whether an indexer access key is a dynamic `any` or `error`
    /// key that defeats key checking, matching Luau's dynamic indexing.
    fn index_key_is_dynamic(&self, key: TypeId) -> bool {
        matches!(
            self.arena.get(self.arena.follow(key)),
            TypeKind::Any | TypeKind::Error
        )
    }

    /// Requires an indexer access key to fit the indexer key type. A dynamic
    /// key defeats the check.
    fn require_indexer_key(
        &mut self,
        key: TypeId,
        indexer_key: TypeId,
    ) -> Result<(), ConstraintSolveError> {
        if self.index_key_is_dynamic(key) {
            return Ok(());
        }
        self.require_subtype(key, indexer_key)
    }

    fn solve_read_indexer(&mut self, plan: MemberPlan) -> Result<(), ConstraintSolveError> {
        let MemberPlan {
            table,
            access,
            value,
        } = plan;
        let key = access.index_key(self.arena);
        let string_key = access.property_name_owned();
        let table = self.arena.follow(table);
        match self.arena.get(table).clone() {
            TypeKind::Table(table_type) => {
                if let Some(name) = string_key.as_ref()
                    && let Some(property) = table_type.properties.get(name)
                {
                    return self.read_table_property(property, table_type.state, name, value);
                }
                if let Some(indexer) = table_type.indexer {
                    if self.index_key_is_dynamic(key) {
                        self.bind_failed_read_value_to_any(value);
                        return Ok(());
                    }
                    self.require_subtype(key, indexer.key)?;
                    let indexer_value =
                        self.indexer_read_value(table_type.state, indexer.key, indexer.value);
                    return self.unify_read_value(value, indexer_value);
                }
                if matches!(table_type.state, TableState::Unsealed | TableState::Free)
                    && string_key.is_none()
                {
                    let unknown = self.arena.primitives().unknown;
                    return self.unify_read_value(value, unknown);
                }
                self.require_read_indexer(table, key, value)
            }
            TypeKind::Metatable {
                table: base_table, ..
            } => self.read_indexer(base_table, key, value),
            TypeKind::Extern {
                properties,
                indexer,
                ..
            } => {
                if let Some(name) = string_key.as_ref()
                    && let Some(property) = properties.get(name)
                {
                    return self.read_extern_property(property, name, value);
                }
                if let Some(indexer) = indexer {
                    if self.index_key_is_dynamic(key) {
                        self.bind_failed_read_value_to_any(value);
                        return Ok(());
                    }
                    self.require_subtype(key, indexer.key)?;
                    return self.unify_read_value(value, indexer.value);
                }
                self.require_read_indexer(table, key, value)
            }
            TypeKind::Union(_) => {
                let options = self.arena.union_options(table);
                if let Some(name) = string_key.as_ref() {
                    let mut property_values = Vec::new();
                    let mut all_options_have_property = true;
                    for option in &options {
                        let option = self.arena.follow(*option);
                        let TypeKind::Table(table_type) = self.arena.get(option).clone() else {
                            all_options_have_property = false;
                            break;
                        };
                        let Some(property) = table_type.properties.get(name.as_str()) else {
                            all_options_have_property = false;
                            break;
                        };
                        if !member_access::table_property_allows_read(property, table_type.state) {
                            all_options_have_property = false;
                            break;
                        }
                        property_values.push(property.ty);
                    }
                    if all_options_have_property {
                        let property_value = self.union_type(property_values);
                        return self.unify_read_value(value, property_value);
                    }
                }
                if let Some(indexer_value) = self.union_indexer_read_value(&options, key) {
                    return self.bind_and_unify_dynamic_read_value(value, indexer_value);
                }
                self.require_read_indexer(table, key, value)
            }
            TypeKind::Intersection(types) => {
                if let Some(indexer_value) = self.intersection_indexer_read_value(&types, key) {
                    return self.bind_and_unify_dynamic_read_value(value, indexer_value);
                }
                self.require_read_indexer(table, key, value)
            }
            TypeKind::Free(_) => {
                if let Some(name) = string_key {
                    self.replace_with_property_table(
                        table,
                        TableState::Unsealed,
                        name,
                        TableProperty::read_only(value),
                    );
                } else {
                    self.replace_with_indexer_table(
                        table,
                        TableState::Sealed,
                        TableIndexer {
                            key,
                            value,
                            read_only: false,
                        },
                    );
                }
                Ok(())
            }
            TypeKind::Any | TypeKind::Unknown | TypeKind::Error | TypeKind::Blocked(_) => Ok(()),
            _ => {
                self.bind_failed_known_non_table_read_value_to_error(table, value);
                self.require_read_indexer(table, key, value)
            }
        }
    }

    fn solve_read_property(&mut self, plan: MemberPlan) -> Result<(), ConstraintSolveError> {
        let MemberPlan {
            table,
            access,
            value,
        } = plan;
        let name = access
            .property_name_owned()
            .expect("property read plan carries a property name");
        self.read_property_with(table, name, value, &mut Vec::new())
    }

    fn read_property_with(
        &mut self,
        table: TypeId,
        name: String,
        value: TypeId,
        seen: &mut Vec<TypeId>,
    ) -> Result<(), ConstraintSolveError> {
        let table = self.arena.follow(table);
        if seen.contains(&table) {
            return self.require_read_property(table, name, value);
        }
        seen.push(table);
        match self.arena.get(table).clone() {
            TypeKind::Table(table_type) => {
                if let Some(property) = table_type.properties.get(&name) {
                    return self.read_table_property(property, table_type.state, &name, value);
                }
                if let Some(indexer) = table_type.indexer.clone() {
                    let string = member_access::property_name_key(self.arena, &name);
                    if let Err(error) = self.require_subtype(string, indexer.key) {
                        self.bind_failed_read_value_to_any(value);
                        return Err(error);
                    }
                    return self.unify_read_value(value, indexer.value);
                }
                if let Some(error) =
                    self.like_key_suggestion_error(table, &table_type, &name, value)
                {
                    self.bind_failed_read_value_to_any(value);
                    return Err(error);
                }
                if matches!(table_type.state, TableState::Sealed) {
                    // Upstream's solver recovers a reported missing-property
                    // read with `any`; only the silent open-table miss keeps
                    // the error type.
                    self.bind_failed_read_value_to_any(value);
                    Err(self.missing_property_read_error(table, name, value))
                } else {
                    self.bind_failed_read_value_to_error(value);
                    self.require_read_property(table, name, value)
                }
            }
            TypeKind::Metatable {
                table: base_table,
                metatable,
                ..
            } => {
                if let Some(property) = self.arena.direct_read_property(base_table, &name) {
                    return self.unify_read_value(value, property);
                }
                if member_access::type_is_dynamic(self.arena, metatable) {
                    let any = self.arena.primitives().any;
                    return self.unify_read_value(value, any);
                }
                if let Some(index) = self.arena.direct_read_property(metatable, "__index") {
                    let index = self.arena.follow(index);
                    if let TypeKind::Function(_) = self.arena.get(index) {
                        let indexed = self.index_function_result_type(index);
                        return self.unify_read_value(value, indexed);
                    }
                    return self.read_property_with(index, name, value, seen);
                }
                self.require_read_property(table, name, value)
            }
            TypeKind::Extern {
                properties,
                indexer,
                ..
            } => {
                if let Some(property) = properties.get(&name) {
                    return self.read_extern_property(property, &name, value);
                }
                if let Some(indexer) = indexer {
                    let string = member_access::property_name_key(self.arena, &name);
                    self.require_subtype(string, indexer.key)?;
                    return self.unify_read_value(value, indexer.value);
                }
                self.require_read_property(table, name, value)
            }
            TypeKind::Union(_) => {
                let mut property_values = Vec::new();
                let mut missing_options = Vec::new();
                let mut unsupported_options = Vec::new();
                let mut nilable = false;
                let mut saw_error_option = false;
                for option in self.arena.union_options(table) {
                    let option = self.arena.follow(option);
                    match self.arena.get(option).clone() {
                        TypeKind::Table(table_type) => {
                            let Some(property) = table_type.properties.get(&name) else {
                                missing_options.push(option);
                                continue;
                            };
                            if !member_access::table_property_allows_read(
                                property,
                                table_type.state,
                            ) {
                                missing_options.push(option);
                                continue;
                            }
                            property_values.push(property.ty);
                        }
                        TypeKind::Extern {
                            properties,
                            indexer,
                            ..
                        } => {
                            if let Some(property) = properties.get(&name) {
                                if !member_access::extern_property_allows_read(property) {
                                    missing_options.push(option);
                                    continue;
                                }
                                property_values.push(property.ty);
                            } else if let Some(indexer) = indexer {
                                let string = member_access::property_name_key(self.arena, &name);
                                if self.require_subtype(string, indexer.key).is_ok() {
                                    property_values.push(indexer.value);
                                } else {
                                    missing_options.push(option);
                                }
                            } else {
                                missing_options.push(option);
                            }
                        }
                        TypeKind::Any | TypeKind::Unknown => {
                            property_values.push(self.arena.primitives().any);
                        }
                        TypeKind::Primitive(PrimitiveType::String)
                        | TypeKind::Singleton(SingletonType::String(_)) => {
                            let Some(property_ty) = member_access::primitive_property_type(
                                self.arena,
                                PrimitiveType::String,
                                &name,
                            ) else {
                                missing_options.push(option);
                                continue;
                            };
                            property_values.push(property_ty);
                        }
                        TypeKind::Primitive(PrimitiveType::Vector) => {
                            let Some(property_ty) = member_access::primitive_property_type(
                                self.arena,
                                PrimitiveType::Vector,
                                &name,
                            ) else {
                                missing_options.push(option);
                                continue;
                            };
                            property_values.push(property_ty);
                        }
                        TypeKind::Primitive(PrimitiveType::Nil) => {
                            nilable = true;
                        }
                        TypeKind::Error => {
                            saw_error_option = true;
                            property_values.push(self.arena.primitives().any);
                        }
                        // An unconstrained generic union member has no arbitrary
                        // property, so reading one is a genuine missing-property
                        // error on that option (`oss_1953`: `A | B | T` has no
                        // `kind` on `T`) rather than an unsupported shape to defer.
                        TypeKind::Generic(_) => {
                            missing_options.push(option);
                        }
                        TypeKind::Intersection(types)
                            if self.intersection_generic_missing_read_property(&types, &name) =>
                        {
                            missing_options.push(option);
                        }
                        _ => {
                            unsupported_options.push(option);
                        }
                    }
                }
                if saw_error_option {
                    let property_value = self.normalized_union_type(property_values);
                    let property_value =
                        if self.arena.get(self.arena.follow(property_value)) == &TypeKind::Never {
                            self.arena.primitives().any
                        } else {
                            property_value
                        };
                    return self.bind_and_unify_dynamic_read_value(value, property_value);
                }
                if !unsupported_options.is_empty() {
                    return self.require_read_property(table, name, value);
                }
                if missing_options.is_empty() && !nilable {
                    let property_value = self.union_type(property_values);
                    return self.bind_and_unify_dynamic_read_value(value, property_value);
                }
                let all_options_missing = property_values.is_empty();
                let property_value = if all_options_missing {
                    self.arena.primitives().any
                } else {
                    self.normalized_union_type(property_values)
                };
                self.bind_and_unify_dynamic_read_value(value, property_value)?;

                let mut errors = Vec::new();
                if nilable {
                    errors.push(ConstraintSolveError::NilablePropertyRead {
                        ty: table,
                        property: name.clone(),
                    });
                }
                if !missing_options.is_empty() {
                    errors.push(ConstraintSolveError::UnionPropertyRead {
                        union: table,
                        property: name,
                        all_options_missing,
                        missing_options,
                    });
                }
                if errors.is_empty() {
                    Ok(())
                } else if errors.len() == 1 {
                    Err(errors.remove(0))
                } else {
                    Err(ConstraintSolveError::Multiple(errors))
                }
            }
            TypeKind::Intersection(types) => {
                if let Some(property_value) = self.intersection_property_read_value(&types, &name)
                    && !self.arena.may_be_nil(property_value)
                {
                    return self.bind_and_unify_dynamic_read_value(value, property_value);
                }
                if self.intersection_contains_union(&types)
                    && let Some(witness) = self.intersection_missing_read_property_witness(&types)
                {
                    return self.require_read_property(witness, name, value);
                }
                self.require_read_property(table, name, value)
            }
            TypeKind::Primitive(PrimitiveType::String)
            | TypeKind::Singleton(SingletonType::String(_)) => {
                let Some(property_ty) = member_access::primitive_property_type(
                    self.arena,
                    PrimitiveType::String,
                    &name,
                ) else {
                    self.bind_failed_read_value_to_error(value);
                    return self.require_read_property(table, name, value);
                };
                self.bind_and_unify_dynamic_read_value(value, property_ty)
            }
            TypeKind::Primitive(PrimitiveType::Vector) => {
                let Some(property_ty) = member_access::primitive_property_type(
                    self.arena,
                    PrimitiveType::Vector,
                    &name,
                ) else {
                    self.bind_failed_read_value_to_error(value);
                    return self.require_read_property(table, name, value);
                };
                self.bind_and_unify_dynamic_read_value(value, property_ty)
            }
            TypeKind::Free(_) => {
                self.replace_with_property_table(
                    table,
                    TableState::Unsealed,
                    name,
                    TableProperty::read_only(value),
                );
                Ok(())
            }
            TypeKind::Negation(inner) if self.negated_type_may_have_properties(inner) => {
                let any = self.arena.primitives().any;
                self.bind_and_unify_dynamic_read_value(value, any)
            }
            TypeKind::Any | TypeKind::Unknown | TypeKind::Error | TypeKind::Blocked(_) => Ok(()),
            _ => {
                self.bind_failed_known_non_table_read_value_to_error(table, value);
                let expected = self.expected_property_table(name, TableProperty::read_only(value));
                self.require_subtype(table, expected)
            }
        }
    }

    fn read_table_property(
        &mut self,
        property: &TableProperty,
        state: TableState,
        name: &str,
        value: TypeId,
    ) -> Result<(), ConstraintSolveError> {
        if !member_access::table_property_allows_read(property, state) {
            return Err(ConstraintSolveError::PropertyAccessViolation {
                property: name.to_owned(),
                access: PropertyAccess::Read,
            });
        }
        self.unify_read_value(value, property.ty)
    }

    fn read_extern_property(
        &mut self,
        property: &TableProperty,
        name: &str,
        value: TypeId,
    ) -> Result<(), ConstraintSolveError> {
        if !member_access::extern_property_allows_read(property) {
            return Err(ConstraintSolveError::PropertyAccessViolation {
                property: name.to_owned(),
                access: PropertyAccess::Read,
            });
        }
        self.unify_read_value(value, property.ty)
    }

    fn require_read_indexer(
        &mut self,
        table: TypeId,
        key: TypeId,
        value: TypeId,
    ) -> Result<(), ConstraintSolveError> {
        let expected = self.expected_indexer_table(key, value);
        self.require_subtype(table, expected)
    }
    fn expected_indexer_table(&mut self, key: TypeId, value: TypeId) -> TypeId {
        let mut expected = TableType::new(TableState::Sealed);
        expected.indexer = Some(TableIndexer {
            key,
            value,
            read_only: false,
        });
        self.arena.alloc(TypeKind::Table(expected))
    }
    fn expected_property_table(&mut self, name: String, property: TableProperty) -> TypeId {
        let mut expected = TableType::new(TableState::Sealed);
        expected.properties.insert(name, property);
        self.arena.alloc(TypeKind::Table(expected))
    }
    fn replace_with_property_table(
        &mut self,
        table: TypeId,
        state: TableState,
        name: String,
        property: TableProperty,
    ) {
        let mut table_type = TableType::new(state);
        table_type.properties.insert(name, property);
        self.arena.replace(table, TypeKind::Table(table_type));
    }
    fn replace_with_indexer_table(
        &mut self,
        table: TypeId,
        state: TableState,
        indexer: TableIndexer,
    ) {
        let mut table_type = TableType::new(state);
        table_type.indexer = Some(indexer);
        self.arena.replace(table, TypeKind::Table(table_type));
    }
    fn negated_type_may_have_properties(&self, inner: TypeId) -> bool {
        !matches!(
            self.arena.get(self.arena.follow(inner)),
            TypeKind::Table(_) | TypeKind::Metatable { .. }
        )
    }
    fn function_result_type(&self, callee: TypeId) -> Option<TypeId> {
        let callee = self.arena.follow(callee);
        let TypeKind::Function(function) = self.arena.get(callee) else {
            return None;
        };
        let returns = self.arena.normalize_pack(function.returns);
        returns
            .types
            .first()
            .copied()
            .or_else(|| match returns.tail {
                Some(TypePackTail::Variadic(ty)) => Some(ty),
                Some(TypePackTail::Error) => Some(self.arena.primitives().error),
                _ => None,
            })
    }
    fn index_function_result_type(&self, index: TypeId) -> TypeId {
        self.function_result_type(index)
            .or_else(|| {
                (self.function_fixed_return_count(index) == Some(0))
                    .then_some(self.arena.primitives().nil)
            })
            .unwrap_or_else(|| self.arena.primitives().any)
    }
    fn function_fixed_return_count(&self, callee: TypeId) -> Option<usize> {
        let callee = self.arena.follow(callee);
        let TypeKind::Function(function) = self.arena.get(callee) else {
            return None;
        };
        let returns = self.arena.normalize_pack(function.returns);
        returns.tail.is_none().then_some(returns.types.len())
    }
    fn require_read_property(
        &mut self,
        table: TypeId,
        name: String,
        value: TypeId,
    ) -> Result<(), ConstraintSolveError> {
        let expected = self.expected_property_table(name, TableProperty::read_only(value));
        self.require_subtype(table, expected)
    }
    fn like_key_suggestion_error(
        &mut self,
        table: TypeId,
        table_type: &TableType,
        name: &str,
        value: TypeId,
    ) -> Option<ConstraintSolveError> {
        let suggestions = like_key_suggestions(table_type, name);
        if suggestions.is_empty() {
            return None;
        }
        let expected =
            self.expected_property_table(name.to_owned(), TableProperty::read_only(value));
        Some(ConstraintSolveError::Subtype(SubtypeError {
            kind: SubtypeErrorKind::LikeKeySuggestion {
                name: name.to_owned(),
                suggestions,
            },
            path: TypePath::new().push(TypePathComponent::read_property(name)),
            sub: SubtypeTarget::Type(table),
            sup: SubtypeTarget::Type(expected),
        }))
    }
    fn missing_property_read_error(
        &mut self,
        table: TypeId,
        name: String,
        value: TypeId,
    ) -> ConstraintSolveError {
        let expected = self.expected_property_table(name.clone(), TableProperty::read_only(value));
        ConstraintSolveError::Subtype(SubtypeError {
            kind: SubtypeErrorKind::MissingProperty,
            path: TypePath::new().push(TypePathComponent::read_property(name)),
            sub: SubtypeTarget::Type(table),
            sup: SubtypeTarget::Type(expected),
        })
    }
    fn intersection_property_read_value(
        &mut self,
        options: &[TypeId],
        name: &str,
    ) -> Option<TypeId> {
        let mut values = Vec::new();
        for option in options {
            let option = self.arena.follow(*option);
            match self.arena.get(option).clone() {
                TypeKind::Table(table_type) => {
                    if let Some(property) = table_type.properties.get(name)
                        && member_access::table_property_allows_read(property, table_type.state)
                    {
                        values.push(property.ty);
                        continue;
                    }
                    if let Some(indexer) = table_type.indexer {
                        let string = member_access::property_name_key(self.arena, name);
                        if Subtyper::new(self.arena)
                            .is_subtype(string, indexer.key)
                            .is_ok()
                        {
                            values.push(indexer.value);
                        }
                    }
                }
                TypeKind::Union(types) => {
                    if let Some(value) = self.union_property_read_value(&types, name) {
                        values.push(value);
                    }
                }
                TypeKind::Any
                | TypeKind::Unknown
                | TypeKind::Error
                | TypeKind::Blocked(_)
                | TypeKind::Free(_) => {
                    values.push(self.arena.primitives().any);
                }
                TypeKind::Negation(inner) if self.negated_type_may_have_properties(inner) => {
                    values.push(self.arena.primitives().any);
                }
                _ => {}
            }
        }
        if values.is_empty() {
            return None;
        }
        let mut unique_values = BTreeMap::new();
        for value in values {
            unique_values
                .entry(self.arena.summary(value))
                .or_insert(value);
        }
        Some(self.intersection_type(unique_values.into_values().collect()))
    }
    fn intersection_generic_missing_read_property(
        &mut self,
        options: &[TypeId],
        name: &str,
    ) -> bool {
        let has_generic = options
            .iter()
            .copied()
            .map(|option| self.arena.follow(option))
            .any(|option| matches!(self.arena.get(option), TypeKind::Generic(_)));
        has_generic
            && self
                .intersection_property_read_value(options, name)
                .is_none()
    }
    fn union_property_read_value(&mut self, options: &[TypeId], name: &str) -> Option<TypeId> {
        let mut values = Vec::new();
        for option in options {
            let option = self.arena.follow(*option);
            match self.arena.get(option).clone() {
                TypeKind::Table(table_type) => {
                    let property = table_type.properties.get(name)?;
                    if !member_access::table_property_allows_read(property, table_type.state) {
                        return None;
                    }
                    values.push(property.ty);
                }
                TypeKind::Any | TypeKind::Unknown | TypeKind::Error => {
                    values.push(self.arena.primitives().any);
                }
                _ => return None,
            }
        }
        Some(self.union_type(values))
    }
    fn intersection_contains_union(&self, options: &[TypeId]) -> bool {
        options
            .iter()
            .copied()
            .map(|option| self.arena.follow(option))
            .any(|option| matches!(self.arena.get(option), TypeKind::Union(_)))
    }
    fn intersection_missing_read_property_witness(&self, options: &[TypeId]) -> Option<TypeId> {
        options
            .iter()
            .copied()
            .map(|option| self.arena.follow(option))
            .find(|option| {
                !matches!(
                    self.arena.get(*option),
                    TypeKind::Any
                        | TypeKind::Unknown
                        | TypeKind::Error
                        | TypeKind::Blocked(_)
                        | TypeKind::Free(_)
                        | TypeKind::Generic(_)
                )
            })
    }
    fn union_indexer_read_value(&mut self, options: &[TypeId], key: TypeId) -> Option<TypeId> {
        let mut values = Vec::new();
        let non_singleton_key = member_access::string_singleton_key(self.arena, key).is_none();
        for option in options {
            let option = self.arena.follow(*option);
            match self.arena.get(option).clone() {
                TypeKind::Table(table_type) => {
                    if let Some(indexer) = table_type.indexer
                        && Subtyper::new(self.arena)
                            .is_subtype(key, indexer.key)
                            .is_ok()
                    {
                        values.push(indexer.value);
                        continue;
                    }
                    if non_singleton_key {
                        let fallback = if matches!(
                            table_type.state,
                            TableState::Unsealed | TableState::Free
                        ) {
                            self.arena.primitives().unknown
                        } else {
                            self.arena.primitives().error
                        };
                        values.push(fallback);
                    } else {
                        return None;
                    }
                }
                TypeKind::Any | TypeKind::Unknown | TypeKind::Error => {
                    values.push(self.arena.primitives().any);
                }
                _ => return None,
            }
        }
        Some(self.normalized_union_type(values))
    }
    fn intersection_indexer_read_value(
        &mut self,
        options: &[TypeId],
        key: TypeId,
    ) -> Option<TypeId> {
        let mut values = Vec::new();
        let string_key = member_access::string_singleton_key(self.arena, key);
        for option in options {
            let option = self.arena.follow(*option);
            match self.arena.get(option).clone() {
                TypeKind::Table(table_type) => {
                    if let Some(name) = string_key.as_ref()
                        && let Some(property) = table_type.properties.get(name)
                        && member_access::table_property_allows_read(property, table_type.state)
                    {
                        values.push(property.ty);
                        continue;
                    }
                    if let Some(indexer) = table_type.indexer
                        && Subtyper::new(self.arena)
                            .is_subtype(key, indexer.key)
                            .is_ok()
                    {
                        values.push(indexer.value);
                    }
                }
                TypeKind::Any | TypeKind::Unknown | TypeKind::Error => {
                    values.push(self.arena.primitives().any);
                }
                _ => {}
            }
        }
        if values.is_empty() && string_key.is_none() {
            Some(self.arena.primitives().error)
        } else {
            (!values.is_empty()).then(|| self.intersection_type(values))
        }
    }
    fn write_union_property(
        &mut self,
        union: TypeId,
        name: &str,
        value: TypeId,
    ) -> Result<(), ConstraintSolveError> {
        let mut expected_values = Vec::new();
        let mut every_option_writable = true;
        let mut first_error = None;

        for option in self.arena.union_options(union) {
            self.collect_union_property_write(
                option,
                name,
                value,
                &mut expected_values,
                &mut every_option_writable,
                &mut first_error,
            );
        }

        if !expected_values.is_empty() {
            let expected = self.intersection_type(expected_values.clone());
            self.bind_free_write_value(value, expected);
            for expected in expected_values {
                if let Err(error) = self.require_write_value(value, expected)
                    && first_error.is_none()
                {
                    first_error = Some(error);
                }
            }
        }

        if !every_option_writable
            && let Err(error) = self.require_subtype(value, self.arena.primitives().never)
            && first_error.is_none()
        {
            first_error = Some(error);
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
    fn collect_union_property_write(
        &mut self,
        ty: TypeId,
        name: &str,
        value: TypeId,
        expected_values: &mut Vec<TypeId>,
        every_option_writable: &mut bool,
        first_error: &mut Option<ConstraintSolveError>,
    ) {
        let ty = self.arena.follow(ty);
        match self.arena.get(ty).clone() {
            TypeKind::Table(table_type) => {
                let mutable_table =
                    member_access::table_state_allows_member_extension(table_type.state);
                if let Some(property) = table_type.properties.get(name).cloned() {
                    expected_values.push(property.write_type());
                    if !mutable_table && property.read_only {
                        *every_option_writable = false;
                    }
                    return;
                }
                if mutable_table {
                    return;
                }
                if let Some(indexer) = table_type.indexer {
                    let key = member_access::property_name_key(self.arena, name);
                    if let Err(error) = self.require_subtype(key, indexer.key)
                        && first_error.is_none()
                    {
                        *first_error = Some(error);
                    }
                    expected_values.push(indexer.value);
                    if indexer.read_only {
                        *every_option_writable = false;
                    }
                    return;
                }
                *every_option_writable = false;
                self.require_missing_property_write(ty, name, value, first_error);
            }
            TypeKind::Metatable {
                table: base_table, ..
            } => self.collect_union_property_write(
                base_table,
                name,
                value,
                expected_values,
                every_option_writable,
                first_error,
            ),
            TypeKind::Extern {
                properties,
                indexer,
                ..
            } => {
                if let Some(property) = properties.get(name) {
                    expected_values.push(property.write_type());
                    if property.read_only {
                        *every_option_writable = false;
                    }
                    return;
                }
                if let Some(indexer) = indexer {
                    let key = member_access::property_name_key(self.arena, name);
                    if let Err(error) = self.require_subtype(key, indexer.key)
                        && first_error.is_none()
                    {
                        *first_error = Some(error);
                    }
                    expected_values.push(indexer.value);
                    if indexer.read_only {
                        *every_option_writable = false;
                    }
                    return;
                }
                *every_option_writable = false;
                self.require_missing_property_write(ty, name, value, first_error);
            }
            TypeKind::Union(options) => {
                for option in options {
                    self.collect_union_property_write(
                        option,
                        name,
                        value,
                        expected_values,
                        every_option_writable,
                        first_error,
                    );
                }
            }
            TypeKind::Intersection(members) => {
                let negates_nil = members.iter().any(|member| {
                    matches!(
                        self.arena.get(self.arena.follow(*member)),
                        TypeKind::Negation(inner) if self.arena.is_nil(*inner)
                    )
                });
                let mut failures = SubtypeFailureSet::default();
                let mut has_writable_target = false;
                let found_expected = expected_values.len();
                for member in members {
                    let member = if negates_nil {
                        self.member_without_negated_nil(member)
                    } else {
                        member
                    };
                    self.collect_intersection_property_write(
                        member,
                        name,
                        expected_values,
                        &mut has_writable_target,
                        &mut failures,
                    );
                }
                if let Err(error) = failures.into_result()
                    && first_error.is_none()
                {
                    *first_error = Some(error);
                }
                if !has_writable_target {
                    *every_option_writable = false;
                    if expected_values.len() == found_expected {
                        self.require_missing_property_write(ty, name, value, first_error);
                    }
                }
            }
            TypeKind::Primitive(PrimitiveType::Nil) => {
                *every_option_writable = false;
            }
            TypeKind::Any | TypeKind::Unknown | TypeKind::Error | TypeKind::Blocked(_) => {}
            TypeKind::Free(_) => {}
            _ => {
                *every_option_writable = false;
                self.require_missing_property_write(ty, name, value, first_error);
            }
        }
    }
    fn write_intersection_property(
        &mut self,
        intersection: TypeId,
        types: &[TypeId],
        name: &str,
        value: TypeId,
    ) -> Result<(), ConstraintSolveError> {
        let mut expected_values = Vec::new();
        let mut has_writable_target = false;
        let mut failures = SubtypeFailureSet::default();

        let negates_nil = types.iter().any(|member| {
            matches!(
                self.arena.get(self.arena.follow(*member)),
                TypeKind::Negation(inner) if self.arena.is_nil(*inner)
            )
        });
        for ty in types {
            let member = if negates_nil {
                self.member_without_negated_nil(*ty)
            } else {
                *ty
            };
            self.collect_intersection_property_write(
                member,
                name,
                &mut expected_values,
                &mut has_writable_target,
                &mut failures,
            );
        }

        let has_expected_values = !expected_values.is_empty();
        if has_expected_values {
            let expected = self.intersection_type(expected_values.clone());
            self.bind_free_write_value(value, expected);
            for expected in expected_values {
                if let Err(error) = self.require_write_value(value, expected) {
                    failures.push(self.arena, error);
                }
            }
        }

        if !has_writable_target {
            if !has_expected_values {
                self.push_missing_property_write_failure(intersection, name, value, &mut failures);
            }
            if failures.is_empty()
                && let Err(error) = self.require_subtype(value, self.arena.primitives().never)
            {
                failures.push(self.arena, error);
            }
        }

        failures.into_result()
    }
    fn collect_intersection_property_write(
        &mut self,
        ty: TypeId,
        name: &str,
        expected_values: &mut Vec<TypeId>,
        has_writable_target: &mut bool,
        failures: &mut SubtypeFailureSet,
    ) {
        let ty = self.arena.follow(ty);
        match self.arena.get(ty).clone() {
            TypeKind::Table(table_type) => {
                let mutable_table =
                    member_access::table_state_allows_member_extension(table_type.state);
                if let Some(property) = table_type.properties.get(name).cloned() {
                    expected_values.push(property.write_type());
                    if mutable_table || !property.read_only {
                        *has_writable_target = true;
                    }
                    return;
                }
                if mutable_table {
                    *has_writable_target = true;
                    return;
                }
                if let Some(indexer) = table_type.indexer {
                    let key = member_access::property_name_key(self.arena, name);
                    if let Err(error) = self.require_subtype(key, indexer.key) {
                        failures.push_with_fallback_path(
                            self.arena,
                            error,
                            &Some(TypePath::new().push(TypePathComponent::write_property(name))),
                        );
                    }
                    expected_values.push(indexer.value);
                    if !indexer.read_only {
                        *has_writable_target = true;
                    }
                }
            }
            TypeKind::Metatable {
                table: base_table, ..
            } => self.collect_intersection_property_write(
                base_table,
                name,
                expected_values,
                has_writable_target,
                failures,
            ),
            TypeKind::Any | TypeKind::Unknown | TypeKind::Error | TypeKind::Blocked(_) => {
                *has_writable_target = true;
            }
            TypeKind::Free(_) => {
                *has_writable_target = true;
            }
            _ => {}
        }
    }
    fn missing_property_write_error(
        &mut self,
        table: TypeId,
        name: &str,
        value: TypeId,
    ) -> ConstraintSolveError {
        let expected = self.expected_property_table(name.to_owned(), TableProperty::new(value));
        let error = SubtypeError {
            kind: SubtypeErrorKind::MissingProperty,
            path: TypePath::new().push(TypePathComponent::write_property(name)),
            sub: SubtypeTarget::Type(table),
            sup: SubtypeTarget::Type(expected),
        };
        let suppression = Subtyper::new(self.arena).suppression(table, expected);
        ConstraintSolveError::SubtypeWithMetadata {
            error: Box::new(error),
            sub: SubtypeTarget::Type(table),
            sup: SubtypeTarget::Type(expected),
            suppression,
        }
    }
    fn require_missing_property_write(
        &mut self,
        table: TypeId,
        name: &str,
        value: TypeId,
        first_error: &mut Option<ConstraintSolveError>,
    ) {
        if first_error.is_none() {
            *first_error = Some(self.missing_property_write_error(table, name, value));
        }
    }
    fn push_missing_property_write_failure(
        &mut self,
        table: TypeId,
        name: &str,
        value: TypeId,
        failures: &mut SubtypeFailureSet,
    ) {
        let error = self.missing_property_write_error(table, name, value);
        failures.push(self.arena, error);
    }
    fn write_intersection_indexer(
        &mut self,
        types: &[TypeId],
        key: TypeId,
        value: TypeId,
    ) -> Result<(), ConstraintSolveError> {
        let string_key = member_access::string_singleton_key(self.arena, key);
        let mut expected_values = Vec::new();
        let mut has_writable_target = false;
        let mut failures = SubtypeFailureSet::default();

        for ty in types {
            self.collect_intersection_indexer_write(
                *ty,
                key,
                value,
                string_key.as_deref(),
                &mut expected_values,
                &mut has_writable_target,
                &mut failures,
            );
        }

        if !expected_values.is_empty() {
            let expected = self.intersection_type(expected_values.clone());
            let value_failure_start = failures.len();
            for expected in expected_values {
                self.bind_free_write_value(value, expected);
                if let Err(error) = self.require_subtype(value, expected) {
                    failures.push_with_fallback_path(
                        self.arena,
                        error,
                        &Some(
                            TypePath::new()
                                .push(TypePathComponent::TypeField(TypeField::IndexResult)),
                        ),
                    );
                }
            }
            if failures.len() == value_failure_start
                && let Err(error) = self.require_subtype(value, expected)
            {
                failures.push_with_fallback_path(
                    self.arena,
                    error,
                    &Some(
                        TypePath::new().push(TypePathComponent::TypeField(TypeField::IndexResult)),
                    ),
                );
            }
            if failures.is_empty() {
                self.bind_free_write_value(value, expected);
            }
        }

        if !has_writable_target
            && let Err(error) = self.require_subtype(value, self.arena.primitives().never)
        {
            failures.push_with_fallback_path(
                self.arena,
                error,
                &Some(TypePath::new().push(TypePathComponent::TypeField(TypeField::IndexResult))),
            );
        }

        failures.into_result()
    }
    #[allow(clippy::too_many_arguments)]
    fn collect_intersection_indexer_write(
        &mut self,
        ty: TypeId,
        key: TypeId,
        value: TypeId,
        string_key: Option<&str>,
        expected_values: &mut Vec<TypeId>,
        has_writable_target: &mut bool,
        failures: &mut SubtypeFailureSet,
    ) {
        let ty = self.arena.follow(ty);
        match self.arena.get(ty).clone() {
            TypeKind::Table(table_type) => {
                let mutable_table =
                    matches!(table_type.state, TableState::Unsealed | TableState::Free);
                if let Some(name) = string_key {
                    if let Some(property) = table_type.properties.get(name).cloned() {
                        expected_values.push(property.write_type());
                        if mutable_table || !property.read_only {
                            *has_writable_target = true;
                        }
                        return;
                    }
                    if mutable_table {
                        *has_writable_target = true;
                        return;
                    }
                }
                if let Some(indexer) = table_type.indexer {
                    expected_values.push(indexer.value);
                    if mutable_table || !indexer.read_only {
                        *has_writable_target = true;
                    }
                    if let Err(error) = self.require_subtype(key, indexer.key) {
                        failures.push_with_fallback_path(
                            self.arena,
                            error,
                            &Some(
                                TypePath::new()
                                    .push(TypePathComponent::TypeField(TypeField::IndexLookup)),
                            ),
                        );
                    }
                    return;
                }
                if mutable_table {
                    *has_writable_target = true;
                    return;
                }
                expected_values.push(self.arena.primitives().never);
                self.require_missing_indexer_write(ty, key, value, failures);
            }
            TypeKind::Metatable {
                table: base_table, ..
            } => self.collect_intersection_indexer_write(
                base_table,
                key,
                value,
                string_key,
                expected_values,
                has_writable_target,
                failures,
            ),
            TypeKind::Any | TypeKind::Unknown | TypeKind::Error | TypeKind::Blocked(_) => {
                *has_writable_target = true;
            }
            TypeKind::Free(_) => {
                *has_writable_target = true;
            }
            _ => {
                expected_values.push(self.arena.primitives().never);
                self.require_missing_indexer_write(ty, key, value, failures);
            }
        }
    }
    fn require_missing_indexer_write(
        &mut self,
        table: TypeId,
        key: TypeId,
        value: TypeId,
        failures: &mut SubtypeFailureSet,
    ) {
        let expected = self.expected_indexer_table(key, value);
        if let Err(error) = self.require_subtype(table, expected) {
            failures.push(self.arena, error);
        }
    }
    fn bind_free_write_value(&mut self, value: TypeId, expected: TypeId) {
        let value = self.arena.follow(value);
        let expected = self.arena.follow(expected);
        if value != expected
            && matches!(self.arena.get(value), TypeKind::Free(_))
            && !matches!(self.arena.get(expected), TypeKind::Free(_))
        {
            drop(self.unifier().unify(value, expected));
        }
    }
    fn indexer_read_value(&mut self, state: TableState, key: TypeId, value: TypeId) -> TypeId {
        if self.arena.unsealed_indexer_read_may_be_absent(state, key) {
            self.union_type(vec![value, self.arena.primitives().nil])
        } else {
            value
        }
    }
    fn unify_read_value(
        &mut self,
        value: TypeId,
        read_ty: TypeId,
    ) -> Result<(), ConstraintSolveError> {
        self.unifier()
            .unify(value, read_ty)
            .map_err(ConstraintSolveError::Unify)
    }
    fn bind_and_unify_dynamic_read_value(
        &mut self,
        value: TypeId,
        read_ty: TypeId,
    ) -> Result<(), ConstraintSolveError> {
        self.bind_dynamic_read_value(value, read_ty);
        self.unify_read_value(value, read_ty)
    }
    fn bind_dynamic_read_value(&mut self, value: TypeId, read_ty: TypeId) {
        let read_ty = self.arena.follow(read_ty);
        if !matches!(
            self.arena.get(read_ty),
            TypeKind::Any | TypeKind::Unknown | TypeKind::Error
        ) {
            return;
        }
        let value = self.arena.follow(value);
        if matches!(self.arena.get(value), TypeKind::Free(_)) {
            self.arena.bind_type(value, read_ty);
        }
    }
    fn bind_failed_read_value_to(&mut self, value: TypeId, fallback: TypeId) {
        self.bind_dynamic_read_value(value, fallback);
        match self.unifier().unify(value, fallback) {
            Ok(()) | Err(_) => {}
        }
    }
    fn bind_failed_read_value_to_any(&mut self, value: TypeId) {
        let any = self.arena.primitives().any;
        self.bind_failed_read_value_to(value, any);
    }
    fn bind_failed_read_value_to_error(&mut self, value: TypeId) {
        let error = self.arena.primitives().error;
        self.bind_failed_read_value_to(value, error);
    }
    fn bind_failed_known_non_table_read_value_to_error(&mut self, table: TypeId, value: TypeId) {
        let table = self.arena.follow(table);
        if matches!(
            self.arena.get(table),
            TypeKind::Primitive(_)
                | TypeKind::Singleton(_)
                | TypeKind::Function(_)
                | TypeKind::TypeFunctionInstance { .. }
                | TypeKind::Never
        ) {
            self.bind_failed_read_value_to_error(value);
        }
    }
}

fn like_key_suggestions(table_type: &TableType, name: &str) -> Vec<String> {
    table_type
        .properties
        .keys()
        .filter(|candidate| property_key_is_like(name, candidate))
        .cloned()
        .collect()
}
fn property_key_is_like(name: &str, candidate: &str) -> bool {
    name != candidate && name.eq_ignore_ascii_case(candidate)
}
