//! Borrow-free module schema extraction for tooling consumers.
//!
//! The schema layer turns a checked module's arena-backed public surface into
//! small rendered records. It deliberately records summaries and coarse type
//! tags instead of exposing raw type handles, so downstream tools can cache and
//! compare module surfaces without owning a checker session.

use std::collections::{BTreeMap, BTreeSet};

use ruau_source::ModuleName;

use crate::{
    GraphChecker,
    checker::{CheckedModule, ExportedType, ImportedModuleSummary, ModuleExports},
    diagnostics::{Diagnostic, Diagnostics, ModuleDiagnostic},
    types::{Arena, KindTag, TypeId, TypeKind},
};

/// Borrow-free schema for one checked module.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Module {
    /// Structured diagnostics produced by the checked module.
    pub diagnostics: Diagnostics,
    /// Module-qualified resolver diagnostics and imported-module checker
    /// diagnostics from source-aware schema extraction.
    ///
    /// This covers the root's statically reachable import graph. Root checker
    /// diagnostics stay in [`Self::diagnostics`]; [`Self::imported_modules`]
    /// remains direct-only.
    pub source_diagnostics: Vec<ModuleDiagnostic>,
    /// Exported type surface, sorted by source-visible export name.
    pub exported_types: Vec<Export>,
    /// Top-level module return surface, in source order.
    pub return_types: Vec<Type>,
    /// Summaries of modules directly imported through the same checked
    /// frontend.
    pub imported_modules: BTreeMap<ModuleName, Import>,
}

impl Module {
    /// Returns true when any diagnostic was produced.
    #[must_use]
    pub fn has_issues(&self) -> bool {
        self.diagnostics.has_issues()
            || !self.source_diagnostics.is_empty()
            || self
                .imported_modules
                .values()
                .any(|imported| imported.has_issues)
    }

    /// Returns true when any error-severity diagnostic was produced.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.diagnostics.has_errors()
            || self
                .source_diagnostics
                .iter()
                .any(|entry| entry.diagnostic.severity == crate::diagnostics::Severity::Error)
            || self
                .imported_modules
                .values()
                .any(|imported| imported.has_errors)
    }

    /// Iterates exported type entries whose resolved surface is a function.
    pub fn exported_functions(&self) -> impl Iterator<Item = &Export> {
        self.exported_types
            .iter()
            .filter(|entry| entry.shape.kind == Some(KindTag::Function))
    }

    /// Iterates exported type entries whose resolved surface is a table.
    pub fn exported_tables(&self) -> impl Iterator<Item = &Export> {
        self.exported_types
            .iter()
            .filter(|entry| entry.shape.kind == Some(KindTag::Table))
    }
}

/// One exported type entry in a module schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Export {
    /// Source-visible export name.
    pub name: String,
    /// Rendered and tagged exported type surface.
    pub shape: Type,
    /// True when the source alias body has generic type or pack parameters.
    pub alias_has_generics: bool,
    /// Ordered generic type parameter names.
    pub generic_names: Vec<String>,
    /// Ordered generic type-pack parameter names.
    pub generic_pack_names: Vec<String>,
}

/// Rendered summary and coarse structural tag for one type.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Type {
    /// Deterministic single-line display summary. Structured fields below are
    /// the durable proxy-generation surface; this text is for diagnostics and
    /// inspection.
    pub summary: Option<String>,
    /// Top-level resolved type tag.
    pub kind: Option<KindTag>,
    /// Direct named table fields, sorted by source-visible field name.
    pub table_fields: Vec<TableField>,
    /// Function pack shape when the resolved type is a function.
    pub function: Option<Function>,
}

/// One direct named field in a table schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableField {
    /// Source-visible field name.
    pub name: String,
    /// Read type for the field.
    pub value: Type,
}

/// Borrow-free function pack schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Function {
    /// Deterministic display summary of the full argument pack.
    pub argument_pack_summary: String,
    /// Fixed argument types from the normalized argument pack.
    pub argument_types: Vec<Type>,
    /// Deterministic display summary of the full return pack.
    pub return_pack_summary: String,
    /// Fixed return types from the normalized return pack.
    pub return_types: Vec<Type>,
}

/// Borrow-free imported-module summary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Import {
    /// Whether the imported module produced diagnostics.
    pub has_issues: bool,
    /// Whether the imported module produced error-severity diagnostics.
    pub has_errors: bool,
    /// Exported type surface of the imported module.
    pub exported_types: Vec<Export>,
    /// Top-level return surface of the imported module.
    pub return_types: Vec<Type>,
}

/// Extracts a borrow-free schema from a checked module and its checker arena.
#[must_use]
pub fn extract_module(arena: &Arena, module: &CheckedModule) -> Module {
    Module {
        diagnostics: module.diagnostics().clone(),
        source_diagnostics: Vec::new(),
        exported_types: exported_type_schemas(arena, module.exports()),
        return_types: type_schemas(arena, module.return_types()),
        imported_modules: module
            .imported_modules()
            .iter()
            .map(|(name, summary)| (name.clone(), imported_module_schema(arena, summary)))
            .collect(),
    }
}

/// Extracts a schema from a checked source frontend, including diagnostics for
/// the root and its statically reachable imports.
#[must_use]
pub fn extract_frontend(frontend: &GraphChecker<'_>, name: &ModuleName) -> Option<Module> {
    let checked = frontend.checked_module(name)?;
    let mut schema = extract_module(frontend.checker().arena(), checked);
    schema.source_diagnostics = source_diagnostics_for(frontend, name);
    Some(schema)
}

fn source_diagnostics_for(frontend: &GraphChecker<'_>, root: &ModuleName) -> Vec<ModuleDiagnostic> {
    let mut module_names = BTreeSet::new();
    let mut pending = vec![root.clone()];
    while let Some(module_name) = pending.pop() {
        if !module_names.insert(module_name.clone()) {
            continue;
        }
        if let Some(node) = frontend.frontend().source_node(&module_name) {
            pending.extend(node.requires().iter().cloned());
        }
    }

    let mut diagnostics = Vec::new();
    for module_name in module_names {
        let display_name = frontend.frontend().module_display_name(&module_name);
        diagnostics.extend(
            frontend
                .frontend()
                .resolver_diagnostics(&module_name)
                .iter()
                .map(|diagnostic| ModuleDiagnostic {
                    module: module_name.clone(),
                    display_name: display_name.clone(),
                    diagnostic: Diagnostic::from_resolver_diagnostic_with_display_name(
                        diagnostic,
                        Some(&display_name),
                    ),
                }),
        );
        if module_name != *root
            && let Some(checked) = frontend.checked_module(&module_name)
        {
            diagnostics.extend(checked.diagnostics().iter().cloned().map(|diagnostic| {
                ModuleDiagnostic {
                    module: module_name.clone(),
                    display_name: display_name.clone(),
                    diagnostic,
                }
            }));
        }
    }
    diagnostics
}

fn imported_module_schema(arena: &Arena, summary: &ImportedModuleSummary) -> Import {
    Import {
        has_issues: summary.has_issues,
        has_errors: summary.has_errors,
        exported_types: exported_type_schemas(arena, &summary.exports),
        return_types: type_schemas(arena, &summary.return_types),
    }
}

fn exported_type_schemas(arena: &Arena, exports: &ModuleExports) -> Vec<Export> {
    exports
        .types()
        .values()
        .map(|export| exported_type_schema(arena, export))
        .collect()
}

fn exported_type_schema(arena: &Arena, export: &ExportedType) -> Export {
    Export {
        name: export.name.clone(),
        shape: maybe_type_schema(arena, export.ty),
        alias_has_generics: export.alias_has_generics,
        generic_names: export
            .generics
            .iter()
            .map(|parameter| parameter.name.clone())
            .collect(),
        generic_pack_names: export
            .generic_packs
            .iter()
            .map(|parameter| parameter.name.clone())
            .collect(),
    }
}

fn type_schemas(arena: &Arena, types: &[TypeId]) -> Vec<Type> {
    types
        .iter()
        .map(|id| maybe_type_schema(arena, Some(*id)))
        .collect()
}

fn maybe_type_schema(arena: &Arena, id: Option<TypeId>) -> Type {
    id.map_or_else(Type::missing, |id| {
        type_schema_with_seen(arena, id, &mut Vec::new())
    })
}

fn type_schema_with_seen(arena: &Arena, id: TypeId, seen: &mut Vec<TypeId>) -> Type {
    let followed = arena.follow(id);
    let summary = Some(arena.summary(id));
    let kind = Some(arena.get(followed).tag());

    if seen.contains(&followed) {
        return Type {
            summary,
            kind,
            table_fields: Vec::new(),
            function: None,
        };
    }

    seen.push(followed);
    let (table_fields, function) = match arena.get(followed) {
        TypeKind::Table(table) => (
            table
                .properties
                .iter()
                .map(|(name, property)| TableField {
                    name: name.clone(),
                    value: type_schema_with_seen(arena, property.ty, seen),
                })
                .collect(),
            None,
        ),
        TypeKind::Function(function) => {
            let arguments = arena.normalize_pack(function.arguments);
            let returns = arena.normalize_pack(function.returns);
            (
                Vec::new(),
                Some(Function {
                    argument_pack_summary: arena.pack_summary(function.arguments),
                    argument_types: arguments
                        .types
                        .iter()
                        .map(|id| type_schema_with_seen(arena, *id, seen))
                        .collect(),
                    return_pack_summary: arena.pack_summary(function.returns),
                    return_types: returns
                        .types
                        .iter()
                        .map(|id| type_schema_with_seen(arena, *id, seen))
                        .collect(),
                }),
            )
        }
        _ => (Vec::new(), None),
    };
    seen.pop();

    Type {
        summary,
        kind,
        table_fields,
        function,
    }
}

impl Type {
    fn missing() -> Self {
        Self {
            summary: None,
            kind: None,
            table_fields: Vec::new(),
            function: None,
        }
    }
}

#[cfg(any())]
mod tests {
    use ruau_source::{InMemorySource, ModuleId, SourceMetadata};

    use super::*;
    use crate::{
        GraphChecker,
        graph::resolve::{SourceCode, config::EmptyResolver},
    };

    #[test]
    fn extracts_exports_returns_and_imports() {
        let mut sources = InMemorySource::new();
        sources.insert(
            ModuleId::new("Dep"),
            SourceCode::new("--!strict\nexport type DepRow = { name: string }\nreturn 7"),
        );
        sources.insert(
            ModuleId::new("Main"),
            SourceCode::new(
                "--!strict\n\
                 local Dep = require(\"Dep\")\n\
                 export type Callback = (number) -> string\n\
                 export type Row = { id: number, dep: Dep.DepRow }\n\
                 return function(value: number): string return tostring(value + Dep) end",
            ),
        );
        let configs = EmptyResolver;
        let mut frontend = GraphChecker::new(&sources, &configs);

        frontend.check("Main");
        let main = frontend
            .checked_module(&ModuleName::from("Main"))
            .expect("main checked");
        let schema = extract_module(frontend.checker().arena(), main);

        assert!(!schema.has_errors(), "{:?}", schema.diagnostics);
        assert_eq!(schema.exported_types.len(), 2);
        assert_eq!(schema.exported_functions().count(), 1);
        assert_eq!(schema.exported_tables().count(), 1);
        assert!(
            schema.return_types[0]
                .summary
                .as_ref()
                .is_some_and(|summary| {
                    summary.contains("(number)") && summary.contains("string")
                })
        );
        let row = schema
            .exported_tables()
            .find(|entry| entry.name == "Row")
            .expect("Row table export");
        assert_eq!(
            row.shape
                .table_fields
                .iter()
                .map(|field| field.name.as_str())
                .collect::<Vec<_>>(),
            ["dep", "id"]
        );
        let callback = schema
            .exported_functions()
            .find(|entry| entry.name == "Callback")
            .expect("Callback function export");
        let callback_function = callback.shape.function.as_ref().expect("function shape");
        assert_eq!(callback_function.argument_types.len(), 1);
        assert_eq!(callback_function.return_types.len(), 1);

        let dep = schema
            .imported_modules
            .get(&ModuleName::from("Dep"))
            .expect("dep summary");
        assert!(!dep.has_errors);
        assert_eq!(dep.exported_types[0].name, "DepRow");
        assert_eq!(dep.exported_types[0].shape.kind, Some(KindTag::Table));
    }

    #[test]
    fn extracts_source_diagnostics_for_imported_modules() {
        let sources = InMemorySource::new()
            .with_module(
                ModuleId::new("Dep"),
                "--!strict\nlocal value: number = \"bad\"\nreturn value",
            )
            .with_metadata(
                ModuleId::new("Dep"),
                SourceMetadata::new("display/Dep.luau"),
            )
            .with_module(
                ModuleId::new("Main"),
                "--!strict\nlocal Dep = require(\"Dep\")\nreturn Dep",
            )
            .with_metadata(
                ModuleId::new("Main"),
                SourceMetadata::new("display/Main.luau"),
            );
        let configs = EmptyResolver;
        let mut frontend = GraphChecker::new(&sources, &configs);

        frontend.check("Main");
        let schema = extract_frontend(&frontend, &ModuleName::from("Main")).expect("schema");

        assert!(schema.has_errors());
        let diagnostic = schema
            .source_diagnostics
            .iter()
            .find(|entry| entry.module == ModuleName::from("Dep"))
            .expect("dep diagnostic");
        assert_eq!(diagnostic.display_name, "display/Dep.luau");
        assert_ne!(
            diagnostic.diagnostic.category,
            crate::diagnostics::DiagnosticCategory::Resolver
        );
    }

    #[test]
    fn frontend_schema_keeps_root_checker_diagnostics_local() {
        let sources = InMemorySource::new()
            .with_module(
                ModuleId::new("Main"),
                "--!strict\nlocal value: number = \"bad\"\nreturn value",
            )
            .with_metadata(
                ModuleId::new("Main"),
                SourceMetadata::new("display/Main.luau"),
            );
        let configs = EmptyResolver;
        let mut frontend = GraphChecker::new(&sources, &configs);

        frontend.check("Main");
        let schema = extract_frontend(&frontend, &ModuleName::from("Main")).expect("schema");

        assert!(!schema.diagnostics.is_empty());
        assert!(schema.has_errors());
        assert!(
            schema.source_diagnostics.iter().all(|entry| {
                entry.module != ModuleName::from("Main")
                    || entry.diagnostic.category == crate::diagnostics::DiagnosticCategory::Resolver
            }),
            "{:?}",
            schema.source_diagnostics
        );
    }

    #[test]
    fn frontend_schema_scopes_source_diagnostics_to_requested_root() {
        let sources = InMemorySource::new()
            .with_module(
                ModuleId::new("GoodMain"),
                "--!strict\nreturn require(\"GoodDep\")",
            )
            .with_module(ModuleId::new("GoodDep"), "--!strict\nreturn 7")
            .with_module(
                ModuleId::new("BadMain"),
                "--!strict\nreturn require(\"BadDep\")",
            )
            .with_module(
                ModuleId::new("BadDep"),
                "--!strict\nlocal value: number = \"bad\"\nreturn value",
            )
            .with_metadata(
                ModuleId::new("BadDep"),
                SourceMetadata::new("display/BadDep.luau"),
            );
        let configs = EmptyResolver;
        let mut frontend = GraphChecker::new(&sources, &configs);

        frontend.check("GoodMain");
        frontend.check("BadMain");
        let good_schema =
            extract_frontend(&frontend, &ModuleName::from("GoodMain")).expect("good schema");
        let bad_schema =
            extract_frontend(&frontend, &ModuleName::from("BadMain")).expect("bad schema");

        assert!(!good_schema.has_errors(), "{good_schema:?}");
        assert!(
            good_schema
                .source_diagnostics
                .iter()
                .all(|entry| entry.module != ModuleName::from("BadDep")),
            "{:?}",
            good_schema.source_diagnostics
        );
        assert!(
            bad_schema
                .source_diagnostics
                .iter()
                .any(|entry| entry.module == ModuleName::from("BadDep")),
            "{:?}",
            bad_schema.source_diagnostics
        );
    }

    #[test]
    fn extracts_source_diagnostics_with_resolver_display_names() {
        let sources = InMemorySource::new()
            .with_module(
                ModuleId::new("Main"),
                "--!strict\nreturn require(\"Missing\")",
            )
            .with_metadata(
                ModuleId::new("Missing"),
                SourceMetadata::new("display/Missing.luau"),
            );
        let configs = EmptyResolver;
        let mut frontend = GraphChecker::new(&sources, &configs);

        frontend.check("Main");
        let schema = extract_frontend(&frontend, &ModuleName::from("Main")).expect("schema");

        let diagnostic = schema
            .source_diagnostics
            .iter()
            .find(|entry| entry.module == ModuleName::from("Missing"))
            .expect("missing-module diagnostic");
        assert_eq!(diagnostic.display_name, "display/Missing.luau");
        assert_eq!(
            diagnostic
                .diagnostic
                .payload()
                .get("displayName")
                .and_then(serde_json::Value::as_str),
            Some("display/Missing.luau")
        );
    }
}
