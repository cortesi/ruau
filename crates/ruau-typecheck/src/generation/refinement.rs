//! Type refinement (narrowing) for expression constraint generation.
//!
//! Owns the truthiness, `type`/`typeof`, singleton, and `isa` narrowing logic:
//! the `*_refinements` entry points that statement/expression generation call
//! to compute per-branch [`RefinementMap`]s, plus the supporting machinery that
//! removes or isolates singleton/typeof options from a type.

use std::collections::{BTreeMap, BTreeSet};

use ruau_syntax::{BinaryOp, Expr, IndexOp, LocalId, SyntaxId, UnaryOp};

use crate::{
    dfg::{RefinementKey, RefinementMap},
    generation::{
        expression::{RefinementSense, TypeofRefinementSense, TypeofTag, callee_name},
        state::ExpressionConstraintGenerator,
    },
    scopes::{ScopeId, Symbol, TypeBindingKind},
    types::{
        PrimitiveType, SingletonType, TableIndexer, TableProperty, TableState, TableType, TypeId,
        TypeKind, TypePackKind, extern_is_subtype,
    },
};

const MAX_GENERATED_INTERSECTION_OPTIONS: usize = 32;
const MAX_PROPERTY_REFINEMENT_TABLE_PROPERTIES: usize = 16;

#[derive(Clone, Debug, Eq, PartialEq)]
enum IsaRefinementTarget {
    Local(LocalId),
    Property(LocalId, String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TypeofRefinementTarget {
    Local(LocalId),
    Global(String),
}

/// Which branch of a refining condition is being typed: the branch the
/// condition makes truthy, or the one it makes falsy.
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum Truthiness {
    Truthy,
    Falsy,
}

#[allow(clippy::multiple_inherent_impl)]
impl<'a> ExpressionConstraintGenerator<'a> {
    pub(crate) fn truthy_refinements(&mut self, condition: &Expr) -> RefinementMap {
        match condition {
            Expr::Local { local, .. } => self.local_refinement(local.id, RefinementSense::Truthy),
            Expr::IndexName { expr, index, .. } => self
                .local_from_grouped_expr(expr)
                .map(|local_id| {
                    self.local_property_refinement(
                        local_id,
                        index.as_str(),
                        RefinementSense::Truthy,
                    )
                })
                .or_else(|| {
                    self.global_from_grouped_expr(expr).map(|global| {
                        self.global_property_refinement(
                            &global,
                            index.as_str(),
                            RefinementSense::Truthy,
                        )
                    })
                })
                .unwrap_or_default(),
            Expr::IndexExpr { expr, index, .. } => self
                .local_from_grouped_expr(expr)
                .zip(self.string_property_index(index))
                .map(|(local_id, property)| {
                    self.local_property_refinement(local_id, &property, RefinementSense::Truthy)
                })
                .or_else(|| {
                    self.global_from_grouped_expr(expr)
                        .zip(self.string_property_index(index))
                        .map(|(global, property)| {
                            self.global_property_refinement(
                                &global,
                                &property,
                                RefinementSense::Truthy,
                            )
                        })
                })
                .unwrap_or_default(),
            Expr::Call { .. } => self
                .isa_refinements(condition, TypeofRefinementSense::Is)
                .unwrap_or_default(),
            Expr::Group { expr, .. } => self.truthy_refinements(expr),
            Expr::Unary { op, expr, .. } if *op == UnaryOp::Not => self.falsy_refinements(expr),
            Expr::Binary {
                op, left, right, ..
            } => match op {
                BinaryOp::Or => self.or_truthy_refinements(left, right),
                BinaryOp::And => self.and_truthy_refinements(left, right),
                _ => self.binary_condition_refinements(*op, left, right, Truthiness::Truthy),
            },
            _ => RefinementMap::new(),
        }
    }
    pub(crate) fn falsy_refinements(&mut self, condition: &Expr) -> RefinementMap {
        match condition {
            Expr::Local { local, .. } => self.local_refinement(local.id, RefinementSense::Falsy),
            Expr::IndexName { expr, index, .. } => self
                .local_from_grouped_expr(expr)
                .map(|local_id| {
                    self.local_property_refinement(local_id, index.as_str(), RefinementSense::Falsy)
                })
                .or_else(|| {
                    self.global_from_grouped_expr(expr).map(|global| {
                        self.global_property_refinement(
                            &global,
                            index.as_str(),
                            RefinementSense::Falsy,
                        )
                    })
                })
                .unwrap_or_default(),
            Expr::IndexExpr { expr, index, .. } => self
                .local_from_grouped_expr(expr)
                .zip(self.string_property_index(index))
                .map(|(local_id, property)| {
                    self.local_property_refinement(local_id, &property, RefinementSense::Falsy)
                })
                .or_else(|| {
                    self.global_from_grouped_expr(expr)
                        .zip(self.string_property_index(index))
                        .map(|(global, property)| {
                            self.global_property_refinement(
                                &global,
                                &property,
                                RefinementSense::Falsy,
                            )
                        })
                })
                .unwrap_or_default(),
            Expr::Call { .. } => self
                .isa_refinements(condition, TypeofRefinementSense::IsNot)
                .unwrap_or_default(),
            Expr::Group { expr, .. } => self.falsy_refinements(expr),
            Expr::Unary { op, expr, .. } if *op == UnaryOp::Not => self.truthy_refinements(expr),
            Expr::Binary {
                op, left, right, ..
            } => match op {
                BinaryOp::Or => self.or_falsy_refinements(left, right),
                BinaryOp::And => self.and_falsy_refinements(left, right),
                _ => self.binary_condition_refinements(*op, left, right, Truthiness::Falsy),
            },
            _ => RefinementMap::new(),
        }
    }
    pub(crate) fn and_truthy_refinements(&mut self, left: &Expr, right: &Expr) -> RefinementMap {
        let left = self.truthy_refinements(left);
        let right = self.truthy_refinements(right);
        self.intersect_refinement_maps(left, right)
    }
    pub(crate) fn and_falsy_refinements(&mut self, left: &Expr, right: &Expr) -> RefinementMap {
        let left = self.falsy_refinements(left);
        let right = self.falsy_refinements(right);
        self.union_common_refinement_maps(left, &right)
    }
    pub(crate) fn or_truthy_refinements(&mut self, left: &Expr, right: &Expr) -> RefinementMap {
        let left = self.truthy_refinements(left);
        let right = self.truthy_refinements(right);
        self.union_common_refinement_maps(left, &right)
    }
    pub(crate) fn or_falsy_refinements(&mut self, left: &Expr, right: &Expr) -> RefinementMap {
        let left = self.falsy_refinements(left);
        let right = self.falsy_refinements(right);
        self.intersect_refinement_maps(left, right)
    }
    pub(crate) fn intersect_refinement_maps(
        &mut self,
        mut left: RefinementMap,
        right: RefinementMap,
    ) -> RefinementMap {
        for (key, right_ty) in right {
            if let Some(left_ty) = left.get(&key).copied() {
                let intersection = if self.intersection_contains_type(right_ty, left_ty) {
                    right_ty
                } else if self.intersection_contains_type(left_ty, right_ty) {
                    left_ty
                } else {
                    self.intersection_type(vec![left_ty, right_ty])
                };
                left.insert(key, intersection);
            } else {
                left.insert(key, right_ty);
            }
        }
        left
    }
    fn intersection_contains_type(&self, intersection: TypeId, candidate: TypeId) -> bool {
        let intersection = self.arena.follow(intersection);
        let candidate = self.arena.follow(candidate);
        match self.arena.get(intersection) {
            TypeKind::Intersection(types) => {
                types.iter().any(|ty| self.arena.follow(*ty) == candidate)
            }
            _ => false,
        }
    }
    pub(crate) fn union_common_refinement_maps(
        &mut self,
        left: RefinementMap,
        right: &RefinementMap,
    ) -> RefinementMap {
        left.into_iter()
            .filter_map(|(key, left_ty)| {
                let right_ty = right.get(&key).copied()?;
                Some((key, self.union_type(vec![left_ty, right_ty])))
            })
            .collect()
    }
    /// Runs the binary-comparison refinement ladder for one branch sense.
    fn binary_condition_refinements(
        &mut self,
        op: BinaryOp,
        left: &Expr,
        right: &Expr,
        condition_truthy: Truthiness,
    ) -> RefinementMap {
        self.binary_nil_refinements(op, left, right, condition_truthy)
            .or_else(|| self.binary_property_nil_refinements(op, left, right, condition_truthy))
            .or_else(|| self.binary_typeof_refinements(op, left, right, condition_truthy))
            .or_else(|| self.binary_property_typeof_refinements(op, left, right, condition_truthy))
            .or_else(|| self.binary_singleton_refinements(op, left, right, condition_truthy))
            .or_else(|| {
                self.binary_property_singleton_refinements(op, left, right, condition_truthy)
            })
            .unwrap_or_default()
    }

    pub(crate) fn binary_nil_refinements(
        &mut self,
        op: BinaryOp,
        left: &Expr,
        right: &Expr,
        condition_truthy: Truthiness,
    ) -> Option<RefinementMap> {
        let local_id = self.nil_comparison_local(left, right)?;
        let nonnil = match (op, condition_truthy) {
            (BinaryOp::CompareNe, Truthiness::Truthy)
            | (BinaryOp::CompareEq, Truthiness::Falsy) => true,
            (BinaryOp::CompareNe, Truthiness::Falsy)
            | (BinaryOp::CompareEq, Truthiness::Truthy) => false,
            _ => return None,
        };
        Some(self.local_nil_refinement(local_id, nonnil))
    }
    pub(crate) fn nil_comparison_local(&self, left: &Expr, right: &Expr) -> Option<LocalId> {
        match (left, right) {
            (Expr::Local { local, .. }, Expr::Nil { .. })
            | (Expr::Nil { .. }, Expr::Local { local, .. }) => Some(local.id),
            _ => None,
        }
    }
    pub(crate) fn binary_property_nil_refinements(
        &mut self,
        op: BinaryOp,
        left: &Expr,
        right: &Expr,
        condition_truthy: Truthiness,
    ) -> Option<RefinementMap> {
        let (local_id, property) = self.nil_comparison_property(left, right)?;
        let nonnil = match (op, condition_truthy) {
            (BinaryOp::CompareNe, Truthiness::Truthy)
            | (BinaryOp::CompareEq, Truthiness::Falsy) => true,
            (BinaryOp::CompareNe, Truthiness::Falsy)
            | (BinaryOp::CompareEq, Truthiness::Truthy) => false,
            _ => return None,
        };
        Some(self.local_property_nil_refinement(local_id, &property, nonnil))
    }
    pub(crate) fn nil_comparison_property(
        &self,
        left: &Expr,
        right: &Expr,
    ) -> Option<(LocalId, String)> {
        match (left, right) {
            (property, Expr::Nil { .. }) | (Expr::Nil { .. }, property) => {
                self.property_access(property)
            }
            _ => None,
        }
    }
    pub(crate) fn binary_singleton_refinements(
        &mut self,
        op: BinaryOp,
        left: &Expr,
        right: &Expr,
        condition_truthy: Truthiness,
    ) -> Option<RefinementMap> {
        let (local_id, target) = self.singleton_comparison(left, right)?;
        let sense = match (op, condition_truthy) {
            (BinaryOp::CompareEq, Truthiness::Truthy)
            | (BinaryOp::CompareNe, Truthiness::Falsy) => TypeofRefinementSense::Is,
            (BinaryOp::CompareEq, Truthiness::Falsy)
            | (BinaryOp::CompareNe, Truthiness::Truthy) => TypeofRefinementSense::IsNot,
            _ => return None,
        };
        Some(self.local_singleton_refinement(local_id, &target, sense))
    }
    pub(crate) fn singleton_comparison(
        &mut self,
        left: &Expr,
        right: &Expr,
    ) -> Option<(LocalId, SingletonType)> {
        self.local_from_grouped_expr(left)
            .zip(self.expr_singleton(right))
            .or_else(|| {
                self.local_from_grouped_expr(right)
                    .zip(self.expr_singleton(left))
            })
    }
    pub(crate) fn expr_singleton(&mut self, expr: &Expr) -> Option<SingletonType> {
        match expr {
            Expr::String { value, .. } => Some(SingletonType::String(value.clone())),
            Expr::Bool { value, .. } => Some(SingletonType::Boolean(*value)),
            Expr::Group { expr, .. } => self.expr_singleton(expr),
            _ => None,
        }
    }
    pub(crate) fn binary_property_singleton_refinements(
        &mut self,
        op: BinaryOp,
        left: &Expr,
        right: &Expr,
        condition_truthy: Truthiness,
    ) -> Option<RefinementMap> {
        let (local_id, property, target) = self.property_singleton_comparison(left, right)?;
        let sense = match (op, condition_truthy) {
            (BinaryOp::CompareEq, Truthiness::Truthy)
            | (BinaryOp::CompareNe, Truthiness::Falsy) => TypeofRefinementSense::Is,
            (BinaryOp::CompareEq, Truthiness::Falsy)
            | (BinaryOp::CompareNe, Truthiness::Truthy) => TypeofRefinementSense::IsNot,
            _ => return None,
        };
        Some(self.local_property_singleton_refinement(local_id, &property, &target, sense))
    }
    pub(crate) fn property_singleton_comparison(
        &mut self,
        left: &Expr,
        right: &Expr,
    ) -> Option<(LocalId, String, SingletonType)> {
        self.property_access(left)
            .zip(self.expr_singleton(right))
            .map(|((local_id, property), singleton)| (local_id, property, singleton))
            .or_else(|| {
                self.property_access(right)
                    .zip(self.expr_singleton(left))
                    .map(|((local_id, property), singleton)| (local_id, property, singleton))
            })
    }
    pub(crate) fn property_access(&self, expr: &Expr) -> Option<(LocalId, String)> {
        self.property_access_path(expr)
            .and_then(|(local_id, path)| {
                let [property] = path.as_slice() else {
                    return None;
                };
                Some((local_id, property.clone()))
            })
    }
    pub(crate) fn property_access_path(&self, expr: &Expr) -> Option<(LocalId, Vec<String>)> {
        match expr {
            Expr::IndexName { expr, index, .. } => {
                let (local_id, mut path) = self
                    .property_access_path(expr)
                    .or_else(|| Some((self.local_from_grouped_expr(expr)?, Vec::new())))?;
                path.push(index.as_str().to_owned());
                Some((local_id, path))
            }
            Expr::IndexExpr { expr, index, .. } => {
                let property = self.string_property_index(index)?;
                let (local_id, mut path) = self
                    .property_access_path(expr)
                    .or_else(|| Some((self.local_from_grouped_expr(expr)?, Vec::new())))?;
                path.push(property);
                Some((local_id, path))
            }
            Expr::Group { expr, .. } => self.property_access_path(expr),
            _ => None,
        }
    }
    fn isa_refinements(
        &mut self,
        condition: &Expr,
        sense: TypeofRefinementSense,
    ) -> Option<RefinementMap> {
        let (target, class_name) = self.isa_refinement_target(condition)?;
        let scope = match &target {
            IsaRefinementTarget::Local(local_id) | IsaRefinementTarget::Property(local_id, _) => {
                self.local_scope(*local_id)?
            }
        };
        let class_ty = self.extern_type_named(scope, &class_name)?;
        Some(match target {
            IsaRefinementTarget::Local(local_id) => {
                self.local_isa_refinement(local_id, &class_name, class_ty, sense)
            }
            IsaRefinementTarget::Property(local_id, property) => self
                .local_property_isa_refinement(local_id, &property, &class_name, class_ty, sense),
        })
    }
    fn isa_refinement_target(&self, expr: &Expr) -> Option<(IsaRefinementTarget, String)> {
        let Expr::Call {
            func,
            args,
            is_self: true,
            ..
        } = expr
        else {
            return None;
        };
        let Expr::IndexName {
            expr: receiver,
            index,
            op,
            ..
        } = func.as_ref()
        else {
            return None;
        };
        if *op != IndexOp::Colon || index.as_str() != "IsA" {
            return None;
        }
        let [arg] = args.as_slice() else {
            return None;
        };
        let class_name = self.string_property_index(arg)?;
        if let Some(local_id) = self.local_from_grouped_expr(receiver) {
            return Some((IsaRefinementTarget::Local(local_id), class_name));
        }
        self.property_access(receiver).map(|(local_id, property)| {
            (
                IsaRefinementTarget::Property(local_id, property),
                class_name,
            )
        })
    }
    fn local_scope(&self, local_id: LocalId) -> Option<ScopeId> {
        self.input
            .dfg
            .local(local_id)
            .map(|def_id| self.input.dfg.get(def_id).scope)
    }
    fn extern_type_named(&mut self, scope: ScopeId, name: &str) -> Option<TypeId> {
        let (binding_scope, binding) = self.input.scopes.lookup_type_with_scope(scope, name)?;
        let is_class = matches!(
            binding.kind,
            TypeBindingKind::Class | TypeBindingKind::DeclaredClass
        );
        if !is_class {
            // Extern types installed from a definition module (e.g. the
            // refinement fixture's `Instance`/`Part` hierarchy) are bound as a
            // pre-lowered `Type`, not a `DeclaredClass` with retained
            // properties. Narrow directly to the lowered extern type.
            let ty = binding.ty?;
            return matches!(
                self.arena.get(self.arena.follow(ty)),
                TypeKind::Extern { .. }
            )
            .then_some(ty);
        }
        let super_name = binding.class_super_name.clone();
        let props = binding.class_props.clone();
        let indexer = binding.class_indexer.clone();
        if self
            .alias_lowering
            .type_alias_stack
            .iter()
            .any(|alias| alias == name)
        {
            return Some(self.lower_class_binding(
                binding_scope,
                name,
                &super_name,
                Vec::new(),
                None,
            ));
        }
        let ty = self.with_type_alias_frame(name.to_owned(), |this| {
            this.lower_class_binding(binding_scope, name, &super_name, props, indexer)
        });
        Some(ty)
    }
    pub(crate) fn string_property_index(&self, expr: &Expr) -> Option<String> {
        match expr {
            Expr::String { value, .. } => Some(value.clone()),
            Expr::Group { expr, .. } => self.string_property_index(expr),
            _ => None,
        }
    }
    pub(crate) fn binary_typeof_refinements(
        &mut self,
        op: BinaryOp,
        left: &Expr,
        right: &Expr,
        condition_truthy: Truthiness,
    ) -> Option<RefinementMap> {
        let (refinement_target, target) = self.typeof_comparison(left, right)?;
        let sense = match (op, condition_truthy) {
            (BinaryOp::CompareEq, Truthiness::Truthy)
            | (BinaryOp::CompareNe, Truthiness::Falsy) => TypeofRefinementSense::Is,
            (BinaryOp::CompareEq, Truthiness::Falsy)
            | (BinaryOp::CompareNe, Truthiness::Truthy) => TypeofRefinementSense::IsNot,
            _ => return None,
        };
        Some(match refinement_target {
            TypeofRefinementTarget::Local(local_id) => {
                self.local_typeof_refinement(local_id, &target, sense)
            }
            TypeofRefinementTarget::Global(name) => {
                self.global_typeof_refinement(&name, &target, sense)
            }
        })
    }
    fn typeof_comparison(
        &self,
        left: &Expr,
        right: &Expr,
    ) -> Option<(TypeofRefinementTarget, TypeofTag)> {
        self.typeof_refinement_target(left)
            .zip(self.typeof_tag_literal(right))
            .or_else(|| {
                self.typeof_refinement_target(right)
                    .zip(self.typeof_tag_literal(left))
            })
    }
    pub(crate) fn binary_property_typeof_refinements(
        &mut self,
        op: BinaryOp,
        left: &Expr,
        right: &Expr,
        condition_truthy: Truthiness,
    ) -> Option<RefinementMap> {
        let (local_id, path, target) = self.typeof_property_path_comparison(left, right)?;
        let sense = match (op, condition_truthy) {
            (BinaryOp::CompareEq, Truthiness::Truthy)
            | (BinaryOp::CompareNe, Truthiness::Falsy) => TypeofRefinementSense::Is,
            (BinaryOp::CompareEq, Truthiness::Falsy)
            | (BinaryOp::CompareNe, Truthiness::Truthy) => TypeofRefinementSense::IsNot,
            _ => return None,
        };
        Some(if let [property] = path.as_slice() {
            self.local_property_typeof_refinement(local_id, property, target, sense)
        } else {
            self.local_property_path_typeof_refinement(local_id, &path, target, sense)
        })
    }
    pub(crate) fn typeof_property_path_comparison(
        &self,
        left: &Expr,
        right: &Expr,
    ) -> Option<(LocalId, Vec<String>, TypeofTag)> {
        self.typeof_property(left)
            .zip(self.typeof_tag_literal(right))
            .map(|((local_id, path), target)| (local_id, path, target))
            .or_else(|| {
                self.typeof_property(right)
                    .zip(self.typeof_tag_literal(left))
                    .map(|((local_id, path), target)| (local_id, path, target))
            })
    }
    pub(crate) fn typeof_property(&self, expr: &Expr) -> Option<(LocalId, Vec<String>)> {
        let Expr::Call { func, args, .. } = expr else {
            return None;
        };
        if !matches!(func.as_ref(), Expr::Global { name, .. } if matches!(name.as_str(), "typeof" | "type"))
        {
            return None;
        }
        let [arg] = args.as_slice() else {
            return None;
        };
        self.property_access_path(arg)
    }
    fn typeof_refinement_target(&self, expr: &Expr) -> Option<TypeofRefinementTarget> {
        let Expr::Call { func, args, .. } = expr else {
            return None;
        };
        if !matches!(func.as_ref(), Expr::Global { name, .. } if matches!(name.as_str(), "typeof" | "type"))
        {
            return None;
        }
        let [arg] = args.as_slice() else {
            return None;
        };
        self.local_from_grouped_expr(arg)
            .map(TypeofRefinementTarget::Local)
            .or_else(|| {
                self.global_from_grouped_expr(arg)
                    .map(TypeofRefinementTarget::Global)
            })
    }
    pub(crate) fn local_from_grouped_expr(&self, expr: &Expr) -> Option<LocalId> {
        match expr {
            Expr::Local { local, .. } => Some(local.id),
            Expr::Group { expr, .. } => self.local_from_grouped_expr(expr),
            _ => None,
        }
    }
    pub(crate) fn record_typeof_nil_snapshot(&mut self, expr: &Expr, ty: TypeId) -> bool {
        let Some(local_id) = self.local_from_grouped_expr(expr) else {
            return false;
        };
        if !self.nil_tracking.local_starts_as_nil(local_id) {
            return false;
        }
        if self.arena.follow(ty) != self.primitives().nil
            && !matches!(self.arena.get(self.arena.follow(ty)), TypeKind::Free(_))
        {
            return false;
        }
        self.nil_tracking.typeof_snapshot_locals.insert(local_id);
        true
    }
    pub(crate) fn global_from_grouped_expr(&self, expr: &Expr) -> Option<String> {
        match expr {
            Expr::Global { name, .. } => Some(name.as_str().to_owned()),
            Expr::Group { expr, .. } => self.global_from_grouped_expr(expr),
            _ => None,
        }
    }
    pub(crate) fn typeof_tag_literal(&self, expr: &Expr) -> Option<TypeofTag> {
        let Expr::String { value, .. } = expr else {
            return None;
        };
        match value.as_str() {
            "nil" => Some(TypeofTag::Primitive(PrimitiveType::Nil)),
            "boolean" => Some(TypeofTag::Primitive(PrimitiveType::Boolean)),
            "number" => Some(TypeofTag::Primitive(PrimitiveType::Number)),
            "string" => Some(TypeofTag::Primitive(PrimitiveType::String)),
            "thread" => Some(TypeofTag::Primitive(PrimitiveType::Thread)),
            "buffer" => Some(TypeofTag::Primitive(PrimitiveType::Buffer)),
            "vector" => Some(TypeofTag::Primitive(PrimitiveType::Vector)),
            "function" => Some(TypeofTag::Function),
            "table" => Some(TypeofTag::Table),
            "userdata" => Some(TypeofTag::Userdata),
            _ => Some(TypeofTag::Extern(value.clone())),
        }
    }
    pub(crate) fn expr_type_in_refinement_context(
        &mut self,
        scope: ScopeId,
        expr: &Expr,
    ) -> TypeId {
        let mut probes = BTreeSet::new();
        self.collect_refinement_property_probes(expr, &mut probes);
        let added = probes
            .into_iter()
            .filter(|probe| self.refinements.property_probes.insert(*probe))
            .collect::<Vec<_>>();
        let ty = self.expr_type(scope, expr);
        for probe in added {
            self.refinements.property_probes.remove(&probe);
        }
        ty
    }
    pub(crate) fn collect_refinement_property_probes(
        &self,
        expr: &Expr,
        probes: &mut BTreeSet<SyntaxId>,
    ) {
        match expr {
            Expr::Binary {
                op, left, right, ..
            } => match op {
                BinaryOp::And | BinaryOp::Or => {
                    self.collect_refinement_property_probes(left, probes);
                    self.collect_refinement_property_probes(right, probes);
                }
                BinaryOp::CompareEq | BinaryOp::CompareNe => {
                    if self.typeof_tag_literal(right).is_some() {
                        self.collect_typeof_property_exprs(left, probes);
                    }
                    if self.typeof_tag_literal(left).is_some() {
                        self.collect_typeof_property_exprs(right, probes);
                    }
                    if self.expr_is_singleton_or_nil_literal(right)
                        && let Some(property) = self.property_access_expr(left)
                    {
                        probes.insert(property.syntax_id());
                    }
                    if self.expr_is_singleton_or_nil_literal(left)
                        && let Some(property) = self.property_access_expr(right)
                    {
                        probes.insert(property.syntax_id());
                    }
                }
                _ => {}
            },
            Expr::Unary { op, expr, .. } if *op == UnaryOp::Not => {
                self.collect_refinement_property_probes(expr, probes);
            }
            Expr::Group { expr, .. } | Expr::TypeAssertion { expr, .. } => {
                self.collect_refinement_property_probes(expr, probes);
            }
            Expr::Call { func, args, .. } if callee_name(func.as_ref()) == Some("assert") => {
                if let Some(condition) = args.first() {
                    self.collect_refinement_property_probes(condition, probes);
                }
            }
            _ => {}
        }
    }
    pub(crate) fn collect_typeof_property_exprs(
        &self,
        expr: &Expr,
        probes: &mut BTreeSet<SyntaxId>,
    ) {
        let Expr::Call { func, args, .. } = expr else {
            return;
        };
        if !matches!(func.as_ref(), Expr::Global { name, .. } if matches!(name.as_str(), "typeof" | "type"))
        {
            return;
        }
        for arg in args {
            if let Some(property) = self.property_access_expr(arg) {
                probes.insert(property.syntax_id());
            } else {
                self.collect_typeof_property_exprs(arg, probes);
            }
        }
    }
    pub(crate) fn property_access_expr<'b>(&self, expr: &'b Expr) -> Option<&'b Expr> {
        match expr {
            Expr::IndexName { .. } | Expr::IndexExpr { .. } => {
                self.property_access_path(expr).map(|_| expr)
            }
            Expr::Group { expr, .. } => self.property_access_expr(expr),
            _ => None,
        }
    }
    pub(crate) fn expr_is_singleton_or_nil_literal(&self, expr: &Expr) -> bool {
        match expr {
            Expr::String { .. } | Expr::Bool { .. } | Expr::Nil { .. } => true,
            Expr::Group { expr, .. } => self.expr_is_singleton_or_nil_literal(expr),
            _ => false,
        }
    }
    pub(crate) fn build_local_refinement<F>(
        &mut self,
        local_id: LocalId,
        refine: F,
    ) -> RefinementMap
    where
        F: FnOnce(&mut Self, TypeId) -> TypeId,
    {
        let mut refinements = RefinementMap::new();
        let Some(def) = self.input.dfg.local(local_id) else {
            return refinements;
        };
        let local_ty = self
            .refined_local_type(local_id)
            .unwrap_or(self.input.dfg.get(def).ty);
        let refined = refine(self, local_ty);
        if refined != self.arena.follow(local_ty) {
            let key = RefinementKey::Symbol(Symbol::Local(local_id));
            refinements.insert(key, refined);
        }
        refinements
    }
    fn build_global_refinement<F>(&mut self, name: &str, refine: F) -> RefinementMap
    where
        F: FnOnce(&mut Self, TypeId) -> TypeId,
    {
        let key = RefinementKey::Symbol(Symbol::Global(name.to_owned()));
        let Some(global_ty) = self
            .refined_type(&key)
            .or_else(|| self.generated.global_defs.get(name).copied())
            .or_else(|| {
                self.input
                    .scopes
                    .lookup_global(self.input.scopes.root(), name)
                    .and_then(|binding| binding.ty)
            })
        else {
            return RefinementMap::new();
        };
        let refined = refine(self, global_ty);
        if refined == self.arena.follow(global_ty) {
            RefinementMap::new()
        } else {
            RefinementMap::from([(key, refined)])
        }
    }
    pub(crate) fn local_typeof_refinement(
        &mut self,
        local_id: LocalId,
        target: &TypeofTag,
        sense: TypeofRefinementSense,
    ) -> RefinementMap {
        if *target == TypeofTag::Primitive(PrimitiveType::Nil)
            && self
                .nil_tracking
                .guard_relaxes_to_nil_locals
                .contains(&local_id)
        {
            return self.build_local_refinement(local_id, |this, local_ty| match sense {
                TypeofRefinementSense::Is => this.primitives().nil,
                TypeofRefinementSense::IsNot => this.strip_nil(local_ty),
            });
        }
        if *target == TypeofTag::Primitive(PrimitiveType::Nil) {
            return self.build_local_refinement(local_id, |this, local_ty| match sense {
                TypeofRefinementSense::Is => this.nil_part(local_ty),
                TypeofRefinementSense::IsNot => this.nonnil_part(local_ty),
            });
        }
        self.build_local_refinement(local_id, |this, local_ty| match sense {
            TypeofRefinementSense::Is => this.only_typeof(local_ty, target),
            TypeofRefinementSense::IsNot => this.remove_typeof(local_ty, target),
        })
    }
    fn global_typeof_refinement(
        &mut self,
        name: &str,
        target: &TypeofTag,
        sense: TypeofRefinementSense,
    ) -> RefinementMap {
        self.build_global_refinement(name, |this, global_ty| {
            if *target == TypeofTag::Primitive(PrimitiveType::Nil) {
                return match sense {
                    TypeofRefinementSense::Is => this.nil_part(global_ty),
                    TypeofRefinementSense::IsNot => this.nonnil_part(global_ty),
                };
            }
            match sense {
                TypeofRefinementSense::Is => {
                    let narrowed = this.only_typeof(global_ty, target);
                    if narrowed == this.primitives().never && this.is_named_typeof_table(global_ty)
                    {
                        let target_ty = this.typeof_tag_type(target);
                        this.raw_intersection_type(vec![global_ty, target_ty])
                    } else {
                        narrowed
                    }
                }
                TypeofRefinementSense::IsNot => this.remove_typeof(global_ty, target),
            }
        })
    }
    pub(crate) fn local_singleton_refinement(
        &mut self,
        local_id: LocalId,
        target: &SingletonType,
        sense: TypeofRefinementSense,
    ) -> RefinementMap {
        self.build_local_refinement(local_id, |this, local_ty| match sense {
            TypeofRefinementSense::Is => this.only_singleton(local_ty, target),
            TypeofRefinementSense::IsNot => this.remove_singleton(local_ty, target),
        })
    }
    pub(crate) fn local_property_singleton_refinement(
        &mut self,
        local_id: LocalId,
        property: &str,
        target: &SingletonType,
        sense: TypeofRefinementSense,
    ) -> RefinementMap {
        self.build_local_refinement(local_id, |this, local_ty| match sense {
            TypeofRefinementSense::Is => this.only_property_singleton(local_ty, property, target),
            TypeofRefinementSense::IsNot => {
                this.remove_property_singleton(local_ty, property, target)
            }
        })
    }
    pub(crate) fn local_property_typeof_refinement(
        &mut self,
        local_id: LocalId,
        property: &str,
        target: TypeofTag,
        sense: TypeofRefinementSense,
    ) -> RefinementMap {
        self.build_local_refinement(local_id, |this, local_ty| {
            this.refine_property_typeof(local_ty, property, target, sense)
        })
    }
    pub(crate) fn local_property_path_typeof_refinement(
        &mut self,
        local_id: LocalId,
        path: &[String],
        target: TypeofTag,
        sense: TypeofRefinementSense,
    ) -> RefinementMap {
        self.build_local_refinement(local_id, |this, local_ty| {
            this.refine_property_path_typeof(local_ty, path, target, sense)
        })
    }
    fn local_isa_refinement(
        &mut self,
        local_id: LocalId,
        target_name: &str,
        target_ty: TypeId,
        sense: TypeofRefinementSense,
    ) -> RefinementMap {
        self.build_local_refinement(local_id, |this, local_ty| {
            this.refine_isa(local_ty, target_name, target_ty, sense)
        })
    }
    fn local_property_isa_refinement(
        &mut self,
        local_id: LocalId,
        property: &str,
        target_name: &str,
        target_ty: TypeId,
        sense: TypeofRefinementSense,
    ) -> RefinementMap {
        self.build_local_refinement(local_id, |this, local_ty| {
            this.refine_property_isa(local_ty, property, target_name, target_ty, sense)
        })
    }
    pub(crate) fn local_property_refinement(
        &mut self,
        local_id: LocalId,
        property: &str,
        sense: RefinementSense,
    ) -> RefinementMap {
        self.build_local_refinement(local_id, |this, local_ty| {
            this.refine_property_truthiness(local_ty, property, sense)
        })
    }
    pub(crate) fn local_nil_refinement(
        &mut self,
        local_id: LocalId,
        nonnil: bool,
    ) -> RefinementMap {
        self.build_local_refinement(local_id, |this, local_ty| {
            if nonnil {
                this.nonnil_part(local_ty)
            } else {
                this.nil_part(local_ty)
            }
        })
    }
    pub(crate) fn local_property_nil_refinement(
        &mut self,
        local_id: LocalId,
        property: &str,
        nonnil: bool,
    ) -> RefinementMap {
        self.build_local_refinement(local_id, |this, local_ty| {
            this.refine_property_nil(local_ty, property, nonnil)
        })
    }
    pub(crate) fn global_property_refinement(
        &mut self,
        name: &str,
        property: &str,
        sense: RefinementSense,
    ) -> RefinementMap {
        let key = RefinementKey::Symbol(Symbol::Global(name.to_owned()));
        let Some(global_ty) = self
            .refined_type(&key)
            .or_else(|| self.generated.global_defs.get(name).copied())
            .or_else(|| {
                self.input
                    .scopes
                    .lookup_global(self.input.scopes.root(), name)
                    .and_then(|binding| binding.ty)
            })
        else {
            return RefinementMap::new();
        };
        let refined = self.refine_property_truthiness(global_ty, property, sense);
        if refined == self.arena.follow(global_ty) {
            RefinementMap::new()
        } else {
            RefinementMap::from([(key, refined)])
        }
    }
    pub(crate) fn local_refinement(
        &mut self,
        local_id: LocalId,
        sense: RefinementSense,
    ) -> RefinementMap {
        self.build_local_refinement(local_id, |this, local_ty| match sense {
            RefinementSense::Truthy => this.truthy_part(local_ty),
            RefinementSense::Falsy => this.falsy_part(local_ty),
        })
    }
    pub(crate) fn refine_property_truthiness(
        &mut self,
        ty: TypeId,
        property: &str,
        sense: RefinementSense,
    ) -> TypeId {
        let impossible_base = |this: &Self, ty| {
            sense == RefinementSense::Truthy
                && matches!(
                    this.arena.get(this.arena.follow(ty)),
                    TypeKind::Primitive(PrimitiveType::Nil)
                        | TypeKind::Singleton(SingletonType::Boolean(false))
                )
        };
        let mut refine = |this: &mut Self, current_ty| match sense {
            RefinementSense::Truthy => this.truthy_part(current_ty),
            RefinementSense::Falsy => this.falsy_part(current_ty),
        };
        self.refine_property_read_type(ty, property, &mut refine, &impossible_base)
    }
    pub(crate) fn refine_property_nil(
        &mut self,
        ty: TypeId,
        property: &str,
        nonnil: bool,
    ) -> TypeId {
        let impossible_base = |this: &Self, ty| {
            nonnil
                && matches!(
                    this.arena.get(this.arena.follow(ty)),
                    TypeKind::Primitive(PrimitiveType::Nil)
                )
        };
        let mut refine = |this: &mut Self, current_ty| {
            if nonnil {
                this.nonnil_part(current_ty)
            } else {
                this.nil_part(current_ty)
            }
        };
        self.refine_property_read_type(ty, property, &mut refine, &impossible_base)
    }
    fn refine_property_read_type<F, B>(
        &mut self,
        ty: TypeId,
        property: &str,
        refine: &mut F,
        impossible_base: &B,
    ) -> TypeId
    where
        F: FnMut(&mut Self, TypeId) -> TypeId,
        B: Fn(&Self, TypeId) -> bool,
    {
        let ty = self.arena.follow(ty);
        if self.property_refinement_surface_too_large(ty) {
            if let Some(refined) =
                self.large_table_property_refinement(ty, property, refine, impossible_base)
            {
                return refined;
            }
            return ty;
        }
        if impossible_base(self, ty) {
            return self.primitives().never;
        }
        match self.arena.get(ty).clone() {
            TypeKind::Union(types) => {
                let refined = types
                    .into_iter()
                    .map(|ty| self.refine_property_read_type(ty, property, refine, impossible_base))
                    .collect::<Vec<_>>();
                self.union_type(refined)
            }
            TypeKind::Table(mut table) => {
                let current = if let Some(current) = table.properties.get(property).cloned() {
                    current
                } else if let Some(indexer) = table.indexer.as_ref()
                    && self.arena.is_string_like(indexer.key)
                {
                    TableProperty::new(indexer.value)
                } else {
                    return ty;
                };
                let refined_ty = refine(self, current.ty);
                if self.arena.follow(refined_ty) == self.primitives().never {
                    return self.primitives().never;
                }
                if refined_ty == self.arena.follow(current.ty) {
                    return ty;
                }
                let refined_property = self.property_with_refined_read_type(current, refined_ty);
                table
                    .properties
                    .insert(property.to_owned(), refined_property);
                self.arena.alloc(TypeKind::Table(table))
            }
            TypeKind::Extern {
                name,
                parents,
                mut properties,
                indexer,
            } => {
                let Some(current) = properties.get(property).cloned() else {
                    return ty;
                };
                let refined_ty = refine(self, current.ty);
                if self.arena.follow(refined_ty) == self.primitives().never {
                    return self.primitives().never;
                }
                if refined_ty == self.arena.follow(current.ty) {
                    return ty;
                }
                if !matches!(
                    self.arena.get(self.arena.follow(current.ty)),
                    TypeKind::Unknown
                ) {
                    let refined_property =
                        self.property_with_refined_read_type(current, refined_ty);
                    properties.insert(property.to_owned(), refined_property);
                    return self.arena.alloc(TypeKind::Extern {
                        name,
                        parents,
                        properties,
                        indexer,
                    });
                }
                let mut structural = TableType::new(TableState::Sealed);
                let mut refined_property = TableProperty::new(refined_ty);
                refined_property.read_only = true;
                structural
                    .properties
                    .insert(property.to_owned(), refined_property);
                let structural = self.arena.alloc(TypeKind::Table(structural));
                self.arena
                    .alloc(TypeKind::Intersection(vec![ty, structural]))
            }
            _ => ty,
        }
    }

    fn large_table_property_refinement<F, B>(
        &mut self,
        ty: TypeId,
        property: &str,
        refine: &mut F,
        impossible_base: &B,
    ) -> Option<TypeId>
    where
        F: FnMut(&mut Self, TypeId) -> TypeId,
        B: Fn(&Self, TypeId) -> bool,
    {
        let TypeKind::Table(table) = self.arena.get(ty).clone() else {
            return None;
        };
        if table.properties.len() <= MAX_PROPERTY_REFINEMENT_TABLE_PROPERTIES {
            return None;
        }
        if impossible_base(self, ty) {
            return Some(self.primitives().never);
        }
        let current = if let Some(current) = table.properties.get(property).cloned() {
            current
        } else if let Some(indexer) = table.indexer.as_ref()
            && self.arena.is_string_like(indexer.key)
        {
            TableProperty::new(indexer.value)
        } else {
            return Some(ty);
        };
        let refined_ty = refine(self, current.ty);
        if self.arena.follow(refined_ty) == self.primitives().never {
            return Some(self.primitives().never);
        }
        if refined_ty == self.arena.follow(current.ty) {
            return Some(ty);
        }

        let mut overlay = TableType::new(TableState::Sealed);
        let mut refined_property = TableProperty::new(refined_ty);
        refined_property.read_only = true;
        overlay
            .properties
            .insert(property.to_owned(), refined_property);
        let overlay = self.arena.alloc(TypeKind::Table(overlay));
        Some(self.raw_intersection_type(vec![ty, overlay]))
    }

    pub(crate) fn refine_property_typeof(
        &mut self,
        ty: TypeId,
        property: &str,
        target: TypeofTag,
        sense: TypeofRefinementSense,
    ) -> TypeId {
        self.refine_property_path_typeof(ty, &[property.to_owned()], target, sense)
    }
    pub(crate) fn refine_property_path_typeof(
        &mut self,
        ty: TypeId,
        path: &[String],
        target: TypeofTag,
        sense: TypeofRefinementSense,
    ) -> TypeId {
        let Some((property, rest)) = path.split_first() else {
            return match sense {
                TypeofRefinementSense::Is => self.only_typeof(ty, &target),
                TypeofRefinementSense::IsNot => self.remove_typeof(ty, &target),
            };
        };
        let ty = self.arena.follow(ty);
        if self.property_refinement_surface_too_large(ty) {
            return ty;
        }
        match self.arena.get(ty).clone() {
            TypeKind::Union(types) => {
                let refined = types
                    .into_iter()
                    .map(|ty| self.refine_property_path_typeof(ty, path, target.clone(), sense))
                    .collect::<Vec<_>>();
                self.union_type(refined)
            }
            TypeKind::Table(mut table) => {
                let current = if let Some(current) = table.properties.get(property).cloned() {
                    current
                } else if let Some(indexer) = table.indexer.as_ref()
                    && self.arena.is_string_like(indexer.key)
                {
                    TableProperty::new(indexer.value)
                } else if table.state == TableState::Free && sense == TypeofRefinementSense::Is {
                    TableProperty::new(self.property_path_refinement_seed(rest, &target))
                } else {
                    return match sense {
                        TypeofRefinementSense::Is => self.primitives().never,
                        TypeofRefinementSense::IsNot => ty,
                    };
                };
                let refined_ty = match sense {
                    TypeofRefinementSense::Is if rest.is_empty() => {
                        self.only_typeof(current.ty, &target)
                    }
                    TypeofRefinementSense::IsNot if rest.is_empty() => {
                        self.remove_typeof(current.ty, &target)
                    }
                    _ => self.refine_property_path_typeof(current.ty, rest, target, sense),
                };
                if self.arena.follow(refined_ty) == self.primitives().never {
                    return self.primitives().never;
                }
                if refined_ty == self.arena.follow(current.ty) {
                    return ty;
                }
                let refined_property = self.property_with_refined_read_type(current, refined_ty);
                table
                    .properties
                    .insert(property.to_owned(), refined_property);
                self.arena.alloc(TypeKind::Table(table))
            }
            TypeKind::Extern {
                name,
                parents,
                mut properties,
                indexer,
            } => {
                let current = if let Some(current) = properties.get(property).cloned() {
                    current
                } else if let Some(indexer) = indexer.as_ref()
                    && self.arena.is_string_like(indexer.key)
                {
                    TableProperty::new(indexer.value)
                } else {
                    return match sense {
                        TypeofRefinementSense::Is => self.primitives().never,
                        TypeofRefinementSense::IsNot => ty,
                    };
                };
                let refined_ty = match sense {
                    TypeofRefinementSense::Is if rest.is_empty() => {
                        self.only_typeof(current.ty, &target)
                    }
                    TypeofRefinementSense::IsNot if rest.is_empty() => {
                        self.remove_typeof(current.ty, &target)
                    }
                    _ => self.refine_property_path_typeof(current.ty, rest, target, sense),
                };
                if self.arena.follow(refined_ty) == self.primitives().never {
                    return self.primitives().never;
                }
                if refined_ty == self.arena.follow(current.ty) {
                    return ty;
                }
                let refined_property = self.property_with_refined_read_type(current, refined_ty);
                properties.insert(property.to_owned(), refined_property);
                self.arena.alloc(TypeKind::Extern {
                    name,
                    parents,
                    properties,
                    indexer,
                })
            }
            TypeKind::Unknown | TypeKind::Free(_) if sense == TypeofRefinementSense::Is => {
                let mut table = TableType::new(TableState::Free);
                table.properties.insert(
                    property.to_owned(),
                    TableProperty::new(self.property_path_refinement_seed(rest, &target)),
                );
                self.arena.alloc(TypeKind::Table(table))
            }
            _ => ty,
        }
    }
    fn property_path_refinement_seed(&mut self, rest: &[String], target: &TypeofTag) -> TypeId {
        let Some((property, nested)) = rest.split_first() else {
            return self.typeof_tag_type(target);
        };
        let value = self.property_path_refinement_seed(nested, target);
        let mut table = TableType::new(TableState::Free);
        table
            .properties
            .insert(property.clone(), TableProperty::new(value));
        self.arena.alloc(TypeKind::Table(table))
    }

    fn property_refinement_surface_too_large(&self, ty: TypeId) -> bool {
        match self.arena.get(self.arena.follow(ty)) {
            TypeKind::Table(table) => {
                table.properties.len() > MAX_PROPERTY_REFINEMENT_TABLE_PROPERTIES
            }
            TypeKind::Union(options) | TypeKind::Intersection(options) => {
                options.len() > MAX_GENERATED_INTERSECTION_OPTIONS
            }
            _ => false,
        }
    }

    fn refine_isa(
        &mut self,
        ty: TypeId,
        target_name: &str,
        target_ty: TypeId,
        sense: TypeofRefinementSense,
    ) -> TypeId {
        let ty = self.arena.follow(ty);
        match self.arena.get(ty).clone() {
            TypeKind::Union(types) => {
                let refined = types
                    .into_iter()
                    .map(|ty| self.refine_isa(ty, target_name, target_ty, sense))
                    .collect::<Vec<_>>();
                self.union_type(refined)
            }
            TypeKind::Extern { name, parents, .. } => match sense {
                TypeofRefinementSense::Is => {
                    if extern_is_subtype(&name, &parents, target_name) {
                        ty
                    } else if self.extern_type_is_subtype_of(target_ty, &name) {
                        target_ty
                    } else {
                        self.primitives().never
                    }
                }
                TypeofRefinementSense::IsNot => {
                    if extern_is_subtype(&name, &parents, target_name) {
                        self.primitives().never
                    } else if self.extern_type_is_subtype_of(target_ty, &name) {
                        let negated = self.arena.alloc(TypeKind::Negation(target_ty));
                        self.intersection_type(vec![ty, negated])
                    } else {
                        ty
                    }
                }
            },
            TypeKind::Any | TypeKind::Unknown | TypeKind::Blocked(_) => match sense {
                TypeofRefinementSense::Is => target_ty,
                TypeofRefinementSense::IsNot => ty,
            },
            _ => match sense {
                TypeofRefinementSense::Is => self.primitives().never,
                TypeofRefinementSense::IsNot => ty,
            },
        }
    }
    fn refine_property_isa(
        &mut self,
        ty: TypeId,
        property: &str,
        target_name: &str,
        target_ty: TypeId,
        sense: TypeofRefinementSense,
    ) -> TypeId {
        let ty = self.arena.follow(ty);
        match self.arena.get(ty).clone() {
            TypeKind::Union(types) => {
                let refined = types
                    .into_iter()
                    .map(|ty| self.refine_property_isa(ty, property, target_name, target_ty, sense))
                    .collect::<Vec<_>>();
                self.union_type(refined)
            }
            TypeKind::Table(mut table) => {
                let current = if let Some(current) = table.properties.get(property).cloned() {
                    current
                } else if let Some(indexer) = table.indexer.as_ref()
                    && self.arena.is_string_like(indexer.key)
                {
                    TableProperty::new(indexer.value)
                } else {
                    return match sense {
                        TypeofRefinementSense::Is => self.primitives().never,
                        TypeofRefinementSense::IsNot => ty,
                    };
                };
                let refined_ty = self.refine_isa(current.ty, target_name, target_ty, sense);
                if self.arena.follow(refined_ty) == self.primitives().never {
                    return self.primitives().never;
                }
                if refined_ty == self.arena.follow(current.ty) {
                    return ty;
                }
                let refined_property = self.property_with_refined_read_type(current, refined_ty);
                table
                    .properties
                    .insert(property.to_owned(), refined_property);
                self.arena.alloc(TypeKind::Table(table))
            }
            TypeKind::Extern {
                name,
                parents,
                mut properties,
                indexer,
            } => {
                let Some(current) = properties.get(property).cloned() else {
                    return ty;
                };
                let refined_ty = self.refine_isa(current.ty, target_name, target_ty, sense);
                if self.arena.follow(refined_ty) == self.primitives().never {
                    return self.primitives().never;
                }
                if refined_ty == self.arena.follow(current.ty) {
                    return ty;
                }
                let refined_property = self.property_with_refined_read_type(current, refined_ty);
                properties.insert(property.to_owned(), refined_property);
                self.arena.alloc(TypeKind::Extern {
                    name,
                    parents,
                    properties,
                    indexer,
                })
            }
            _ => ty,
        }
    }
    fn extern_type_is_subtype_of(&self, ty: TypeId, super_name: &str) -> bool {
        let TypeKind::Extern { name, parents, .. } = self.arena.get(self.arena.follow(ty)) else {
            return false;
        };
        extern_is_subtype(name, parents, super_name)
    }
    fn property_with_refined_read_type(
        &self,
        mut property: TableProperty,
        refined_ty: TypeId,
    ) -> TableProperty {
        let write_ty = property.write_type();
        if property.write_ty.is_none() && !property.read_only && !property.write_only {
            property.write_ty = Some(write_ty);
        }
        property.ty = refined_ty;
        property
    }
    pub(crate) fn remove_singleton(&mut self, ty: TypeId, target: &SingletonType) -> TypeId {
        let ty = self.arena.follow(ty);
        match self.arena.get(ty).clone() {
            TypeKind::Union(types) => {
                let remaining = types
                    .into_iter()
                    .map(|ty| self.remove_singleton(ty, target))
                    .collect::<Vec<_>>();
                self.union_type(remaining)
            }
            TypeKind::Singleton(singleton) if singleton == *target => self.primitives().never,
            TypeKind::Primitive(primitive) if primitive == target.primitive() => {
                self.primitive_without_singleton(primitive, target)
            }
            TypeKind::Any | TypeKind::Unknown => {
                let target = self.arena.alloc(TypeKind::Singleton(target.clone()));
                self.arena.alloc(TypeKind::Negation(target))
            }
            TypeKind::Free(_) | TypeKind::Generic(_) => {
                let target = self.arena.alloc(TypeKind::Singleton(target.clone()));
                let negated = self.arena.alloc(TypeKind::Negation(target));
                self.intersection_type(vec![ty, negated])
            }
            _ => ty,
        }
    }
    pub(crate) fn only_singleton(&mut self, ty: TypeId, target: &SingletonType) -> TypeId {
        let ty = self.arena.follow(ty);
        match self.arena.get(ty).clone() {
            TypeKind::Union(types) => {
                let matching = types
                    .into_iter()
                    .map(|ty| self.only_singleton(ty, target))
                    .collect::<Vec<_>>();
                self.union_type(matching)
            }
            TypeKind::Singleton(singleton) if singleton == *target => ty,
            TypeKind::Primitive(primitive) if primitive == target.primitive() => {
                self.arena.alloc(TypeKind::Singleton(target.clone()))
            }
            TypeKind::Any | TypeKind::Unknown => {
                self.arena.alloc(TypeKind::Singleton(target.clone()))
            }
            TypeKind::Free(_) | TypeKind::Generic(_) => {
                let target = self.arena.alloc(TypeKind::Singleton(target.clone()));
                self.intersection_type(vec![ty, target])
            }
            _ => self.primitives().never,
        }
    }
    pub(crate) fn primitive_without_singleton(
        &mut self,
        primitive: PrimitiveType,
        target: &SingletonType,
    ) -> TypeId {
        match target {
            SingletonType::Boolean(true) if primitive == PrimitiveType::Boolean => self
                .arena
                .alloc(TypeKind::Singleton(SingletonType::Boolean(false))),
            SingletonType::Boolean(false) if primitive == PrimitiveType::Boolean => self
                .arena
                .alloc(TypeKind::Singleton(SingletonType::Boolean(true))),
            SingletonType::String(_) if primitive == PrimitiveType::String => {
                let target = self.arena.alloc(TypeKind::Singleton(target.clone()));
                let negated = self.arena.alloc(TypeKind::Negation(target));
                self.arena.alloc(TypeKind::Intersection(vec![
                    self.primitives().string,
                    negated,
                ]))
            }
            _ => self.primitive_type_id(primitive),
        }
    }
    pub(crate) fn singleton_option_matches(&self, ty: TypeId, target: &SingletonType) -> bool {
        match (self.arena.get(self.arena.follow(ty)), target) {
            (
                TypeKind::Singleton(SingletonType::Boolean(value)),
                SingletonType::Boolean(target),
            ) => value == target,
            (TypeKind::Primitive(PrimitiveType::Boolean), SingletonType::Boolean(_)) => true,
            (TypeKind::Singleton(SingletonType::String(value)), SingletonType::String(target)) => {
                value == target
            }
            (TypeKind::Primitive(PrimitiveType::String), SingletonType::String(_)) => true,
            _ => false,
        }
    }
    pub(crate) fn remove_property_singleton(
        &mut self,
        ty: TypeId,
        property: &str,
        target: &SingletonType,
    ) -> TypeId {
        let impossible_base =
            |this: &Self, ty| this.table_property_is_exact_singleton(ty, property, target);
        let mut refine = |this: &mut Self, current_ty| this.remove_singleton(current_ty, target);
        self.refine_property_read_type(ty, property, &mut refine, &impossible_base)
    }
    pub(crate) fn only_property_singleton(
        &mut self,
        ty: TypeId,
        property: &str,
        target: &SingletonType,
    ) -> TypeId {
        let impossible_base =
            |this: &Self, ty| !this.table_property_may_be_singleton(ty, property, target);
        let mut refine = |this: &mut Self, current_ty| this.only_singleton(current_ty, target);
        self.refine_property_read_type(ty, property, &mut refine, &impossible_base)
    }
    pub(crate) fn table_property_may_be_singleton(
        &self,
        ty: TypeId,
        property: &str,
        target: &SingletonType,
    ) -> bool {
        let TypeKind::Table(table) = self.arena.get(self.arena.follow(ty)) else {
            return true;
        };
        let Some(property) = table.properties.get(property) else {
            return false;
        };
        self.type_may_be_singleton(property.ty, target)
    }
    pub(crate) fn table_property_is_exact_singleton(
        &self,
        ty: TypeId,
        property: &str,
        target: &SingletonType,
    ) -> bool {
        let TypeKind::Table(table) = self.arena.get(self.arena.follow(ty)) else {
            return false;
        };
        let Some(property) = table.properties.get(property) else {
            return false;
        };
        self.type_is_exact_singleton(property.ty, target)
    }
    pub(crate) fn type_may_be_singleton(&self, ty: TypeId, target: &SingletonType) -> bool {
        let ty = self.arena.follow(ty);
        match self.arena.get(ty) {
            TypeKind::Union(types) => types
                .iter()
                .any(|ty| self.type_may_be_singleton(*ty, target)),
            TypeKind::Any | TypeKind::Unknown | TypeKind::Error | TypeKind::Blocked(_) => true,
            _ => self.singleton_option_matches(ty, target),
        }
    }
    pub(crate) fn type_is_exact_singleton(&self, ty: TypeId, target: &SingletonType) -> bool {
        let ty = self.arena.follow(ty);
        match self.arena.get(ty) {
            TypeKind::Union(types) => {
                !types.is_empty()
                    && types
                        .iter()
                        .all(|ty| self.type_is_exact_singleton(*ty, target))
            }
            TypeKind::Singleton(singleton) => singleton == target,
            _ => false,
        }
    }
    pub(crate) fn remove_typeof(&mut self, ty: TypeId, target: &TypeofTag) -> TypeId {
        let ty = self.arena.follow(ty);
        match self.arena.get(ty).clone() {
            TypeKind::Union(types) => {
                let remaining = types
                    .into_iter()
                    .filter(|ty| !self.typeof_option_matches(*ty, target))
                    .collect::<Vec<_>>();
                self.union_type(remaining)
            }
            TypeKind::Any => {
                let target = self.typeof_tag_type(target);
                let negated = self.arena.alloc(TypeKind::Negation(target));
                self.union_type(vec![self.primitives().error, negated])
            }
            TypeKind::Unknown | TypeKind::Blocked(_) => {
                let target = self.typeof_tag_type(target);
                self.arena.alloc(TypeKind::Negation(target))
            }
            TypeKind::Free(_) | TypeKind::Generic(_) => {
                self.remove_typeof_from_indeterminate(ty, target)
            }
            _ if self.typeof_option_matches(ty, target) => self.primitives().never,
            _ => ty,
        }
    }
    pub(crate) fn only_typeof(&mut self, ty: TypeId, target: &TypeofTag) -> TypeId {
        let ty = self.arena.follow(ty);
        match self.arena.get(ty).clone() {
            TypeKind::Union(types) => {
                let mut matching = Vec::new();
                for ty in types {
                    let ty = self.arena.follow(ty);
                    let refined = match self.arena.get(ty).clone() {
                        TypeKind::Free(_) | TypeKind::Generic(_) => {
                            self.only_typeof_indeterminate(ty, target)
                        }
                        TypeKind::Negation(_) => self.only_typeof_indeterminate(ty, target),
                        _ if self.typeof_option_matches(ty, target) => self.widen_typeof_option(ty),
                        _ => continue,
                    };
                    matching.push(refined);
                }
                self.union_type(matching)
            }
            TypeKind::Any => self.error_suppressed_typeof_target(target),
            TypeKind::Unknown | TypeKind::Blocked(_) => self.typeof_tag_type(target),
            TypeKind::Free(_) | TypeKind::Generic(_) => self.only_typeof_indeterminate(ty, target),
            TypeKind::Negation(_) => self.only_typeof_indeterminate(ty, target),
            _ if self.typeof_option_matches(ty, target) => self.widen_typeof_option(ty),
            _ => self.primitives().never,
        }
    }
    fn remove_typeof_from_indeterminate(&mut self, ty: TypeId, target: &TypeofTag) -> TypeId {
        let target = self.typeof_tag_type(target);
        let negated = self.arena.alloc(TypeKind::Negation(target));
        self.intersection_type(vec![ty, negated])
    }
    fn only_typeof_indeterminate(&mut self, ty: TypeId, target: &TypeofTag) -> TypeId {
        let target = self.typeof_tag_type(target);
        self.intersection_type(vec![ty, target])
    }
    fn error_suppressed_typeof_target(&mut self, target: &TypeofTag) -> TypeId {
        let target = if *target == TypeofTag::Table {
            self.dynamic_table_type()
        } else {
            self.typeof_tag_type(target)
        };
        self.union_type(vec![self.primitives().error, target])
    }
    fn is_named_typeof_table(&self, ty: TypeId) -> bool {
        match self.arena.get(self.arena.follow(ty)) {
            TypeKind::Table(table) => table.synthetic_typeof_table,
            _ => false,
        }
    }
    pub(crate) fn typeof_option_matches(&self, ty: TypeId, target: &TypeofTag) -> bool {
        match self.arena.get(self.arena.follow(ty)) {
            TypeKind::Primitive(primitive) => *target == TypeofTag::Primitive(*primitive),
            TypeKind::Singleton(SingletonType::Boolean(_)) => {
                *target == TypeofTag::Primitive(PrimitiveType::Boolean)
            }
            TypeKind::Singleton(SingletonType::String(_)) => {
                *target == TypeofTag::Primitive(PrimitiveType::String)
            }
            TypeKind::Function(_) => *target == TypeofTag::Function,
            TypeKind::Table(_) | TypeKind::Metatable { .. } => *target == TypeofTag::Table,
            TypeKind::Intersection(types) => types
                .iter()
                .any(|ty| self.typeof_intersection_part_matches(*ty, target)),
            TypeKind::Extern { name, parents, .. } => match target {
                TypeofTag::Userdata => true,
                TypeofTag::Extern(target) => extern_is_subtype(name, parents, target),
                _ => false,
            },
            TypeKind::Generic(_) => true,
            _ => false,
        }
    }
    fn typeof_intersection_part_matches(&self, ty: TypeId, target: &TypeofTag) -> bool {
        match self.arena.get(self.arena.follow(ty)) {
            TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Function(_)
            | TypeKind::Table(_)
            | TypeKind::Metatable { .. }
            | TypeKind::Extern { .. } => self.typeof_option_matches(ty, target),
            TypeKind::Intersection(types) => types
                .iter()
                .any(|ty| self.typeof_intersection_part_matches(*ty, target)),
            _ => false,
        }
    }
    pub(crate) fn typeof_tag_type(&mut self, target: &TypeofTag) -> TypeId {
        match target {
            TypeofTag::Primitive(primitive) => self.primitive_type_id(*primitive),
            TypeofTag::Function => {
                let any = self.primitives().any;
                let arguments = self.arena.alloc_pack(TypePackKind::Variadic { ty: any });
                let returns = self.arena.alloc_pack(TypePackKind::Variadic { ty: any });
                self.arena
                    .alloc(TypeKind::Function(crate::types::FunctionType::new(
                        arguments, returns,
                    )))
            }
            TypeofTag::Table => {
                let mut table = TableType::new(TableState::Free);
                table.name = Some("table".to_owned());
                table.synthetic_typeof_table = true;
                self.arena.alloc(TypeKind::Table(table))
            }
            TypeofTag::Userdata => self.arena.alloc(TypeKind::Extern {
                name: "userdata".to_owned(),
                parents: Vec::new(),
                properties: BTreeMap::new(),
                indexer: None,
            }),
            TypeofTag::Extern(name) => self.arena.alloc(TypeKind::Extern {
                name: name.clone(),
                parents: Vec::new(),
                properties: BTreeMap::new(),
                indexer: None,
            }),
        }
    }
    pub(crate) fn dynamic_table_type(&mut self) -> TypeId {
        let primitives = self.primitives();
        let mut table = TableType::new(TableState::Free);
        table.name = Some("table".to_owned());
        table.synthetic_typeof_table = true;
        table.indexer = Some(TableIndexer {
            key: primitives.string,
            value: primitives.any,
            read_only: false,
        });
        self.arena.alloc(TypeKind::Table(table))
    }
    pub(crate) fn widen_typeof_option(&self, ty: TypeId) -> TypeId {
        match self.arena.get(self.arena.follow(ty)) {
            TypeKind::Singleton(SingletonType::Boolean(_)) => self.primitives().boolean,
            TypeKind::Singleton(SingletonType::String(_)) => self.primitives().string,
            _ => self.arena.follow(ty),
        }
    }
    pub(crate) fn primitive_type_id(&self, primitive: PrimitiveType) -> TypeId {
        let primitives = self.primitives();
        match primitive {
            PrimitiveType::Nil => primitives.nil,
            PrimitiveType::Boolean => primitives.boolean,
            PrimitiveType::Number => primitives.number,
            PrimitiveType::String => primitives.string,
            PrimitiveType::Thread => primitives.thread,
            PrimitiveType::Buffer => primitives.buffer,
            PrimitiveType::Vector => primitives.vector,
        }
    }
    pub(crate) fn expr_is_truthy_literal(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Bool { value: true, .. }
            | Expr::Number { .. }
            | Expr::Integer { .. }
            | Expr::String { .. }
            | Expr::Function { .. }
            | Expr::Table { .. } => true,
            Expr::Group { expr, .. } => self.expr_is_truthy_literal(expr),
            _ => false,
        }
    }
    pub(crate) fn expr_is_falsy_literal(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Nil { .. } | Expr::Bool { value: false, .. } => true,
            Expr::Group { expr, .. } => self.expr_is_falsy_literal(expr),
            _ => false,
        }
    }
    pub(crate) fn expr_exits(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Call { func, .. } if callee_name(func.as_ref()) == Some("error") => true,
            Expr::Call { func, args, .. } if callee_name(func.as_ref()) == Some("assert") => {
                matches!(args.first(), Some(Expr::Bool { value: false, .. }))
            }
            Expr::Group { expr, .. } => self.expr_exits(expr),
            _ => false,
        }
    }
    pub(crate) fn assertion_refinements(&mut self, expr: &Expr) -> Option<RefinementMap> {
        let Expr::Call { func, args, .. } = expr else {
            return None;
        };
        if !matches!(func.as_ref(), Expr::Global { name, .. } if name.as_str() == "assert") {
            return None;
        }
        args.first()
            .map(|condition| self.truthy_refinements(condition))
    }
}
