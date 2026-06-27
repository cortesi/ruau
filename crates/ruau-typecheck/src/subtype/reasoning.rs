//! Diagnostic reason collection for structural subtyping.

use super::{
    SubtypeError, SubtypeReasoning, SubtypeSuppression, SubtypeTarget, SubtypeVariance, Subtyper,
};
use crate::{
    member_access,
    types::{
        Arena, PackField, TableIndexer, TableProperty, TableType, TypeField, TypeId, TypeKind,
        TypePackId, TypePackKind, TypePath, TypePathComponent, TypePathRoot,
    },
};

#[allow(clippy::multiple_inherent_impl)]
impl<'a> Subtyper<'a> {
    /// Collects structural reason paths for a failing subtype relation.
    ///
    /// This is a diagnostic-facing companion to `is_subtype`: `is_subtype`
    /// remains the fast yes/no relation and this method walks the same
    /// structural shapes to retain every independent failure path the checker
    /// can currently explain.
    #[must_use]
    pub fn reasonings(&self, sub: TypeId, sup: TypeId) -> Vec<SubtypeReasoning> {
        self.collect_type_reasonings(
            sub,
            sup,
            TypePath::new(),
            TypePath::new(),
            SubtypeVariance::Covariant,
        )
    }

    /// Collects all independently failing structural paths for detailed
    /// diagnostics. Unlike `reasonings`, this expands union and intersection
    /// branches even when they are not error-suppressing.
    #[must_use]
    pub fn detailed_reasonings(&self, sub: TypeId, sup: TypeId) -> Vec<SubtypeReasoning> {
        self.collect_detailed_type_reasonings(
            sub,
            sup,
            TypePath::new(),
            TypePath::new(),
            SubtypeVariance::Covariant,
        )
    }

    /// Summarizes which retained reason paths are error-suppressing.
    #[must_use]
    pub fn suppression(&self, sub: TypeId, sup: TypeId) -> SubtypeSuppression {
        let reasonings = self.reasonings(sub, sup);
        let suppressing_reasonings = reasonings
            .iter()
            .filter(|reasoning| {
                self.reasoning_suppresses_errors_from_roots(
                    TypePathRoot::Type(sub),
                    TypePathRoot::Type(sup),
                    reasoning,
                )
            })
            .cloned()
            .collect::<Vec<_>>();

        SubtypeSuppression {
            fully_suppressing: !reasonings.is_empty()
                && suppressing_reasonings.len() == reasonings.len(),
            suppressing_reasonings,
        }
    }

    pub(super) fn subtype_error_suppresses_errors(&self, error: &SubtypeError) -> bool {
        self.subtype_target_suppresses_errors(error.sub)
            || self.subtype_target_suppresses_errors(error.sup)
    }

    fn subtype_target_suppresses_errors(&self, target: SubtypeTarget) -> bool {
        match target {
            SubtypeTarget::Type(ty) => self.type_suppresses_errors(self.arena, ty, &mut Vec::new()),
            SubtypeTarget::Pack(pack) => {
                self.pack_suppresses_errors(self.arena, pack, &mut Vec::new(), &mut Vec::new())
            }
        }
    }

    /// Collects structural reason paths for a failing subtype-pack relation.
    #[must_use]
    pub fn pack_reasonings(&self, sub: TypePackId, sup: TypePackId) -> Vec<SubtypeReasoning> {
        self.collect_pack_reasonings(
            sub,
            sup,
            TypePath::new(),
            TypePath::new(),
            SubtypeVariance::Covariant,
        )
    }

    /// Summarizes which retained subtype-pack reason paths are
    /// error-suppressing.
    #[must_use]
    pub fn pack_suppression(&self, sub: TypePackId, sup: TypePackId) -> SubtypeSuppression {
        let reasonings = self.pack_reasonings(sub, sup);
        let suppressing_reasonings = reasonings
            .iter()
            .filter(|reasoning| {
                self.reasoning_suppresses_errors_from_roots(
                    TypePathRoot::Pack(sub),
                    TypePathRoot::Pack(sup),
                    reasoning,
                )
            })
            .cloned()
            .collect::<Vec<_>>();

        SubtypeSuppression {
            fully_suppressing: !reasonings.is_empty()
                && suppressing_reasonings.len() == reasonings.len(),
            suppressing_reasonings,
        }
    }

    fn collect_type_reasonings(
        &self,
        sub: TypeId,
        sup: TypeId,
        sub_path: TypePath,
        sup_path: TypePath,
        variance: SubtypeVariance,
    ) -> Vec<SubtypeReasoning> {
        let sub = self.arena.follow(sub);
        let sup = self.arena.follow(sup);
        if self.spawn_same_arena().is_subtype(sub, sup).is_ok() {
            return Vec::new();
        }
        let Some(_reasoning_guard) = self.enter_reasoning(sub, sup) else {
            return Vec::new();
        };

        let sub_kind = self.arena.get(sub).clone();
        let sup_kind = self.arena.get(sup).clone();

        if let TypeKind::TypeFunctionInstance { name, arguments } = &sub_kind
            && name == "union"
        {
            let mut reasonings = Vec::new();
            for (index, argument) in arguments.iter().copied().enumerate() {
                if matches!(self.arena.get(self.arena.follow(argument)), TypeKind::Never) {
                    continue;
                }
                reasonings.extend(self.collect_type_reasonings(
                    argument,
                    sup,
                    sub_path.push(TypePathComponent::Index { index }),
                    sup_path.clone(),
                    variance,
                ));
            }
            if !reasonings.is_empty() {
                return reasonings;
            }
        }

        match (sub_kind, sup_kind) {
            (TypeKind::Union(options), _) => {
                let suppressing_reasonings = self.collect_suppressing_option_reasonings(
                    options,
                    &sub_path,
                    &sup_path,
                    variance,
                    ReasoningSide::Sub,
                );
                if !suppressing_reasonings.is_empty() {
                    return suppressing_reasonings;
                }

                vec![SubtypeReasoning {
                    sub_path,
                    sup_path,
                    variance,
                }]
            }
            (_, TypeKind::Intersection(options)) => {
                let suppressing_reasonings = self.collect_suppressing_option_reasonings(
                    options,
                    &sub_path,
                    &sup_path,
                    variance,
                    ReasoningSide::Sup,
                );
                if !suppressing_reasonings.is_empty() {
                    return suppressing_reasonings;
                }

                vec![SubtypeReasoning {
                    sub_path,
                    sup_path,
                    variance,
                }]
            }
            (TypeKind::Function(sub_function), TypeKind::Function(sup_function)) => {
                let mut reasonings = self.collect_pack_reasonings(
                    sub_function.returns,
                    sup_function.returns,
                    sub_path.push(TypePathComponent::PackField(PackField::Returns)),
                    sup_path.push(TypePathComponent::PackField(PackField::Returns)),
                    variance,
                );
                let mut argument_reasonings = self.collect_pack_reasonings(
                    sup_function.arguments,
                    sub_function.arguments,
                    sup_path.push(TypePathComponent::PackField(PackField::Arguments)),
                    sub_path.push(TypePathComponent::PackField(PackField::Arguments)),
                    SubtypeVariance::Contravariant,
                );
                for reasoning in &mut argument_reasonings {
                    std::mem::swap(&mut reasoning.sub_path, &mut reasoning.sup_path);
                }
                reasonings.extend(argument_reasonings);
                reasonings
            }
            (TypeKind::Table(sub_table), TypeKind::Table(sup_table)) => {
                self.collect_table_reasonings(sub_table, sup_table, sub_path, sup_path, variance)
            }
            (
                TypeKind::Metatable {
                    table: sub_table,
                    metatable: sub_metatable,
                    ..
                },
                TypeKind::Table(sup_table),
            ) => {
                let sub_table = self.arena.follow(sub_table);
                let mut sub_table_type = match self.arena.get(sub_table).clone() {
                    TypeKind::Table(table) => table,
                    _ => {
                        return vec![SubtypeReasoning {
                            sub_path,
                            sup_path,
                            variance,
                        }];
                    }
                };
                if sub_table_type.indexer.is_none()
                    && let Some(indexer) =
                        member_access::function_indexer_metatable(self.arena, sub_metatable)
                {
                    sub_table_type.indexer = Some(indexer);
                }
                self.collect_table_reasonings(
                    sub_table_type,
                    sup_table,
                    sub_path.push(TypePathComponent::TypeField(TypeField::Table)),
                    sup_path,
                    variance,
                )
            }
            (
                TypeKind::Metatable {
                    table: sub_table,
                    metatable: sub_metatable,
                    ..
                },
                TypeKind::Metatable {
                    table: sup_table,
                    metatable: sup_metatable,
                    ..
                },
            ) => {
                let mut reasonings = self.collect_type_reasonings(
                    sub_table,
                    sup_table,
                    sub_path.push(TypePathComponent::TypeField(TypeField::Table)),
                    sup_path.push(TypePathComponent::TypeField(TypeField::Table)),
                    variance,
                );
                reasonings.extend(self.collect_type_reasonings(
                    sub_metatable,
                    sup_metatable,
                    sub_path.push(TypePathComponent::TypeField(TypeField::Metatable)),
                    sup_path.push(TypePathComponent::TypeField(TypeField::Metatable)),
                    variance,
                ));
                reasonings
            }
            (_, TypeKind::Negation(_)) => {
                vec![SubtypeReasoning {
                    sub_path,
                    sup_path: sup_path.push(TypePathComponent::TypeField(TypeField::Negated)),
                    variance,
                }]
            }
            _ => vec![SubtypeReasoning {
                sub_path,
                sup_path,
                variance,
            }],
        }
    }

    fn collect_detailed_type_reasonings(
        &self,
        sub: TypeId,
        sup: TypeId,
        sub_path: TypePath,
        sup_path: TypePath,
        variance: SubtypeVariance,
    ) -> Vec<SubtypeReasoning> {
        let sub = self.arena.follow(sub);
        let sup = self.arena.follow(sup);
        if self.spawn_same_arena().is_subtype(sub, sup).is_ok() {
            return Vec::new();
        }
        let Some(_reasoning_guard) = self.enter_reasoning(sub, sup) else {
            return Vec::new();
        };

        match (self.arena.get(sub).clone(), self.arena.get(sup).clone()) {
            (TypeKind::Union(options), _) | (TypeKind::Intersection(options), _) => self
                .collect_detailed_option_reasonings(
                    options,
                    sub_path,
                    sup_path,
                    variance,
                    ReasoningSide::Sub,
                    sup,
                ),
            (_, TypeKind::Union(options)) | (_, TypeKind::Intersection(options)) => self
                .collect_detailed_option_reasonings(
                    options,
                    sub_path,
                    sup_path,
                    variance,
                    ReasoningSide::Sup,
                    sub,
                ),
            _ => self.collect_type_reasonings(sub, sup, sub_path, sup_path, variance),
        }
    }

    fn collect_detailed_option_reasonings(
        &self,
        options: Vec<TypeId>,
        sub_path: TypePath,
        sup_path: TypePath,
        variance: SubtypeVariance,
        side: ReasoningSide,
        other: TypeId,
    ) -> Vec<SubtypeReasoning> {
        let mut reasonings = Vec::new();
        for (index, option) in options.into_iter().enumerate() {
            let option_path = TypePathComponent::Index { index };
            let option_reasonings = match side {
                ReasoningSide::Sub => self.collect_detailed_type_reasonings(
                    option,
                    other,
                    sub_path.push(option_path),
                    sup_path.clone(),
                    variance,
                ),
                ReasoningSide::Sup => self.collect_detailed_type_reasonings(
                    other,
                    option,
                    sub_path.clone(),
                    sup_path.push(option_path),
                    variance,
                ),
            };
            reasonings.extend(option_reasonings);
        }
        if reasonings.is_empty() {
            reasonings.push(SubtypeReasoning {
                sub_path,
                sup_path,
                variance,
            });
        }
        reasonings
    }

    fn collect_suppressing_option_reasonings(
        &self,
        options: Vec<TypeId>,
        sub_path: &TypePath,
        sup_path: &TypePath,
        variance: SubtypeVariance,
        side: ReasoningSide,
    ) -> Vec<SubtypeReasoning> {
        options
            .into_iter()
            .enumerate()
            .filter(|(_, option)| self.type_suppresses_errors(self.arena, *option, &mut Vec::new()))
            .map(|(index, _)| {
                let option_path = TypePathComponent::Index { index };
                match side {
                    ReasoningSide::Sub => SubtypeReasoning {
                        sub_path: sub_path.push(option_path),
                        sup_path: sup_path.clone(),
                        variance,
                    },
                    ReasoningSide::Sup => SubtypeReasoning {
                        sub_path: sub_path.clone(),
                        sup_path: sup_path.push(option_path),
                        variance,
                    },
                }
            })
            .collect()
    }

    fn collect_table_reasonings(
        &self,
        sub: TableType,
        sup: TableType,
        sub_path: TypePath,
        sup_path: TypePath,
        variance: SubtypeVariance,
    ) -> Vec<SubtypeReasoning> {
        let mut reasonings = Vec::new();

        for (name, sup_property) in sup.properties {
            let sub_property = sub.properties.get(&name).cloned().or_else(|| {
                let sub_indexer = sub.indexer.as_ref()?;
                let mut subtyper = self.spawn_same_arena();
                subtyper
                    .subtype_property_name_key(&name, sub_indexer.key, sub_path.clone())
                    .ok()?;
                Some(TableProperty {
                    ty: sub_indexer.value,
                    write_ty: None,
                    location: None,
                    documentation_symbol: None,
                    read_only: sub_indexer.read_only,
                    write_only: false,
                    deprecated: false,
                })
            });

            let property_path = TypePathComponent::read_property(name.clone());
            let Some(sub_property) = sub_property else {
                reasonings.push(SubtypeReasoning {
                    sub_path: sub_path.push(property_path.clone()),
                    sup_path: sup_path.push(property_path),
                    variance,
                });
                continue;
            };

            reasonings.extend(self.collect_property_reasonings(
                &sub_property,
                &sup_property,
                sub_path.push(property_path.clone()),
                sup_path.push(property_path),
                SubtypeVariance::Invariant,
            ));
        }

        match (sub.indexer, sup.indexer) {
            (Some(sub_indexer), Some(sup_indexer)) => {
                reasonings.extend(self.collect_indexer_reasonings(
                    &sub_indexer,
                    &sup_indexer,
                    &sub_path,
                    &sup_path,
                ));
            }
            (None, Some(_)) => {
                reasonings.push(SubtypeReasoning {
                    sub_path,
                    sup_path,
                    variance,
                });
            }
            _ => {}
        }

        reasonings
    }

    fn collect_property_reasonings(
        &self,
        sub: &TableProperty,
        sup: &TableProperty,
        sub_path: TypePath,
        sup_path: TypePath,
        variance: SubtypeVariance,
    ) -> Vec<SubtypeReasoning> {
        if sub.deprecated != sup.deprecated
            || sub.read_only
                && !sup.read_only
                && member_access::property_modifier_is_concrete(self.arena, sub.ty)
            || sub.write_only
                && !sup.write_only
                && member_access::property_modifier_is_concrete(self.arena, sub.ty)
        {
            return vec![SubtypeReasoning {
                sub_path,
                sup_path,
                variance,
            }];
        }

        if sup.read_only {
            return self.collect_type_reasonings(sub.ty, sup.ty, sub_path, sup_path, variance);
        }
        if sup.write_only {
            return self.collect_type_reasonings(
                sup.ty,
                sub.ty,
                sup_path,
                sub_path,
                SubtypeVariance::Contravariant,
            );
        }

        if self.arena.follow(sub.ty) == self.arena.follow(sup.ty) {
            return Vec::new();
        }
        let forward = self.collect_type_reasonings(
            sub.ty,
            sup.ty,
            sub_path.clone(),
            sup_path.clone(),
            variance,
        );
        if !forward.is_empty() {
            return forward;
        }
        self.collect_type_reasonings(sup.ty, sub.ty, sup_path, sub_path, variance)
    }

    fn collect_indexer_reasonings(
        &self,
        sub: &TableIndexer,
        sup: &TableIndexer,
        sub_path: &TypePath,
        sup_path: &TypePath,
    ) -> Vec<SubtypeReasoning> {
        let mut reasonings = Vec::new();
        if self.arena.follow(sub.key) != self.arena.follow(sup.key) {
            reasonings.push(SubtypeReasoning {
                sub_path: sub_path.push(TypePathComponent::TypeField(TypeField::IndexLookup)),
                sup_path: sup_path.push(TypePathComponent::TypeField(TypeField::IndexLookup)),
                variance: SubtypeVariance::Invariant,
            });
        }

        let value_path = TypePathComponent::TypeField(TypeField::IndexResult);
        if sub.read_only && !sup.read_only {
            reasonings.push(SubtypeReasoning {
                sub_path: sub_path.push(value_path.clone()),
                sup_path: sup_path.push(value_path),
                variance: SubtypeVariance::Invariant,
            });
        } else if sup.read_only {
            reasonings.extend(self.collect_type_reasonings(
                sub.value,
                sup.value,
                sub_path.push(value_path.clone()),
                sup_path.push(value_path),
                SubtypeVariance::Covariant,
            ));
        } else if self.arena.follow(sub.value) != self.arena.follow(sup.value) {
            reasonings.push(SubtypeReasoning {
                sub_path: sub_path.push(value_path.clone()),
                sup_path: sup_path.push(value_path),
                variance: SubtypeVariance::Invariant,
            });
        }

        reasonings
    }

    fn collect_pack_reasonings(
        &self,
        sub: TypePackId,
        sup: TypePackId,
        sub_path: TypePath,
        sup_path: TypePath,
        variance: SubtypeVariance,
    ) -> Vec<SubtypeReasoning> {
        let sub = self.arena.follow_pack(sub);
        let sup = self.arena.follow_pack(sup);
        let Some(_reasoning_guard) = self.enter_pack_reasoning(sub, sup) else {
            return Vec::new();
        };
        if self.spawn_same_arena().is_subtype_pack(sub, sup).is_ok() {
            return Vec::new();
        }

        match (
            self.arena.get_pack(sub).clone(),
            self.arena.get_pack(sup).clone(),
        ) {
            (TypePackKind::Variadic { ty: sub_ty }, TypePackKind::Variadic { ty: sup_ty }) => self
                .collect_type_reasonings(
                    sub_ty,
                    sup_ty,
                    sub_path.push(TypePathComponent::TypeField(TypeField::Variadic)),
                    sup_path.push(TypePathComponent::TypeField(TypeField::Variadic)),
                    variance,
                ),
            (
                TypePackKind::List {
                    types: sub_types,
                    tail: sub_tail,
                },
                TypePackKind::List {
                    types: sup_types,
                    tail: sup_tail,
                },
            ) => self.collect_list_pack_reasonings(
                sub_types,
                sub_tail,
                sup_types,
                sup_tail,
                PackReasoningContext::new(sub_path, sup_path, variance),
            ),
            (
                TypePackKind::List {
                    types: sub_types,
                    tail: sub_tail,
                },
                TypePackKind::Variadic { ty: sup_ty },
            ) => {
                let mut reasonings = Vec::new();
                for (index, sub_ty) in sub_types.into_iter().enumerate() {
                    reasonings.extend(self.collect_type_reasonings(
                        sub_ty,
                        sup_ty,
                        sub_path.push(TypePathComponent::Index { index }),
                        sup_path.push(TypePathComponent::TypeField(TypeField::Variadic)),
                        variance,
                    ));
                }
                if let Some(sub_tail) = sub_tail {
                    reasonings.extend(self.collect_pack_reasonings(
                        sub_tail,
                        sup,
                        sub_path.push(TypePathComponent::PackField(PackField::Tail)),
                        sup_path,
                        variance,
                    ));
                }
                reasonings
            }
            _ => vec![SubtypeReasoning {
                sub_path,
                sup_path,
                variance,
            }],
        }
    }

    fn collect_list_pack_reasonings(
        &self,
        sub_types: Vec<TypeId>,
        sub_tail: Option<TypePackId>,
        sup_types: Vec<TypeId>,
        sup_tail: Option<TypePackId>,
        context: PackReasoningContext,
    ) -> Vec<SubtypeReasoning> {
        let PackReasoningContext {
            sub_path,
            sup_path,
            variance,
        } = context;
        let mut reasonings = Vec::new();
        let common_len = sub_types.len().min(sup_types.len());
        for index in 0..common_len {
            reasonings.extend(self.collect_type_reasonings(
                sub_types[index],
                sup_types[index],
                sub_path.push(TypePathComponent::Index { index }),
                sup_path.push(TypePathComponent::Index { index }),
                variance,
            ));
        }

        match sub_types.len().cmp(&sup_types.len()) {
            std::cmp::Ordering::Equal => {
                if let (Some(sub_tail), Some(sup_tail)) = (sub_tail, sup_tail) {
                    reasonings.extend(self.collect_pack_reasonings(
                        sub_tail,
                        sup_tail,
                        sub_path.push(TypePathComponent::PackField(PackField::Tail)),
                        sup_path.push(TypePathComponent::PackField(PackField::Tail)),
                        variance,
                    ));
                }
            }
            std::cmp::Ordering::Greater => {
                let Some(sup_tail) = sup_tail else {
                    reasonings.push(SubtypeReasoning {
                        sub_path,
                        sup_path,
                        variance,
                    });
                    return reasonings;
                };
                let TypePackKind::Variadic { ty: sup_ty } = self.arena.get_pack(sup_tail).clone()
                else {
                    reasonings.push(SubtypeReasoning {
                        sub_path,
                        sup_path,
                        variance,
                    });
                    return reasonings;
                };
                for (index, sub_ty) in sub_types.into_iter().enumerate().skip(common_len) {
                    reasonings.extend(
                        self.collect_type_reasonings(
                            sub_ty,
                            sup_ty,
                            sub_path.push(TypePathComponent::Index { index }),
                            sup_path
                                .push(TypePathComponent::PackField(PackField::Tail))
                                .push(TypePathComponent::TypeField(TypeField::Variadic)),
                            variance,
                        ),
                    );
                }
            }
            std::cmp::Ordering::Less => {
                let Some(sub_tail) = sub_tail else {
                    reasonings.push(SubtypeReasoning {
                        sub_path,
                        sup_path,
                        variance,
                    });
                    return reasonings;
                };
                let TypePackKind::Variadic { ty: sub_ty } = self.arena.get_pack(sub_tail).clone()
                else {
                    reasonings.push(SubtypeReasoning {
                        sub_path,
                        sup_path,
                        variance,
                    });
                    return reasonings;
                };
                for (index, sup_ty) in sup_types.into_iter().enumerate().skip(common_len) {
                    reasonings.extend(
                        self.collect_type_reasonings(
                            sub_ty,
                            sup_ty,
                            sub_path
                                .push(TypePathComponent::PackField(PackField::Tail))
                                .push(TypePathComponent::TypeField(TypeField::Variadic)),
                            sup_path.push(TypePathComponent::Index { index }),
                            variance,
                        ),
                    );
                }
            }
        }

        reasonings
    }

    fn reasoning_suppresses_errors_from_roots(
        &self,
        sub: TypePathRoot,
        sup: TypePathRoot,
        reasoning: &SubtypeReasoning,
    ) -> bool {
        let sub_ty = self.arena.traverse_path_for_type(sub, &reasoning.sub_path);
        let sup_ty = self.arena.traverse_path_for_type(sup, &reasoning.sup_path);
        if let (Some(sub_ty), Some(sup_ty)) = (sub_ty, sup_ty) {
            return self.type_suppresses_errors(self.arena, sub_ty, &mut Vec::new())
                || self.type_suppresses_errors(self.arena, sup_ty, &mut Vec::new());
        }

        let mut scratch = self.arena.clone();
        let sub_pack = scratch.traverse_path_for_pack(sub, &reasoning.sub_path);
        let sup_pack = scratch.traverse_path_for_pack(sup, &reasoning.sup_path);
        if let (Some(sub_pack), Some(sup_pack)) = (sub_pack, sup_pack) {
            return self.pack_suppresses_errors(
                &scratch,
                sub_pack,
                &mut Vec::new(),
                &mut Vec::new(),
            ) || self.pack_suppresses_errors(
                &scratch,
                sup_pack,
                &mut Vec::new(),
                &mut Vec::new(),
            );
        }

        false
    }

    fn type_suppresses_errors(&self, arena: &Arena, ty: TypeId, seen: &mut Vec<TypeId>) -> bool {
        let ty = arena.follow(ty);
        if seen.contains(&ty) {
            return false;
        }
        seen.push(ty);
        let suppresses = match arena.get(ty) {
            TypeKind::Any | TypeKind::Error => true,
            TypeKind::Bound(bound) | TypeKind::Negation(bound) => {
                self.type_suppresses_errors(arena, *bound, seen)
            }
            TypeKind::Union(options) | TypeKind::Intersection(options) => options
                .iter()
                .any(|option| self.type_suppresses_errors(arena, *option, seen)),
            TypeKind::Metatable {
                table, metatable, ..
            } => {
                self.type_suppresses_errors(arena, *table, seen)
                    || self.type_suppresses_errors(arena, *metatable, seen)
            }
            TypeKind::TypeFunctionInstance { arguments, .. } => arguments
                .iter()
                .any(|argument| self.type_suppresses_errors(arena, *argument, seen)),
            _ => false,
        };
        seen.pop();
        suppresses
    }

    fn pack_suppresses_errors(
        &self,
        arena: &Arena,
        pack: TypePackId,
        seen_types: &mut Vec<TypeId>,
        seen_packs: &mut Vec<TypePackId>,
    ) -> bool {
        let pack = arena.follow_pack(pack);
        if seen_packs.contains(&pack) {
            return false;
        }
        seen_packs.push(pack);
        let suppresses = match arena.get_pack(pack) {
            TypePackKind::Error => true,
            TypePackKind::Bound(bound) => {
                self.pack_suppresses_errors(arena, *bound, seen_types, seen_packs)
            }
            TypePackKind::Variadic { ty } => self.type_suppresses_errors(arena, *ty, seen_types),
            TypePackKind::List { types, tail } => {
                types
                    .iter()
                    .any(|ty| self.type_suppresses_errors(arena, *ty, seen_types))
                    || tail.is_some_and(|tail| {
                        self.pack_suppresses_errors(arena, tail, seen_types, seen_packs)
                    })
            }
            TypePackKind::Free { .. } | TypePackKind::Generic(_) => false,
        };
        seen_packs.pop();
        suppresses
    }
}

#[derive(Clone, Copy)]
enum ReasoningSide {
    Sub,
    Sup,
}

struct PackReasoningContext {
    sub_path: TypePath,
    sup_path: TypePath,
    variance: SubtypeVariance,
}

impl PackReasoningContext {
    fn new(sub_path: TypePath, sup_path: TypePath, variance: SubtypeVariance) -> Self {
        Self {
            sub_path,
            sup_path,
            variance,
        }
    }
}
