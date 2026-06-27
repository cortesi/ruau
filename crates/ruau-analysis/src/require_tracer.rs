//! Static `require` call tracer over a parsed AST.
//!
//! The tracer walks the AST collecting `require(...)` calls and the expressions
//! whose values flow into them. Public callers should use [`crate::Frontend`]
//! over the shared module source model; the resolver-shaped tracer entry point
//! remains hidden for fixture machinery that needs Roblox expression semantics.

use std::collections::BTreeMap;

use ruau_ast::{
    Location,
    syntax::{Expr, LocalId, Stat, SyntaxId, Type},
    visit::{NodePath, Visitor, WalkControl, walk_stat},
};
use ruau_source::{ModuleId, ModuleName, ModuleSource, poll_ready_once};

#[cfg(any())]
use crate::resolve::resolver::FileResolver;
use crate::resolve::resolver::{ModuleInfo, ResolverError, resolver_error_from_module_source};

/// Resolution state of a syntax id in a [`RequireTraceResult`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequireResolution<'trace> {
    /// The syntax id is not a tracked require argument or call.
    NotTracked,
    /// The syntax id is a require call whose argument could not be resolved.
    Unresolved,
    /// The syntax id resolved to a module.
    Resolved(&'trace ModuleInfo),
}

/// Static require trace result.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RequireTraceResult {
    /// Resolved module per expression or type syntax id. An absent key is not a
    /// tracked require; `Some(None)` is a require call whose argument failed to
    /// resolve; `Some(Some(_))` resolved. Use [`resolution`](Self::resolution).
    nodes: BTreeMap<SyntaxId, Option<ModuleInfo>>,
    /// Resolved require calls in source traversal order.
    pub require_list: Vec<RequireListEntry>,
    /// Resolver errors raised while resolving require expressions.
    pub diagnostics: Vec<ResolverError>,
}

impl RequireTraceResult {
    /// Returns the traced module for a syntax id, if it resolved.
    #[must_use]
    pub fn module_for(&self, syntax_id: SyntaxId) -> Option<&ModuleInfo> {
        self.nodes.get(&syntax_id)?.as_ref()
    }

    /// Returns the three-state resolution of a syntax id.
    #[must_use]
    pub fn resolution(&self, syntax_id: SyntaxId) -> RequireResolution<'_> {
        match self.nodes.get(&syntax_id) {
            None => RequireResolution::NotTracked,
            Some(None) => RequireResolution::Unresolved,
            Some(Some(module)) => RequireResolution::Resolved(module),
        }
    }

    /// Iterates the modules that resolved, in syntax-id order.
    pub fn resolved_modules(&self) -> impl Iterator<Item = &ModuleInfo> {
        self.nodes.values().filter_map(Option::as_ref)
    }

    /// Returns whether any traced require call failed to resolve.
    #[must_use]
    pub fn has_unresolved_requires(&self) -> bool {
        self.nodes.values().any(Option::is_none)
    }

    /// Iterates module names for resolved require calls in source order.
    pub fn required_modules(&self) -> impl Iterator<Item = &ModuleName> {
        self.require_list.iter().map(|entry| &entry.module)
    }
}

/// One resolved require call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequireListEntry {
    /// Require-call syntax id.
    pub call: SyntaxId,
    /// Required module name.
    pub module: ModuleName,
    /// Require-call source location.
    pub location: Option<Location>,
}

/// One direct string request found in a static `require("...")` call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StaticRequireRequest {
    /// Raw request string as written in the source.
    pub request: String,
    /// Require-call source location.
    pub location: Option<Location>,
}

/// Traces static requires in an AST block.
#[doc(hidden)]
#[must_use]
#[cfg(any())]
pub fn trace_requires(
    file_resolver: &dyn FileResolver,
    root: &Stat,
    current_module_name: &ModuleName,
) -> RequireTraceResult {
    let mut tracer = RequireTracer::new(current_module_name);
    walk_stat(root, &mut tracer);
    tracer.process(file_resolver)
}

/// Traces static requires in an AST block through immediately-ready
/// [`ModuleSource`] operations.
pub fn trace_requires_ready(
    module_source: &dyn ModuleSource,
    root: &Stat,
    current_module_name: &ModuleName,
) -> RequireTraceResult {
    let mut tracer = RequireTracer::new(current_module_name);
    walk_stat(root, &mut tracer);
    tracer.process_ready(module_source)
}

/// Traces static requires in an AST block, resolving string requests through
/// the shared async-first module source model.
pub async fn trace_requires_async(
    module_source: &dyn ModuleSource,
    root: &Stat,
    current_module_name: &ModuleName,
) -> RequireTraceResult {
    let mut tracer = RequireTracer::new(current_module_name);
    walk_stat(root, &mut tracer);
    tracer.process_async(module_source).await
}

/// Borrowed syntax node used by the worklist.
#[derive(Clone, Copy)]
enum Node<'ast> {
    /// Expression node.
    Expr(&'ast Expr),
    /// Type node.
    Type(&'ast Type),
}

impl Node<'_> {
    /// Returns this node's syntax id.
    fn syntax_id(self) -> SyntaxId {
        match self {
            Self::Expr(expr) => expr.syntax_id(),
            Self::Type(luau_type) => luau_type.syntax_id(),
        }
    }

    /// Returns whether this node forwards its dependent context directly.
    fn propagates_dependent_context(self) -> bool {
        matches!(
            self,
            Self::Expr(Expr::Local { .. })
                | Self::Expr(Expr::Group { .. })
                | Self::Expr(Expr::TypeAssertion { .. })
                | Self::Type(Type::Group { .. })
                | Self::Type(Type::Typeof { .. })
        )
    }
}

/// Static require tracer.
struct RequireTracer<'ast> {
    /// Current module context.
    module_context: ModuleInfo,
    /// Local initializer expressions that remain valid.
    locals: BTreeMap<LocalId, Option<Node<'ast>>>,
    /// Require-call expressions in traversal order.
    require_calls: Vec<&'ast Expr>,
    /// Worklist of dependent expression and type nodes.
    work: Vec<Node<'ast>>,
    /// Accumulated result.
    result: RequireTraceResult,
}

impl<'ast> Visitor<'ast> for RequireTracer<'ast> {
    fn visit_stat(&mut self, _path: &NodePath, stat: &'ast Stat) -> WalkControl {
        match stat {
            Stat::Local { vars, values, .. } => {
                for (local, value) in vars.iter().zip(values) {
                    self.locals.insert(local.id, Some(Node::Expr(value)));
                }
            }
            Stat::Assign { vars, .. } => {
                for var in vars {
                    self.invalidate_local(var);
                }
            }
            Stat::CompoundAssign { var, .. } => {
                self.invalidate_local(var);
            }
            _ => {}
        }
        WalkControl::Continue
    }

    fn visit_expr(&mut self, _path: &NodePath, expr: &'ast Expr) -> WalkControl {
        if require_call(expr).is_some() {
            self.require_calls.push(expr);
        }

        if let Expr::TypeAssertion { annotation, .. } = expr
            && type_annotation_is_any(annotation)
        {
            return WalkControl::SkipChildren;
        }

        WalkControl::Continue
    }
}

impl<'ast> RequireTracer<'ast> {
    /// Creates a require tracer.
    fn new(current_module_name: &ModuleName) -> Self {
        Self {
            module_context: ModuleInfo::new(current_module_name.clone()),
            locals: BTreeMap::new(),
            require_calls: Vec::new(),
            work: Vec::new(),
            result: RequireTraceResult::default(),
        }
    }

    /// Processes the collected worklist.
    #[cfg(any())]
    fn process(mut self, file_resolver: &dyn FileResolver) -> RequireTraceResult {
        self.seed_require_work();
        self.expand_dependent_work();
        let module_context = self.module_context.clone();
        for index in (0..self.work.len()).rev() {
            let node = self.work[index];
            let syntax_id = node.syntax_id();
            if self.result.nodes.contains_key(&syntax_id) {
                continue;
            }

            if let Some(module) = self.resolve_work_node(file_resolver, node, &module_context) {
                self.result.nodes.insert(syntax_id, Some(module));
            }
        }

        for require in std::mem::take(&mut self.require_calls) {
            self.record_require_call(require);
        }

        self.result
    }

    /// Processes the collected worklist through ready-only module source
    /// operations.
    fn process_ready(mut self, module_source: &dyn ModuleSource) -> RequireTraceResult {
        self.seed_require_work();
        self.expand_dependent_work();
        let module_context = self.module_context.clone();
        for index in (0..self.work.len()).rev() {
            let node = self.work[index];
            let syntax_id = node.syntax_id();
            if self.result.nodes.contains_key(&syntax_id) {
                continue;
            }

            if let Some(module) = self.resolve_work_node_ready(module_source, node, &module_context)
            {
                self.result.nodes.insert(syntax_id, Some(module));
            }
        }

        for require in std::mem::take(&mut self.require_calls) {
            self.record_require_call(require);
        }

        self.result
    }

    /// Processes the collected worklist through the async module source model.
    async fn process_async(mut self, module_source: &dyn ModuleSource) -> RequireTraceResult {
        self.seed_require_work();
        self.expand_dependent_work();
        let module_context = self.module_context.clone();
        for index in (0..self.work.len()).rev() {
            let node = self.work[index];
            let syntax_id = node.syntax_id();
            if self.result.nodes.contains_key(&syntax_id) {
                continue;
            }

            if let Some(module) = self
                .resolve_work_node_async(module_source, node, &module_context)
                .await
            {
                self.result.nodes.insert(syntax_id, Some(module));
            }
        }

        for require in std::mem::take(&mut self.require_calls) {
            self.record_require_call(require);
        }

        self.result
    }

    /// Records one traced require call and its resolution in the result map.
    fn record_require_call(&mut self, require: &'ast Expr) {
        let Some((arg, call_id, location)) = require_call(require) else {
            return;
        };
        let resolved = self.result.module_for(arg.syntax_id()).cloned();
        if let Some(module) = &resolved {
            self.result.require_list.push(RequireListEntry {
                call: call_id,
                module: module.name.clone(),
                location,
            });
        }
        self.result.nodes.insert(call_id, resolved);
    }

    /// Seeds the worklist with the argument expression from each require call.
    fn seed_require_work(&mut self) {
        self.work.reserve(self.require_calls.len());
        for require in &self.require_calls {
            if let Some((arg, ..)) = require_call(require) {
                self.work.push(Node::Expr(arg));
            }
        }
    }

    fn invalidate_local(&mut self, expr: &Expr) {
        if let Expr::Local { local, .. } = expr {
            self.locals.insert(local.id, None);
        }
    }

    /// Expands the worklist with dependent expressions and types.
    fn expand_dependent_work(&mut self) {
        let mut index = 0;
        while index < self.work.len() {
            if let Some(dependent) = self.dependent(self.work[index]) {
                self.work.push(dependent);
            }
            index += 1;
        }
    }

    /// Resolves one worklist node to module information, collecting any
    /// resolver diagnostic raised for a failed module reference.
    #[cfg(any())]
    fn resolve_work_node(
        &mut self,
        file_resolver: &dyn FileResolver,
        node: Node<'ast>,
        module_context: &ModuleInfo,
    ) -> Option<ModuleInfo> {
        let context = match self.dependent(node) {
            Some(dependent) => {
                let dependent_context = self.result.module_for(dependent.syntax_id())?;
                if node.propagates_dependent_context() {
                    return Some(dependent_context.clone());
                }
                dependent_context
            }
            None => module_context,
        };
        let Node::Expr(expr) = node else {
            return None;
        };
        match file_resolver.resolve_module(Some(context), expr) {
            Ok(module) => module,
            Err(diagnostic) => {
                self.result.diagnostics.push(diagnostic);
                None
            }
        }
    }

    /// Resolves one worklist node through immediately-ready [`ModuleSource`]
    /// operations, collecting diagnostics for failed source requests.
    fn resolve_work_node_ready(
        &mut self,
        module_source: &dyn ModuleSource,
        node: Node<'ast>,
        module_context: &ModuleInfo,
    ) -> Option<ModuleInfo> {
        let context = match self.dependent(node) {
            Some(dependent) => {
                let dependent_context = self.result.module_for(dependent.syntax_id())?;
                if node.propagates_dependent_context() {
                    return Some(dependent_context.clone());
                }
                dependent_context
            }
            None => module_context,
        };
        let Node::Expr(Expr::String { value, .. }) = node else {
            return None;
        };

        let requester = ModuleId::from(&context.name);
        match poll_ready_once(
            module_source.resolve(Some(&requester), value.as_bytes()),
            "resolving module source",
        )
        .and_then(|id| ModuleName::from_id(&id).map(ModuleInfo::new))
        {
            Ok(module) => Some(module),
            Err(error) => {
                self.result
                    .diagnostics
                    .push(resolver_error_from_module_source(
                        error,
                        Some(context.name.clone()),
                    ));
                None
            }
        }
    }

    /// Resolves one worklist node through [`ModuleSource`], collecting any
    /// source diagnostic raised for a failed module reference.
    async fn resolve_work_node_async(
        &mut self,
        module_source: &dyn ModuleSource,
        node: Node<'ast>,
        module_context: &ModuleInfo,
    ) -> Option<ModuleInfo> {
        let context = match self.dependent(node) {
            Some(dependent) => {
                let dependent_context = self.result.module_for(dependent.syntax_id())?;
                if node.propagates_dependent_context() {
                    return Some(dependent_context.clone());
                }
                dependent_context
            }
            None => module_context,
        };
        let Node::Expr(Expr::String { value, .. }) = node else {
            return None;
        };

        let requester = ModuleId::from(&context.name);
        match module_source
            .resolve(Some(&requester), value.as_bytes())
            .await
            .and_then(|id| ModuleName::from_id(&id).map(ModuleInfo::new))
        {
            Ok(module) => Some(module),
            Err(error) => {
                self.result
                    .diagnostics
                    .push(resolver_error_from_module_source(
                        error,
                        Some(context.name.clone()),
                    ));
                None
            }
        }
    }

    /// Returns the node this syntax node depends on.
    fn dependent(&self, node: Node<'ast>) -> Option<Node<'ast>> {
        match node {
            Node::Expr(Expr::Local { local, .. }) => self.locals.get(&local.id).copied().flatten(),
            Node::Expr(Expr::IndexName { expr, .. })
            | Node::Expr(Expr::IndexExpr { expr, .. })
            | Node::Expr(Expr::Group { expr, .. }) => Some(Node::Expr(expr)),
            Node::Expr(Expr::Call {
                func,
                is_self: true,
                ..
            }) => {
                let Expr::IndexName { expr, .. } = func.as_ref() else {
                    return None;
                };
                Some(Node::Expr(expr))
            }
            Node::Expr(Expr::TypeAssertion { annotation, .. }) => Some(Node::Type(annotation)),
            Node::Type(Type::Group { inner, .. }) => Some(Node::Type(inner)),
            Node::Type(Type::Typeof { expr, .. }) => Some(Node::Expr(expr)),
            _ => None,
        }
    }
}

/// Collects the raw string request of every static `require("...")` call in
/// an AST block, in source traversal order. Arguments that are not direct
/// string literals are skipped — those requests resolve at runtime.
#[must_use]
pub fn static_require_requests(root: &Stat) -> Vec<String> {
    static_require_requests_with_locations(root)
        .into_iter()
        .map(|entry| entry.request)
        .collect()
}

/// Collects every direct string `require("...")` request with its call
/// location, in source traversal order.
///
/// Arguments that are not direct string literals are skipped — those requests
/// resolve at runtime. Locations are the full `require(...)` call range.
#[must_use]
pub fn static_require_requests_with_locations(root: &Stat) -> Vec<StaticRequireRequest> {
    struct Requests(Vec<StaticRequireRequest>);
    impl<'ast> Visitor<'ast> for Requests {
        fn visit_expr(&mut self, _path: &NodePath, expr: &'ast Expr) -> WalkControl {
            if let Some((arg, _, location)) = require_call(expr)
                && let Expr::String { value, .. } = arg
            {
                self.0.push(StaticRequireRequest {
                    request: value.clone(),
                    location,
                });
            }
            WalkControl::Continue
        }
    }
    let mut requests = Requests(Vec::new());
    walk_stat(root, &mut requests);
    requests.0
}

fn require_call(expr: &Expr) -> Option<(&Expr, SyntaxId, Option<Location>)> {
    let Expr::Call {
        func,
        args,
        location,
        ..
    } = expr
    else {
        return None;
    };
    if !matches!(func.as_ref(), Expr::Global { name, .. } if name.as_str() == "require")
        || args.is_empty()
    {
        return None;
    }
    Some((&args[0], expr.syntax_id(), *location))
}

fn type_annotation_is_any(luau_type: &Type) -> bool {
    match luau_type {
        Type::Reference { prefix, name, .. } => prefix.is_none() && name.as_str() == "any",
        Type::Group { inner, .. } => type_annotation_is_any(inner),
        _ => false,
    }
}
