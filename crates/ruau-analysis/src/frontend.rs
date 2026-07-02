//! Source-graph frontend: parses modules, tracks require edges and dirty
//! state, and surfaces cycle reports.
//!
//! The require-graph topology (nodes, forward/reverse edges, and the
//! traversals over them) lives in [`crate::graph::RequireGraph`]; this module
//! drives parsing, caching, and resolution on top of it.

use std::{collections::BTreeMap, pin::Pin};

use ruau_source::{ModuleName, SourceMetadata, poll_ready_once};

/// `Future + Send` on native targets, plain `Future` on wasm32 (where the
/// executor is single-threaded and JS-backed futures are `!Send`).
#[cfg(not(target_arch = "wasm32"))]
trait MaybeSendFuture: std::future::Future + Send {}
#[cfg(not(target_arch = "wasm32"))]
impl<F: std::future::Future + Send> MaybeSendFuture for F {}
#[cfg(target_arch = "wasm32")]
trait MaybeSendFuture: std::future::Future {}
#[cfg(target_arch = "wasm32")]
impl<F: std::future::Future> MaybeSendFuture for F {}
use ruau_ast::{
    Location, Position,
    parse::{Comment, Error, ParseConfig, SyntaxFlags, parse_file_with},
    syntax::Stat,
};
use ruau_source::{ModuleId, ModuleSource, ReadRequest};

use crate::{
    graph::{RequireGraph, SourceNode},
    require_tracer::{RequireTraceResult, trace_requires_async, trace_requires_ready},
    resolve::{
        AnalysisMode,
        config::{AnalysisConfig, Resolver},
        effective_mode,
        resolver::{ResolverError, ResolverResult, SourceCode, resolver_error_from_module_source},
    },
};
#[cfg(any())]
use crate::{require_tracer::trace_requires, resolve::resolver::FileResolver};

/// Parsed source plus metadata used by analysis phases above the parser.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceModule {
    /// Resolved module name.
    pub name: ModuleName,
    /// Human-readable name for diagnostics.
    pub human_readable_name: String,
    /// Optional environment name supplied by the source resolver.
    pub environment_name: Option<String>,
    /// Whether this source participates in a require cycle.
    pub cyclic: bool,
    /// Parsed root block. If parsing fails before producing a root, this is an
    /// empty block.
    pub root: Stat,
    /// Parse errors for the source.
    pub parse_errors: Vec<Error>,
    /// Captured comments.
    pub comments: Vec<Comment>,
    /// Header mode inferred from hot comments.
    pub mode: Option<AnalysisMode>,
    /// Effective portable config consumed while parsing this module.
    pub config: AnalysisConfig,
}

impl SourceModule {
    /// Returns whether a position falls within a captured comment.
    #[must_use]
    pub fn is_within_comment(&self, position: Position) -> bool {
        self.comments
            .iter()
            .any(|comment| comment.location.contains(position))
    }
}

/// One static require cycle discovered from a source node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequireCycle {
    /// Location of the require edge in the starting module.
    pub location: Option<Location>,
    /// Dependency path that reaches the starting module.
    pub path: Vec<ModuleName>,
}

/// Result of parsing a module graph.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseGraphResult {
    /// Root requested by the caller.
    pub root: ModuleName,
    /// Modules reached and processed in dependency-first order.
    pub build_queue: Vec<ModuleName>,
    /// Whether the parsed graph contains a cycle.
    pub cycle_detected: bool,
}

/// Cumulative source-frontend statistics.
///
/// Mirrors upstream Frontend behavior; retained for conformance parity
/// (tests in `src/tests.rs`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FrontendStats {
    /// Number of source files parsed since the last explicit stats reset.
    pub files: usize,
}

/// Frontend source graph state above type inference.
pub struct Frontend<'resolver> {
    /// Source resolver facade.
    source_resolver: FrontendSourceResolver<'resolver>,
    /// Effective config resolver.
    config_resolver: &'resolver dyn Resolver,
    /// Parser configuration used when refreshing modules.
    parse_config: ParseConfig,
    /// Parsed source modules.
    source_modules: BTreeMap<ModuleName, SourceModule>,
    /// Require-graph topology.
    graph: RequireGraph,
    /// Require traces keyed by module.
    require_traces: BTreeMap<ModuleName, RequireTraceResult>,
    /// Resolver errors surfaced while loading each module.
    resolver_diagnostics: BTreeMap<ModuleName, Vec<ResolverError>>,
    /// Cumulative source graph statistics.
    stats: FrontendStats,
}

impl<'resolver> Frontend<'resolver> {
    /// Creates a frontend over the shared async-first module source model.
    ///
    /// Call [`Self::parse_async`] to await source futures. The synchronous
    /// [`Self::parse`] method remains a ready-only bridge for static tools and
    /// reports pending futures as resolver diagnostics.
    #[must_use]
    pub fn new(
        module_source: &'resolver dyn ModuleSource,
        config_resolver: &'resolver dyn Resolver,
    ) -> Self {
        Self {
            source_resolver: FrontendSourceResolver::module_source(module_source),
            config_resolver,
            parse_config: ParseConfig::upstream_default(),
            source_modules: BTreeMap::new(),
            graph: RequireGraph::default(),
            require_traces: BTreeMap::new(),
            resolver_diagnostics: BTreeMap::new(),
            stats: FrontendStats::default(),
        }
    }

    /// Creates a frontend over a Roblox-shaped file resolver.
    ///
    /// This is internal development scaffolding for upstream fixture and
    /// expression-resolution tests. Public graph callers should pass
    /// [`ModuleSource`] through [`Self::new`].
    #[doc(hidden)]
    #[must_use]
    #[cfg(any())]
    pub fn with_file_resolver(
        file_resolver: &'resolver dyn FileResolver,
        config_resolver: &'resolver dyn Resolver,
    ) -> Self {
        Self {
            source_resolver: FrontendSourceResolver::file_resolver(file_resolver),
            config_resolver,
            parse_config: ParseConfig::upstream_default(),
            source_modules: BTreeMap::new(),
            graph: RequireGraph::default(),
            require_traces: BTreeMap::new(),
            resolver_diagnostics: BTreeMap::new(),
            stats: FrontendStats::default(),
        }
    }

    /// Returns cumulative source-frontend statistics.
    ///
    /// Mirrors upstream Frontend behavior; retained for conformance parity
    /// (tests in `src/tests.rs`).
    #[must_use]
    pub const fn stats(&self) -> FrontendStats {
        self.stats
    }

    /// Sets the parser configuration for future module refreshes.
    ///
    /// Comment capture is always enabled because header modes and config
    /// interaction depend on parsed hot comments.
    pub fn set_parse_config(&mut self, config: ParseConfig) {
        self.parse_config = config;
    }

    /// Sets syntax feature flags for future module refreshes.
    pub fn set_syntax_flags(&mut self, flags: SyntaxFlags) {
        self.parse_config.syntax = flags;
    }

    /// Resets cumulative source-frontend statistics.
    ///
    /// Mirrors upstream Frontend behavior; retained for conformance parity
    /// (tests in `src/tests.rs`).
    pub fn clear_stats(&mut self) {
        self.stats = FrontendStats::default();
    }

    /// Parses a root module and all statically reachable modules.
    pub fn parse(&mut self, name: impl Into<ModuleName>) -> ParseGraphResult {
        let root = name.into();
        let mut build_queue = Vec::new();
        let mut cycle_detected = false;
        let mut marks = BTreeMap::new();
        self.parse_graph_node(&root, &mut build_queue, &mut cycle_detected, &mut marks);
        self.update_cycle_flags();
        ParseGraphResult {
            root,
            build_queue,
            cycle_detected,
        }
    }

    /// Parses a root module and all statically reachable modules, awaiting
    /// async [`ModuleSource`] reads and resolutions.
    pub async fn parse_async(&mut self, name: impl Into<ModuleName>) -> ParseGraphResult {
        let root = name.into();
        let mut build_queue = Vec::new();
        let mut cycle_detected = false;
        let mut marks = BTreeMap::new();
        self.parse_graph_node_async(&root, &mut build_queue, &mut cycle_detected, &mut marks)
            .await;
        self.update_cycle_flags();
        ParseGraphResult {
            root,
            build_queue,
            cycle_detected,
        }
    }

    /// Iterates source graph nodes by module name.
    pub fn iter_source_nodes(&self) -> impl Iterator<Item = (&ModuleName, &SourceNode)> {
        self.graph.iter()
    }

    /// Iterates parsed source modules by module name.
    pub fn iter_source_modules(&self) -> impl Iterator<Item = (&ModuleName, &SourceModule)> {
        self.source_modules.iter()
    }

    /// Returns one source node.
    #[must_use]
    pub fn source_node(&self, name: &ModuleName) -> Option<&SourceNode> {
        self.graph.node(name)
    }

    /// Returns one parsed source module.
    #[must_use]
    pub fn source_module(&self, name: &ModuleName) -> Option<&SourceModule> {
        self.source_modules.get(name)
    }

    /// Returns one module's require trace.
    #[must_use]
    pub fn require_trace(&self, name: &ModuleName) -> Option<&RequireTraceResult> {
        self.require_traces.get(name)
    }

    /// Returns resolver errors surfaced while loading `name`.
    #[must_use]
    pub fn resolver_diagnostics(&self, name: &ModuleName) -> &[ResolverError] {
        self.resolver_diagnostics
            .get(name)
            .map_or(&[], Vec::as_slice)
    }

    /// Returns the human-readable display name for `name`.
    #[must_use]
    pub fn module_display_name(&self, name: &ModuleName) -> String {
        self.source_modules
            .get(name)
            .map(|source| source.human_readable_name.clone())
            .unwrap_or_else(|| self.source_resolver.module_metadata(name).display_name)
    }

    /// Reparses `name` if its cached source is stale, leaving it clean.
    ///
    /// Mirrors upstream Frontend behavior; retained for conformance parity
    /// (tests in `src/tests.rs`).
    pub fn refresh(&mut self, name: impl Into<ModuleName>) {
        let name = name.into();
        if !self.graph.is_clean(&name) {
            self.parse(name);
        }
    }

    /// Returns whether the parsed source for `name` is stale.
    ///
    /// Mirrors upstream Frontend behavior; retained for conformance parity
    /// (tests in `src/tests.rs`).
    #[must_use]
    pub fn is_dirty(&self, name: &ModuleName) -> bool {
        !self.graph.is_clean(name)
    }

    /// Marks a module and all known dependents dirty, returning the names newly
    /// marked by this call.
    pub fn mark_dirty(&mut self, name: impl Into<ModuleName>) -> Vec<ModuleName> {
        self.graph.mark_dirty_subtree(&name.into())
    }

    /// Traverses known dependents, including the starting node.
    ///
    /// `descend` decides whether to descend into a node's dependents; returning
    /// `false` prunes that subtree.
    ///
    /// Mirrors upstream Frontend behavior; retained for conformance parity
    /// (tests in `src/tests.rs`).
    pub fn traverse_dependents(
        &self,
        name: impl Into<ModuleName>,
        descend: impl FnMut(&ModuleName) -> bool,
    ) {
        self.graph.traverse_dependents(&name.into(), descend);
    }

    /// Returns require cycles starting from `name`.
    #[must_use]
    pub fn require_cycles(&self, name: &ModuleName) -> Vec<RequireCycle> {
        let Some(trace) = self.require_traces.get(name) else {
            return Vec::new();
        };

        trace
            .require_list
            .iter()
            .filter_map(|require| {
                let path = self.graph.cycle_path(&require.module, name)?;
                Some(RequireCycle {
                    location: require.location,
                    path,
                })
            })
            .collect()
    }

    /// Clears all cached frontend state. Cumulative [`stats`](Self::stats) are
    /// retained; use [`clear_stats`](Self::clear_stats) to reset them.
    pub fn clear_cache(&mut self) {
        self.source_modules.clear();
        self.graph.clear();
        self.require_traces.clear();
        self.resolver_diagnostics.clear();
    }

    /// Parses one graph node recursively.
    fn parse_graph_node(
        &mut self,
        name: &ModuleName,
        build_queue: &mut Vec<ModuleName>,
        cycle_detected: &mut bool,
        marks: &mut BTreeMap<ModuleName, VisitMark>,
    ) {
        if !self.refresh_source_node(name) {
            return;
        }

        match marks.get(name) {
            Some(VisitMark::Temporary) => {
                *cycle_detected = true;
                return;
            }
            Some(VisitMark::Permanent) => return,
            None => {}
        }

        marks.insert(name.clone(), VisitMark::Temporary);
        let dependencies = self
            .graph
            .node(name)
            .map(|node| node.requires().clone())
            .unwrap_or_default();
        for dependency in dependencies {
            self.parse_graph_node(&dependency, build_queue, cycle_detected, marks);
            self.graph.link_dependent(&dependency, name);
        }

        marks.insert(name.clone(), VisitMark::Permanent);
        build_queue.push(name.clone());
    }

    /// Parses one graph node recursively through the async source path.
    /// The boxed recursion is `Send` on native targets and drops the bound on
    /// wasm32, matching `ModuleSourceFuture`.
    fn parse_graph_node_async<'a>(
        &'a mut self,
        name: &'a ModuleName,
        build_queue: &'a mut Vec<ModuleName>,
        cycle_detected: &'a mut bool,
        marks: &'a mut BTreeMap<ModuleName, VisitMark>,
    ) -> Pin<Box<dyn MaybeSendFuture<Output = ()> + 'a>> {
        Box::pin(async move {
            if !self.refresh_source_node_async(name).await {
                return;
            }

            match marks.get(name) {
                Some(VisitMark::Temporary) => {
                    *cycle_detected = true;
                    return;
                }
                Some(VisitMark::Permanent) => return,
                None => {}
            }

            marks.insert(name.clone(), VisitMark::Temporary);
            let dependencies = self
                .graph
                .node(name)
                .map(|node| node.requires().clone())
                .unwrap_or_default();
            for dependency in dependencies {
                self.parse_graph_node_async(&dependency, build_queue, cycle_detected, marks)
                    .await;
                self.graph.link_dependent(&dependency, name);
            }

            marks.insert(name.clone(), VisitMark::Permanent);
            build_queue.push(name.clone());
        })
    }

    /// Refreshes one source node if dirty.
    fn refresh_source_node(&mut self, name: &ModuleName) -> bool {
        if self.graph.is_clean(name) {
            return true;
        }

        let name = name.clone();
        self.resolver_diagnostics.remove(&name);

        let source = match self.source_resolver.read_source(&name) {
            Ok(source) => source,
            Err(diagnostic) => {
                self.remove_source_node(&name);
                self.resolver_errors_mut(&name).push(diagnostic);
                return false;
            }
        };

        let config = match self.config_resolver.config_for_module(&name) {
            Ok(config) => config,
            Err(diagnostic) => {
                self.resolver_errors_mut(&name).push(diagnostic);
                AnalysisConfig::default()
            }
        };
        let source_module = self.parse_source_module(name, &source.source, config);
        self.stats.files += 1;
        let mut require_trace = self
            .source_resolver
            .trace_requires(&source_module.root, &source_module.name);
        self.graph.unlink_forward_edges(&source_module.name);
        let source_node = self.source_node_from_trace(&source_module.name, &require_trace);
        let diagnostics = std::mem::take(&mut require_trace.diagnostics);
        self.resolver_errors_mut(&source_module.name)
            .extend(diagnostics);
        let name = source_module.name.clone();
        self.require_traces.insert(name.clone(), require_trace);
        self.graph.insert(name.clone(), source_node);
        self.source_modules.insert(name, source_module);

        true
    }

    /// Refreshes one source node through the async source path if dirty.
    async fn refresh_source_node_async(&mut self, name: &ModuleName) -> bool {
        if self.graph.is_clean(name) {
            return true;
        }

        let name = name.clone();
        self.resolver_diagnostics.remove(&name);

        let source = match self.source_resolver.read_source_async(&name).await {
            Ok(source) => source,
            Err(diagnostic) => {
                self.remove_source_node(&name);
                self.resolver_errors_mut(&name).push(diagnostic);
                return false;
            }
        };

        let config = match self.config_resolver.config_for_module(&name) {
            Ok(config) => config,
            Err(diagnostic) => {
                self.resolver_errors_mut(&name).push(diagnostic);
                AnalysisConfig::default()
            }
        };
        let source_module = self.parse_source_module(name, &source.source, config);
        self.stats.files += 1;
        let mut require_trace = self
            .source_resolver
            .trace_requires_async(&source_module.root, &source_module.name)
            .await;
        self.graph.unlink_forward_edges(&source_module.name);
        let source_node = self.source_node_from_trace(&source_module.name, &require_trace);
        let diagnostics = std::mem::take(&mut require_trace.diagnostics);
        self.resolver_errors_mut(&source_module.name)
            .extend(diagnostics);
        let name = source_module.name.clone();
        self.require_traces.insert(name.clone(), require_trace);
        self.graph.insert(name.clone(), source_node);
        self.source_modules.insert(name, source_module);

        true
    }

    /// Parses source text into a source module.
    fn parse_source_module(
        &self,
        name: ModuleName,
        source: &str,
        config: AnalysisConfig,
    ) -> SourceModule {
        let mut parse_config = self.parse_config;
        parse_config.capture_comments = true;
        let result = parse_file_with(source, &parse_config);
        let root = result.root;
        let mode = effective_mode(&result.errors, &result.hot_comments, config.mode());
        let metadata = self.source_resolver.module_metadata(&name);
        SourceModule {
            name,
            human_readable_name: metadata.display_name,
            environment_name: metadata.environment,
            cyclic: false,
            root,
            parse_errors: result.errors,
            comments: result.comments,
            mode,
            config,
        }
    }

    /// Removes one source node and its cached state.
    fn remove_source_node(&mut self, name: &ModuleName) {
        self.graph.remove(name);
        self.source_modules.remove(name);
        self.require_traces.remove(name);
    }

    fn resolver_errors_mut(&mut self, name: &ModuleName) -> &mut Vec<ResolverError> {
        self.resolver_diagnostics.entry(name.clone()).or_default()
    }

    /// Builds a graph node from a fresh require trace, preserving reverse edges.
    fn source_node_from_trace(
        &self,
        name: &ModuleName,
        require_trace: &RequireTraceResult,
    ) -> SourceNode {
        SourceNode::new(
            require_trace.required_modules().cloned().collect(),
            self.graph
                .node(name)
                .map(|node| node.dependents().clone())
                .unwrap_or_default(),
        )
    }

    /// Updates cached cyclic flags from current graph cycles.
    fn update_cycle_flags(&mut self) {
        let cyclic = self.graph.cyclic_modules();
        for source_module in self.source_modules.values_mut() {
            source_module.cyclic = cyclic.contains(&source_module.name);
        }
    }
}

enum FrontendSourceResolver<'resolver> {
    ModuleSource(&'resolver dyn ModuleSource),
    #[cfg(any())]
    FileResolver(&'resolver dyn FileResolver),
}

impl<'resolver> FrontendSourceResolver<'resolver> {
    fn module_source(module_source: &'resolver dyn ModuleSource) -> Self {
        Self::ModuleSource(module_source)
    }

    #[cfg(any())]
    fn file_resolver(file_resolver: &'resolver dyn FileResolver) -> Self {
        Self::FileResolver(file_resolver)
    }

    fn read_source(&self, name: &ModuleName) -> ResolverResult<SourceCode> {
        match self {
            Self::ModuleSource(source) => read_module_source_ready(*source, name),
            #[cfg(any())]
            Self::FileResolver(resolver) => resolver.read_source(name),
        }
    }

    async fn read_source_async(&self, name: &ModuleName) -> ResolverResult<SourceCode> {
        match self {
            Self::ModuleSource(source) => read_module_source_async(*source, name).await,
            #[cfg(any())]
            Self::FileResolver(resolver) => resolver.read_source(name),
        }
    }

    fn trace_requires(&self, root: &Stat, current_module_name: &ModuleName) -> RequireTraceResult {
        match self {
            Self::ModuleSource(source) => trace_requires_ready(*source, root, current_module_name),
            #[cfg(any())]
            Self::FileResolver(resolver) => trace_requires(*resolver, root, current_module_name),
        }
    }

    async fn trace_requires_async(
        &self,
        root: &Stat,
        current_module_name: &ModuleName,
    ) -> RequireTraceResult {
        match self {
            Self::ModuleSource(source) => {
                trace_requires_async(*source, root, current_module_name).await
            }
            #[cfg(any())]
            Self::FileResolver(resolver) => trace_requires(*resolver, root, current_module_name),
        }
    }

    fn module_metadata(&self, name: &ModuleName) -> SourceMetadata {
        match self {
            Self::ModuleSource(source) => source.metadata(&ModuleId::from(name)),
            #[cfg(any())]
            Self::FileResolver(resolver) => resolver.module_metadata(name),
        }
    }
}

fn read_module_source_ready(
    source: &dyn ModuleSource,
    name: &ModuleName,
) -> ResolverResult<SourceCode> {
    let id = ModuleId::from(name);
    let bytes = poll_ready_once(
        source.read_request(ReadRequest::new(&id)),
        "reading module source",
    )
    .map_err(|error| resolver_error_from_module_source(error, Some(name.clone())))?;
    String::from_utf8(bytes)
        .map(SourceCode::new)
        .map_err(|error| ResolverError::ModuleSource {
            module: Some(name.clone()),
            detail: format!("source is not UTF-8: {error}"),
        })
}

async fn read_module_source_async(
    source: &dyn ModuleSource,
    name: &ModuleName,
) -> ResolverResult<SourceCode> {
    let id = ModuleId::from(name);
    let bytes = source
        .read_request(ReadRequest::new(&id))
        .await
        .map_err(|error| resolver_error_from_module_source(error, Some(name.clone())))?;
    String::from_utf8(bytes)
        .map(SourceCode::new)
        .map_err(|error| ResolverError::ModuleSource {
            module: Some(name.clone()),
            detail: format!("source is not UTF-8: {error}"),
        })
}

/// DFS mark for source graph parsing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VisitMark {
    /// On the active DFS path.
    Temporary,
    /// Fully processed.
    Permanent,
}
