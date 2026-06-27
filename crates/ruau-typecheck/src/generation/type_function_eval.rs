//! Tree-walking interpreter for user-defined type functions and type aliases.
//!
//! [`TypeFunctionEvaluator`] evaluates the Luau body of a `type function` against
//! a set of type arguments, producing the reduced [`TypeId`]. It borrows the
//! owning [`ExpressionConstraintGenerator`] for arena allocation, scope lookups,
//! and type lowering, while owning its own evaluation scope and variable
//! environment.

use std::collections::{BTreeMap, BTreeSet};

use ruau_ast::{
    json::{JsonBinaryOp, JsonTableItemKind, JsonUnaryOp},
    syntax::{Expr, Stat, TableItem},
    visit::{NodePath, Visitor, WalkControl, walk_stat},
};

use crate::{
    diagnostic::{DiagnosticCategory, DiagnosticLocation, TypeDiagnostic},
    generalize::generalize_function_frees,
    generation::state::ExpressionConstraintGenerator,
    scopes::{ScopeId, TypeBindingKind},
    types::{
        FunctionType, GenericType, GenericTypePack, PrimitiveType, SingletonType, TableIndexer,
        TableProperty, TableState, TableType, TypeId, TypeKind, TypeLevel, TypePackId,
        TypePackKind,
    },
};

#[derive(Clone, Debug)]
pub enum TypeFunctionValue {
    Type(TypeId),
    Pack(TypePackId),
    FunctionBuilder(TypeFunctionBuilder),
    List(Vec<Self>),
    TableBuilder(TypeFunctionTableBuilder),
    Property {
        read: Option<TypeId>,
        write: Option<TypeId>,
    },
    String(String),
    Bool(bool),
}

enum TypeFunctionControl {
    Continue,
    Return(TypeId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TypeFunctionEvaluation {
    Reduced(TypeId),
    Uninhabited,
    RuntimeError,
    Deferred,
}

#[derive(Default)]
struct InvalidSingletonCallVisitor {
    found: bool,
}

impl<'ast> Visitor<'ast> for InvalidSingletonCallVisitor {
    fn visit_expr(&mut self, _path: &NodePath, expr: &'ast Expr) -> WalkControl {
        if let Expr::Call {
            func,
            args,
            is_self: false,
            ..
        } = expr
            && type_library_call_name(func) == Some("singleton")
            && let [arg] = args.as_slice()
            && singleton_argument_is_definitely_invalid(arg)
        {
            self.found = true;
            return WalkControl::SkipChildren;
        }
        WalkControl::Continue
    }
}

pub fn type_function_needs_eager_singleton_validation(func: &Expr) -> bool {
    let Expr::Function { args, body, .. } = func else {
        return false;
    };
    if !args.is_empty() {
        return false;
    }
    let mut visitor = InvalidSingletonCallVisitor::default();
    walk_stat(body, &mut visitor);
    visitor.found
}

#[derive(Clone, Debug, Default)]
pub struct TypeFunctionBuilder {
    arguments: Option<TypePackId>,
    returns: Option<TypePackId>,
    generics: Vec<GenericType>,
    generic_packs: Vec<GenericTypePack>,
}

enum TypeFunctionBuilderUpdate {
    Generics {
        types: Vec<GenericType>,
        packs: Vec<GenericTypePack>,
    },
    Parameters(TypePackId),
    Returns(TypePackId),
}

enum TypeFunctionTableBuilderUpdate {
    Indexer(TypeId, TypeId),
    Metatable(TypeId),
    Property(String, TypeFunctionPropertyUpdate),
}

enum TypeFunctionPropertyUpdate {
    Read(TypeId),
    ReadWrite(Option<TypeId>),
    Write(TypeId),
}

enum TypeFunctionNewFunction {
    Type(TypeId),
    Builder(TypeFunctionBuilder),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FunctionPackSlot {
    Parameters,
    Returns,
}

#[derive(Clone, Debug, Default)]
pub struct TypeFunctionTableBuilder {
    fields: BTreeMap<String, TypeFunctionValue>,
    list: Vec<TypeFunctionValue>,
    properties: BTreeMap<String, TableProperty>,
    indexer: Option<(TypeId, TypeId)>,
    metatable: Option<TypeId>,
}

/// Evaluates the body of a user-defined type function against bound arguments.
/// Field/method names that a type userdata exposes in the type-function
/// runtime (upstream `typeUserdataMethods` in `TypeFunctionRuntime.cpp`, plus
/// the dynamic `tag` field). Accessing any other name on a type value is a
/// runtime "attempt to call/index a nil value" error upstream; ruau may not
/// implement a given valid method yet (so reduction still defers on those),
/// but an *invalid* name is an unambiguous runtime error we can report.
const VALID_TYPE_FIELDS: &[&str] = &[
    "tag",
    "is",
    "inner",
    "value",
    "setproperty",
    "setreadproperty",
    "setwriteproperty",
    "readproperty",
    "writeproperty",
    "properties",
    "setindexer",
    "setreadindexer",
    "setwriteindexer",
    "indexer",
    "readindexer",
    "writeindexer",
    "setmetatable",
    "metatable",
    "setparameters",
    "parameters",
    "setreturns",
    "returns",
    "setgenerics",
    "generics",
    "components",
    "readparent",
    "writeparent",
    "name",
    "ispack",
];

fn is_valid_type_field(name: &str) -> bool {
    VALID_TYPE_FIELDS.contains(&name)
}

/// Type userdata methods that mutate the receiver. Calling one on a sealed type
/// (e.g. a module-scope type alias) is a runtime error upstream.
fn is_type_mutator(name: &str) -> bool {
    matches!(
        name,
        "setproperty"
            | "setreadproperty"
            | "setwriteproperty"
            | "setindexer"
            | "setreadindexer"
            | "setwriteindexer"
            | "setmetatable"
            | "setparameters"
            | "setreturns"
            | "setgenerics"
    )
}

pub struct TypeFunctionEvaluator<'g, 'a> {
    generator: &'g mut ExpressionConstraintGenerator<'a>,
    scope: ScopeId,
    env: BTreeMap<String, TypeFunctionValue>,
    /// Set once a runtime-error diagnostic has been emitted, so the fallback
    /// reporting in [`Self::run`] does not double-report.
    emitted_runtime_error: bool,
    emitted_uninhabited_runtime_error: bool,
    /// Set when the body performed an operation that is an unambiguous runtime
    /// error upstream: indexing/calling a name outside the type userdata API on
    /// a concrete type value, or calling a type alias with too few arguments.
    hit_invalid_type_access: bool,
    /// Set when the body read or called a global that is not bound in the
    /// type-function runtime environment. The body would error at runtime
    /// upstream (reading a `nil` global value), so a runtime-error diagnostic is
    /// emitted at the use site when the reduction otherwise yields no type.
    hit_unknown_global: bool,
    /// Whether the bound arguments are fully concrete. Recorded so side-effecting
    /// runtime behavior (e.g. `print` emitting its argument as an error) only
    /// fires when the function would actually run, not while deferred.
    arguments_concrete: bool,
    /// Location of the use site driving this reduction, attached to runtime-error
    /// diagnostics so the same error raised by reductions at different sites is
    /// not collapsed by diagnostic de-duplication.
    use_location: Option<DiagnosticLocation>,
}

impl<'g, 'a> TypeFunctionEvaluator<'g, 'a> {
    pub(crate) fn new(
        generator: &'g mut ExpressionConstraintGenerator<'a>,
        scope: ScopeId,
        env: BTreeMap<String, TypeFunctionValue>,
        use_location: Option<DiagnosticLocation>,
    ) -> Self {
        Self {
            generator,
            scope,
            env,
            emitted_runtime_error: false,
            emitted_uninhabited_runtime_error: false,
            hit_invalid_type_access: false,
            hit_unknown_global: false,
            arguments_concrete: false,
            use_location,
        }
    }

    /// Run the function body, returning the type yielded by its `return`.
    ///
    /// When the body fails to yield a type because it indexed/called an
    /// invalid name on a concrete type value, the function would have errored
    /// at runtime upstream (a real Luau VM runs the body), so a
    /// `type-function-runtime-error` diagnostic is emitted. With non-concrete
    /// arguments the reduction is merely deferred and nothing is reported.
    pub(crate) fn run(mut self, body: &Stat, arguments_concrete: bool) -> TypeFunctionEvaluation {
        self.arguments_concrete = arguments_concrete;
        let outcome = self.exec_stat(body);
        let ran_to_completion = matches!(outcome, Some(TypeFunctionControl::Continue));
        let result = match outcome {
            Some(TypeFunctionControl::Return(ty)) => Some(ty),
            Some(TypeFunctionControl::Continue) | None => None,
        };
        if let Some(result) = result {
            return TypeFunctionEvaluation::Reduced(result);
        }
        if !arguments_concrete {
            return TypeFunctionEvaluation::Deferred;
        }
        if ran_to_completion {
            // The body executed fully without returning a type — a runtime
            // error upstream ("type function ... did not return a type").
            // Independent of `emitted_runtime_error` so it stacks with any
            // `print` diagnostics raised along the way.
            self.report_type_function_runtime_error("no-result");
            return self.runtime_error_outcome();
        }
        if self.hit_invalid_type_access && !self.emitted_runtime_error {
            self.report_type_function_runtime_error("invalid-type-access");
            return self.runtime_error_outcome();
        }
        if self.hit_unknown_global && !self.emitted_runtime_error {
            // The body dereferenced an unbound global, which is `nil` at
            // runtime; reading or calling it errors upstream.
            self.report_type_function_runtime_error("unknown-global");
            return self.runtime_error_outcome();
        }
        if self.emitted_runtime_error {
            self.runtime_error_outcome()
        } else {
            TypeFunctionEvaluation::Deferred
        }
    }

    fn exec_stat(&mut self, stat: &Stat) -> Option<TypeFunctionControl> {
        self.consume_step()?;
        match stat {
            Stat::Block { body, .. } => {
                for stat in body {
                    match self.exec_stat(stat)? {
                        TypeFunctionControl::Continue => {}
                        TypeFunctionControl::Return(ty) => {
                            return Some(TypeFunctionControl::Return(ty));
                        }
                    }
                }
                Some(TypeFunctionControl::Continue)
            }
            Stat::Return { list, .. } => {
                let [expr] = list.as_slice() else {
                    return None;
                };
                self.eval_type(expr).map(TypeFunctionControl::Return)
            }
            Stat::Local { vars, values, .. } => {
                for (index, var) in vars.iter().enumerate() {
                    let value = values.get(index).or_else(|| values.first())?;
                    let value = self.eval_value(value)?;
                    self.env.insert(var.name.as_str().to_owned(), value);
                }
                Some(TypeFunctionControl::Continue)
            }
            Stat::Assign { vars, values, .. } => {
                for (index, var) in vars.iter().enumerate() {
                    let value = values.get(index).or_else(|| values.first())?;
                    self.assign(var, value)?;
                }
                Some(TypeFunctionControl::Continue)
            }
            Stat::Expr { expr, .. } => {
                self.exec_expr(expr)?;
                Some(TypeFunctionControl::Continue)
            }
            Stat::ForIn {
                vars, values, body, ..
            } => {
                let [iterable] = values.as_slice() else {
                    return None;
                };
                let [key_var, value_var] = vars.as_slice() else {
                    return None;
                };
                // `for k, v in tbl:properties()` enumerates a type's properties;
                // `for _, ty in tys` iterates the array part of a table value
                // (the index key is a positional placeholder — the common idiom
                // ignores it).
                let entries = if let Some(entries) = self.properties_call(iterable) {
                    entries
                } else if let TypeFunctionValue::TableBuilder(builder) =
                    self.eval_value(iterable)?
                {
                    builder
                        .list
                        .iter()
                        .enumerate()
                        .map(|(index, value)| {
                            (
                                TypeFunctionValue::String((index + 1).to_string()),
                                value.clone(),
                            )
                        })
                        .collect()
                } else {
                    return None;
                };
                let old_key = self.env.get(key_var.name.as_str()).cloned();
                let old_value = self.env.get(value_var.name.as_str()).cloned();
                for (key, property) in entries {
                    self.env.insert(key_var.name.as_str().to_owned(), key);
                    self.env
                        .insert(value_var.name.as_str().to_owned(), property);
                    match self.exec_stat(body)? {
                        TypeFunctionControl::Continue => {}
                        TypeFunctionControl::Return(ty) => {
                            restore_binding(&mut self.env, key_var.name.as_str(), old_key);
                            restore_binding(&mut self.env, value_var.name.as_str(), old_value);
                            return Some(TypeFunctionControl::Return(ty));
                        }
                    }
                }
                restore_binding(&mut self.env, key_var.name.as_str(), old_key);
                restore_binding(&mut self.env, value_var.name.as_str(), old_value);
                Some(TypeFunctionControl::Continue)
            }
            Stat::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                if self.eval_bool(condition)? {
                    self.exec_stat(then_body)
                } else if let Some(else_body) = else_body {
                    self.exec_stat(else_body)
                } else {
                    Some(TypeFunctionControl::Continue)
                }
            }
            _ => None,
        }
    }

    fn assign(&mut self, var: &Expr, value: &Expr) -> Option<()> {
        // Plain local reassignment (`result = types.intersectionof(result, ty)`)
        // rebinds the variable in the environment.
        if let Expr::Local { local, .. } = var {
            let value = self.eval_value(value)?;
            self.env.insert(local.name.as_str().to_owned(), value);
            return Some(());
        }
        let Expr::IndexExpr { expr, index, .. } = var else {
            return None;
        };
        let Expr::Local { local, .. } = expr.as_ref() else {
            return None;
        };
        let key = self.eval_value(index)?;
        let value = self.eval_type(value)?;
        match key {
            TypeFunctionValue::String(key) => {
                let property = self.table_property(value);
                let Some(TypeFunctionValue::TableBuilder(properties)) =
                    self.env.get_mut(local.name.as_str())
                else {
                    return None;
                };
                properties
                    .fields
                    .insert(key.clone(), TypeFunctionValue::Type(value));
                properties.properties.insert(key, property);
            }
            TypeFunctionValue::Type(key) => {
                let old = match self.env.get(local.name.as_str()) {
                    Some(TypeFunctionValue::TableBuilder(properties)) => properties.indexer,
                    _ => return None,
                };
                let indexer = match old {
                    Some((old_key, old_value)) => (
                        self.generator.union_type(vec![old_key, key]),
                        self.generator.union_type(vec![old_value, value]),
                    ),
                    None => (key, value),
                };
                let Some(TypeFunctionValue::TableBuilder(properties)) =
                    self.env.get_mut(local.name.as_str())
                else {
                    return None;
                };
                properties.indexer = Some(indexer);
            }
            TypeFunctionValue::Bool(_)
            | TypeFunctionValue::Pack(_)
            | TypeFunctionValue::FunctionBuilder(_)
            | TypeFunctionValue::List(_)
            | TypeFunctionValue::TableBuilder(_)
            | TypeFunctionValue::Property { .. } => return None,
        }
        Some(())
    }

    fn exec_expr(&mut self, expr: &Expr) -> Option<()> {
        if let Expr::Call { func, .. } = expr
            && let Expr::Global { name, .. } = func.as_ref()
            && name.as_str() != "print"
            && self.note_global_reference(name.as_str())
        {
            // Calling an unbound global (`nil` at runtime) errors upstream.
            return None;
        }
        if let Expr::Call { func, location, .. } = expr
            && let Expr::Global { name, .. } = func.as_ref()
            && name.as_str() == "print"
        {
            // `print` in a type function writes its argument to the error
            // stream upstream; mirror that as one diagnostic per call, then
            // continue evaluating. Only fires for a function that actually
            // runs (concrete arguments), and carries the call location so
            // repeated prints survive diagnostic de-duplication.
            if self.arguments_concrete {
                self.report_type_function_runtime_error_at(
                    "print",
                    location.map(DiagnosticLocation::from),
                );
            }
            return Some(());
        }
        if let Expr::Call {
            func,
            args,
            is_self: false,
            ..
        } = expr
            && (assert_call_arg(func, args).is_some()
                || type_library_call_name(func).is_some()
                || type_function_plain_call_name(func).is_some())
        {
            self.eval_value(expr)?;
            return Some(());
        }
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
            ..
        } = func.as_ref()
        else {
            return None;
        };
        // A mutating method (setproperty/setindexer/setmetatable/...) on a
        // module-scope type reference targets a sealed type alias, which is a
        // runtime error upstream. In-function `local` builders are mutable and
        // are handled below.
        if matches!(receiver.as_ref(), Expr::Global { .. }) && is_type_mutator(index.as_str()) {
            self.hit_invalid_type_access = true;
            return None;
        }
        let Expr::Local { local, .. } = receiver.as_ref() else {
            return None;
        };
        let function_update = match index.as_str() {
            "setgenerics" => {
                let (types, packs) = self.eval_generics_arg(args)?;
                Some(TypeFunctionBuilderUpdate::Generics { types, packs })
            }
            "setparameters" => Some(TypeFunctionBuilderUpdate::Parameters(
                self.eval_pack_args(args)?,
            )),
            "setreturns" => Some(TypeFunctionBuilderUpdate::Returns(
                self.eval_pack_args(args)?,
            )),
            _ => None,
        };
        if let Some(update) = function_update {
            let Some(TypeFunctionValue::FunctionBuilder(builder)) =
                self.env.get_mut(local.name.as_str())
            else {
                return None;
            };
            match update {
                TypeFunctionBuilderUpdate::Generics { types, packs } => {
                    builder.generics = types;
                    builder.generic_packs = packs;
                }
                TypeFunctionBuilderUpdate::Parameters(parameters) => {
                    builder.arguments = Some(parameters)
                }
                TypeFunctionBuilderUpdate::Returns(returns) => builder.returns = Some(returns),
            }
            return Some(());
        }

        let update = match index.as_str() {
            "setindexer" => {
                let [key, value] = args.as_slice() else {
                    return None;
                };
                TypeFunctionTableBuilderUpdate::Indexer(
                    self.eval_type(key)?,
                    self.eval_type(value)?,
                )
            }
            "setmetatable" => {
                let [metatable] = args.as_slice() else {
                    return None;
                };
                let metatable = self.eval_type(metatable)?;
                if !self.type_is_table_like(metatable) {
                    self.report_type_function_runtime_error("invalid-metatable");
                    return None;
                }
                TypeFunctionTableBuilderUpdate::Metatable(metatable)
            }
            "setproperty" => {
                let [key, value] = args.as_slice() else {
                    return None;
                };
                TypeFunctionTableBuilderUpdate::Property(
                    self.property_key(key)?,
                    TypeFunctionPropertyUpdate::ReadWrite(if expr_is_nil(value) {
                        None
                    } else {
                        Some(self.eval_type(value)?)
                    }),
                )
            }
            "setreadproperty" => {
                let [key, value] = args.as_slice() else {
                    return None;
                };
                TypeFunctionTableBuilderUpdate::Property(
                    self.property_key(key)?,
                    TypeFunctionPropertyUpdate::Read(self.eval_type(value)?),
                )
            }
            "setwriteproperty" => {
                let [key, value] = args.as_slice() else {
                    return None;
                };
                TypeFunctionTableBuilderUpdate::Property(
                    self.property_key(key)?,
                    TypeFunctionPropertyUpdate::Write(self.eval_type(value)?),
                )
            }
            _ => return None,
        };
        let Some(TypeFunctionValue::TableBuilder(builder)) = self.env.get_mut(local.name.as_str())
        else {
            return None;
        };
        match update {
            TypeFunctionTableBuilderUpdate::Indexer(key, value) => {
                builder.indexer = Some((key, value));
            }
            TypeFunctionTableBuilderUpdate::Metatable(metatable) => {
                builder.metatable = Some(metatable);
            }
            TypeFunctionTableBuilderUpdate::Property(key, update) => {
                apply_property_update(&mut builder.properties, key, &update);
            }
        }
        Some(())
    }

    fn value_is_falsey(&self, value: &TypeFunctionValue) -> bool {
        match value {
            TypeFunctionValue::Bool(false) => true,
            TypeFunctionValue::Type(ty) => {
                self.generator.arena.follow(*ty) == self.generator.primitives().nil
            }
            _ => false,
        }
    }

    fn eval_type(&mut self, expr: &Expr) -> Option<TypeId> {
        let value = self.eval_value(expr)?;
        self.value_into_type(value)
    }

    /// Builds `types.intersectionof(...)`'s result: flatten nested
    /// intersections, drop `unknown` operands (`unknown & T = T`), and
    /// de-duplicate — but keep an intersection of disjoint members (`number &
    /// string`) formal rather than collapsing it to `never`, which is what
    /// upstream's runtime does. The comparator sorts `&` operands, so the member
    /// order here is not significant.
    fn intersection_of_values(&mut self, options: Vec<TypeId>) -> TypeId {
        let unknown = self.generator.primitives().unknown;
        let mut members: Vec<TypeId> = Vec::new();
        let push = |member: TypeId, members: &mut Vec<TypeId>| {
            if member != unknown && !members.contains(&member) {
                members.push(member);
            }
        };
        for ty in options {
            let ty = self.generator.arena.follow(ty);
            match self.generator.arena.get(ty).clone() {
                TypeKind::Intersection(inner) => {
                    for member in inner {
                        push(self.generator.arena.follow(member), &mut members);
                    }
                }
                _ => push(ty, &mut members),
            }
        }
        match members.as_slice() {
            [] => unknown,
            [only] => *only,
            _ => self.generator.arena.alloc(TypeKind::Intersection(members)),
        }
    }

    fn value_into_type(&mut self, value: TypeFunctionValue) -> Option<TypeId> {
        match value {
            TypeFunctionValue::Type(ty) => Some(ty),
            TypeFunctionValue::FunctionBuilder(builder) => {
                let arguments = builder.arguments.unwrap_or_else(|| {
                    self.generator.arena.alloc_pack(TypePackKind::List {
                        types: Vec::new(),
                        tail: None,
                    })
                });
                let returns = builder.returns.unwrap_or_else(|| {
                    self.generator.arena.alloc_pack(TypePackKind::List {
                        types: Vec::new(),
                        tail: None,
                    })
                });
                let mut function = FunctionType::new(arguments, returns);
                function.generics = builder.generics;
                function.generic_packs = builder.generic_packs;
                Some(self.generator.arena.alloc(TypeKind::Function(function)))
            }
            TypeFunctionValue::TableBuilder(builder) => {
                let mut table = TableType::new(TableState::Sealed);
                table.properties = builder.properties;
                if let Some((key, value)) = builder.indexer {
                    table.indexer = Some(TableIndexer {
                        key,
                        value,
                        read_only: false,
                    });
                }
                let table = self.generator.arena.alloc(TypeKind::Table(table));
                Some(if let Some(metatable) = builder.metatable {
                    self.generator.arena.alloc(TypeKind::Metatable {
                        table,
                        metatable,
                        name: None,
                    })
                } else {
                    table
                })
            }
            TypeFunctionValue::String(value) => Some(
                self.generator
                    .arena
                    .alloc(TypeKind::Singleton(SingletonType::String(value))),
            ),
            TypeFunctionValue::Bool(value) => Some(
                self.generator
                    .arena
                    .alloc(TypeKind::Singleton(SingletonType::Boolean(value))),
            ),
            TypeFunctionValue::List(_) | TypeFunctionValue::Pack(_) => None,
            TypeFunctionValue::Property { read, write } => read.or(write),
        }
    }

    fn eval_value(&mut self, expr: &Expr) -> Option<TypeFunctionValue> {
        self.consume_step()?;
        match expr {
            Expr::Local { local, .. } => self.env.get(local.name.as_str()).cloned(),
            Expr::Group { expr, .. } | Expr::TypeAssertion { expr, .. } => self.eval_value(expr),
            Expr::Nil { .. } => Some(TypeFunctionValue::Type(self.generator.primitives().nil)),
            Expr::Bool { value, .. } => Some(TypeFunctionValue::Bool(*value)),
            Expr::String { value, .. } => Some(TypeFunctionValue::String(value.clone())),
            Expr::Global { name, .. } => match self.global_type(name.as_str()) {
                Some(ty) => Some(TypeFunctionValue::Type(ty)),
                None => {
                    self.note_global_reference(name.as_str());
                    None
                }
            },
            Expr::Binary {
                op: JsonBinaryOp::Concat,
                left,
                right,
                ..
            } => {
                // Evaluate both operands (so a side-effecting call such as a
                // sibling type function still runs and can raise its runtime
                // error), then concatenate when both yield string values.
                let left = self.eval_value(left);
                let right = self.eval_value(right);
                match (left, right) {
                    (
                        Some(TypeFunctionValue::String(left)),
                        Some(TypeFunctionValue::String(right)),
                    ) => Some(TypeFunctionValue::String(format!("{left}{right}"))),
                    _ => None,
                }
            }
            Expr::Binary { .. } => self.eval_bool(expr).map(TypeFunctionValue::Bool),
            Expr::Table { items, .. } => Some(TypeFunctionValue::TableBuilder(
                self.eval_table_builder(items)?,
            )),
            Expr::IndexName { expr, index, .. } if expr_is_types_global(expr) => self
                .types_global(index.as_str())
                .map(TypeFunctionValue::Type),
            Expr::IndexName { expr, index, .. } => match self.eval_value(expr)? {
                TypeFunctionValue::TableBuilder(builder) => {
                    builder.fields.get(index.as_str()).cloned().or_else(|| {
                        builder.properties.get(index.as_str()).map(|property| {
                            TypeFunctionValue::Property {
                                read: (!property.write_only).then_some(property.ty),
                                write: (!property.read_only).then_some(property.ty),
                            }
                        })
                    })
                }
                TypeFunctionValue::Property {
                    read: Some(read), ..
                } if index.as_str() == "read" => Some(TypeFunctionValue::Type(read)),
                TypeFunctionValue::Property {
                    write: Some(write), ..
                } if index.as_str() == "write" => Some(TypeFunctionValue::Type(write)),
                TypeFunctionValue::Type(ty) if index.as_str() == "tag" => {
                    self.type_tag(ty).map(TypeFunctionValue::String)
                }
                TypeFunctionValue::Type(ty) if index.as_str() == "value" => {
                    self.singleton_value(ty)
                }
                TypeFunctionValue::Type(_) if !is_valid_type_field(index.as_str()) => {
                    self.hit_invalid_type_access = true;
                    None
                }
                _ => None,
            },
            Expr::Call {
                func,
                args,
                is_self: false,
                ..
            } => self.eval_plain_call_value(func, args),
            Expr::Call {
                func,
                args,
                is_self: true,
                ..
            } => self.eval_method_call_value(func, args),
            Expr::IndexExpr { expr, index, .. } => {
                let values = match self.eval_value(expr)? {
                    TypeFunctionValue::List(values) => values,
                    TypeFunctionValue::TableBuilder(builder) => builder.list,
                    _ => return None,
                };
                let index = numeric_index(index)?;
                values.get(index.checked_sub(1)?).cloned()
            }
            _ => None,
        }
    }

    /// Evaluates a non-method call expression: `assert(...)`, a `types`
    /// library constructor (`types.optional`, `types.unionof`, …), or a
    /// plain named type-function call.
    fn eval_plain_call_value(&mut self, func: &Expr, args: &[Expr]) -> Option<TypeFunctionValue> {
        if let Some(assert_arg) = assert_call_arg(func, args) {
            if let Some(value) = self.eval_bool(assert_arg) {
                if !value {
                    self.hit_invalid_type_access = true;
                    return None;
                }
                return Some(TypeFunctionValue::Bool(value));
            }
            let value = self.eval_value(assert_arg)?;
            if self.value_is_falsey(&value) {
                self.hit_invalid_type_access = true;
                return None;
            }
            return Some(value);
        }
        if let Some(call_name) = type_library_call_name(func) {
            return match call_name {
                "copy" => {
                    let [arg] = args else {
                        return None;
                    };
                    let value = self.eval_value(arg)?;
                    Some(match value {
                        TypeFunctionValue::Type(ty) => self
                            .table_builder_from_type(ty)
                            .map(TypeFunctionValue::TableBuilder)
                            .unwrap_or(TypeFunctionValue::Type(ty)),
                        value => value,
                    })
                }
                "generic" => self.eval_generic(args),
                "newfunction" => self.eval_newfunction(args).map(|value| match value {
                    TypeFunctionNewFunction::Type(ty) => TypeFunctionValue::Type(ty),
                    TypeFunctionNewFunction::Builder(builder) => {
                        TypeFunctionValue::FunctionBuilder(builder)
                    }
                }),
                "optional" => {
                    let [arg] = args else {
                        return None;
                    };
                    let ty = self.eval_type(arg)?;
                    Some(TypeFunctionValue::Type(
                        self.generator
                            .union_type(vec![ty, self.generator.primitives().nil]),
                    ))
                }
                "singleton" => {
                    let [arg] = args else {
                        return None;
                    };
                    self.eval_singleton_value(arg)
                }
                "unionof" => {
                    let mut options = Vec::with_capacity(args.len());
                    for arg in args {
                        options.push(self.eval_type(arg)?);
                    }
                    Some(TypeFunctionValue::Type(self.generator.union_type(options)))
                }
                "intersectionof" => {
                    let mut options = Vec::with_capacity(args.len());
                    for arg in args {
                        options.push(self.eval_type(arg)?);
                    }
                    Some(TypeFunctionValue::Type(
                        self.intersection_of_values(options),
                    ))
                }
                "negationof" => {
                    let [arg] = args else {
                        return None;
                    };
                    let ty = self.eval_type(arg)?;
                    Some(TypeFunctionValue::Type(
                        self.generator.arena.alloc(TypeKind::Negation(ty)),
                    ))
                }
                "newtable" => self
                    .eval_newtable(args)
                    .map(TypeFunctionValue::TableBuilder),
                _ => None,
            };
        }
        if let Some(call_name) = type_function_plain_call_name(func) {
            return self
                .eval_named_call(call_name, args)
                .map(TypeFunctionValue::Type);
        }
        self.note_invalid_type_method_call(func);
        None
    }

    /// Evaluates a method call expression (`receiver:field()`): the no-argument
    /// type-introspection methods (`value`, `generics`, `parameters`, …).
    fn eval_method_call_value(&mut self, func: &Expr, args: &[Expr]) -> Option<TypeFunctionValue> {
        if !args.is_empty() {
            self.note_invalid_type_method_call(func);
            return None;
        }
        let Expr::IndexName {
            expr: receiver,
            index,
            ..
        } = func
        else {
            return None;
        };
        match index.as_str() {
            "value" => {
                let ty = self.eval_type(receiver)?;
                self.singleton_value(ty)
            }
            "generics" => {
                let ty = self.eval_type(receiver)?;
                self.generics_values(ty).map(TypeFunctionValue::List)
            }
            "parameters" => {
                let ty = self.eval_type(receiver)?;
                self.function_pack_builder(ty, FunctionPackSlot::Parameters)
                    .map(TypeFunctionValue::TableBuilder)
            }
            "returns" => {
                let ty = self.eval_type(receiver)?;
                self.function_pack_builder(ty, FunctionPackSlot::Returns)
                    .map(TypeFunctionValue::TableBuilder)
            }
            "metatable" => {
                let ty = self.eval_type(receiver)?;
                self.metatable_type(ty).map(TypeFunctionValue::Type)
            }
            "inner" => {
                let ty = self.eval_type(receiver)?;
                let followed = self.generator.arena.follow(ty);
                let inner = match self.generator.arena.get(followed) {
                    TypeKind::Negation(inner) => Some(*inner),
                    _ => None,
                };
                match inner {
                    Some(inner) => Some(TypeFunctionValue::Type(inner)),
                    None => {
                        // `inner` on a non-negation type is a runtime error upstream.
                        self.hit_invalid_type_access = true;
                        None
                    }
                }
            }
            "properties" => {
                let ty = self.eval_type(receiver)?;
                let followed = self.generator.arena.follow(ty);
                // `properties()` requires a table-like type; calling it on a
                // type that plainly has no properties (function/primitive/
                // singleton) is a runtime error upstream. Other shapes
                // (tables, extern types, still-unresolved types) defer.
                if matches!(
                    self.generator.arena.get(followed),
                    TypeKind::Function(_) | TypeKind::Primitive(_) | TypeKind::Singleton(_)
                ) {
                    self.hit_invalid_type_access = true;
                }
                None
            }
            "readparent" | "writeparent" => {
                let ty = self.eval_type(receiver)?;
                self.extern_parent_type(ty).map(TypeFunctionValue::Type)
            }
            other => {
                if !is_valid_type_field(other)
                    && matches!(self.eval_value(receiver), Some(TypeFunctionValue::Type(_)))
                {
                    self.hit_invalid_type_access = true;
                }
                None
            }
        }
    }

    fn eval_bool(&mut self, expr: &Expr) -> Option<bool> {
        self.consume_step()?;
        match expr {
            Expr::Bool { value, .. } => Some(*value),
            Expr::Unary {
                op: JsonUnaryOp::Not,
                expr,
                ..
            } => self.eval_bool(expr).map(|value| !value),
            Expr::Binary {
                op, left, right, ..
            } => match op {
                JsonBinaryOp::And => Some(self.eval_bool(left)? && self.eval_bool(right)?),
                JsonBinaryOp::Or => Some(self.eval_bool(left)? || self.eval_bool(right)?),
                JsonBinaryOp::CompareEq | JsonBinaryOp::CompareNe => {
                    let left = self.eval_value(left)?;
                    let right = self.eval_value(right)?;
                    let equal = self.values_equal(&left, &right)?;
                    Some(equal == matches!(op, JsonBinaryOp::CompareEq))
                }
                _ => None,
            },
            Expr::Call {
                func,
                args,
                is_self: true,
                ..
            } => {
                let Expr::IndexName {
                    expr: receiver,
                    index,
                    ..
                } = func.as_ref()
                else {
                    return None;
                };
                if index.as_str() != "is" {
                    return match self.eval_value(expr)? {
                        TypeFunctionValue::Bool(value) => Some(value),
                        _ => None,
                    };
                }
                let [target] = args.as_slice() else {
                    return None;
                };
                let TypeFunctionValue::String(target) = self.eval_value(target)? else {
                    return None;
                };
                let ty = self.eval_type(receiver)?;
                if target == "singleton" {
                    return Some(matches!(
                        self.generator.arena.get(self.generator.arena.follow(ty)),
                        TypeKind::Singleton(_)
                    ));
                }
                self.type_tag(ty).map(|tag| tag == target)
            }
            Expr::Call { .. } => match self.eval_value(expr)? {
                TypeFunctionValue::Bool(value) => Some(value),
                _ => None,
            },
            _ => {
                // Evaluate the operand for its side effects (e.g. noting an
                // unbound global read like `if not glob`) even when it cannot be
                // reduced to a concrete boolean.
                self.eval_value(expr);
                None
            }
        }
    }

    fn eval_table_builder(&mut self, items: &[TableItem]) -> Option<TypeFunctionTableBuilder> {
        let mut builder = TypeFunctionTableBuilder::default();
        for item in items {
            let value = self.eval_value(&item.value)?;
            if let Some(key) = self.table_item_key(item)? {
                if let Some(value_ty) = self.value_into_type(value.clone()) {
                    builder
                        .properties
                        .insert(key.clone(), self.table_property(value_ty));
                }
                builder.fields.insert(key.clone(), value);
            } else {
                builder.list.push(value);
            }
        }
        Some(builder)
    }

    fn eval_newtable(&mut self, args: &[Expr]) -> Option<TypeFunctionTableBuilder> {
        let mut builder = if let Some(properties) = args.first().filter(|expr| !expr_is_nil(expr)) {
            let TypeFunctionValue::TableBuilder(properties) = self.eval_value(properties)? else {
                return None;
            };
            self.table_builder_from_property_specs(properties)?
        } else {
            TypeFunctionTableBuilder::default()
        };
        if let Some(options) = args.get(1)
            && !expr_is_nil(options)
        {
            let TypeFunctionValue::TableBuilder(options) = self.eval_value(options)? else {
                return None;
            };
            if let Some(key) = options.properties.get("index").map(|property| property.ty)
                && let Some(value) = options
                    .properties
                    .get("readresult")
                    .or_else(|| options.properties.get("writeresult"))
                    .map(|property| property.ty)
            {
                let indexer = if let Some((extra_key, extra_value)) = builder.indexer {
                    (
                        self.generator.union_type(vec![key, extra_key]),
                        self.generator.union_type(vec![value, extra_value]),
                    )
                } else {
                    (key, value)
                };
                builder.indexer = Some(indexer);
            }
        }
        if let Some(metatable) = args.get(2).filter(|expr| !expr_is_nil(expr)) {
            let metatable = self.eval_type(metatable)?;
            if !self.type_is_table_like(metatable) {
                self.report_type_function_runtime_error("invalid-metatable");
                return None;
            }
            builder.metatable = Some(metatable);
        }
        Some(builder)
    }

    fn properties_call(
        &mut self,
        expr: &Expr,
    ) -> Option<Vec<(TypeFunctionValue, TypeFunctionValue)>> {
        let Expr::Call {
            func,
            args,
            is_self: true,
            ..
        } = expr
        else {
            return None;
        };
        if !args.is_empty() {
            return None;
        }
        let Expr::IndexName {
            expr: receiver,
            index,
            ..
        } = func.as_ref()
        else {
            return None;
        };
        if index.as_str() != "properties" {
            return None;
        }
        match self.eval_value(receiver)? {
            TypeFunctionValue::TableBuilder(builder) => Some(property_values(builder.properties)),
            value => {
                let ty = self.value_into_type(value)?;
                self.property_entries(ty)
            }
        }
    }

    fn property_entries(
        &mut self,
        ty: TypeId,
    ) -> Option<Vec<(TypeFunctionValue, TypeFunctionValue)>> {
        match self
            .generator
            .arena
            .get(self.generator.arena.follow(ty))
            .clone()
        {
            TypeKind::Table(table) => Some(property_values(table.properties)),
            TypeKind::Generic(_) => Some(vec![(
                TypeFunctionValue::Type(self.generator.primitives().string),
                TypeFunctionValue::Property {
                    read: Some(self.generator.primitives().any),
                    write: Some(self.generator.primitives().any),
                },
            )]),
            TypeKind::Union(options) => {
                let mut entries = Vec::new();
                for option in options {
                    entries.extend(self.property_entries(option)?);
                }
                Some(entries)
            }
            _ => None,
        }
    }

    /// Records a reference to a global that is not bound in the type-function
    /// runtime environment, returning whether it was unbound.
    fn note_global_reference(&mut self, name: &str) -> bool {
        if self
            .generator
            .type_function_global_is_bound(self.scope, name)
        {
            return false;
        }
        self.hit_unknown_global = true;
        true
    }

    fn global_type(&mut self, name: &str) -> Option<TypeId> {
        let (binding_scope, binding) = self
            .generator
            .input
            .scopes
            .lookup_type_with_scope(self.scope, name)?;
        if !binding.alias_has_generics
            && let Some(alias) = binding.alias.clone()
        {
            let alias_identity = binding.alias_identity.clone().unwrap_or_else(|| {
                self.generator
                    .input
                    .scopes
                    .alias_identity(binding_scope, name)
            });
            return Some(self.generator.lower_non_generic_alias(
                binding_scope,
                name,
                alias_identity,
                &alias,
            ));
        }
        None
    }

    fn declared_type(&mut self, name: &str) -> Option<TypeId> {
        let (binding_scope, binding) = self
            .generator
            .input
            .scopes
            .lookup_type_with_scope(self.scope, name)?;
        if let Some(ty) = binding.ty {
            return Some(ty);
        }
        if !binding.alias_has_generics
            && let Some(alias) = binding.alias.clone()
        {
            let alias_identity = binding.alias_identity.clone().unwrap_or_else(|| {
                self.generator
                    .input
                    .scopes
                    .alias_identity(binding_scope, name)
            });
            return Some(self.generator.lower_non_generic_alias(
                binding_scope,
                name,
                alias_identity,
                &alias,
            ));
        }
        None
    }

    fn eval_named_call(&mut self, name: &str, args: &[Expr]) -> Option<TypeId> {
        let Some((binding_scope, binding)) = self
            .generator
            .input
            .scopes
            .lookup_type_with_scope(self.scope, name)
        else {
            self.note_global_reference(name);
            return None;
        };
        let kind = binding.kind;
        let type_function = binding.type_function.clone();
        let alias = binding.alias.clone();
        let alias_has_generics = binding.alias_has_generics;
        let alias_identity = binding.alias_identity.clone().unwrap_or_else(|| {
            self.generator
                .input
                .scopes
                .alias_identity(binding_scope, name)
        });
        let generic_names = binding.generic_names.clone();
        let generic_pack_names = binding.generic_pack_names.clone();
        let generic_defaults = binding.generic_defaults.clone();

        let arguments = self.eval_type_arguments(args)?;
        if kind == TypeBindingKind::TypeFunction {
            return match self.generator.reduce_user_type_function_with_arguments(
                binding_scope,
                name,
                type_function.as_ref()?,
                arguments,
                self.use_location,
            ) {
                TypeFunctionEvaluation::Reduced(reduced) => Some(reduced),
                TypeFunctionEvaluation::Uninhabited | TypeFunctionEvaluation::RuntimeError => {
                    self.emitted_runtime_error = true;
                    None
                }
                TypeFunctionEvaluation::Deferred => None,
            };
        }

        let alias = alias?;
        if !alias_has_generics {
            return if arguments.is_empty() {
                Some(self.generator.lower_non_generic_alias(
                    binding_scope,
                    name,
                    alias_identity,
                    &alias,
                ))
            } else {
                None
            };
        }
        if let [pack_name] = generic_pack_names.as_slice() {
            // `Test<T, U...>(num, str, bool)`: the leading `generic_names.len()`
            // arguments bind the type generics, the rest form the trailing pack.
            if arguments.len() < generic_names.len() {
                if generic_defaults
                    .iter()
                    .skip(arguments.len())
                    .any(Option::is_none)
                {
                    self.hit_invalid_type_access = true;
                }
                return None;
            }
            let (type_args, pack_args) = arguments.split_at(generic_names.len());
            let type_substitutions = generic_names
                .iter()
                .cloned()
                .zip(type_args.iter().copied())
                .collect::<BTreeMap<_, _>>();
            let pack = self.generator.arena.alloc_pack(TypePackKind::List {
                types: pack_args.to_vec(),
                tail: None,
            });
            let pack_substitutions = BTreeMap::from([(pack_name.clone(), pack)]);
            let ty = self.generator.with_generic_type_substitution_frame(
                type_substitutions,
                pack_substitutions,
                |generator| generator.lower_type(binding_scope, &alias),
            );
            return Some(self.generator.name_type_alias_result(
                ty,
                name,
                Some(alias_identity),
                type_args.to_vec(),
                vec![pack],
            ));
        }
        if !generic_pack_names.is_empty() || arguments.len() != generic_names.len() {
            // Too few type arguments with no default to cover the gap is a
            // runtime "not enough arguments to call" error upstream.
            if arguments.len() < generic_names.len()
                && generic_defaults
                    .iter()
                    .skip(arguments.len())
                    .any(Option::is_none)
            {
                self.hit_invalid_type_access = true;
            }
            return None;
        }
        let substitutions = generic_names
            .iter()
            .cloned()
            .zip(arguments.iter().copied())
            .collect::<BTreeMap<_, _>>();
        let ty = self.generator.with_generic_type_substitution_frame(
            substitutions,
            BTreeMap::new(),
            |generator| generator.lower_type(binding_scope, &alias),
        );
        Some(self.generator.name_type_alias_result(
            ty,
            name,
            Some(alias_identity),
            arguments,
            Vec::new(),
        ))
    }

    fn eval_type_arguments(&mut self, args: &[Expr]) -> Option<Vec<TypeId>> {
        let mut arguments = Vec::with_capacity(args.len());
        for arg in args {
            arguments.push(self.eval_type(arg)?);
        }
        Some(arguments)
    }

    fn eval_generic(&mut self, args: &[Expr]) -> Option<TypeFunctionValue> {
        let ([name] | [name, Expr::Bool { .. }]) = args else {
            return None;
        };
        let TypeFunctionValue::String(name) = self.eval_value(name)? else {
            return None;
        };
        match args.get(1) {
            Some(Expr::Bool { value: true, .. }) => {
                Some(TypeFunctionValue::Pack(self.generator.arena.alloc_pack(
                    TypePackKind::Generic(GenericTypePack {
                        name,
                        level: TypeLevel(0),
                    }),
                )))
            }
            Some(Expr::Bool { value: false, .. }) | None => Some(TypeFunctionValue::Type(
                self.generator.arena.alloc(TypeKind::Generic(GenericType {
                    name,
                    level: TypeLevel(0),
                })),
            )),
            _ => None,
        }
    }

    fn eval_newfunction(&mut self, args: &[Expr]) -> Option<TypeFunctionNewFunction> {
        if args.is_empty() {
            return Some(TypeFunctionNewFunction::Builder(
                TypeFunctionBuilder::default(),
            ));
        }
        let Some(function) = self.eval_newfunction_type(args) else {
            self.report_type_function_runtime_error("invalid-newfunction");
            return None;
        };
        Some(TypeFunctionNewFunction::Type(
            self.generator.arena.alloc(TypeKind::Function(function)),
        ))
    }

    fn eval_newfunction_type(&mut self, args: &[Expr]) -> Option<FunctionType> {
        let [parameters, returns, generics] = args else {
            return None;
        };
        let parameters = self.eval_pack_option(parameters)?;
        let returns = self.eval_pack_option(returns)?;
        let TypeFunctionValue::TableBuilder(generics) = self.eval_value(generics)? else {
            return None;
        };
        if !self.generic_values_are_ordered_and_unique(&generics.list) {
            return None;
        }
        let mut function = FunctionType::new(parameters, returns);
        let (generics, generic_packs) = self.generic_list(&generics.list)?;
        function.generics = generics;
        function.generic_packs = generic_packs;
        (!self.function_has_undeclared_generics(&function)).then_some(function)
    }

    fn eval_pack_args(&mut self, args: &[Expr]) -> Option<TypePackId> {
        let ([head] | [head, _]) = args else {
            return None;
        };
        let TypeFunctionValue::TableBuilder(head) = self.eval_value(head)? else {
            return None;
        };
        let types = self.type_list(&head.list)?;
        let tail = if let Some(tail) = args.get(1) {
            self.eval_pack_tail(tail)?
        } else {
            None
        };
        Some(
            self.generator
                .arena
                .alloc_pack(TypePackKind::List { types, tail }),
        )
    }

    fn eval_generics_arg(
        &mut self,
        args: &[Expr],
    ) -> Option<(Vec<GenericType>, Vec<GenericTypePack>)> {
        let [generics] = args else {
            return None;
        };
        match self.eval_value(generics)? {
            TypeFunctionValue::List(values) => self.generic_list(&values),
            TypeFunctionValue::TableBuilder(generics) => self.generic_list(&generics.list),
            _ => None,
        }
    }

    fn eval_pack_option(&mut self, expr: &Expr) -> Option<TypePackId> {
        let TypeFunctionValue::TableBuilder(builder) = self.eval_value(expr)? else {
            return None;
        };
        let head = match builder.fields.get("head") {
            Some(TypeFunctionValue::TableBuilder(head)) => self.type_list(&head.list)?,
            Some(_) => return None,
            None => Vec::new(),
        };
        let tail = match builder.fields.get("tail").cloned() {
            Some(TypeFunctionValue::Type(ty))
                if self.generator.arena.follow(ty) == self.generator.primitives().nil =>
            {
                None
            }
            Some(TypeFunctionValue::Type(ty)) => Some(
                self.generator
                    .arena
                    .alloc_pack(TypePackKind::Variadic { ty }),
            ),
            Some(TypeFunctionValue::Pack(pack)) => Some(pack),
            Some(_) => return None,
            None => None,
        };
        Some(
            self.generator
                .arena
                .alloc_pack(TypePackKind::List { types: head, tail }),
        )
    }

    fn eval_pack_tail(&mut self, expr: &Expr) -> Option<Option<TypePackId>> {
        match self.eval_value(expr)? {
            TypeFunctionValue::Type(ty)
                if self.generator.arena.follow(ty) == self.generator.primitives().nil =>
            {
                Some(None)
            }
            TypeFunctionValue::Type(ty) => Some(Some(
                self.generator
                    .arena
                    .alloc_pack(TypePackKind::Variadic { ty }),
            )),
            TypeFunctionValue::Pack(pack) => Some(Some(pack)),
            _ => None,
        }
    }

    fn type_list(&mut self, values: &[TypeFunctionValue]) -> Option<Vec<TypeId>> {
        let mut types = Vec::with_capacity(values.len());
        for value in values {
            types.push(self.value_into_type(value.clone())?);
        }
        Some(types)
    }

    fn generic_list(
        &self,
        values: &[TypeFunctionValue],
    ) -> Option<(Vec<GenericType>, Vec<GenericTypePack>)> {
        let mut types = Vec::new();
        let mut packs = Vec::new();
        for value in values {
            match value {
                TypeFunctionValue::Type(ty) => {
                    let TypeKind::Generic(generic) =
                        self.generator.arena.get(self.generator.arena.follow(*ty))
                    else {
                        return None;
                    };
                    types.push(generic.clone());
                }
                TypeFunctionValue::Pack(pack) => {
                    let TypePackKind::Generic(generic_pack) = self
                        .generator
                        .arena
                        .get_pack(self.generator.arena.follow_pack(*pack))
                    else {
                        return None;
                    };
                    packs.push(generic_pack.clone());
                }
                _ => return None,
            }
        }
        Some((types, packs))
    }

    fn generic_values_are_ordered_and_unique(&self, values: &[TypeFunctionValue]) -> bool {
        let mut seen_names = BTreeSet::new();
        let mut seen_pack = false;
        for value in values {
            match value {
                TypeFunctionValue::Type(ty) => {
                    if seen_pack {
                        return false;
                    }
                    let TypeKind::Generic(generic) =
                        self.generator.arena.get(self.generator.arena.follow(*ty))
                    else {
                        return false;
                    };
                    if !seen_names.insert(generic.name.clone()) {
                        return false;
                    }
                }
                TypeFunctionValue::Pack(pack) => {
                    seen_pack = true;
                    let TypePackKind::Generic(generic) = self
                        .generator
                        .arena
                        .get_pack(self.generator.arena.follow_pack(*pack))
                    else {
                        return false;
                    };
                    if !seen_names.insert(generic.name.clone()) {
                        return false;
                    }
                }
                _ => return false,
            }
        }
        true
    }

    fn generics_values(&mut self, ty: TypeId) -> Option<Vec<TypeFunctionValue>> {
        let ty = self.type_function_inspection_type(ty);
        let TypeKind::Function(function) = self
            .generator
            .arena
            .get(self.generator.arena.follow(ty))
            .clone()
        else {
            return None;
        };
        let mut values = Vec::with_capacity(function.generics.len() + function.generic_packs.len());
        for generic in function.generics {
            values.push(TypeFunctionValue::Type(
                self.generator.arena.alloc(TypeKind::Generic(generic)),
            ));
        }
        for generic_pack in function.generic_packs {
            values.push(TypeFunctionValue::Pack(
                self.generator
                    .arena
                    .alloc_pack(TypePackKind::Generic(generic_pack)),
            ));
        }
        Some(values)
    }

    fn type_function_inspection_type(&mut self, ty: TypeId) -> TypeId {
        let followed = self.generator.arena.follow(ty);
        let TypeKind::Function(function) = self.generator.arena.get(followed) else {
            return followed;
        };
        if function.generics.is_empty() && function.generic_packs.is_empty() {
            generalize_function_frees(self.generator.arena, followed)
        } else {
            followed
        }
    }

    fn function_pack_builder(
        &mut self,
        ty: TypeId,
        slot: FunctionPackSlot,
    ) -> Option<TypeFunctionTableBuilder> {
        let ty = self.type_function_inspection_type(ty);
        let TypeKind::Function(function) = self
            .generator
            .arena
            .get(self.generator.arena.follow(ty))
            .clone()
        else {
            return None;
        };
        let pack = match slot {
            FunctionPackSlot::Parameters => function.arguments,
            FunctionPackSlot::Returns => function.returns,
        };
        self.pack_builder_from_pack(pack)
    }

    fn pack_builder_from_pack(&self, pack: TypePackId) -> Option<TypeFunctionTableBuilder> {
        let pack = self.generator.arena.follow_pack(pack);
        let (head, tail) = match self.generator.arena.get_pack(pack).clone() {
            TypePackKind::List { types, tail } => {
                let head = types.into_iter().map(TypeFunctionValue::Type).collect();
                let tail = match tail {
                    Some(tail) => Some(self.pack_tail_value(tail)?),
                    None => None,
                };
                (head, tail)
            }
            TypePackKind::Variadic { ty } => (Vec::new(), Some(TypeFunctionValue::Type(ty))),
            TypePackKind::Generic(_) => (Vec::new(), Some(TypeFunctionValue::Pack(pack))),
            TypePackKind::Bound(_) => unreachable!("follow_pack removes bound packs"),
            TypePackKind::Free { .. } | TypePackKind::Error => return None,
        };
        let mut builder = TypeFunctionTableBuilder::default();
        builder.fields.insert(
            "head".to_owned(),
            TypeFunctionValue::TableBuilder(TypeFunctionTableBuilder {
                list: head,
                ..TypeFunctionTableBuilder::default()
            }),
        );
        builder.fields.insert(
            "tail".to_owned(),
            tail.unwrap_or_else(|| TypeFunctionValue::Type(self.generator.primitives().nil)),
        );
        Some(builder)
    }

    fn pack_tail_value(&self, pack: TypePackId) -> Option<TypeFunctionValue> {
        let pack = self.generator.arena.follow_pack(pack);
        match self.generator.arena.get_pack(pack).clone() {
            TypePackKind::Variadic { ty } => Some(TypeFunctionValue::Type(ty)),
            TypePackKind::Generic(_) => Some(TypeFunctionValue::Pack(pack)),
            TypePackKind::Bound(_) => unreachable!("follow_pack removes bound packs"),
            TypePackKind::List { .. } | TypePackKind::Free { .. } | TypePackKind::Error => None,
        }
    }

    fn table_item_key(&mut self, item: &TableItem) -> Option<Option<String>> {
        if let Some(key) = table_item_literal_key(item) {
            return Some(Some(key));
        }
        let Some(key) = item.key.as_ref() else {
            return Some(None);
        };
        match self.eval_value(key)? {
            TypeFunctionValue::String(key) => Some(Some(key)),
            TypeFunctionValue::Type(ty) => self.singleton_value(ty).and_then(|value| match value {
                TypeFunctionValue::String(key) => Some(Some(key)),
                _ => None,
            }),
            _ => None,
        }
    }

    fn property_key(&mut self, expr: &Expr) -> Option<String> {
        match self.eval_value(expr)? {
            TypeFunctionValue::String(key) => Some(key),
            TypeFunctionValue::Type(ty) => self.singleton_value(ty).and_then(|value| match value {
                TypeFunctionValue::String(key) => Some(key),
                _ => None,
            }),
            _ => None,
        }
    }

    fn types_global(&self, name: &str) -> Option<TypeId> {
        let primitives = self.generator.primitives();
        match name {
            "any" => Some(primitives.any),
            "boolean" => Some(primitives.boolean),
            "buffer" => Some(primitives.buffer),
            "nil" => Some(primitives.nil),
            "never" => Some(primitives.never),
            "number" => Some(primitives.number),
            "string" => Some(primitives.string),
            "thread" => Some(primitives.thread),
            "unknown" => Some(primitives.unknown),
            _ => None,
        }
    }

    fn type_tag(&self, ty: TypeId) -> Option<String> {
        match self.generator.arena.get(self.generator.arena.follow(ty)) {
            TypeKind::Table(_) | TypeKind::Metatable { .. } => Some("table".to_owned()),
            TypeKind::Primitive(PrimitiveType::Nil) => Some("nil".to_owned()),
            TypeKind::Never => Some("never".to_owned()),
            TypeKind::Primitive(PrimitiveType::Boolean) => Some("boolean".to_owned()),
            TypeKind::Primitive(PrimitiveType::Number) => Some("number".to_owned()),
            TypeKind::Primitive(PrimitiveType::String) => Some("string".to_owned()),
            TypeKind::Primitive(PrimitiveType::Thread) => Some("thread".to_owned()),
            TypeKind::Primitive(PrimitiveType::Buffer) => Some("buffer".to_owned()),
            TypeKind::Singleton(SingletonType::Boolean(_)) => Some("boolean".to_owned()),
            TypeKind::Singleton(SingletonType::String(_)) => Some("string".to_owned()),
            TypeKind::Generic(_) => Some("table".to_owned()),
            _ => None,
        }
    }

    fn type_is_table_like(&self, ty: TypeId) -> bool {
        matches!(
            self.generator.arena.get(self.generator.arena.follow(ty)),
            TypeKind::Table(_) | TypeKind::Metatable { .. }
        )
    }

    fn metatable_type(&self, ty: TypeId) -> Option<TypeId> {
        match self.generator.arena.get(self.generator.arena.follow(ty)) {
            TypeKind::Metatable { metatable, .. } => Some(*metatable),
            _ => None,
        }
    }

    fn extern_parent_type(&mut self, ty: TypeId) -> Option<TypeId> {
        let parent = match self.generator.arena.get(self.generator.arena.follow(ty)) {
            TypeKind::Extern { parents, name, .. } => parents.first().cloned().or_else(|| {
                self.generator
                    .input
                    .scopes
                    .lookup_type_with_scope(self.scope, name)
                    .and_then(|(_, binding)| binding.class_super_name.clone())
            })?,
            TypeKind::Table(TableType {
                name: Some(name), ..
            }) => self
                .generator
                .input
                .scopes
                .lookup_type_with_scope(self.scope, name)
                .and_then(|(_, binding)| binding.class_super_name.clone())?,
            _ => return None,
        };
        self.declared_type(&parent)
    }

    fn singleton_value(&self, ty: TypeId) -> Option<TypeFunctionValue> {
        match self.generator.arena.get(self.generator.arena.follow(ty)) {
            TypeKind::Singleton(SingletonType::Boolean(value)) => {
                Some(TypeFunctionValue::Bool(*value))
            }
            TypeKind::Singleton(SingletonType::String(value)) => {
                Some(TypeFunctionValue::String(value.clone()))
            }
            _ => None,
        }
    }

    fn eval_singleton_value(&mut self, arg: &Expr) -> Option<TypeFunctionValue> {
        let Some(value) = self.eval_value(arg) else {
            if singleton_argument_is_definitely_invalid(arg) {
                self.hit_invalid_type_access = true;
            }
            return None;
        };
        match &value {
            TypeFunctionValue::String(_) | TypeFunctionValue::Bool(_) => Some(value),
            TypeFunctionValue::Type(ty) if self.type_is_singleton_argument(*ty) => Some(value),
            _ => {
                self.hit_invalid_type_access = true;
                None
            }
        }
    }

    fn type_is_singleton_argument(&self, ty: TypeId) -> bool {
        let ty = self.generator.arena.follow(ty);
        ty == self.generator.primitives().nil
            || matches!(self.generator.arena.get(ty), TypeKind::Singleton(_))
    }

    fn values_equal(&self, left: &TypeFunctionValue, right: &TypeFunctionValue) -> Option<bool> {
        match (left, right) {
            (TypeFunctionValue::String(left), TypeFunctionValue::String(right)) => {
                Some(left == right)
            }
            (TypeFunctionValue::Bool(left), TypeFunctionValue::Bool(right)) => Some(left == right),
            (TypeFunctionValue::Type(left), TypeFunctionValue::Type(right)) => {
                Some(self.generator.arena.follow(*left) == self.generator.arena.follow(*right))
            }
            (TypeFunctionValue::Pack(left), TypeFunctionValue::Pack(right)) => Some(
                self.generator.arena.follow_pack(*left) == self.generator.arena.follow_pack(*right),
            ),
            (TypeFunctionValue::TableBuilder(left), TypeFunctionValue::TableBuilder(right)) => {
                self.table_builders_equal(left, right)
            }
            (TypeFunctionValue::Bool(value), TypeFunctionValue::Type(ty))
            | (TypeFunctionValue::Type(ty), TypeFunctionValue::Bool(value)) => Some(matches!(
                self.generator.arena.get(self.generator.arena.follow(*ty)),
                TypeKind::Singleton(SingletonType::Boolean(singleton)) if singleton == value
            )),
            (TypeFunctionValue::String(value), TypeFunctionValue::Type(ty))
            | (TypeFunctionValue::Type(ty), TypeFunctionValue::String(value)) => Some(matches!(
                self.generator.arena.get(self.generator.arena.follow(*ty)),
                TypeKind::Singleton(SingletonType::String(singleton)) if singleton == value
            )),
            _ => Some(false),
        }
    }

    fn table_builders_equal(
        &self,
        left: &TypeFunctionTableBuilder,
        right: &TypeFunctionTableBuilder,
    ) -> Option<bool> {
        if left.list.len() != right.list.len()
            || left.fields.len() != right.fields.len()
            || left.properties.len() != right.properties.len()
        {
            return Some(false);
        }

        for (left, right) in left.list.iter().zip(&right.list) {
            if !self.values_equal(left, right)? {
                return Some(false);
            }
        }

        for (key, left_value) in &left.fields {
            let Some(right_value) = right.fields.get(key) else {
                return Some(false);
            };
            if !self.values_equal(left_value, right_value)? {
                return Some(false);
            }
        }

        for (key, left_property) in &left.properties {
            let Some(right_property) = right.properties.get(key) else {
                return Some(false);
            };
            if !self.properties_equal(left_property, right_property) {
                return Some(false);
            }
        }

        Some(match (&left.indexer, &right.indexer) {
            (Some((left_key, left_value)), Some((right_key, right_value))) => {
                self.generator.arena.follow(*left_key) == self.generator.arena.follow(*right_key)
                    && self.generator.arena.follow(*left_value)
                        == self.generator.arena.follow(*right_value)
            }
            (None, None) => true,
            _ => false,
        })
    }

    fn properties_equal(&self, left: &TableProperty, right: &TableProperty) -> bool {
        self.generator.arena.follow(left.ty) == self.generator.arena.follow(right.ty)
            && left.read_only == right.read_only
            && left.write_only == right.write_only
            && left.deprecated == right.deprecated
    }

    /// Notes a call `receiver.method(...)` / `receiver:method(...)` where the
    /// method name is not part of the type userdata API and the receiver is a
    /// concrete type value — calling a nil method, a runtime error upstream.
    fn note_invalid_type_method_call(&mut self, func: &Expr) {
        let Expr::IndexName {
            expr: receiver,
            index,
            ..
        } = func
        else {
            return;
        };
        if expr_is_types_global(receiver) || is_valid_type_field(index.as_str()) {
            return;
        }
        if matches!(self.eval_value(receiver), Some(TypeFunctionValue::Type(_))) {
            self.hit_invalid_type_access = true;
        }
    }

    fn consume_step(&mut self) -> Option<()> {
        match self.generator.type_function_evaluation.consume_step() {
            Ok(()) => Some(()),
            Err(limit) => {
                if !self.emitted_runtime_error {
                    self.report_type_function_runtime_error(limit.reason());
                }
                None
            }
        }
    }

    fn report_type_function_runtime_error(&mut self, kind: &str) {
        self.report_type_function_runtime_error_at(kind, self.use_location);
    }

    fn runtime_error_outcome(&self) -> TypeFunctionEvaluation {
        if self.emitted_uninhabited_runtime_error {
            TypeFunctionEvaluation::Uninhabited
        } else {
            TypeFunctionEvaluation::RuntimeError
        }
    }

    fn report_type_function_runtime_error_at(
        &mut self,
        kind: &str,
        location: Option<DiagnosticLocation>,
    ) {
        self.emitted_runtime_error = true;
        self.emitted_uninhabited_runtime_error |= kind == "invalid-metatable";
        self.generator
            .report_type_function_runtime_error_at(kind, location);
    }

    fn function_has_undeclared_generics(&self, function: &FunctionType) -> bool {
        let type_generics = function
            .generics
            .iter()
            .map(|generic| generic.name.clone())
            .collect::<BTreeSet<_>>();
        let pack_generics = function
            .generic_packs
            .iter()
            .map(|generic| generic.name.clone())
            .collect::<BTreeSet<_>>();
        let mut seen_types = BTreeSet::new();
        let mut seen_packs = BTreeSet::new();
        self.pack_has_undeclared_generics(
            function.arguments,
            &type_generics,
            &pack_generics,
            &mut seen_types,
            &mut seen_packs,
        ) || self.pack_has_undeclared_generics(
            function.returns,
            &type_generics,
            &pack_generics,
            &mut seen_types,
            &mut seen_packs,
        )
    }

    fn type_has_undeclared_generics(
        &self,
        ty: TypeId,
        type_generics: &BTreeSet<String>,
        pack_generics: &BTreeSet<String>,
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
    ) -> bool {
        let ty = self.generator.arena.follow(ty);
        if let TypeKind::Generic(generic) = self.generator.arena.get(ty) {
            return !type_generics.contains(&generic.name);
        }
        if !seen_types.insert(ty) {
            return false;
        }
        match self.generator.arena.get(ty).clone() {
            TypeKind::Function(function) => {
                let mut type_generics = type_generics.clone();
                type_generics.extend(function.generics.iter().map(|generic| generic.name.clone()));
                let mut pack_generics = pack_generics.clone();
                pack_generics.extend(
                    function
                        .generic_packs
                        .iter()
                        .map(|generic| generic.name.clone()),
                );
                self.pack_has_undeclared_generics(
                    function.arguments,
                    &type_generics,
                    &pack_generics,
                    seen_types,
                    seen_packs,
                ) || self.pack_has_undeclared_generics(
                    function.returns,
                    &type_generics,
                    &pack_generics,
                    seen_types,
                    seen_packs,
                )
            }
            TypeKind::Table(table) => {
                table.instantiated_type_params.iter().any(|ty| {
                    self.type_has_undeclared_generics(
                        *ty,
                        type_generics,
                        pack_generics,
                        seen_types,
                        seen_packs,
                    )
                }) || table.properties.values().any(|property| {
                    self.type_has_undeclared_generics(
                        property.ty,
                        type_generics,
                        pack_generics,
                        seen_types,
                        seen_packs,
                    )
                }) || table.indexer.is_some_and(|indexer| {
                    self.type_has_undeclared_generics(
                        indexer.key,
                        type_generics,
                        pack_generics,
                        seen_types,
                        seen_packs,
                    ) || self.type_has_undeclared_generics(
                        indexer.value,
                        type_generics,
                        pack_generics,
                        seen_types,
                        seen_packs,
                    )
                })
            }
            TypeKind::Extern {
                properties,
                indexer,
                ..
            } => {
                properties.values().any(|property| {
                    self.type_has_undeclared_generics(
                        property.ty,
                        type_generics,
                        pack_generics,
                        seen_types,
                        seen_packs,
                    )
                }) || indexer.is_some_and(|indexer| {
                    self.type_has_undeclared_generics(
                        indexer.key,
                        type_generics,
                        pack_generics,
                        seen_types,
                        seen_packs,
                    ) || self.type_has_undeclared_generics(
                        indexer.value,
                        type_generics,
                        pack_generics,
                        seen_types,
                        seen_packs,
                    )
                })
            }
            TypeKind::Metatable {
                table, metatable, ..
            } => {
                self.type_has_undeclared_generics(
                    table,
                    type_generics,
                    pack_generics,
                    seen_types,
                    seen_packs,
                ) || self.type_has_undeclared_generics(
                    metatable,
                    type_generics,
                    pack_generics,
                    seen_types,
                    seen_packs,
                )
            }
            TypeKind::TypeFunctionInstance { arguments, .. } => arguments.iter().any(|argument| {
                self.type_has_undeclared_generics(
                    *argument,
                    type_generics,
                    pack_generics,
                    seen_types,
                    seen_packs,
                )
            }),
            TypeKind::Union(options) | TypeKind::Intersection(options) => {
                options.iter().any(|option| {
                    self.type_has_undeclared_generics(
                        *option,
                        type_generics,
                        pack_generics,
                        seen_types,
                        seen_packs,
                    )
                })
            }
            TypeKind::Negation(inner) | TypeKind::Bound(inner) => self
                .type_has_undeclared_generics(
                    inner,
                    type_generics,
                    pack_generics,
                    seen_types,
                    seen_packs,
                ),
            TypeKind::Primitive(_)
            | TypeKind::Singleton(_)
            | TypeKind::Free(_)
            | TypeKind::Blocked(_)
            | TypeKind::Generic(_)
            | TypeKind::Error
            | TypeKind::Unknown
            | TypeKind::Never
            | TypeKind::Any => false,
        }
    }

    fn pack_has_undeclared_generics(
        &self,
        pack: TypePackId,
        type_generics: &BTreeSet<String>,
        pack_generics: &BTreeSet<String>,
        seen_types: &mut BTreeSet<TypeId>,
        seen_packs: &mut BTreeSet<TypePackId>,
    ) -> bool {
        let pack = self.generator.arena.follow_pack(pack);
        if let TypePackKind::Generic(generic) = self.generator.arena.get_pack(pack) {
            return !pack_generics.contains(&generic.name);
        }
        if !seen_packs.insert(pack) {
            return false;
        }
        match self.generator.arena.get_pack(pack).clone() {
            TypePackKind::List { types, tail } => {
                types.iter().any(|ty| {
                    self.type_has_undeclared_generics(
                        *ty,
                        type_generics,
                        pack_generics,
                        seen_types,
                        seen_packs,
                    )
                }) || tail.is_some_and(|tail| {
                    self.pack_has_undeclared_generics(
                        tail,
                        type_generics,
                        pack_generics,
                        seen_types,
                        seen_packs,
                    )
                })
            }
            TypePackKind::Variadic { ty } => self.type_has_undeclared_generics(
                ty,
                type_generics,
                pack_generics,
                seen_types,
                seen_packs,
            ),
            TypePackKind::Bound(bound) => self.pack_has_undeclared_generics(
                bound,
                type_generics,
                pack_generics,
                seen_types,
                seen_packs,
            ),
            TypePackKind::Free { .. } | TypePackKind::Generic(_) | TypePackKind::Error => false,
        }
    }

    fn table_property(&self, ty: TypeId) -> TableProperty {
        let mut property = TableProperty::new(ty);
        if self.generator.arena.may_be_nil(ty) {
            property.read_only = true;
        }
        property
    }

    fn table_builder_from_type(&self, ty: TypeId) -> Option<TypeFunctionTableBuilder> {
        let kind = self
            .generator
            .arena
            .get(self.generator.arena.follow(ty))
            .clone();
        match kind {
            TypeKind::Table(table) => Some(TypeFunctionTableBuilder {
                properties: table.properties,
                indexer: table.indexer.map(|indexer| (indexer.key, indexer.value)),
                ..TypeFunctionTableBuilder::default()
            }),
            TypeKind::Metatable {
                table, metatable, ..
            } => {
                let mut builder = self.table_builder_from_type(table)?;
                builder.metatable = Some(metatable);
                Some(builder)
            }
            _ => None,
        }
    }

    fn table_builder_from_property_specs(
        &mut self,
        specs: TypeFunctionTableBuilder,
    ) -> Option<TypeFunctionTableBuilder> {
        let mut builder = TypeFunctionTableBuilder {
            indexer: specs.indexer,
            metatable: specs.metatable,
            ..TypeFunctionTableBuilder::default()
        };
        for (key, value) in specs.fields {
            let property = match value.clone() {
                TypeFunctionValue::TableBuilder(spec) => self.property_from_read_write_spec(&spec),
                value => {
                    let ty = self.value_into_type(value);
                    ty.map(|ty| self.table_property(ty))
                }
            };
            builder.fields.insert(key.clone(), value);
            if let Some(property) = property {
                builder.properties.insert(key, property);
            }
        }
        Some(builder)
    }

    fn property_from_read_write_spec(
        &mut self,
        spec: &TypeFunctionTableBuilder,
    ) -> Option<TableProperty> {
        let read = match spec.fields.get("read").cloned() {
            Some(value) => Some(self.value_into_type(value)?),
            None => None,
        };
        let write = match spec.fields.get("write").cloned() {
            Some(value) => Some(self.value_into_type(value)?),
            None => None,
        };
        match (read, write) {
            (Some(read), Some(write))
                if self.generator.arena.follow(read) == self.generator.arena.follow(write) =>
            {
                Some(TableProperty::new(read))
            }
            (Some(read), Some(write)) => Some(TableProperty::new(
                self.generator.union_type(vec![read, write]),
            )),
            (Some(read), None) => {
                let mut property = TableProperty::new(read);
                property.read_only = true;
                Some(property)
            }
            (None, Some(write)) => {
                let mut property = TableProperty::new(write);
                property.write_only = true;
                Some(property)
            }
            (None, None) => None,
        }
    }
}

fn apply_property_update(
    properties: &mut BTreeMap<String, TableProperty>,
    key: String,
    update: &TypeFunctionPropertyUpdate,
) {
    match update {
        TypeFunctionPropertyUpdate::Read(ty) => {
            properties
                .entry(key)
                .and_modify(|property| {
                    property.ty = *ty;
                    property.read_only = property.write_only;
                    property.write_only = false;
                })
                .or_insert_with(|| {
                    let mut property = TableProperty::new(*ty);
                    property.read_only = true;
                    property
                });
        }
        TypeFunctionPropertyUpdate::ReadWrite(Some(ty)) => {
            properties.insert(key, TableProperty::new(*ty));
        }
        TypeFunctionPropertyUpdate::ReadWrite(None) => {
            properties.remove(&key);
        }
        TypeFunctionPropertyUpdate::Write(ty) => {
            properties
                .entry(key)
                .and_modify(|property| {
                    property.ty = *ty;
                    property.write_only = property.read_only;
                    property.read_only = false;
                })
                .or_insert_with(|| {
                    let mut property = TableProperty::new(*ty);
                    property.write_only = true;
                    property
                });
        }
    }
}

impl ExpressionConstraintGenerator<'_> {
    pub(crate) fn report_type_function_runtime_error_at(
        &mut self,
        kind: &str,
        location: Option<DiagnosticLocation>,
    ) {
        self.generated.diagnostics.push(
            TypeDiagnostic::error(
                DiagnosticCategory::Generic,
                location.unwrap_or_else(DiagnosticLocation::missing),
            )
            .with_typed(crate::diagnostic::Payload::TypeFunctionRuntimeError {
                reason: kind.to_owned(),
            }),
        );
    }
}

fn property_values(
    properties: impl IntoIterator<Item = (String, TableProperty)>,
) -> Vec<(TypeFunctionValue, TypeFunctionValue)> {
    properties
        .into_iter()
        .map(|(name, property)| {
            (
                TypeFunctionValue::String(name),
                TypeFunctionValue::Property {
                    read: (!property.write_only).then_some(property.ty),
                    write: (!property.read_only).then_some(property.ty),
                },
            )
        })
        .collect()
}

fn assert_call_arg<'a>(callee: &Expr, args: &'a [Expr]) -> Option<&'a Expr> {
    let Expr::Global { name, .. } = callee else {
        return None;
    };
    if name.as_str() != "assert" {
        return None;
    }
    args.first()
}

fn type_library_call_name(callee: &Expr) -> Option<&str> {
    match callee {
        Expr::Group { expr, .. } => type_library_call_name(expr),
        Expr::IndexName { expr, index, .. } if expr_is_types_global(expr) => Some(index.as_str()),
        Expr::IndexName { .. } => None,
        _ => None,
    }
}

fn singleton_argument_is_definitely_invalid(expr: &Expr) -> bool {
    match expr {
        Expr::Group { expr, .. } | Expr::TypeAssertion { expr, .. } => {
            singleton_argument_is_definitely_invalid(expr)
        }
        Expr::Nil { .. } | Expr::Bool { .. } | Expr::String { .. } => false,
        Expr::Number { .. } | Expr::Integer { .. } | Expr::Table { .. } => true,
        _ => false,
    }
}

fn type_function_plain_call_name(callee: &Expr) -> Option<&str> {
    match callee {
        Expr::Group { expr, .. } => type_function_plain_call_name(expr),
        Expr::Global { name, .. } => Some(name.as_str()),
        _ => None,
    }
}

fn numeric_index(expr: &Expr) -> Option<usize> {
    match expr {
        Expr::Group { expr, .. } => numeric_index(expr),
        Expr::Number { value, .. } => {
            let value = value.as_f64()?;
            if value.fract() == 0.0 && value >= 1.0 {
                Some(value as usize)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn expr_is_types_global(expr: &Expr) -> bool {
    match expr {
        Expr::Group { expr, .. } => expr_is_types_global(expr),
        Expr::Global { name, .. } => name.as_str() == "types",
        _ => false,
    }
}

fn expr_is_nil(expr: &Expr) -> bool {
    match expr {
        Expr::Group { expr, .. } | Expr::TypeAssertion { expr, .. } => expr_is_nil(expr),
        Expr::Nil { .. } => true,
        _ => false,
    }
}

fn restore_binding(
    env: &mut BTreeMap<String, TypeFunctionValue>,
    name: &str,
    old: Option<TypeFunctionValue>,
) {
    if let Some(old) = old {
        env.insert(name.to_owned(), old);
    } else {
        env.remove(name);
    }
}

fn table_item_literal_key(item: &TableItem) -> Option<String> {
    match (&item.kind, &item.key) {
        (JsonTableItemKind::Record, Some(Expr::String { value, .. }))
        | (JsonTableItemKind::General, Some(Expr::String { value, .. })) => Some(value.clone()),
        (JsonTableItemKind::Record, Some(Expr::Global { name, .. })) => {
            Some(name.as_str().to_owned())
        }
        _ => None,
    }
}
