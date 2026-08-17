//! Read-only module surface derivation for declaration sources.

use std::collections::BTreeSet;

use crate::{
    parse::{Config, Error, parse_with_config},
    syntax::{Stat, Type},
};

/// Derives the requireable read-only form of one module API source.
///
/// The transform recognizes two module roots:
///
/// - An exported non-generic `Module` type alias. A table alias becomes
///   read-only. When the source has no top-level `return`, the output gains
///   `declare module: Module` and `return module`.
/// - The first top-level `declare <name>: <type>` global. A table type
///   becomes read-only. When the source has no top-level `return`, the output
///   gains `return <name>`.
///
/// Aliases that the root table's methods take as `self` receivers become
/// read-only too, transitively. A source without either root is returned
/// unchanged. Insertions preserve the order of the remaining code but not its
/// column positions.
///
/// # Errors
/// Returns the parse errors when the source does not parse. Each error renders
/// "line:col: message" via its `Display` impl.
pub fn read_only_module_surface(source: &str) -> Result<String, Vec<Error>> {
    let parsed = parse_with_config(source, &declaration_config());
    if !parsed.errors.is_empty() {
        return Err(parsed.errors);
    }
    let root = parsed.root;
    let has_return = block_has_return(&root);

    match surface_root(&root) {
        Some(ModuleRoot::Alias) => {
            let body = type_alias_table(&root, "Module", true)
                .and_then(|table| read_only_table_type_source(source, table))
                .unwrap_or_else(|| source.to_owned());
            let body = read_only_self_alias_closure(body, &ModuleRoot::Alias);
            if has_return {
                return Ok(body);
            }
            if let Some(name) = declared_root_name(&root) {
                return Ok(format!("{body}\nreturn {name}\n"));
            }
            Ok(format!("{body}\ndeclare module: Module\nreturn module\n"))
        }
        Some(ModuleRoot::Declare(name)) => {
            let body = declare_global_table(&root, &name)
                .and_then(|table| read_only_table_type_source(source, table))
                .unwrap_or_else(|| source.to_owned());
            let body = read_only_self_alias_closure(body, &ModuleRoot::Declare(name.clone()));
            if has_return {
                return Ok(body);
            }
            Ok(format!("{body}\nreturn {name}\n"))
        }
        None => Ok(source.to_owned()),
    }
}

/// How a module source names its root table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleRoot {
    /// An exported non-generic `Module` type alias.
    Alias,
    /// A `declare <name>: <type>` global binding carrying the bound name.
    Declare(String),
}

/// Identifies how a module source names its root binding.
///
/// A declared global wins over an exported `Module` alias: a source that
/// declares a root already binds the module value, and the alias only matters
/// when nothing is declared. Note that [`read_only_module_surface`] rewrites
/// the alias table first when both are present. Returns `None` when the
/// source does not parse or names no module root, so callers can treat both
/// cases as "not a module surface".
#[must_use]
pub fn module_root(source: &str) -> Option<ModuleRoot> {
    let parsed = parse_with_config(source, &declaration_config());
    if !parsed.errors.is_empty() {
        return None;
    }
    if let Some(name) = declared_root_name(&parsed.root) {
        return Some(ModuleRoot::Declare(name.to_owned()));
    }
    exported_type_alias(&parsed.root, "Module").map(|_alias| ModuleRoot::Alias)
}

/// Returns the root that the surface transform rewrites: the alias first.
fn surface_root(root: &Stat) -> Option<ModuleRoot> {
    if exported_type_alias(root, "Module").is_some() {
        return Some(ModuleRoot::Alias);
    }
    declared_root_name(root).map(|name| ModuleRoot::Declare(name.to_owned()))
}

/// The parse configuration for declaration sources.
fn declaration_config() -> Config {
    Config {
        allow_declaration_syntax: true,
        ..Config::default()
    }
}

/// Parses an intermediate rewrite without reporting errors.
///
/// Rewrites only insert `read` modifiers into parsed positions, so a rewrite
/// of a parsed source stays parseable.
fn parse_rewritten(source: &str) -> Option<Stat> {
    let parsed = parse_with_config(source, &declaration_config());
    parsed.errors.is_empty().then_some(parsed.root)
}

/// Marks the aliases the module root's methods use as `self` types read-only
/// too. A read-only root whose methods keep `self: Queue` against a
/// read-write `Queue` alias splits one object type into two variance-
/// incompatible spellings, so `queue:push(...)` stops type-checking. The
/// receiver alias must carry the same read-only surface as the module value.
fn read_only_self_alias_closure(mut source: String, root: &ModuleRoot) -> String {
    let mut pending = BTreeSet::new();
    if let Some(stat) = parse_rewritten(&source) {
        let table = match root {
            ModuleRoot::Alias => type_alias_table(&stat, "Module", true),
            ModuleRoot::Declare(name) => declare_global_table(&stat, name),
        };
        if let Some(table) = table {
            collect_table_self_alias_names(table, &mut pending);
        }
    }
    let mut done = BTreeSet::new();
    while let Some(name) = pending.pop_first() {
        if !done.insert(name.clone()) {
            continue;
        }
        let Some(stat) = parse_rewritten(&source) else {
            break;
        };
        let Some(table) = type_alias_table(&stat, &name, false) else {
            continue;
        };
        collect_table_self_alias_names(table, &mut pending);
        if let Some(rewritten) = read_only_table_type_source(&source, table) {
            source = rewritten;
        }
    }
    source
}

/// Collects non-generic type-reference names used as a `self` parameter by a
/// table type's top-level function-typed properties.
fn collect_table_self_alias_names(table: &Type, names: &mut BTreeSet<String>) {
    let Type::Table { props, .. } = table else {
        return;
    };
    for prop in props {
        let Type::Function {
            arg_types,
            arg_names,
            ..
        } = &prop.prop_type
        else {
            continue;
        };
        let first_is_self = arg_names.first().is_some_and(|name| {
            name.as_ref()
                .is_some_and(|name| name.name.as_str() == "self")
        });
        if !first_is_self {
            continue;
        }
        let Some(Type::Reference {
            prefix: None,
            name,
            parameters,
            ..
        }) = arg_types.types.first()
        else {
            continue;
        };
        if parameters.is_empty() {
            names.insert(name.as_str().to_owned());
        }
    }
}

/// Returns whether the parsed root contains a top-level return statement.
fn block_has_return(root: &Stat) -> bool {
    match root {
        Stat::Block { body, .. } => body.iter().any(|stat| matches!(stat, Stat::Return { .. })),
        Stat::Return { .. } => true,
        _ => false,
    }
}

/// Finds a non-generic exported type alias by name.
fn exported_type_alias<'a>(stat: &'a Stat, alias: &str) -> Option<&'a Type> {
    match stat {
        Stat::Block { body, .. } => body
            .iter()
            .find_map(|stat| exported_type_alias(stat, alias)),
        Stat::TypeAlias {
            name,
            generics,
            generic_packs,
            value,
            exported,
            ..
        } if name.as_str() == alias
            && *exported
            && generics.is_empty()
            && generic_packs.is_empty() =>
        {
            Some(value)
        }
        _ => None,
    }
}

/// Returns the first top-level `declare <name>: <type>` binding name.
fn declared_root_name(stat: &Stat) -> Option<&str> {
    match stat {
        Stat::Block { body, .. } => body.iter().find_map(declared_root_name),
        Stat::DeclareGlobal { name, .. } => Some(name.as_str()),
        _ => None,
    }
}

/// Finds a global declaration's table type by name.
fn declare_global_table<'a>(stat: &'a Stat, name: &str) -> Option<&'a Type> {
    match stat {
        Stat::Block { body, .. } => body
            .iter()
            .find_map(|stat| declare_global_table(stat, name)),
        Stat::DeclareGlobal {
            name: binding,
            declared_type,
            ..
        } if binding.as_str() == name && matches!(declared_type.as_ref(), Type::Table { .. }) => {
            Some(declared_type)
        }
        _ => None,
    }
}

/// Finds a non-generic table-valued type alias.
fn type_alias_table<'a>(stat: &'a Stat, alias: &str, require_exported: bool) -> Option<&'a Type> {
    match stat {
        Stat::Block { body, .. } => body
            .iter()
            .find_map(|stat| type_alias_table(stat, alias, require_exported)),
        Stat::TypeAlias {
            name,
            generics,
            generic_packs,
            value,
            exported,
            ..
        } if name.as_str() == alias
            && (!require_exported || *exported)
            && generics.is_empty()
            && generic_packs.is_empty()
            && matches!(value.as_ref(), Type::Table { .. }) =>
        {
            Some(value)
        }
        _ => None,
    }
}

/// Returns the full source with the target table's immediate fields marked read-only.
fn read_only_table_type_source(source: &str, table: &Type) -> Option<String> {
    let insertions = read_only_table_insertions(source, table)?;
    insert_read_prefixes(source, insertions)
}

/// Returns insertion offsets for read-only modifiers on one table type's top-level fields.
fn read_only_table_insertions(source: &str, table: &Type) -> Option<Vec<usize>> {
    let Type::Table { props, indexer, .. } = table else {
        return None;
    };
    let mut insertions = Vec::new();
    for prop in props {
        if prop.read_only || prop.write_only {
            continue;
        }
        let location = prop.location?;
        insertions.push(location.begin.byte_offset(source)?);
    }
    if let Some(indexer) = indexer
        && !indexer.read_only
    {
        let location = indexer.location?;
        insertions.push(location.begin.byte_offset(source)?);
    }
    Some(insertions)
}

/// Inserts read-only modifiers from the end of the source toward the start.
fn insert_read_prefixes(source: &str, mut offsets: Vec<usize>) -> Option<String> {
    offsets.sort_unstable();
    offsets.dedup();
    let mut output = source.to_owned();
    for offset in offsets.into_iter().rev() {
        if offset > output.len() || !output.is_char_boundary(offset) {
            return None;
        }
        output.insert_str(offset, "read ");
    }
    Some(output)
}

#[cfg(any())]
mod tests {
    use super::{ModuleRoot, module_root, parse_rewritten, read_only_module_surface};

    #[test]
    fn copies_exported_module_as_read_only_table() {
        let source = "\
export type Module = {
    rows: { get: () -> string },
    label: string,
}
";

        let generated = read_only_module_surface(source).expect("source parses");

        assert_eq!(
            generated,
            "\
export type Module = {
    read rows: { get: () -> string },
    read label: string,
}

declare module: Module
return module
"
        );
        assert!(parse_rewritten(&generated).is_some());
    }

    #[test]
    fn ignores_return_in_comment() {
        let source = "\
-- return module
export type Module = {
    label: string,
}
";

        let generated = read_only_module_surface(source).expect("source parses");

        assert!(generated.contains("declare module: Module"));
        assert!(generated.contains("return module"));
    }

    #[test]
    fn read_only_declare_root_table() {
        let source = "declare demo: { count: () -> number, items: { value: string } }\n";

        let generated = read_only_module_surface(source).expect("source parses");

        assert_eq!(
            generated,
            "declare demo: { read count: () -> number, read items: { value: string } }\n\n\
             return demo\n"
        );
        assert!(parse_rewritten(&generated).is_some());
    }

    #[test]
    fn read_only_returned_exported_module_table() {
        let source = "\
export type Module = {
    rows: { get: () -> string },
    label: string,
}
declare module: Module
return module
";

        let generated = read_only_module_surface(source).expect("source parses");

        assert_eq!(
            generated,
            "\
export type Module = {
    read rows: { get: () -> string },
    read label: string,
}
declare module: Module
return module
"
        );
        assert!(parse_rewritten(&generated).is_some());
    }

    #[test]
    fn exported_module_with_existing_declare_does_not_add_a_second_root() {
        let source = "\
export type Module = {
    label: string,
}
declare jobs: Module
";

        let generated = read_only_module_surface(source).expect("source parses");

        assert_eq!(
            generated,
            "\
export type Module = {
    read label: string,
}
declare jobs: Module

return jobs
"
        );
        assert!(parse_rewritten(&generated).is_some());
    }

    #[test]
    fn read_only_returned_declare_root_table() {
        let source =
            "declare demo: { count: () -> number, items: { value: string } }\nreturn demo\n";

        let generated = read_only_module_surface(source).expect("source parses");

        assert_eq!(
            generated,
            "declare demo: { read count: () -> number, read items: { value: string } }\nreturn demo\n"
        );
        assert!(parse_rewritten(&generated).is_some());
    }

    #[test]
    fn declare_root_non_table_appends_return_unchanged() {
        let source = "export type Alias = { value: string }\ndeclare demo: Alias\n";
        let expected = format!("{source}\nreturn demo\n");

        let generated = read_only_module_surface(source).expect("source parses");

        assert_eq!(generated, expected);
    }

    #[test]
    fn passthrough_without_module_root_is_unchanged() {
        let source = "local x = {}\nreturn x\n";

        let generated = read_only_module_surface(source).expect("source parses");

        assert_eq!(generated, source);
    }

    #[test]
    fn self_receiver_aliases_become_read_only_transitively() {
        let source = "\
export type Inner = {
    value: string,
}
export type Queue = {
    push: (self: Queue, value: string) -> (),
    peek: (self: Inner) -> string,
}
declare queue: {
    open: (self: Queue, name: string) -> Queue,
}
return queue
";

        let generated = read_only_module_surface(source).expect("source parses");

        assert_eq!(
            generated,
            "\
export type Inner = {
    read value: string,
}
export type Queue = {
    read push: (self: Queue, value: string) -> (),
    read peek: (self: Inner) -> string,
}
declare queue: {
    read open: (self: Queue, name: string) -> Queue,
}
return queue
"
        );
        assert!(parse_rewritten(&generated).is_some());
    }

    #[test]
    fn reports_parse_errors() {
        let errors = read_only_module_surface("declare oops:").expect_err("invalid source");

        assert!(!errors.is_empty());
    }

    #[test]
    fn module_root_identifies_alias_declare_and_absent_roots() {
        assert_eq!(
            module_root("export type Module = { name: string }"),
            Some(ModuleRoot::Alias)
        );
        assert_eq!(
            module_root("declare demo: { name: string }"),
            Some(ModuleRoot::Declare("demo".to_owned()))
        );
        assert_eq!(
            module_root("export type Module = { name: string }\ndeclare demo: Module\n"),
            Some(ModuleRoot::Declare("demo".to_owned()))
        );
        assert_eq!(
            module_root("declare function greet(name: string): string"),
            None
        );
        assert_eq!(
            module_root("export type ModuleConfig = { x: number }"),
            None
        );
        assert_eq!(module_root("local x = 1"), None);
        assert_eq!(module_root("declare oops:"), None);
    }

    #[test]
    fn read_only_insertions_preserve_utf8_and_comments() {
        let source = "declare demo: { --[[ \u{3053}\u{3093}\u{306b}\u{3061}\u{306f} ]] name: string, -- between\n age: number }\n";
        let generated = read_only_module_surface(source).expect("transform utf-8 source");

        assert_eq!(
            generated,
            "declare demo: { --[[ \u{3053}\u{3093}\u{306b}\u{3061}\u{306f} ]] read name: string, -- between\n read age: number }\n\nreturn demo\n"
        );
        assert!(parse_rewritten(&generated).is_some());
    }
}
