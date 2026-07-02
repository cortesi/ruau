use ruau_analysis::AnalysisMode;
use ruau_ast::{parse::SyntaxFlags, syntax::Stat};

use super::{
    CheckedModule, Checker, Config, ConformanceCheck, ConformanceFingerprint, RequiredGlobalPolicy,
};
use crate::{
    diagnostics::{Diagnostic, DiagnosticCategory, DiagnosticLocation, Diagnostics, Payload},
    subtype::Subtyper,
    types::{Arena, TypeId},
};

impl Checker {
    /// Checks an implementation source against a `.d.luau`-style declaration
    /// source with explicit checker configuration.
    ///
    /// The declaration source is parsed as ordinary Luau declaration syntax
    /// (`declare module: {...}`, `export type ...`, `declare class ...`).
    /// It must contain exactly one declared root global or declared function;
    /// the implementation module's single return value must be a subtype of
    /// that declared root type. Width subtyping, read-only properties, and
    /// recursive receiver variance use the checker subtype relation directly.
    ///
    /// The fingerprint covers the implementation source, declaration source,
    /// and checker configuration. Graph-oriented callers can store this value
    /// with their own dependency digest for the checked root.
    pub fn check_conformance_report_with_config(
        &mut self,
        implementation_source: &str,
        declaration_source: &str,
        config: Config,
    ) -> ConformanceCheck {
        let fingerprint =
            conformance_fingerprint_for_sources(implementation_source, declaration_source, &config);
        let implementation = self.check_source_with_required_globals(
            implementation_source,
            config.clone(),
            RequiredGlobalPolicy::Skip,
        );
        self.conformance_report_for_checked_module(
            &implementation,
            declaration_source,
            config,
            fingerprint,
            implementation.diagnostics().clone(),
        )
    }

    pub(crate) fn conformance_report_for_checked_module(
        &mut self,
        implementation: &CheckedModule,
        declaration_source: &str,
        config: Config,
        fingerprint: ConformanceFingerprint,
        mut diagnostics: Diagnostics,
    ) -> ConformanceCheck {
        let declaration = self.check_source_with_required_globals(
            declaration_source,
            declaration_config(config),
            RequiredGlobalPolicy::Skip,
        );
        diagnostics.extend(declaration.diagnostics().iter().cloned());
        if diagnostics.is_empty() {
            diagnostics.extend(self.conformance_diagnostics(implementation, &declaration));
        }
        ConformanceCheck::new(diagnostics, fingerprint)
    }

    fn conformance_diagnostics(
        &self,
        implementation: &CheckedModule,
        declaration: &CheckedModule,
    ) -> Diagnostics {
        if implementation.mode() == AnalysisMode::NoCheck {
            return Diagnostics::new();
        }
        let (declared_name, declared_type) = match conformance_root_type(declaration) {
            Ok(root) => root,
            Err(diagnostic) => return Diagnostics::from_vec(vec![*diagnostic]),
        };
        let required_summary = self.arena.summary(declared_type);
        let [actual] = implementation.return_types() else {
            return Diagnostics::from_vec(vec![conformance_diagnostic(
                declared_name,
                required_summary,
                implementation_return_summary(self.arena(), implementation),
            )]);
        };
        if Subtyper::new(&self.arena)
            .is_subtype(*actual, declared_type)
            .is_ok()
        {
            return Diagnostics::new();
        }
        Diagnostics::from_vec(vec![conformance_diagnostic(
            declared_name,
            required_summary,
            Some(self.arena.summary(*actual)),
        )])
    }
}

fn declaration_config(mut config: Config) -> Config {
    config.source_mode_override = Some(AnalysisMode::Strict);
    config.parse.allow_declaration_syntax = true;
    config.parse.capture_comments = true;
    config.parse.syntax = SyntaxFlags::all_luau();
    config
}

fn conformance_root_type(module: &CheckedModule) -> Result<(String, TypeId), Box<Diagnostic>> {
    let mut roots = Vec::new();
    collect_conformance_declared_roots(module.root(), &mut roots);
    roots.sort();
    roots.dedup();
    match roots.as_slice() {
        [] => Err(Box::new(conformance_shape_diagnostic(
            "<module>",
            "declaration source must contain exactly one `declare <name>: <type>` or `declare function <name>` root",
        ))),
        [name] => module
            .global_def(name)
            .map(|ty| (name.clone(), ty))
            .ok_or_else(|| {
                Box::new(conformance_shape_diagnostic(
                    name,
                    "declared conformance root did not lower to a type",
                ))
            }),
        names => Err(Box::new(conformance_shape_diagnostic(
            "<module>",
            format!(
                "declaration source must contain one declared root, found {}: {}",
                names.len(),
                names.join(", ")
            ),
        ))),
    }
}

fn collect_conformance_declared_roots(stat: &Stat, roots: &mut Vec<String>) {
    match stat {
        Stat::Block { body, .. } => {
            for stat in body {
                collect_conformance_declared_roots(stat, roots);
            }
        }
        Stat::DeclareGlobal { name, .. } | Stat::DeclareFunction { name, .. } => {
            roots.push(name.as_str().to_owned());
        }
        _ => {}
    }
}

fn implementation_return_summary(arena: &Arena, implementation: &CheckedModule) -> Option<String> {
    match implementation.return_types() {
        [] => None,
        [actual] => Some(arena.summary(*actual)),
        returns => Some(format!(
            "({})",
            returns
                .iter()
                .map(|ty| arena.summary(*ty))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn conformance_diagnostic(
    name: impl Into<String>,
    required: String,
    actual: Option<String>,
) -> Diagnostic {
    let name = name.into();
    let context = match &actual {
        Some(actual) => format!(
            "Module '{name}' has type '{actual}', which does not conform to the declared type '{required}'"
        ),
        None => format!("Module '{name}' does not export a value; expected '{required}'"),
    };
    Diagnostic::error(
        DiagnosticCategory::Conformance,
        DiagnosticLocation::missing(),
    )
    .with_context(context)
    .with_typed(Payload::Conformance {
        name,
        required,
        actual,
    })
}

fn conformance_shape_diagnostic(name: impl Into<String>, message: impl Into<String>) -> Diagnostic {
    let message = message.into();
    Diagnostic::error(
        DiagnosticCategory::Conformance,
        DiagnosticLocation::missing(),
    )
    .with_context(message.clone())
    .with_typed(Payload::Conformance {
        name: name.into(),
        required: message,
        actual: None,
    })
}

const CONFORMANCE_FNV1A64_OFFSET: u64 = 0xcbf29ce484222325;
const CONFORMANCE_FNV1A64_PRIME: u64 = 0x100000001b3;

fn conformance_fingerprint_for_sources(
    implementation_source: &str,
    declaration_source: &str,
    config: &Config,
) -> ConformanceFingerprint {
    let mut hash = CONFORMANCE_FNV1A64_OFFSET;
    conformance_hash_update(&mut hash, b"ruau:conformance:v1\0implementation\0");
    conformance_hash_update(&mut hash, implementation_source.as_bytes());
    conformance_hash_update(&mut hash, b"\0declaration\0");
    conformance_hash_update(&mut hash, declaration_source.as_bytes());
    conformance_hash_update(&mut hash, b"\0config\0");
    conformance_hash_update(&mut hash, format!("{config:?}").as_bytes());
    ConformanceFingerprint::new(hash)
}

fn conformance_hash_update(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(CONFORMANCE_FNV1A64_PRIME);
    }
}
