//! Source-graph frontend: parses modules, tracks require edges and dirty
//! state, and surfaces cycle reports.
//!
//! The require-graph topology (nodes, forward/reverse edges, and the
//! traversals over them) lives in [`crate::graph::RequireGraph`]; this module
//! drives parsing, caching, and resolution on top of it.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    pin::Pin,
    sync::Arc,
};

use ruau_source::{ModuleName, ReadySourceFutureExt, SourceMetadata, SourceRead};

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
use ruau_source::{ModuleId, ReadContext, SourceProvider};
#[cfg(any())]
use ruau_syntax::Position;
use ruau_syntax::{
    Location, Stat,
    parse::{Comment, Config, Error, ParsedModule, SyntaxFlags, parse_module_with_config},
};

use super::{
    Mode, effective_mode,
    graph::{RequireGraph, SourceNode},
    require_tracer::{
        RequireAdmission, RequireTraceResult, trace_requires_async_with_admission,
        trace_requires_ready_with_admission,
    },
    resolve::{
        config::{ModuleConfig, Resolver},
        resolver::{ResolverError, ResolverResult, SourceCode, resolver_error_from_module_source},
    },
};
#[cfg(any())]
use super::{require_tracer::trace_requires, resolve::resolver::FileResolver};
use crate::{GraphLimitError, GraphLimitKind, GraphLimits};

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
    pub root: Arc<Stat>,
    /// Parse errors for the source.
    pub parse_errors: Vec<Error>,
    /// Captured comments.
    pub comments: Vec<Comment>,
    /// Header mode inferred from hot comments.
    pub mode: Option<Mode>,
    /// Effective portable config consumed while parsing this module.
    pub config: ModuleConfig,
}

impl SourceModule {
    /// Returns whether a position falls within a captured comment.
    #[must_use]
    #[cfg(any())]
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
    parse_config: Config,
    /// Parsed source modules.
    source_modules: BTreeMap<ModuleName, SourceModule>,
    /// Exact source observations used to parse the current graph modules.
    source_reads: BTreeMap<ModuleName, Arc<SourceRead>>,
    /// Caller-supplied shared parse products keyed by their root module.
    parsed_modules: BTreeMap<ModuleName, ParsedModule>,
    /// Require-graph topology.
    graph: RequireGraph,
    /// Require traces keyed by module.
    require_traces: BTreeMap<ModuleName, RequireTraceResult>,
    /// Resolver errors surfaced while loading each module.
    resolver_diagnostics: BTreeMap<ModuleName, Vec<ResolverError>>,
    /// Cumulative source graph statistics.
    stats: FrontendStats,
    /// Optional finite traversal limits.
    graph_limits: Option<GraphLimits>,
    /// Per-check finite traversal state.
    graph_limit_state: Option<GraphLimitState>,
}

struct GraphLimitState {
    limits: GraphLimits,
    depths: HashMap<ModuleId, usize>,
    counted_sources: HashSet<ModuleId>,
    source_bytes: usize,
    failure: Option<GraphLimitError>,
}

impl GraphLimitState {
    fn new(root: ModuleId, limits: GraphLimits) -> Self {
        Self {
            limits,
            depths: HashMap::from([(root, 0)]),
            counted_sources: HashSet::new(),
            source_bytes: 0,
            failure: None,
        }
    }

    fn admit_edge(&mut self, requester: &ModuleId, module: &ModuleId) -> bool {
        if self.failure.is_some() {
            return false;
        }
        let depth = self
            .depths
            .get(requester)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        if depth > self.limits.max_require_depth().get() {
            self.failure = Some(GraphLimitError::new(
                GraphLimitKind::RequireDepth,
                self.limits.max_require_depth().get(),
                depth,
                module.clone(),
                Some(requester.clone()),
            ));
            return false;
        }
        self.depths
            .entry(module.clone())
            .and_modify(|known| *known = (*known).min(depth))
            .or_insert(depth);
        let observed = self.depths.len();
        if observed > self.limits.max_modules().get() {
            self.failure = Some(GraphLimitError::new(
                GraphLimitKind::Modules,
                self.limits.max_modules().get(),
                observed,
                module.clone(),
                Some(requester.clone()),
            ));
            return false;
        }
        true
    }

    fn observe_source(&mut self, read: &SourceRead) -> bool {
        if self.failure.is_some() {
            return false;
        }
        let id = read.source().id();
        if !self.counted_sources.insert(id.clone()) {
            return true;
        }
        self.source_bytes = self
            .source_bytes
            .saturating_add(read.source().as_bytes().len());
        if self.source_bytes > self.limits.max_source_bytes().get() {
            self.failure = Some(GraphLimitError::new(
                GraphLimitKind::SourceBytes,
                self.limits.max_source_bytes().get(),
                self.source_bytes,
                id.clone(),
                read.instance_key().requester().cloned(),
            ));
            return false;
        }
        true
    }
}

impl<'resolver> Frontend<'resolver> {
    /// Creates a frontend over the shared async-first module source model.
    ///
    /// Call [`Self::parse_async`] to await source futures. The synchronous
    /// [`Self::parse`] method remains a ready-only bridge for static tools and
    /// reports pending futures as resolver diagnostics.
    #[must_use]
    pub fn new(
        module_source: &'resolver dyn SourceProvider,
        config_resolver: &'resolver dyn Resolver,
    ) -> Self {
        Self {
            source_resolver: FrontendSourceResolver::module_source(module_source),
            config_resolver,
            parse_config: Config::upstream_default(),
            source_modules: BTreeMap::new(),
            source_reads: BTreeMap::new(),
            parsed_modules: BTreeMap::new(),
            graph: RequireGraph::default(),
            require_traces: BTreeMap::new(),
            resolver_diagnostics: BTreeMap::new(),
            stats: FrontendStats::default(),
            graph_limits: Some(GraphLimits::default()),
            graph_limit_state: None,
        }
    }

    /// Creates a frontend over a Roblox-shaped file resolver.
    ///
    /// This is internal development scaffolding for upstream fixture and
    /// expression-resolution tests. Public graph callers should pass
    /// [`SourceProvider`] through [`Self::new`].
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
            parse_config: Config::upstream_default(),
            source_modules: BTreeMap::new(),
            source_reads: BTreeMap::new(),
            parsed_modules: BTreeMap::new(),
            graph: RequireGraph::default(),
            require_traces: BTreeMap::new(),
            resolver_diagnostics: BTreeMap::new(),
            stats: FrontendStats::default(),
            graph_limits: Some(GraphLimits::default()),
            graph_limit_state: None,
        }
    }

    /// Returns cumulative source-frontend statistics.
    ///
    /// Mirrors upstream Frontend behavior; retained for conformance parity
    /// (tests in `src/tests.rs`).
    #[must_use]
    #[cfg(any())]
    pub const fn stats(&self) -> FrontendStats {
        self.stats
    }

    /// Sets the parser configuration for future module refreshes.
    ///
    /// Comment capture is always enabled because header modes and config
    /// interaction depend on parsed hot comments.
    pub fn set_parse_config(&mut self, config: Config) {
        self.parse_config = config;
    }

    pub(crate) fn set_parsed_module(&mut self, name: ModuleName, parsed: ParsedModule) {
        self.parsed_modules.insert(name, parsed);
    }

    /// Sets syntax feature flags for future module refreshes.
    pub fn set_syntax_flags(&mut self, flags: SyntaxFlags) {
        self.parse_config.syntax = flags;
    }

    /// Resets cumulative source-frontend statistics.
    ///
    /// Mirrors upstream Frontend behavior; retained for conformance parity
    /// (tests in `src/tests.rs`).
    #[cfg(any())]
    pub fn clear_stats(&mut self) {
        self.stats = FrontendStats::default();
    }

    pub(crate) fn set_graph_limits(&mut self, limits: Option<GraphLimits>) {
        self.graph_limits = limits;
        self.graph_limit_state = None;
    }

    pub(crate) fn graph_limit_failure(&self) -> Option<&GraphLimitError> {
        self.graph_limit_state
            .as_ref()
            .and_then(|state| state.failure.as_ref())
    }

    /// Parses a root module and all statically reachable modules.
    pub fn parse(&mut self, name: impl Into<ModuleName>) -> ParseGraphResult {
        let root = name.into();
        self.begin_graph_limits(&root);
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
    /// async [`SourceProvider`] reads and resolutions.
    pub async fn parse_async(&mut self, name: impl Into<ModuleName>) -> ParseGraphResult {
        let root = name.into();
        self.begin_graph_limits(&root);
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

    pub(crate) const fn source_reads(&self) -> &BTreeMap<ModuleName, Arc<SourceRead>> {
        &self.source_reads
    }

    /// Iterates parsed source modules by module name.
    #[cfg(any())]
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
    #[cfg(any())]
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
    #[cfg(any())]
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
    #[cfg(any())]
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

    fn begin_graph_limits(&mut self, root: &ModuleName) {
        self.graph_limit_state = self
            .graph_limits
            .map(|limits| GraphLimitState::new(ModuleId::from(root), limits));
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
        if let Some(read) = self.source_reads.get(name)
            && !self
                .graph_limit_state
                .as_mut()
                .is_none_or(|state| state.observe_source(read))
        {
            return;
        }
        let dependencies = self
            .graph
            .node(name)
            .map(|node| node.requires().clone())
            .unwrap_or_default();
        for dependency in dependencies {
            if !self.graph_limit_state.as_mut().is_none_or(|state| {
                state.admit_edge(&ModuleId::from(name), &ModuleId::from(&dependency))
            }) {
                return;
            }
            self.parse_graph_node(&dependency, build_queue, cycle_detected, marks);
            self.graph.link_dependent(&dependency, name);
        }

        marks.insert(name.clone(), VisitMark::Permanent);
        build_queue.push(name.clone());
    }

    /// Parses one graph node recursively through the async source path.
    /// The boxed recursion is `Send` on native targets and drops the bound on
    /// wasm32, matching `SourceFuture`.
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
            if let Some(read) = self.source_reads.get(name)
                && !self
                    .graph_limit_state
                    .as_mut()
                    .is_none_or(|state| state.observe_source(read))
            {
                return;
            }
            let dependencies = self
                .graph
                .node(name)
                .map(|node| node.requires().clone())
                .unwrap_or_default();
            for dependency in dependencies {
                if !self.graph_limit_state.as_mut().is_none_or(|state| {
                    state.admit_edge(&ModuleId::from(name), &ModuleId::from(&dependency))
                }) {
                    return;
                }
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
        if self.graph_limit_failure().is_some() {
            return false;
        }
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
        let Some(source_module) = self.prepare_source_module(name, source) else {
            return false;
        };
        let require_trace = {
            let state = &mut self.graph_limit_state;
            self.source_resolver.trace_requires(
                &source_module.root,
                &source_module.name,
                &mut |requester, module| {
                    state
                        .as_mut()
                        .is_none_or(|state| state.admit_edge(requester, module))
                },
            )
        };
        self.install_source_module(source_module, require_trace);

        true
    }

    /// Refreshes one source node through the async source path if dirty.
    async fn refresh_source_node_async(&mut self, name: &ModuleName) -> bool {
        if self.graph_limit_failure().is_some() {
            return false;
        }
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
        let Some(source_module) = self.prepare_source_module(name, source) else {
            return false;
        };
        let require_trace = {
            let state = &mut self.graph_limit_state;
            self.source_resolver
                .trace_requires_async(
                    &source_module.root,
                    &source_module.name,
                    &mut |requester, module| {
                        state
                            .as_mut()
                            .is_none_or(|state| state.admit_edge(requester, module))
                    },
                )
                .await
        };
        self.install_source_module(source_module, require_trace);

        true
    }

    /// Applies the source observation and parser pipeline shared by ready and
    /// async source reads.
    fn prepare_source_module(
        &mut self,
        name: ModuleName,
        source: FrontendSource,
    ) -> Option<SourceModule> {
        if let Some(observation) = &source.observation
            && !self
                .graph_limit_state
                .as_mut()
                .is_none_or(|state| state.observe_source(observation))
        {
            self.remove_source_node(&name);
            return None;
        }
        match source.observation {
            Some(observation) => {
                self.source_reads
                    .insert(name.clone(), Arc::new(observation));
            }
            None => {
                self.source_reads.remove(&name);
            }
        }

        let config = match self.config_resolver.config_for_module(&name) {
            Ok(config) => config,
            Err(diagnostic) => {
                self.resolver_errors_mut(&name).push(diagnostic);
                ModuleConfig::default()
            }
        };
        let source_module = self.parse_source_module(name, &source.code.source, config);
        self.stats.files += 1;
        Some(source_module)
    }

    /// Installs one traced source module into every frontend cache.
    fn install_source_module(
        &mut self,
        source_module: SourceModule,
        mut require_trace: RequireTraceResult,
    ) {
        self.graph.unlink_forward_edges(&source_module.name);
        let source_node = self.source_node_from_trace(&source_module.name, &require_trace);
        let diagnostics = std::mem::take(&mut require_trace.diagnostics);
        self.resolver_errors_mut(&source_module.name)
            .extend(diagnostics);
        let name = source_module.name.clone();
        self.require_traces.insert(name.clone(), require_trace);
        self.graph.insert(name.clone(), source_node);
        self.source_modules.insert(name, source_module);
    }

    /// Parses source text into a source module.
    fn parse_source_module(
        &self,
        name: ModuleName,
        source: &str,
        config: ModuleConfig,
    ) -> SourceModule {
        let mut parse_config = self.parse_config;
        parse_config.capture_comments = true;
        let parsed = self
            .parsed_modules
            .get(&name)
            .filter(|parsed| {
                parsed.source().as_ref() == source.as_bytes()
                    && parsed.config().ast_compatible_with(parse_config)
                    && parsed.config().capture_comments
            })
            .cloned()
            .unwrap_or_else(|| parse_module_with_config(source, &parse_config));
        let mode = effective_mode(parsed.errors(), parsed.hot_comments(), config.mode());
        let metadata = self.source_resolver.module_metadata(&name);
        SourceModule {
            name,
            human_readable_name: metadata.display_name,
            environment_name: metadata.environment,
            cyclic: false,
            root: Arc::clone(parsed.root()),
            parse_errors: parsed.errors().to_vec(),
            comments: parsed.comments().to_vec(),
            mode,
            config,
        }
    }

    /// Removes one source node and its cached state.
    fn remove_source_node(&mut self, name: &ModuleName) {
        self.graph.remove(name);
        self.source_modules.remove(name);
        self.source_reads.remove(name);
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
    SourceProvider(&'resolver dyn SourceProvider),
    #[cfg(any())]
    FileResolver(&'resolver dyn FileResolver),
}

struct FrontendSource {
    code: SourceCode,
    observation: Option<SourceRead>,
}

impl<'resolver> FrontendSourceResolver<'resolver> {
    fn module_source(module_source: &'resolver dyn SourceProvider) -> Self {
        Self::SourceProvider(module_source)
    }

    #[cfg(any())]
    fn file_resolver(file_resolver: &'resolver dyn FileResolver) -> Self {
        Self::FileResolver(file_resolver)
    }

    fn read_source(&self, name: &ModuleName) -> ResolverResult<FrontendSource> {
        match self {
            Self::SourceProvider(source) => read_module_source_ready(*source, name),
            #[cfg(any())]
            Self::FileResolver(resolver) => resolver.read_source(name).map(|code| FrontendSource {
                code,
                observation: None,
            }),
        }
    }

    async fn read_source_async(&self, name: &ModuleName) -> ResolverResult<FrontendSource> {
        match self {
            Self::SourceProvider(source) => read_module_source_async(*source, name).await,
            #[cfg(any())]
            Self::FileResolver(resolver) => resolver.read_source(name).map(|code| FrontendSource {
                code,
                observation: None,
            }),
        }
    }

    fn trace_requires(
        &self,
        root: &Stat,
        current_module_name: &ModuleName,
        admit: &mut RequireAdmission<'_>,
    ) -> RequireTraceResult {
        match self {
            Self::SourceProvider(source) => {
                trace_requires_ready_with_admission(*source, root, current_module_name, admit)
            }
            #[cfg(any())]
            Self::FileResolver(resolver) => trace_requires(*resolver, root, current_module_name),
        }
    }

    async fn trace_requires_async(
        &self,
        root: &Stat,
        current_module_name: &ModuleName,
        admit: &mut RequireAdmission<'_>,
    ) -> RequireTraceResult {
        match self {
            Self::SourceProvider(source) => {
                trace_requires_async_with_admission(*source, root, current_module_name, admit).await
            }
            #[cfg(any())]
            Self::FileResolver(resolver) => trace_requires(*resolver, root, current_module_name),
        }
    }

    fn module_metadata(&self, name: &ModuleName) -> SourceMetadata {
        match self {
            Self::SourceProvider(source) => source.metadata(&ModuleId::from(name)),
            #[cfg(any())]
            Self::FileResolver(resolver) => resolver.module_metadata(name),
        }
    }
}

fn read_module_source_ready(
    source: &dyn SourceProvider,
    name: &ModuleName,
) -> ResolverResult<FrontendSource> {
    let id = ModuleId::from(name);
    let observation = (source.read_observation(ReadContext::new(&id)))
        .ready_only("reading module source")
        .map_err(|error| resolver_error_from_module_source(error, Some(name.clone())))?;
    String::from_utf8(observation.source().as_bytes().to_vec())
        .map(|source| FrontendSource {
            code: SourceCode::new(source),
            observation: Some(observation),
        })
        .map_err(|error| ResolverError::SourceProvider {
            module: Some(name.clone()),
            detail: format!("source is not UTF-8: {error}"),
        })
}

async fn read_module_source_async(
    source: &dyn SourceProvider,
    name: &ModuleName,
) -> ResolverResult<FrontendSource> {
    let id = ModuleId::from(name);
    let observation = source
        .read_observation(ReadContext::new(&id))
        .await
        .map_err(|error| resolver_error_from_module_source(error, Some(name.clone())))?;
    String::from_utf8(observation.source().as_bytes().to_vec())
        .map(|source| FrontendSource {
            code: SourceCode::new(source),
            observation: Some(observation),
        })
        .map_err(|error| ResolverError::SourceProvider {
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

#[cfg(any())]
mod limit_tests {
    use std::num::NonZeroUsize;

    use ruau_source::{InstanceKey, Source};

    use super::*;

    #[test]
    fn requester_specific_observations_count_one_module_id_once() {
        let id = ModuleId::new("shared");
        let bytes = b"return 1";
        let limits = GraphLimits::new(
            NonZeroUsize::new(8).expect("non-zero"),
            NonZeroUsize::new(8).expect("non-zero"),
            NonZeroUsize::new(bytes.len()).expect("non-zero"),
        );
        let mut state = GraphLimitState::new(ModuleId::new("root"), limits);
        for requester in ["first", "second"] {
            let read = SourceRead::new(
                Source::bytes(id.clone(), bytes),
                InstanceKey::per_requester(id.clone(), ModuleId::new(requester)),
                0,
                None,
            );
            assert!(state.observe_source(&read));
        }
        assert_eq!(state.source_bytes, bytes.len());
        assert!(state.failure.is_none());
    }
}
