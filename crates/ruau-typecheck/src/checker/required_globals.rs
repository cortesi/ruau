use ruau_analysis::resolve::AnalysisMode;

use super::{CheckedModule, Checker};
use crate::{
    diagnostic::{DiagnosticCategory, DiagnosticLocation, Payload, TypeDiagnostic},
    subtype::Subtyper,
    types::TypeId,
};

/// One required-export obligation: a global the checked module must define
/// with a conforming type.
#[derive(Clone, Debug)]
pub(super) struct RequiredGlobal {
    /// Required global name.
    pub(super) name: String,
    /// Required type as registered, in declaration syntax.
    pub(super) type_text: String,
    /// Required type lowered into the checker-session arena.
    pub(super) required: TypeId,
}

impl Checker {
    /// Registers a required export: every module subsequently checked through
    /// this checker's single-module entry points (and the root module of
    /// [`GraphChecker`](crate::frontend::GraphChecker) graph checks)
    /// must define a global named `name` whose type conforms to `type_text`.
    ///
    /// `type_text` is a Luau type annotation in `.d.luau` declaration syntax
    /// (the type portion of `declare name: <type>`), resolved against this
    /// checker's builtin environment — so type names declared by the
    /// environment's definition modules are usable.
    ///
    /// Conformance is the checker's standard subtype relation: the module's
    /// definition must be a subtype of the required type. For function
    /// requirements that means parameters are compared contravariantly and
    /// returns covariantly, with exact pack arities on both sides: a
    /// definition may not take more parameters than the requirement supplies
    /// (taking fewer is accepted — extra call arguments are dropped), and it
    /// must produce exactly the required number of return values unless the
    /// required return pack ends in a variadic tail. Modules checked in
    /// `nocheck` mode (including modules downgraded by parse errors) are not
    /// judged.
    ///
    /// Violations are reported as [`DiagnosticCategory::RequiredExport`]
    /// diagnostics carrying a [`Payload::RequiredExport`] payload.
    ///
    /// # Errors
    /// Returns the parse or lowering diagnostics when `type_text` is not a
    /// valid type annotation or references type names the environment does
    /// not declare.
    pub fn require_global(
        &mut self,
        name: &str,
        type_text: &str,
    ) -> Result<(), Vec<TypeDiagnostic>> {
        let (required, diagnostics) = self.lower_annotation_text(type_text)?;
        if !diagnostics.is_empty() {
            return Err(diagnostics);
        }
        self.required_globals.push(RequiredGlobal {
            name: name.to_owned(),
            type_text: type_text.to_owned(),
            required,
        });
        Ok(())
    }

    /// Judges the required exports registered through
    /// [`Self::require_global`] against a checked module: each required
    /// global must be defined by the module with a type that conforms to the
    /// requirement (see `require_global` for the conformance relation).
    ///
    /// Returns one diagnostic per violated requirement; modules checked in
    /// `nocheck` mode are not judged (there are no solved types to compare).
    #[must_use]
    pub fn required_global_diagnostics(&self, module: &CheckedModule) -> Vec<TypeDiagnostic> {
        if self.required_globals.is_empty() || module.mode() == AnalysisMode::NoCheck {
            return Vec::new();
        }
        self.required_globals
            .iter()
            .filter_map(|required| self.required_global_diagnostic(required, module))
            .collect()
    }

    /// Judges one required export against a checked module.
    fn required_global_diagnostic(
        &self,
        required: &RequiredGlobal,
        module: &CheckedModule,
    ) -> Option<TypeDiagnostic> {
        let Some(actual) = module.global_def(&required.name) else {
            return Some(
                TypeDiagnostic::error(
                    DiagnosticCategory::RequiredExport,
                    DiagnosticLocation::missing(),
                )
                .with_context(format!(
                    "Required global '{}' is not defined; expected a definition of type '{}'",
                    required.name, required.type_text
                ))
                .with_typed(Payload::RequiredExport {
                    name: required.name.clone(),
                    required: required.type_text.clone(),
                    actual: None,
                }),
            );
        };
        if Subtyper::new(&self.arena)
            .is_subtype(actual, required.required)
            .is_ok()
        {
            return None;
        }
        let actual_summary = self.arena.summary(actual);
        Some(
            TypeDiagnostic::error(
                DiagnosticCategory::RequiredExport,
                DiagnosticLocation::missing(),
            )
            .with_context(format!(
                "Required global '{}' has type '{actual_summary}', which does not conform to \
                 the required type '{}'",
                required.name, required.type_text
            ))
            .with_typed(Payload::RequiredExport {
                name: required.name.clone(),
                required: required.type_text.clone(),
                actual: Some(actual_summary),
            }),
        )
    }
}
