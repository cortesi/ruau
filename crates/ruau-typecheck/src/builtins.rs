//! Builtin globals and standard-library type environment scaffolding.

use std::{borrow::Cow, collections::BTreeMap};

use ruau_analysis::resolve::AnalysisMode;
#[cfg(any())]
use ruau_ast::parse::Error;
use ruau_ast::{
    parse::{Options, ParseResult, SyntaxFlags, parse_file_with},
    syntax::{Stat, SyntaxId, Type},
};

use crate::{
    annotation::lower_type_annotation,
    dfg::DataFlowGraph,
    scopes::{ScopeId, ScopeTree, TypeBindingKind, ValueBindingKind},
    types::{
        Arena, FunctionType, GenericType, GenericTypePack, TableIndexer, TableProperty, TableState,
        TableType, TypeId, TypeKind, TypeLevel, TypePackKind, alloc_top_function_type,
    },
};

/// Standard `.d.luau` declaration sources for builtins and standard libraries.
mod defs {
    /// `base.d.luau` — the base globals.
    pub const BASE: &str = include_str!("../defs/base.d.luau");
    /// `bit32.d.luau`.
    pub const BIT32: &str = include_str!("../defs/bit32.d.luau");
    /// `buffer.d.luau`.
    pub const BUFFER: &str = include_str!("../defs/buffer.d.luau");
    /// `coroutine.d.luau`.
    pub const COROUTINE: &str = include_str!("../defs/coroutine.d.luau");
    /// `debug.d.luau`.
    pub const DEBUG: &str = include_str!("../defs/debug.d.luau");
    /// `math.d.luau`.
    pub const MATH: &str = include_str!("../defs/math.d.luau");
    /// `integer.d.luau`.
    pub const INTEGER: &str = include_str!("../defs/integer.d.luau");
    /// `os.d.luau`.
    pub const OS: &str = include_str!("../defs/os.d.luau");
    /// `string.d.luau`.
    pub const STRING: &str = include_str!("../defs/string.d.luau");
    /// `table.d.luau`.
    pub const TABLE: &str = include_str!("../defs/table.d.luau");
    /// `utf8.d.luau`.
    pub const UTF8: &str = include_str!("../defs/utf8.d.luau");
    /// `vector.d.luau`.
    pub const VECTOR: &str = include_str!("../defs/vector.d.luau");
}

/// Fixture-only access to embedded builtin declaration sources.
#[cfg(any())]
pub mod fixture_defs {
    pub use super::defs::*;
}

/// One parsed builtin declaration module. The name and source are `Cow`s so
/// the standard modules stay allocation-free statics while a host can feed
/// runtime-rendered declarations (a generated `.d.luau` surface) without
/// leaking.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DefinitionModule {
    /// Stable module name.
    pub name: Cow<'static, str>,
    /// Module declaration source.
    pub source: Cow<'static, str>,
}

impl DefinitionModule {
    /// Builds a module from static name and source, const-friendly for the
    /// standard definition tables.
    #[must_use]
    pub const fn from_static(name: &'static str, source: &'static str) -> Self {
        Self {
            name: Cow::Borrowed(name),
            source: Cow::Borrowed(source),
        }
    }
}

/// A builtin global entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Global {
    /// Global name.
    pub name: String,
    /// Provisional global type.
    pub ty: TypeId,
    /// Documentation symbol for query consumers.
    pub documentation_symbol: Option<String>,
}

/// A builtin type name entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuiltinType {
    /// Type name.
    pub name: String,
    /// Provisional type handle.
    pub ty: TypeId,
}

/// Installable builtin environment for a checker session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuiltinEnvironment {
    globals: BTreeMap<String, Global>,
    types: BTreeMap<String, BuiltinType>,
}

impl BuiltinEnvironment {
    /// Builds the minimal standard builtin environment over an existing checker
    /// arena. Full `BuiltinDefinitions.test.cpp` parity is staged later; this
    /// scaffold gives name resolution stable roots immediately.
    #[must_use]
    pub fn standard(arena: &mut Arena) -> Self {
        Self::standard_with_definition_modules(arena, &[])
    }

    /// Builds the standard builtin environment plus opt-in declaration
    /// modules, such as test-only Roblox globals or audited host modules.
    ///
    /// An extra module's declaration of a global the standard environment
    /// also defines *replaces* the builtin's type — the checker half of the
    /// host builtin-override path, so strict scripts check against the
    /// override's signature. Accidental collisions are gated upstream by
    /// surface validation, not here.
    #[must_use]
    pub fn standard_with_definition_modules(
        arena: &mut Arena,
        extra_modules: &[DefinitionModule],
    ) -> Self {
        let primitives = arena.primitives();
        let mut environment = Self {
            globals: BTreeMap::new(),
            types: BTreeMap::new(),
        };

        for (name, ty) in [
            ("nil", primitives.nil),
            ("boolean", primitives.boolean),
            ("number", primitives.number),
            ("string", primitives.string),
            ("thread", primitives.thread),
            ("buffer", primitives.buffer),
            ("vector", primitives.vector),
            ("any", primitives.any),
            ("unknown", primitives.unknown),
            ("never", primitives.never),
        ] {
            environment.define_type(name, ty);
        }
        let top_function = alloc_top_function_type(arena);
        environment.define_type("fun", top_function);

        for name in [
            "_G",
            "_VERSION",
            "assert",
            "bit32",
            "buffer",
            "coroutine",
            "debug",
            "collectgarbage",
            "error",
            "gcinfo",
            "getfenv",
            "getmetatable",
            "ipairs",
            "loadstring",
            "math",
            "newproxy",
            "next",
            "os",
            "pairs",
            "pcall",
            "print",
            "rawequal",
            "rawget",
            "rawlen",
            "rawset",
            "require",
            "select",
            "setfenv",
            "setmetatable",
            "table",
            "tonumber",
            "tostring",
            "type",
            "typeof",
            "unpack",
            "utf8",
            "vector",
            "xpcall",
        ] {
            environment.define_global(name, primitives.any);
        }
        let parsed_embedded_builtins =
            EmbeddedBuiltinDeclarations::parse(arena, &environment, extra_modules);
        for name in PARSED_EMBEDDED_BUILTIN_GLOBALS {
            if let Some(ty) = parsed_embedded_builtins
                .as_ref()
                .and_then(|root| root.lower_global(arena, name))
            {
                environment.define_global(*name, ty);
            }
        }
        if let Some(parsed_embedded_builtins) = parsed_embedded_builtins.as_ref() {
            for name in declared_type_names_in_modules(extra_modules) {
                if let Some(ty) = parsed_embedded_builtins.lower_type_name(arena, &name) {
                    environment.define_type(name, ty);
                }
            }
        }
        let table_library = table_library_type(arena);
        overlay_embedded_table_properties(
            arena,
            &environment,
            table_library,
            &[
                "clear", "clone", "concat", "freeze", "insert", "remove", "sort",
            ],
        );
        environment.define_global("table", table_library);
        let coroutine_library = coroutine_library_type(arena);
        overlay_embedded_library_properties(
            arena,
            &environment,
            "coroutine",
            coroutine_library,
            &["create", "running", "status", "isyieldable", "close"],
        );
        environment.define_global("coroutine", coroutine_library);
        overlay_modeled_string_properties(arena, &environment);
        let error_fn = error_type(arena);
        environment.define_global("error", error_fn);
        let pcall = pcall_type(arena);
        environment.define_global("pcall", pcall);
        let xpcall = xpcall_type(arena);
        environment.define_global("xpcall", xpcall);

        // Extra-module globals land last, after every builtin scaffold and
        // overlay, so a module that redeclares a builtin global (the audited
        // host-override path) replaces the builtin's type rather than being
        // clobbered by it. Within the concatenated declaration source the
        // *last* declaration of a name wins, so the extra module's
        // redeclaration also beats the embedded builtin declaration itself.
        if let Some(parsed_embedded_builtins) = parsed_embedded_builtins.as_ref() {
            for name in declared_global_names_in_modules(extra_modules) {
                if let Some(ty) = parsed_embedded_builtins.lower_global(arena, &name) {
                    environment.define_global(name, ty);
                }
            }
        }

        environment.ensure_documentation(arena);
        environment
    }

    /// Defines one builtin global.
    pub fn define_global(&mut self, name: impl Into<String>, ty: TypeId) {
        self.define_global_with_documentation(name, ty, None);
    }

    /// Defines one builtin global with documentation metadata.
    pub fn define_global_with_documentation(
        &mut self,
        name: impl Into<String>,
        ty: TypeId,
        documentation_symbol: Option<String>,
    ) {
        let name = name.into();
        self.globals.insert(
            name.clone(),
            Global {
                name,
                ty,
                documentation_symbol,
            },
        );
    }

    /// Defines one builtin type name.
    pub fn define_type(&mut self, name: impl Into<String>, ty: TypeId) {
        let name = name.into();
        self.types.insert(name.clone(), BuiltinType { name, ty });
    }

    fn ensure_documentation(&mut self, arena: &mut Arena) {
        for global in self.globals.values_mut() {
            let base_symbol = global
                .documentation_symbol
                .get_or_insert_with(|| luau_global_symbol(&global.name))
                .clone();
            attach_builtin_property_documentation(arena, global.ty, &base_symbol);
        }
    }

    /// Returns a builtin global by name.
    #[must_use]
    pub fn global(&self, name: &str) -> Option<&Global> {
        self.globals.get(name)
    }

    /// Returns this environment with the named globals removed.
    ///
    /// Primitive type names such as `buffer` and `vector` remain available.
    #[must_use]
    pub fn without_globals<'a>(mut self, names: impl IntoIterator<Item = &'a str>) -> Self {
        for name in names {
            self.globals.remove(name);
        }
        self
    }

    /// Returns a builtin type by name.
    #[must_use]
    pub fn ty(&self, name: &str) -> Option<&BuiltinType> {
        self.types.get(name)
    }

    /// Iterates builtin globals in deterministic order.
    pub fn globals(&self) -> impl Iterator<Item = &Global> {
        self.globals.values()
    }

    /// Iterates builtin types in deterministic order.
    pub fn types(&self) -> impl Iterator<Item = &BuiltinType> {
        self.types.values()
    }

    /// Installs builtin globals and type names into a scope tree.
    pub(crate) fn install_into_scope(&self, scopes: &mut ScopeTree, scope: ScopeId) {
        for global in self.globals() {
            scopes.define_global_with_documentation(
                scope,
                &global.name,
                ValueBindingKind::Builtin,
                Some(global.ty),
                global.documentation_symbol.clone(),
            );
        }

        for ty in self.types() {
            scopes.define_type_with_kind(
                scope,
                &ty.name,
                TypeBindingKind::BuiltinType,
                Some(ty.ty),
                true,
            );
        }
    }
}

fn function_type(arena: &mut Arena, args: Vec<TypeId>, returns: Vec<TypeId>) -> TypeId {
    let arguments = arena.alloc_pack(TypePackKind::List {
        types: args,
        tail: None,
    });
    let returns = arena.alloc_pack(TypePackKind::List {
        types: returns,
        tail: None,
    });
    arena.alloc(TypeKind::Function(FunctionType::new(arguments, returns)))
}

fn optional_number_type(arena: &mut Arena) -> TypeId {
    let primitives = arena.primitives();
    arena.alloc(TypeKind::Union(vec![primitives.nil, primitives.number]))
}

fn string_byte_type(arena: &mut Arena) -> TypeId {
    let primitives = arena.primitives();
    let optional_number = optional_number_type(arena);
    let arguments = arena.alloc_pack(TypePackKind::List {
        types: vec![primitives.string, optional_number, optional_number],
        tail: None,
    });
    let returns = arena.alloc_pack(TypePackKind::Variadic {
        ty: primitives.number,
    });
    arena.alloc(TypeKind::Function(FunctionType::new(arguments, returns)))
}

fn luau_global_property_symbol(table: &str, property: &str) -> String {
    format!("{}.{property}", luau_global_symbol(table))
}

fn luau_global_symbol(global: &str) -> String {
    format!("@luau/global/{global}")
}

fn attach_builtin_property_documentation(arena: &mut Arena, ty: TypeId, base_symbol: &str) {
    let ty = arena.follow(ty);
    match arena.get(ty).clone() {
        TypeKind::Table(mut table) => {
            for (name, property) in &mut table.properties {
                property
                    .documentation_symbol
                    .get_or_insert_with(|| format!("{base_symbol}.{name}"));
            }
            arena.replace(ty, TypeKind::Table(table));
        }
        TypeKind::Extern {
            name,
            parents,
            mut properties,
            indexer,
        } => {
            for (property_name, property) in &mut properties {
                property
                    .documentation_symbol
                    .get_or_insert_with(|| format!("{base_symbol}.{property_name}"));
            }
            arena.replace(
                ty,
                TypeKind::Extern {
                    name,
                    parents,
                    properties,
                    indexer,
                },
            );
        }
        _ => {}
    }
}

fn error_type(arena: &mut Arena) -> TypeId {
    let primitives = arena.primitives();
    let optional_number = arena.alloc(TypeKind::Union(vec![primitives.nil, primitives.number]));
    function_type(
        arena,
        vec![primitives.any, optional_number],
        vec![primitives.never],
    )
}

fn generic_type(name: &str) -> GenericType {
    GenericType {
        name: name.to_owned(),
        level: TypeLevel(0),
    }
}

fn generic_pack(name: &str) -> GenericTypePack {
    GenericTypePack {
        name: name.to_owned(),
        level: TypeLevel(0),
    }
}

fn pcall_type(arena: &mut Arena) -> TypeId {
    let primitives = arena.primitives();
    let args_pack = arena.alloc_pack(TypePackKind::Generic(generic_pack("A")));
    let returns_pack = arena.alloc_pack(TypePackKind::Generic(generic_pack("R")));
    let protected = arena.alloc(TypeKind::Function(FunctionType::new(
        args_pack,
        returns_pack,
    )));
    let arguments = arena.alloc_pack(TypePackKind::List {
        types: vec![protected],
        tail: Some(args_pack),
    });
    let returns = arena.alloc_pack(TypePackKind::List {
        types: vec![primitives.boolean],
        tail: Some(returns_pack),
    });

    arena.alloc(TypeKind::Function(FunctionType {
        generic_packs: vec![generic_pack("A"), generic_pack("R")],
        ..FunctionType::new(arguments, returns)
    }))
}

fn xpcall_type(arena: &mut Arena) -> TypeId {
    let primitives = arena.primitives();
    let error_type = arena.alloc(TypeKind::Generic(generic_type("E")));
    let args_pack = arena.alloc_pack(TypePackKind::Generic(generic_pack("A")));
    let success_returns = arena.alloc_pack(TypePackKind::Generic(generic_pack("R1")));
    let error_returns = arena.alloc_pack(TypePackKind::Generic(generic_pack("R2")));

    let protected = arena.alloc(TypeKind::Function(FunctionType::new(
        args_pack,
        success_returns,
    )));
    let error_arguments = arena.alloc_pack(TypePackKind::List {
        types: vec![error_type],
        tail: None,
    });
    let error_handler = arena.alloc(TypeKind::Function(FunctionType::new(
        error_arguments,
        error_returns,
    )));
    let arguments = arena.alloc_pack(TypePackKind::List {
        types: vec![protected, error_handler],
        tail: Some(args_pack),
    });
    let returns = arena.alloc_pack(TypePackKind::List {
        types: vec![primitives.boolean],
        tail: Some(success_returns),
    });

    arena.alloc(TypeKind::Function(FunctionType {
        generics: vec![generic_type("E")],
        generic_packs: vec![generic_pack("A"), generic_pack("R1"), generic_pack("R2")],
        ..FunctionType::new(arguments, returns)
    }))
}

fn table_library_type(arena: &mut Arena) -> TypeId {
    let primitives = arena.primitives();
    let mut table = TableType::new(TableState::Sealed);
    table.name = Some("typeof(table)".to_owned());
    let any_variadic_args = arena.alloc_pack(TypePackKind::Variadic { ty: primitives.any });
    let any_variadic_returns = arena.alloc_pack(TypePackKind::Variadic { ty: primitives.any });
    let variadic_fn = arena.alloc(TypeKind::Function(FunctionType::new(
        any_variadic_args,
        any_variadic_returns,
    )));
    for name in [
        "insert", "remove", "concat", "create", "find", "move", "sort", "pack", "unpack", "freeze",
        "clone", "clear", "isfrozen", "maxn", "foreach", "foreachi", "getn",
    ] {
        table
            .properties
            .insert(name.to_owned(), TableProperty::new(variadic_fn));
    }
    table.properties.insert(
        "insert".to_owned(),
        TableProperty::new(table_insert_type(arena)),
    );
    arena.alloc(TypeKind::Table(table))
}

/// Scaffold `coroutine` library: every method defaults to the permissive
/// `(...any) -> ...any`, with the precise signatures overlaid afterwards for
/// the methods whose call behaviour the checker models cleanly. `wrap`/`yield`
/// deliberately stay on the variadic-fn scaffold — their real generic-pack
/// signatures need result-call inference the checker does not model yet.
fn coroutine_library_type(arena: &mut Arena) -> TypeId {
    let primitives = arena.primitives();
    let mut table = TableType::new(TableState::Sealed);
    table.name = Some("typeof(coroutine)".to_owned());
    let any_variadic_args = arena.alloc_pack(TypePackKind::Variadic { ty: primitives.any });
    let any_variadic_returns = arena.alloc_pack(TypePackKind::Variadic { ty: primitives.any });
    let variadic_fn = arena.alloc(TypeKind::Function(FunctionType::new(
        any_variadic_args,
        any_variadic_returns,
    )));
    for name in [
        "create",
        "resume",
        "running",
        "status",
        "wrap",
        "yield",
        "isyieldable",
        "close",
    ] {
        table
            .properties
            .insert(name.to_owned(), TableProperty::new(variadic_fn));
    }
    arena.alloc(TypeKind::Table(table))
}

fn array_table_type(arena: &mut Arena, value: TypeId) -> TypeId {
    let mut table = TableType::new(TableState::Sealed);
    table.indexer = Some(TableIndexer {
        key: arena.primitives().number,
        value,
        read_only: false,
    });
    arena.alloc(TypeKind::Table(table))
}

fn table_insert_type(arena: &mut Arena) -> TypeId {
    let primitives = arena.primitives();
    let array = array_table_type(arena, primitives.any);
    let tail = arena.alloc_pack(TypePackKind::Variadic { ty: primitives.any });
    let arguments = arena.alloc_pack(TypePackKind::List {
        types: vec![array],
        tail: Some(tail),
    });
    arena.alloc(TypeKind::Function(FunctionType::new(
        arguments,
        arena.empty_pack(),
    )))
}

fn overlay_embedded_table_properties(
    arena: &mut Arena,
    environment: &BuiltinEnvironment,
    table_library: TypeId,
    property_names: &[&str],
) {
    overlay_embedded_library_properties(arena, environment, "table", table_library, property_names);
}

/// Copies the named properties from the parsed declaration module for
/// `global_name` onto a scaffold library table, replacing the variadic-fn
/// placeholders with the precise modelled signatures. Used to lift specific
/// methods of a magic-sensitive global (`table`, `coroutine`) to upstream
/// parity while leaving the rest of the table on the permissive scaffold.
fn overlay_embedded_library_properties(
    arena: &mut Arena,
    environment: &BuiltinEnvironment,
    global_name: &str,
    table_library: TypeId,
    property_names: &[&str],
) {
    let Some(embedded_table) = lower_embedded_global_type(arena, environment, global_name) else {
        return;
    };
    let TypeKind::Table(embedded_table) = arena.get(arena.follow(embedded_table)).clone() else {
        return;
    };
    let TypeKind::Table(mut table) = arena.get(arena.follow(table_library)).clone() else {
        return;
    };

    for name in property_names {
        if let Some(property) = embedded_table.properties.get(*name) {
            table
                .properties
                .insert((*name).to_owned(), property.clone());
        }
    }

    arena.replace(table_library, TypeKind::Table(table));
}

fn overlay_modeled_string_properties(arena: &mut Arena, environment: &BuiltinEnvironment) {
    let Some(string_library) = environment.global("string").map(|global| global.ty) else {
        return;
    };
    let string_library = arena.follow(string_library);
    let TypeKind::Table(mut table) = arena.get(string_library).clone() else {
        return;
    };

    table.name = Some("typeof(string)".to_owned());
    table.properties.insert(
        "byte".to_owned(),
        TableProperty::new(string_byte_type(arena)),
    );
    arena.replace(string_library, TypeKind::Table(table));
}

fn lower_embedded_global_type(
    arena: &mut Arena,
    environment: &BuiltinEnvironment,
    name: &str,
) -> Option<TypeId> {
    let declarations = EmbeddedBuiltinDeclarations::parse(arena, environment, &[])?;
    declarations.lower_global(arena, name)
}

struct EmbeddedBuiltinDeclarations {
    root: Stat,
    scopes: ScopeTree,
    dfg: DataFlowGraph,
}

impl EmbeddedBuiltinDeclarations {
    fn parse(
        arena: &mut Arena,
        environment: &BuiltinEnvironment,
        extra_modules: &[DefinitionModule],
    ) -> Option<Self> {
        let parsed = parse_builtin_definition_modules(extra_modules);
        if !parsed.errors.is_empty() {
            return None;
        }
        let root = parsed.root?;

        let mut scopes = ScopeTree::new();
        let root_scope = scopes.root();
        environment.install_into_scope(&mut scopes, root_scope);
        scopes.populate_statement_bindings(root_scope, &root);
        let dfg = DataFlowGraph::build(&root, &scopes, arena);

        Some(Self { root, scopes, dfg })
    }

    fn lower_global(&self, arena: &mut Arena, name: &str) -> Option<TypeId> {
        let luau_type = declared_global_type(&self.root, name)?;
        let (ty, diagnostics) = lower_type_annotation(
            &luau_type,
            &self.scopes,
            &self.dfg,
            arena,
            AnalysisMode::Strict,
        );
        diagnostics.is_empty().then_some(ty)
    }

    fn lower_type_name(&self, arena: &mut Arena, name: &str) -> Option<TypeId> {
        let luau_type = Type::Reference {
            syntax_id: SyntaxId::default(),
            location: None,
            prefix: None,
            prefix_location: None,
            name: ruau_ast::syntax::Name::new(name),
            name_location: None,
            parameters: Vec::new(),
        };
        let (ty, diagnostics) = lower_type_annotation(
            &luau_type,
            &self.scopes,
            &self.dfg,
            arena,
            AnalysisMode::Strict,
        );
        diagnostics.is_empty().then_some(ty)
    }
}

/// The declared type of global `name`, scanning declarations in reverse so the
/// *last* declaration of a name wins. The concatenated declaration source puts
/// the embedded builtin modules first and extra (host) modules after them, so
/// a host module that redeclares a builtin global overrides the builtin's
/// declaration.
fn declared_global_type(stat: &Stat, name: &str) -> Option<Type> {
    match stat {
        Stat::Block { body, .. } => body
            .iter()
            .rev()
            .find_map(|stat| declared_global_type(stat, name)),
        Stat::DeclareGlobal {
            name: global_name,
            luau_type,
            ..
        } if global_name.as_str() == name => Some((**luau_type).clone()),
        Stat::DeclareFunction {
            location,
            attributes,
            name: function_name,
            generics,
            generic_packs,
            params,
            param_names,
            ret_types,
            ..
        } if function_name.as_str() == name => Some(Type::Function {
            syntax_id: SyntaxId::default(),
            location: *location,
            attributes: attributes.clone(),
            generics: generics.clone(),
            generic_packs: generic_packs.clone(),
            arg_types: params.clone(),
            arg_names: param_names.iter().cloned().map(Some).collect(),
            return_types: (**ret_types).clone(),
        }),
        _ => None,
    }
}

fn declared_global_names_in_modules(modules: &[DefinitionModule]) -> Vec<String> {
    let parsed = parse_definition_modules_source(&normalized_definition_modules_source(modules));
    let Some(root) = parsed.root.filter(|_| parsed.errors.is_empty()) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    collect_declared_global_names(&root, &mut names);
    names
}

fn declared_type_names_in_modules(modules: &[DefinitionModule]) -> Vec<String> {
    let parsed = parse_definition_modules_source(&normalized_definition_modules_source(modules));
    let Some(root) = parsed.root.filter(|_| parsed.errors.is_empty()) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    collect_declared_type_names(&root, &mut names);
    names
}

fn collect_declared_global_names(stat: &Stat, names: &mut Vec<String>) {
    match stat {
        Stat::Block { body, .. } => {
            for stat in body {
                collect_declared_global_names(stat, names);
            }
        }
        Stat::DeclareGlobal { name, .. } | Stat::DeclareFunction { name, .. } => {
            names.push(name.as_str().to_owned());
        }
        _ => {}
    }
}

fn collect_declared_type_names(stat: &Stat, names: &mut Vec<String>) {
    match stat {
        Stat::Block { body, .. } => {
            for stat in body {
                collect_declared_type_names(stat, names);
            }
        }
        Stat::DeclareClass { name, .. } | Stat::TypeAlias { name, .. } => {
            names.push(name.as_str().to_owned());
        }
        _ => {}
    }
}

/// Returns true if `name` is a known method on the `string` library primitive
/// (either explicitly modeled or falling back to `any`).
pub(crate) fn is_string_library_property(name: &str) -> bool {
    matches!(name, "byte" | "find" | "gsub" | "match" | "len" | "lower")
        || STRING_LIBRARY_ANY_PROPERTIES.contains(&name)
}

/// Returns the modeled function type (or `any`) for a `string` library method
/// when accessed on the primitive string type.
pub(crate) fn string_primitive_property_type(arena: &mut Arena, name: &str) -> Option<TypeId> {
    let primitives = arena.primitives();
    match name {
        "byte" => Some(string_byte_type(arena)),
        "len" => Some(function_type(
            arena,
            vec![primitives.string],
            vec![primitives.number],
        )),
        "lower" => Some(function_type(
            arena,
            vec![primitives.string],
            vec![primitives.string],
        )),
        // Precise signature mirroring `string.d.luau`, so method-call syntax
        // (`s:match(p)`) infers the same `string?` as `string.match(s, p)`
        // instead of widening to `any`. `find`/`gsub` keep `any` here: their
        // real overloads (optional init/plain args, function/table replacements)
        // are wider than the modeled signature, and a precise form would reject
        // valid calls.
        "match" => {
            let optional_string =
                arena.alloc(TypeKind::Union(vec![primitives.nil, primitives.string]));
            Some(function_type(
                arena,
                vec![primitives.string, primitives.string],
                vec![optional_string],
            ))
        }
        // `s:gmatch(pattern)` returns an iterator `() -> (...string)`.
        "gmatch" => {
            let variadic_string = arena.alloc_pack(TypePackKind::Variadic {
                ty: primitives.string,
            });
            let iterator = arena.alloc(TypeKind::Function(FunctionType::new(
                arena.empty_pack(),
                variadic_string,
            )));
            Some(function_type(
                arena,
                vec![primitives.string, primitives.string],
                vec![iterator],
            ))
        }
        _ if is_string_library_property(name) => Some(primitives.any),
        _ => None,
    }
}

/// Returns the documentation symbol for a `string` library method when
/// accessed via the primitive string type, if one is known.
pub(crate) fn string_primitive_documentation_symbol(name: &str) -> Option<String> {
    is_string_library_property(name).then(|| luau_global_property_symbol("string", name))
}

/// Returns the element type (`number`) for the `x`/`y`/`z` (and uppercase)
/// fields when accessed on the primitive `vector` type.
pub(crate) fn vector_primitive_property_type(arena: &Arena, name: &str) -> Option<TypeId> {
    matches!(name, "x" | "y" | "z" | "X" | "Y" | "Z").then(|| arena.primitives().number)
}

const STRING_LIBRARY_ANY_PROPERTIES: &[&str] = &[
    "char", "format", "gmatch", "rep", "reverse", "split", "sub", "upper", "pack", "packsize",
    "unpack",
];

// These globals can be represented directly by the parsed declaration modules.
// Magic-sensitive builtins such as `assert`, `coroutine`, `select`, `pcall`,
// `print`, `rawget`, `table`, `unpack`, and `xpcall` stay on their current
// scaffold until the checker grows their upstream-specific behavior.
const PARSED_EMBEDDED_BUILTIN_GLOBALS: &[&str] = &[
    "_G",
    "_VERSION",
    "bit32",
    "buffer",
    "collectgarbage",
    "debug",
    "gcinfo",
    "getfenv",
    "ipairs",
    "integer",
    "loadstring",
    "math",
    "newproxy",
    "os",
    "rawequal",
    "rawlen",
    "rawset",
    "setfenv",
    "string",
    "tonumber",
    "tostring",
    "type",
    "typeof",
    "utf8",
    "vector",
];

/// Builtin declaration modules installed into the standard environment.
pub(crate) const BUILTIN_DEFINITION_MODULES: &[DefinitionModule] = &[
    DefinitionModule::from_static("base", defs::BASE),
    DefinitionModule::from_static("bit32", defs::BIT32),
    DefinitionModule::from_static("math", defs::MATH),
    DefinitionModule::from_static("integer", defs::INTEGER),
    DefinitionModule::from_static("os", defs::OS),
    DefinitionModule::from_static("string", defs::STRING),
    DefinitionModule::from_static("coroutine", defs::COROUTINE),
    DefinitionModule::from_static("table", defs::TABLE),
    DefinitionModule::from_static("debug", defs::DEBUG),
    DefinitionModule::from_static("utf8", defs::UTF8),
    DefinitionModule::from_static("buffer", defs::BUFFER),
    DefinitionModule::from_static("vector", defs::VECTOR),
];

/// Test-only nonstandard declaration modules used by upstream fixture parity
/// tests. Callers opt in explicitly.
#[cfg(any())]
pub const TEST_ROBLOX_DEFINITION_MODULES: &[DefinitionModule] = &[DefinitionModule::from_static(
    "test-roblox",
    include_str!("../upstream/builtins/test/roblox.d.luau"),
)];

/// Parses the portable embedded builtin definitions artifact.
#[must_use]
#[cfg(any())]
pub(crate) fn parse_embedded_builtin_definitions() -> ParseResult {
    parse_builtin_definition_modules(&[])
}

fn parse_builtin_definition_modules(extra_modules: &[DefinitionModule]) -> ParseResult {
    let source = normalized_builtin_definition_modules_source(extra_modules);
    parse_definition_modules_source(&source)
}

fn parse_definition_modules_source(source: &str) -> ParseResult {
    parse_file_with(
        source,
        Options {
            allow_declaration_syntax: true,
            ..Options::default()
        },
        SyntaxFlags {
            luau_integer_type: true,
            luau_type_functions: true,
            luau_extern_read_write_attributes: true,
            ..SyntaxFlags::default()
        },
    )
}

fn normalized_builtin_definition_modules_source(extra_modules: &[DefinitionModule]) -> String {
    normalized_definition_modules_source(&builtin_definition_modules(
        BUILTIN_DEFINITION_MODULES,
        extra_modules,
    ))
}

fn normalized_definition_modules_source(modules: &[DefinitionModule]) -> String {
    builtin_definition_modules_source(modules)
        .replace("number | (T...) -> R...", "number | ((T...) -> R...)")
        .replace("): ((T...) -> R...)?", "): (((T...) -> R...) | nil)")
}

fn builtin_definition_modules(
    standard_modules: &[DefinitionModule],
    extra_modules: &[DefinitionModule],
) -> Vec<DefinitionModule> {
    standard_modules
        .iter()
        .chain(extra_modules)
        .cloned()
        .collect()
}

fn builtin_definition_modules_source(modules: &[DefinitionModule]) -> String {
    modules
        .iter()
        .map(|module| module.source.as_ref())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Parses and installs embedded builtin declaration bindings into a scope.
///
/// Full type elaboration is intentionally staged after relation and statement
/// checking. This pass still exercises Ruau's parser/type-annotation pipeline
/// and records declaration names for later checker stages.
#[cfg(any())]
pub(crate) fn populate_embedded_builtin_bindings(
    scopes: &mut ScopeTree,
    scope: ScopeId,
) -> Vec<Error> {
    let parsed = parse_embedded_builtin_definitions();
    if let Some(root) = &parsed.root {
        scopes.populate_statement_bindings(scope, root);
    }
    parsed.errors
}

#[cfg(any())]
mod tests {
    use super::*;
    use crate::types::TableState;

    #[test]
    fn without_globals_drops_only_the_named_library_globals() {
        let mut arena = Arena::new();
        let full = BuiltinEnvironment::standard(&mut arena);
        assert!(full.global("os").is_some());
        assert!(full.global("buffer").is_some());

        let subset = BuiltinEnvironment::standard(&mut arena).without_globals(["os", "buffer"]);
        // The omitted library globals no longer resolve.
        assert!(subset.global("os").is_none());
        assert!(subset.global("buffer").is_none());
        // Base globals and unrelated libraries remain.
        assert!(subset.global("print").is_some());
        assert!(subset.global("math").is_some());
        // The primitive `buffer` type name is independent of the library global.
        assert!(subset.ty("buffer").is_some());
    }

    #[test]
    fn every_stdlib_library_declaration_type_checks_into_a_table() {
        // The conformance gate's type-checking half: each stdlib
        // `.d.luau` must not merely parse but lower into a concrete table type. A
        // declaration that parses yet fails to lower (e.g. a reference to an
        // undefined type) silently falls back to `any` (`lower_global` returns
        // `None`), which this gate turns into a hard failure.
        let mut arena = Arena::new();
        let builtins = BuiltinEnvironment::standard(&mut arena);
        for library in [
            "bit32",
            "buffer",
            "coroutine",
            "debug",
            "math",
            "os",
            "string",
            "table",
            "utf8",
            "vector",
        ] {
            let global = builtins
                .global(library)
                .unwrap_or_else(|| panic!("{library} global is not declared"));
            let kind = arena.get(arena.follow(global.ty));
            assert!(
                matches!(kind, TypeKind::Table(_)),
                "{library} .d.luau did not type-check into a table (got {kind:?}); a lowering \
                 failure silently degrades it to `any`"
            );
        }
    }

    #[test]
    fn standard_environment_installs_builtin_roots() {
        let mut arena = Arena::new();
        let primitives = arena.primitives();
        let builtins = BuiltinEnvironment::standard(&mut arena);
        let mut scopes = ScopeTree::new();
        let root = scopes.root();

        builtins.install_into_scope(&mut scopes, root);

        assert_eq!(builtins.ty("number").unwrap().ty, primitives.number);
        assert_eq!(builtins.global("print").unwrap().ty, primitives.any);
        let math_global = builtins.global("math").unwrap();
        assert!(
            matches!(arena.get(math_global.ty), TypeKind::Table(_)),
            "math is now a sealed table type, got {:?}",
            arena.get(math_global.ty)
        );
        assert_eq!(builtins.global("rawget").unwrap().ty, primitives.any);
        let rawlen_global = builtins.global("rawlen").unwrap();
        assert!(
            matches!(
                arena.get(arena.follow(rawlen_global.ty)),
                TypeKind::Function(_)
            ),
            "rawlen should be lowered from parsed builtin declarations, got {:?}",
            arena.get(arena.follow(rawlen_global.ty))
        );
        let tostring_global = builtins.global("tostring").unwrap();
        let TypeKind::Function(tostring) = arena.get(arena.follow(tostring_global.ty)) else {
            panic!(
                "tostring should be lowered from parsed builtin declarations, got {:?}",
                arena.get(arena.follow(tostring_global.ty))
            );
        };
        assert_eq!(
            arena.normalize_pack(tostring.returns).types,
            vec![primitives.string]
        );
        let table_global = builtins.global("table").unwrap();
        assert!(
            matches!(arena.get(table_global.ty), TypeKind::Table(_)),
            "table is now a sealed table type, got {:?}",
            arena.get(table_global.ty)
        );
        assert!(builtins.global("string").is_some());
        assert_eq!(
            scopes.lookup_global(root, "print").unwrap().kind,
            ValueBindingKind::Builtin
        );
        assert_eq!(
            scopes
                .lookup_type_with_scope(root, "string")
                .unwrap()
                .1
                .kind,
            TypeBindingKind::BuiltinType
        );
        assert_eq!(
            scopes.lookup_type_with_scope(root, "string").unwrap().1.ty,
            Some(primitives.string)
        );
    }

    #[test]
    fn embedded_builtin_artifact_parses_and_populates_declarations() {
        let parsed = parse_embedded_builtin_definitions();
        assert!(
            parsed.root.is_some(),
            "embedded builtins should parse a root block"
        );
        assert!(
            parsed.errors.is_empty(),
            "embedded builtins should parse cleanly: {:?}",
            parsed.errors
        );

        let mut scopes = ScopeTree::new();
        let root = scopes.root();
        let errors = populate_embedded_builtin_bindings(&mut scopes, root);

        assert!(errors.is_empty());
        assert_eq!(
            scopes.lookup_global(root, "require").unwrap().kind,
            ValueBindingKind::DeclaredFunction
        );
        assert_eq!(
            scopes.lookup_global(root, "math").unwrap().kind,
            ValueBindingKind::Global
        );
        assert_eq!(
            scopes
                .lookup_type_with_scope(root, "DateTypeArg")
                .unwrap()
                .1
                .kind,
            TypeBindingKind::TypeAlias
        );
    }

    #[test]
    fn standard_environment_attaches_builtin_documentation_symbols() {
        ruau_upstream::upstream_case!(
            "BuiltinDefinitions.test.cpp::BuiltinDefinitionsTest::lib_documentation_symbols"
        );

        let mut arena = Arena::new();
        let builtins = BuiltinEnvironment::standard(&mut arena);

        assert!(
            builtins.globals().next().is_some(),
            "standard builtins should install globals"
        );
        for global in builtins.globals() {
            let expected = luau_global_symbol(&global.name);
            assert_eq!(
                global.documentation_symbol.as_deref(),
                Some(expected.as_str())
            );
            assert_property_documentation_symbols(&arena, global.ty, &expected);
        }
    }

    fn assert_property_documentation_symbols(arena: &Arena, ty: TypeId, base_symbol: &str) {
        match arena.get(arena.follow(ty)) {
            TypeKind::Table(table) => {
                for (name, property) in &table.properties {
                    let expected = format!("{base_symbol}.{name}");
                    assert_eq!(
                        property.documentation_symbol.as_deref(),
                        Some(expected.as_str())
                    );
                }
            }
            TypeKind::Extern { properties, .. } => {
                for (name, property) in properties {
                    let expected = format!("{base_symbol}.{name}");
                    assert_eq!(
                        property.documentation_symbol.as_deref(),
                        Some(expected.as_str())
                    );
                }
            }
            _ => {}
        }
    }

    #[test]
    fn standard_environment_builtin_tables_are_sealed() {
        ruau_upstream::upstream_case!(
            "TypeInfer.builtins.test.cpp::BuiltinTests::builtin_tables_sealed"
        );

        let mut arena = Arena::new();
        let builtins = BuiltinEnvironment::standard(&mut arena);
        let bit32 = builtins.global("bit32").unwrap().ty;
        let TypeKind::Table(table) = arena.get(arena.follow(bit32)) else {
            panic!("bit32 should be a table library type");
        };
        assert_eq!(table.state, TableState::Sealed);
    }

    #[test]
    fn standard_environment_overlays_table_declaration_signatures() {
        let mut arena = Arena::new();
        let builtins = BuiltinEnvironment::standard(&mut arena);
        let table = builtins.global("table").unwrap().ty;
        let TypeKind::Table(table) = arena.get(arena.follow(table)) else {
            panic!("table should be a table library type");
        };
        let insert = table.properties.get("insert").unwrap().ty;
        let TypeKind::Intersection(insert_overloads) = arena.get(arena.follow(insert)) else {
            panic!("table.insert should come from the embedded overload declaration");
        };
        assert_eq!(insert_overloads.len(), 2);
        let concat = table.properties.get("concat").unwrap().ty;
        let TypeKind::Function(concat) = arena.get(arena.follow(concat)) else {
            panic!("table.concat should come from the embedded function declaration");
        };
        let string = arena.primitives().string;
        assert!(matches!(
            arena.get_pack(arena.follow_pack(concat.returns)),
            TypePackKind::List { types, tail: None } if types.as_slice() == [string]
        ));
        let sort = table.properties.get("sort").unwrap().ty;
        let TypeKind::Function(sort) = arena.get(arena.follow(sort)) else {
            panic!("table.sort should come from the embedded function declaration");
        };
        assert_eq!(sort.generics.len(), 1);
        let clear = table.properties.get("clear").unwrap().ty;
        let TypeKind::Function(clear) = arena.get(arena.follow(clear)) else {
            panic!("table.clear should come from the embedded function declaration");
        };
        assert!(matches!(
            arena.get_pack(arena.follow_pack(clear.returns)),
            TypePackKind::List { types, tail: None } if types.is_empty()
        ));
        let freeze = table.properties.get("freeze").unwrap().ty;
        let TypeKind::Function(freeze) = arena.get(arena.follow(freeze)) else {
            panic!("table.freeze should come from the embedded function declaration");
        };
        assert_eq!(freeze.generics.len(), 1);
    }

    #[test]
    fn standard_environment_models_string_byte_as_variadic_number() {
        let mut arena = Arena::new();
        let builtins = BuiltinEnvironment::standard(&mut arena);
        let string = builtins.global("string").unwrap().ty;
        let TypeKind::Table(string) = arena.get(arena.follow(string)) else {
            panic!("string should be a table library type");
        };
        let byte = string.properties.get("byte").unwrap().ty;
        let TypeKind::Function(byte) = arena.get(arena.follow(byte)) else {
            panic!("string.byte should be a modeled function");
        };
        assert!(matches!(
            arena.get_pack(arena.follow_pack(byte.returns)),
            TypePackKind::Variadic { ty } if arena.follow(*ty) == arena.primitives().number
        ));
    }

    #[test]
    fn standard_environment_accepts_opt_in_test_roblox_module() {
        let mut arena = Arena::new();
        let builtins = BuiltinEnvironment::standard_with_definition_modules(
            &mut arena,
            TEST_ROBLOX_DEFINITION_MODULES,
        );

        assert!(builtins.global("game").is_some());
        assert!(builtins.global("workspace").is_some());
        assert!(builtins.global("script").is_some());
        assert!(builtins.ty("Instance").is_some());
        assert!(builtins.ty("Part").is_some());
        assert!(builtins.ty("Workspace").is_some());
    }
}
