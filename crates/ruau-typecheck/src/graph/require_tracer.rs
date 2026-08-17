//! Static `require` call tracer over a parsed AST.
//!
//! The tracer walks the AST collecting `require(...)` calls and the expressions
//! whose values flow into them. Public callers should use [`crate::GraphChecker`]
//! over the shared source-provider model; the internal frontend and resolver-shaped
//! tracer entry point remain fixture machinery for upstream expression semantics.

use std::collections::BTreeMap;

use ruau_source::{ModuleId, ModuleName, ReadySourceFutureExt, SourceProvider};
use ruau_syntax::{
    Expr, LocalId, Location, Stat, SyntaxId, Type,
    visit::{Visitor, WalkControl, walk_stat},
};

#[cfg(any())]
use super::resolve::resolver::FileResolver;
use super::resolve::resolver::{ModuleInfo, ResolverError, resolver_error_from_module_source};

#[cfg(not(target_arch = "wasm32"))]
pub type RequireAdmission<'a> = dyn FnMut(&ModuleId, &ModuleId) -> bool + Send + 'a;
#[cfg(target_arch = "wasm32")]
pub(crate) type RequireAdmission<'a> = dyn FnMut(&ModuleId, &ModuleId) -> bool + 'a;

/// Resolution state of a syntax id in a [`RequireTraceResult`].
///
/// Mirrors upstream Frontend behavior; retained for conformance parity
/// (tests in `src/tests.rs`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(any())]
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
    /// Byte-exact source-provider request per resolved expression.
    requests: BTreeMap<SyntaxId, Vec<u8>>,
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
    ///
    /// Mirrors upstream Frontend behavior; retained for conformance parity
    /// (tests in `src/tests.rs`).
    #[must_use]
    #[cfg(any())]
    pub fn resolution(&self, syntax_id: SyntaxId) -> RequireResolution<'_> {
        match self.nodes.get(&syntax_id) {
            None => RequireResolution::NotTracked,
            Some(None) => RequireResolution::Unresolved,
            Some(Some(module)) => RequireResolution::Resolved(module),
        }
    }

    /// Iterates the modules that resolved, in syntax-id order.
    #[cfg(any())]
    pub fn resolved_modules(&self) -> impl Iterator<Item = &ModuleInfo> {
        self.nodes.values().filter_map(Option::as_ref)
    }

    /// Returns whether any traced require call failed to resolve.
    ///
    /// Mirrors upstream Frontend behavior; retained for conformance parity
    /// (tests in `src/tests.rs`).
    #[must_use]
    #[cfg(any())]
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
    /// Byte-exact request passed to the source provider, when source-provider
    /// resolution produced this entry.
    pub request: Option<Vec<u8>>,
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

/// One complete scan of global `require(...)` calls.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RequireScan {
    /// Direct string requests in source traversal order.
    pub static_requests: Vec<StaticRequireRequest>,
    /// Whether any recognized call has no first argument or a non-literal first argument.
    pub has_dynamic: bool,
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

pub fn trace_requires_ready_with_admission(
    module_source: &dyn SourceProvider,
    root: &Stat,
    current_module_name: &ModuleName,
    admit: &mut RequireAdmission<'_>,
) -> RequireTraceResult {
    let mut tracer = RequireTracer::new(current_module_name);
    walk_stat(root, &mut tracer);
    tracer.process_ready(module_source, admit)
}

pub async fn trace_requires_async_with_admission(
    module_source: &dyn SourceProvider,
    root: &Stat,
    current_module_name: &ModuleName,
    admit: &mut RequireAdmission<'_>,
) -> RequireTraceResult {
    let mut tracer = RequireTracer::new(current_module_name);
    walk_stat(root, &mut tracer);
    tracer.process_async(module_source, admit).await
}

/// Borrowed syntax node used by the worklist.
#[derive(Clone, Copy)]
enum Node<'ast> {
    /// Expression node.
    Expr(&'ast Expr),
    /// Type node.
    Type(&'ast Type),
}

enum WorkResolution {
    Propagated(ModuleInfo),
    Provider {
        requester: ModuleId,
        module: ModuleInfo,
    },
}

enum PreparedWorkNode<'ast> {
    Propagated(ModuleInfo),
    Provider {
        syntax_id: SyntaxId,
        requester: ModuleId,
        context_name: ModuleName,
        request: &'ast [u8],
    },
}

impl WorkResolution {
    fn into_module(self) -> ModuleInfo {
        match self {
            Self::Propagated(module) | Self::Provider { module, .. } => module,
        }
    }
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
    fn visit_stat(&mut self, stat: &'ast Stat) -> WalkControl {
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

    fn visit_expr(&mut self, expr: &'ast Expr) -> WalkControl {
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
    fn process_ready(
        mut self,
        module_source: &dyn SourceProvider,
        admit: &mut RequireAdmission<'_>,
    ) -> RequireTraceResult {
        self.seed_require_work();
        self.expand_dependent_work();
        let module_context = self.module_context.clone();
        for index in (0..self.work.len()).rev() {
            let node = self.work[index];
            let syntax_id = node.syntax_id();
            if self.result.nodes.contains_key(&syntax_id) {
                continue;
            }

            if let Some(resolution) =
                self.resolve_work_node_ready(module_source, node, &module_context)
            {
                if let WorkResolution::Provider { requester, module } = &resolution {
                    let module = ModuleId::from(&module.name);
                    if !admit(requester, &module) {
                        break;
                    }
                }
                self.result
                    .nodes
                    .insert(syntax_id, Some(resolution.into_module()));
            }
        }

        for require in std::mem::take(&mut self.require_calls) {
            self.record_require_call(require);
        }

        self.result
    }

    /// Processes the collected worklist through the async module source model.
    async fn process_async(
        mut self,
        module_source: &dyn SourceProvider,
        admit: &mut RequireAdmission<'_>,
    ) -> RequireTraceResult {
        self.seed_require_work();
        self.expand_dependent_work();
        let module_context = self.module_context.clone();
        for index in (0..self.work.len()).rev() {
            let node = self.work[index];
            let syntax_id = node.syntax_id();
            if self.result.nodes.contains_key(&syntax_id) {
                continue;
            }

            if let Some(resolution) = self
                .resolve_work_node_async(module_source, node, &module_context)
                .await
            {
                if let WorkResolution::Provider { requester, module } = &resolution {
                    let module = ModuleId::from(&module.name);
                    if !admit(requester, &module) {
                        break;
                    }
                }
                self.result
                    .nodes
                    .insert(syntax_id, Some(resolution.into_module()));
            }
        }

        for require in std::mem::take(&mut self.require_calls) {
            self.record_require_call(require);
        }

        self.result
    }

    /// Records one traced require call and its resolution in the result map.
    fn record_require_call(&mut self, require: &'ast Expr) {
        let Some((Some(arg), call_id, location)) = require_call(require) else {
            return;
        };
        let resolved = self.result.module_for(arg.syntax_id()).cloned();
        if let Some(module) = &resolved {
            let request = self.result.requests.get(&arg.syntax_id()).cloned();
            self.result.require_list.push(RequireListEntry {
                call: call_id,
                module: module.name.clone(),
                request,
                location,
            });
        }
        self.result.nodes.insert(call_id, resolved);
    }

    /// Seeds the worklist with the argument expression from each require call.
    fn seed_require_work(&mut self) {
        self.work.reserve(self.require_calls.len());
        for require in &self.require_calls {
            if let Some((Some(arg), ..)) = require_call(require) {
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
                let dependent_id = dependent.syntax_id();
                if node.propagates_dependent_context() {
                    let dependent_context = self.result.module_for(dependent_id)?.clone();
                    if let Some(request) = self.result.requests.get(&dependent_id).cloned() {
                        self.result.requests.insert(node.syntax_id(), request);
                    }
                    return Some(dependent_context);
                }
                self.result.module_for(dependent_id)?
            }
            None => module_context,
        };
        let Node::Expr(expr) = node else {
            return None;
        };
        match file_resolver.resolve_module(Some(context), expr) {
            Ok(module) => {
                if module.is_some()
                    && let Expr::String { value, .. } = expr
                {
                    self.result
                        .requests
                        .insert(node.syntax_id(), value.as_bytes().to_vec());
                }
                module
            }
            Err(diagnostic) => {
                self.result.diagnostics.push(diagnostic);
                None
            }
        }
    }

    /// Resolves one worklist node through immediately-ready [`SourceProvider`]
    /// operations, collecting diagnostics for failed source requests.
    fn resolve_work_node_ready(
        &mut self,
        module_source: &dyn SourceProvider,
        node: Node<'ast>,
        module_context: &ModuleInfo,
    ) -> Option<WorkResolution> {
        let (syntax_id, requester, context_name, request) =
            match self.prepare_work_node(node, module_context)? {
                PreparedWorkNode::Propagated(module) => {
                    return Some(WorkResolution::Propagated(module));
                }
                PreparedWorkNode::Provider {
                    syntax_id,
                    requester,
                    context_name,
                    request,
                } => (syntax_id, requester, context_name, request),
            };

        match (module_source.resolve(Some(&requester), request))
            .ready_only("resolving module source")
            .and_then(|id| ModuleName::from_id(&id).map(ModuleInfo::new))
        {
            Ok(module) => {
                self.result.requests.insert(syntax_id, request.to_vec());
                Some(WorkResolution::Provider { requester, module })
            }
            Err(error) => {
                self.result
                    .diagnostics
                    .push(resolver_error_from_module_source(error, Some(context_name)));
                None
            }
        }
    }

    /// Resolves one worklist node through [`SourceProvider`], collecting any
    /// source diagnostic raised for a failed module reference.
    async fn resolve_work_node_async(
        &mut self,
        module_source: &dyn SourceProvider,
        node: Node<'ast>,
        module_context: &ModuleInfo,
    ) -> Option<WorkResolution> {
        let (syntax_id, requester, context_name, request) =
            match self.prepare_work_node(node, module_context)? {
                PreparedWorkNode::Propagated(module) => {
                    return Some(WorkResolution::Propagated(module));
                }
                PreparedWorkNode::Provider {
                    syntax_id,
                    requester,
                    context_name,
                    request,
                } => (syntax_id, requester, context_name, request),
            };

        match module_source
            .resolve(Some(&requester), request)
            .await
            .and_then(|id| ModuleName::from_id(&id).map(ModuleInfo::new))
        {
            Ok(module) => {
                self.result.requests.insert(syntax_id, request.to_vec());
                Some(WorkResolution::Provider { requester, module })
            }
            Err(error) => {
                self.result
                    .diagnostics
                    .push(resolver_error_from_module_source(error, Some(context_name)));
                None
            }
        }
    }

    /// Prepares the dependency propagation or provider request shared by the
    /// ready and async resolution paths.
    fn prepare_work_node(
        &mut self,
        node: Node<'ast>,
        module_context: &ModuleInfo,
    ) -> Option<PreparedWorkNode<'ast>> {
        let context = match self.dependent(node) {
            Some(dependent) => {
                let dependent_id = dependent.syntax_id();
                if node.propagates_dependent_context() {
                    let dependent_context = self.result.module_for(dependent_id)?.clone();
                    if let Some(request) = self.result.requests.get(&dependent_id).cloned() {
                        self.result.requests.insert(node.syntax_id(), request);
                    }
                    return Some(PreparedWorkNode::Propagated(dependent_context));
                }
                self.result.module_for(dependent_id)?
            }
            None => module_context,
        };
        let Node::Expr(Expr::String { value, .. }) = node else {
            return None;
        };
        Some(PreparedWorkNode::Provider {
            syntax_id: node.syntax_id(),
            requester: ModuleId::from(&context.name),
            context_name: context.name.clone(),
            request: value.as_bytes(),
        })
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
    scan_requires(root).static_requests
}

/// Scans global `require(...)` calls once for static requests and dynamic use.
///
/// A direct string first argument is static, including when the call has extra
/// arguments. A missing or non-literal first argument is dynamic. A local that
/// shadows `require` is represented as a local expression and is not recognized.
#[must_use]
pub fn scan_requires(root: &Stat) -> RequireScan {
    struct Requests(RequireScan);
    impl<'ast> Visitor<'ast> for Requests {
        fn visit_expr(&mut self, expr: &'ast Expr) -> WalkControl {
            if let Some((arg, _, location)) = require_call(expr) {
                match arg {
                    Some(Expr::String { value, .. }) => {
                        self.0.static_requests.push(StaticRequireRequest {
                            request: value.clone(),
                            location,
                        });
                    }
                    Some(_) | None => self.0.has_dynamic = true,
                }
            }
            WalkControl::Continue
        }
    }
    let mut requests = Requests(RequireScan::default());
    walk_stat(root, &mut requests);
    requests.0
}

fn require_call(expr: &Expr) -> Option<(Option<&Expr>, SyntaxId, Option<Location>)> {
    let Expr::Call {
        func,
        args,
        location,
        ..
    } = expr
    else {
        return None;
    };
    if !matches!(func.as_ref(), Expr::Global { name, .. } if name.as_str() == "require") {
        return None;
    }
    Some((args.first(), expr.syntax_id(), *location))
}

fn type_annotation_is_any(luau_type: &Type) -> bool {
    match luau_type {
        Type::Reference { prefix, name, .. } => prefix.is_none() && name.as_str() == "any",
        Type::Group { inner, .. } => type_annotation_is_any(inner),
        _ => false,
    }
}
