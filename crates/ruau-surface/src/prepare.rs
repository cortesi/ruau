//! Checked source preparation: the policy-gated check-then-compile pipeline
//! producing [`PreparedScript`] artifacts.

use std::{error::Error, fmt};

use ruau_bytecode::{BytecodeChunk, CompileError, CompileOptions};
use ruau_source::Source;
use ruau_typecheck::{checker::Config, diagnostics::Diagnostics};
use ruau_vm::{
    CallOptions, ExecError, LoadError, LoadedModule, MarshaledValue, RuntimeCapabilities, Vm,
};

use crate::Surface;

/// Diagnostic gate used by [`Surface::prepare_with_options`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PrepareDiagnosticPolicy {
    /// Reject error-severity diagnostics and preserve warning diagnostics.
    #[default]
    RejectErrors,
    /// Reject any diagnostic, including warnings.
    RejectIssues,
    /// Compile even when checking produced diagnostics.
    AllowDiagnostics,
}

impl PrepareDiagnosticPolicy {
    fn accepts(self, diagnostics: &Diagnostics) -> bool {
        match self {
            Self::RejectErrors => !diagnostics.has_errors(),
            Self::RejectIssues => !diagnostics.has_issues(),
            Self::AllowDiagnostics => true,
        }
    }
}

impl fmt::Display for PrepareDiagnosticPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RejectErrors => formatter.write_str("reject errors"),
            Self::RejectIssues => formatter.write_str("reject diagnostics"),
            Self::AllowDiagnostics => formatter.write_str("allow diagnostics"),
        }
    }
}

/// Configuration for checked source preparation.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct PrepareOptions {
    diagnostic_policy: PrepareDiagnosticPolicy,
    check_config: Config,
    compile_options: CompileOptions,
}

impl PrepareOptions {
    /// Creates default preparation options.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the diagnostic policy.
    #[must_use]
    pub const fn diagnostic_policy(&self) -> PrepareDiagnosticPolicy {
        self.diagnostic_policy
    }

    /// Returns the checker configuration.
    #[must_use]
    pub const fn check_config(&self) -> &Config {
        &self.check_config
    }

    /// Returns the public VM compile policy.
    #[must_use]
    pub const fn compile_options(&self) -> &CompileOptions {
        &self.compile_options
    }

    /// Replaces the diagnostic policy.
    #[must_use]
    pub const fn with_diagnostic_policy(mut self, policy: PrepareDiagnosticPolicy) -> Self {
        self.diagnostic_policy = policy;
        self
    }

    /// Rejects error-severity diagnostics and preserves warnings.
    #[must_use]
    pub const fn reject_errors(self) -> Self {
        self.with_diagnostic_policy(PrepareDiagnosticPolicy::RejectErrors)
    }

    /// Rejects any diagnostic, including warnings.
    #[must_use]
    pub const fn reject_issues(self) -> Self {
        self.with_diagnostic_policy(PrepareDiagnosticPolicy::RejectIssues)
    }

    /// Compiles even when checking produced diagnostics.
    #[must_use]
    pub const fn allow_diagnostics(self) -> Self {
        self.with_diagnostic_policy(PrepareDiagnosticPolicy::AllowDiagnostics)
    }

    /// Replaces the checker configuration.
    ///
    /// If the config does not force a source mode, the surface analysis mode is
    /// still applied before checking.
    #[must_use]
    pub fn with_check_config(mut self, config: Config) -> Self {
        self.check_config = config;
        self
    }

    /// Replaces the public VM compile policy.
    #[must_use]
    pub fn with_compile_options(mut self, options: CompileOptions) -> Self {
        self.compile_options = options;
        self
    }
}

/// A checked and compiled source artifact ready to load into a matching VM.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedScript {
    source: Source,
    diagnostics: Diagnostics,
    chunk: BytecodeChunk,
    runtime_capabilities: RuntimeCapabilities,
}

impl PreparedScript {
    /// Returns the source identity and bytes used for checking and compilation.
    #[must_use]
    pub const fn source(&self) -> &Source {
        &self.source
    }

    /// Returns diagnostics produced during checking.
    #[must_use]
    pub const fn diagnostics(&self) -> &Diagnostics {
        &self.diagnostics
    }

    /// Returns the compiled bytecode chunk.
    #[must_use]
    pub const fn chunk(&self) -> &BytecodeChunk {
        &self.chunk
    }

    /// Returns the runtime capabilities used for compilation.
    #[must_use]
    pub const fn runtime_capabilities(&self) -> &RuntimeCapabilities {
        &self.runtime_capabilities
    }

    /// Returns the Lua chunk name bytes for loading this script.
    #[must_use]
    pub fn load_name(&self) -> Vec<u8> {
        self.source.load_name()
    }

    /// Loads this prepared source into `vm`, preserving both its traceback
    /// load name and its module requester identity.
    ///
    /// # Errors
    /// Returns [`LoadError`] when the prepared chunk cannot be instantiated in
    /// the VM.
    pub fn load_in(&self, vm: &mut Vm) -> Result<LoadedModule, LoadError> {
        let load_name = self.source.load_name();
        vm.load_named_module(&self.chunk, self.source.id().clone(), &load_name)
    }

    /// Loads and executes this prepared source in `vm` with empty call options.
    ///
    /// # Errors
    /// Returns [`PreparedRunError`] when loading or execution fails.
    pub fn run_in(&self, vm: &mut Vm) -> Result<Vec<MarshaledValue>, PreparedRunError> {
        self.run_in_with_options(vm, CallOptions::new())
    }

    /// Loads and executes this prepared source in `vm` with explicit call
    /// options.
    ///
    /// # Errors
    /// Returns [`PreparedRunError`] when loading or execution fails.
    pub fn run_in_with_options(
        &self,
        vm: &mut Vm,
        options: CallOptions,
    ) -> Result<Vec<MarshaledValue>, PreparedRunError> {
        let module = self.load_in(vm).map_err(PreparedRunError::Load)?;
        let result = vm.exec(&module, options).map_err(PreparedRunError::Exec);
        vm.unload(module);
        result
    }

    /// Consumes the artifact and returns its parts.
    #[must_use]
    pub fn into_parts(self) -> (Source, Diagnostics, BytecodeChunk, RuntimeCapabilities) {
        (
            self.source,
            self.diagnostics,
            self.chunk,
            self.runtime_capabilities,
        )
    }

    /// Consumes the artifact and returns its compiled bytecode chunk.
    #[must_use]
    pub fn into_chunk(self) -> BytecodeChunk {
        self.chunk
    }
}

/// Error returned by checked preparation.
#[derive(Clone, Debug, PartialEq)]
pub enum PrepareError {
    /// Checking produced diagnostics rejected by the selected policy.
    DiagnosticsRejected {
        /// Source that was checked.
        source: Box<Source>,
        /// Diagnostics produced by the checker.
        diagnostics: Diagnostics,
        /// Policy that rejected those diagnostics.
        policy: PrepareDiagnosticPolicy,
    },
    /// Compilation failed after diagnostics were accepted.
    Compile {
        /// Source that was checked and then compiled.
        source: Box<Source>,
        /// Diagnostics produced by the checker before compilation.
        diagnostics: Diagnostics,
        /// Compiler failure.
        error: CompileError,
    },
}

impl PrepareError {
    /// Returns the source that failed preparation.
    #[must_use]
    pub const fn source(&self) -> &Source {
        match self {
            Self::DiagnosticsRejected { source, .. } | Self::Compile { source, .. } => source,
        }
    }

    /// Returns diagnostics produced before preparation stopped.
    #[must_use]
    pub const fn diagnostics(&self) -> &Diagnostics {
        match self {
            Self::DiagnosticsRejected { diagnostics, .. } | Self::Compile { diagnostics, .. } => {
                diagnostics
            }
        }
    }

    /// Returns the rejecting diagnostic policy, if diagnostics stopped preparation.
    #[must_use]
    pub const fn diagnostic_policy(&self) -> Option<PrepareDiagnosticPolicy> {
        match self {
            Self::DiagnosticsRejected { policy, .. } => Some(*policy),
            Self::Compile { .. } => None,
        }
    }

    /// Returns the compiler failure, if compilation stopped preparation.
    #[must_use]
    pub const fn compile_error(&self) -> Option<&CompileError> {
        match self {
            Self::DiagnosticsRejected { .. } => None,
            Self::Compile { error, .. } => Some(error),
        }
    }
}

impl fmt::Display for PrepareError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DiagnosticsRejected {
                source,
                diagnostics,
                policy,
            } => write!(
                formatter,
                "{} rejected by diagnostic policy '{policy}' ({} errors, {} warnings)",
                source.display_name(),
                diagnostics.error_count(),
                diagnostics.warning_count()
            ),
            Self::Compile { source, error, .. } => {
                write!(formatter, "compile {}: {error}", source.display_name())
            }
        }
    }
}

impl Error for PrepareError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::DiagnosticsRejected { .. } => None,
            Self::Compile { error, .. } => Some(error),
        }
    }
}

/// Error returned while loading or executing a prepared source artifact.
#[derive(Clone, Debug, PartialEq)]
pub enum PreparedRunError {
    /// Loading the prepared bytecode into the VM failed.
    Load(LoadError),
    /// Executing the loaded module failed.
    Exec(ExecError),
}

impl fmt::Display for PreparedRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load(error) => write!(formatter, "prepared source load failed: {error}"),
            Self::Exec(error) => write!(formatter, "prepared source execution failed: {error}"),
        }
    }
}

impl Error for PreparedRunError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Load(error) => Some(error),
            Self::Exec(error) => Some(error),
        }
    }
}

impl From<LoadError> for PreparedRunError {
    fn from(error: LoadError) -> Self {
        Self::Load(error)
    }
}

impl From<ExecError> for PreparedRunError {
    fn from(error: ExecError) -> Self {
        Self::Exec(error)
    }
}

// The preparation entry points live beside the pipeline they drive; the core
// surface accessors keep their own impl block in the crate root.
#[allow(clippy::multiple_inherent_impl)]
impl Surface {
    /// Checks and compiles a named source with default preparation options.
    ///
    /// The default diagnostic policy rejects error-severity diagnostics,
    /// preserves warnings on the returned artifact, and compiles with the
    /// public VM compile policy.
    ///
    /// # Errors
    /// Returns [`PrepareError`] when diagnostics fail the policy or compilation
    /// fails after diagnostics are accepted.
    pub fn prepare(&self, source: Source) -> Result<PreparedScript, PrepareError> {
        self.prepare_with_options(source, PrepareOptions::default())
    }

    /// Checks and compiles a named source with explicit preparation options.
    ///
    /// # Errors
    /// Returns [`PrepareError`] when diagnostics fail the policy or compilation
    /// fails after diagnostics are accepted.
    pub fn prepare_with_options(
        &self,
        source: Source,
        options: PrepareOptions,
    ) -> Result<PreparedScript, PrepareError> {
        let checked = self.check_with_config(&source, options.check_config);
        let diagnostics = checked.diagnostics().clone();
        if !options.diagnostic_policy.accepts(&diagnostics) {
            return Err(PrepareError::DiagnosticsRejected {
                source: Box::new(source),
                diagnostics,
                policy: options.diagnostic_policy,
            });
        }

        let chunk = self
            .compile_with_options(&source, &options.compile_options)
            .map_err(|error| PrepareError::Compile {
                source: Box::new(source.clone()),
                diagnostics: diagnostics.clone(),
                error,
            })?;
        Ok(PreparedScript {
            source,
            diagnostics,
            chunk,
            runtime_capabilities: self.runtime_capabilities().clone(),
        })
    }
}
