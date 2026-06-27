//! Deterministic type and type-pack summary rendering.

use std::collections::BTreeMap;

use super::{
    Arena, FunctionType, GenericType, GenericTypePack, PrimitiveType, SingletonType, TableState,
    TableType, TypeId, TypeKind, TypeLevel, TypePackId, TypePackKind, TypePackTail, TypeVariable,
    is_top_function_type,
};

/// Options for deterministic type summaries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SummaryOptions {
    /// Print table and composite operands over multiple lines when useful.
    pub use_line_breaks: bool,
    /// Print function argument names when function types retained them.
    pub function_type_arguments: bool,
    /// Hide table properties whose type is the internal error recovery type.
    pub hide_error_properties: bool,
    /// Include `@checked` before checked function types.
    pub show_checked_function_marker: bool,
    /// Maximum composite operand count that remains on one line.
    pub composite_types_single_line_limit: usize,
    /// Approximate maximum rendered table length before truncating properties.
    pub max_table_length: Option<usize>,
    /// Approximate maximum rendered type length before truncating output.
    pub max_type_length: Option<usize>,
    /// Render named table aliases as their structure (upstream's
    /// `ToStringOptions{true}` exhaustive mode), expanding `Test`/`wrap<string>`
    /// to `{ a: number }`/`{ a: string? }`. Recursion is still broken by the
    /// active type stack, which renders the alias name on re-entry.
    pub expand_aliases: bool,
}

impl Default for SummaryOptions {
    fn default() -> Self {
        Self {
            use_line_breaks: false,
            function_type_arguments: false,
            hide_error_properties: false,
            show_checked_function_marker: true,
            composite_types_single_line_limit: usize::MAX,
            max_table_length: None,
            max_type_length: None,
            expand_aliases: false,
        }
    }
}

/// Options for [`Arena::named_function_summary`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FunctionSummaryOptions {
    /// Hide top-level type parameters.
    pub hide_type_parameters: bool,
    /// Hide an implicit leading self argument.
    pub hide_self_argument: bool,
    /// Override positional argument names.
    pub override_argument_names: Vec<String>,
}

impl Arena {
    /// Returns a deterministic human-readable type summary.
    #[must_use]
    pub fn summary(&self, id: TypeId) -> String {
        let mut renderer = TypeSummary::new(self);
        renderer.render_type(id)
    }

    /// Returns a deterministic type summary with rendering options.
    #[must_use]
    pub fn summary_with_options(&self, id: TypeId, options: SummaryOptions) -> String {
        let mut renderer = TypeSummary::with_options(self, options);
        truncate_type_summary(renderer.render_type(id), options.max_type_length)
    }

    /// Returns a deterministic summary for a function as it appears in a named
    /// declaration.
    #[must_use]
    #[cfg(any())]
    pub fn named_function_summary(
        &self,
        name: &str,
        function: TypeId,
        options: &FunctionSummaryOptions,
    ) -> Option<String> {
        let TypeKind::Function(function) = self.get(function) else {
            return None;
        };
        let mut renderer = TypeSummary::new(self);
        Some(renderer.named_function_summary(name, function, options))
    }

    /// Returns a deterministic human-readable type-pack summary.
    #[must_use]
    pub fn pack_summary(&self, id: TypePackId) -> String {
        let mut renderer = TypeSummary::new(self);
        renderer.render_pack(id)
    }

    /// Returns a deterministic type-pack summary as a top-level pack value.
    #[must_use]
    #[cfg(any())]
    pub fn pack_summary_parenthesized(&self, id: TypePackId) -> String {
        let summary = self.pack_summary(id);
        format!("({summary})")
    }
}

/// Renderer for deterministic debug summaries.
struct TypeSummary<'arena> {
    /// Type arena being rendered.
    arena: &'arena Arena,
    /// Rendering options.
    options: SummaryOptions,
    /// Active type stack used to break cycles.
    type_stack: Vec<TypeId>,
    /// Active pack stack used to break cycles.
    pack_stack: Vec<TypePackId>,
    /// Assigned names for recursive type definitions.
    type_cycle_names: BTreeMap<TypeId, String>,
    /// Assigned names for recursive type-pack definitions.
    pack_cycle_names: BTreeMap<TypePackId, String>,
    /// Rendered recursive type definitions.
    type_cycle_definitions: BTreeMap<TypeId, String>,
    /// Rendered recursive type-pack definitions.
    pack_cycle_definitions: BTreeMap<TypePackId, String>,
    /// Recursive definitions in first-discovery order.
    where_order: Vec<WhereEntry>,
}

/// Recursive where-clause entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WhereEntry {
    /// Type definition.
    Type(TypeId),
    /// Type-pack definition.
    Pack(TypePackId),
}

/// Composite type render context.
#[derive(Clone, Copy, Eq, PartialEq)]
enum CompositeKind {
    /// Union context.
    Union,
    /// Intersection context.
    Intersection,
}

impl<'arena> TypeSummary<'arena> {
    /// Creates a renderer.
    fn new(arena: &'arena Arena) -> Self {
        Self::with_options(arena, SummaryOptions::default())
    }

    /// Creates a renderer with options.
    fn with_options(arena: &'arena Arena, options: SummaryOptions) -> Self {
        Self {
            arena,
            options,
            type_stack: Vec::new(),
            pack_stack: Vec::new(),
            type_cycle_names: BTreeMap::new(),
            pack_cycle_names: BTreeMap::new(),
            type_cycle_definitions: BTreeMap::new(),
            pack_cycle_definitions: BTreeMap::new(),
            where_order: Vec::new(),
        }
    }

    /// Renders a top-level type and appends recursive definitions.
    fn render_type(&mut self, id: TypeId) -> String {
        let summary = self.type_summary(id);
        self.with_where_clause(summary)
    }

    /// Renders a top-level type pack and appends recursive definitions.
    fn render_pack(&mut self, id: TypePackId) -> String {
        let summary = self.pack_summary(id);
        self.with_where_clause(summary)
    }

    /// Renders one type.
    fn type_summary(&mut self, id: TypeId) -> String {
        let followed = self.arena.follow(id);
        if followed != id {
            return self.type_summary(followed);
        }

        if let Some(name) = self.active_metatable_cycle_name(id) {
            return name;
        }

        if self.type_stack.contains(&id) {
            if let TypeKind::Table(TableType {
                name: Some(name), ..
            }) = self.arena.get(id)
            {
                return name.clone();
            }
            return self.type_cycle_name(id);
        }

        self.type_stack.push(id);
        let summary = match self.arena.get(id) {
            TypeKind::Primitive(primitive) => primitive.as_str().to_owned(),
            TypeKind::Singleton(SingletonType::Boolean(value)) => value.to_string(),
            TypeKind::Singleton(SingletonType::String(value)) => format!("{value:?}"),
            TypeKind::Function(function) => self.function_summary(function),
            TypeKind::Table(table) => self.table_summary(table),
            TypeKind::Extern { name, .. } => name.clone(),
            TypeKind::Metatable {
                table,
                metatable,
                name,
            } => match name {
                Some(name) if !self.options.expand_aliases => name.clone(),
                _ => format!(
                    "{{ @metatable {}, {} }}",
                    self.type_summary(*metatable),
                    self.type_summary(*table)
                ),
            },
            TypeKind::TypeFunctionInstance { name, arguments } => {
                self.type_function_summary(name, arguments)
            }
            TypeKind::Union(types) => {
                self.composite_summary(id, types, " | ", CompositeKind::Union)
            }
            TypeKind::Intersection(types) => {
                self.composite_summary(id, types, " & ", CompositeKind::Intersection)
            }
            TypeKind::Negation(ty) => self.negation_summary(*ty),
            TypeKind::Bound(bound) => self.type_summary(*bound),
            TypeKind::Free(variable) => self.free_type_summary(variable),
            TypeKind::Blocked(blocked) => match &blocked.reason {
                Some(reason) => format!("*blocked:{reason}*"),
                None => "*blocked*".to_owned(),
            },
            TypeKind::Generic(generic) => generic.name.clone(),
            TypeKind::Error => "*error-type*".to_owned(),
            TypeKind::Unknown => "unknown".to_owned(),
            TypeKind::Never => "never".to_owned(),
            TypeKind::Any => "any".to_owned(),
        };
        self.type_stack.pop();
        if let Some(name) = self.type_cycle_names.get(&id).cloned() {
            self.type_cycle_definitions.entry(id).or_insert(summary);
            name
        } else {
            summary
        }
    }

    fn active_metatable_cycle_name(&mut self, id: TypeId) -> Option<String> {
        let TypeKind::Metatable {
            table,
            metatable,
            name: None,
        } = self.arena.get(id)
        else {
            return None;
        };
        let table = self.arena.follow(*table);
        let metatable = self.arena.follow(*metatable);
        let active = self.type_stack.iter().copied().rev().find(|active| {
            let TypeKind::Metatable {
                table: active_table,
                metatable: active_metatable,
                name: None,
            } = self.arena.get(*active)
            else {
                return false;
            };
            self.arena.follow(*active_table) == table
                && self.arena.follow(*active_metatable) == metatable
        })?;

        Some(self.type_cycle_name(active))
    }

    /// Renders one pack.
    fn pack_summary(&mut self, id: TypePackId) -> String {
        if self.pack_stack.contains(&id) {
            return self.pack_cycle_name(id);
        }

        self.pack_stack.push(id);
        let summary = match self.arena.get_pack(id) {
            TypePackKind::List { types, tail } => self.list_pack_summary(types, *tail),
            TypePackKind::Variadic { ty } => format!("...{}", self.type_summary(*ty)),
            TypePackKind::Free { level, name } => pack_variable_summary(*level, name),
            TypePackKind::Generic(pack) => generic_pack_summary(pack),
            TypePackKind::Bound(bound) => self.pack_summary(*bound),
            TypePackKind::Error => "...*error-type*".to_owned(),
        };
        self.pack_stack.pop();
        if let Some(name) = self.pack_cycle_names.get(&id).cloned() {
            self.pack_cycle_definitions.entry(id).or_insert(summary);
            name
        } else {
            summary
        }
    }

    /// Renders a function type.
    fn function_summary(&mut self, function: &FunctionType) -> String {
        if is_top_function_type(self.arena, function) {
            return "function".to_owned();
        }

        let generics = generic_prefix(&function.generics, &function.generic_packs);
        let checked = if self.options.show_checked_function_marker && function.is_checked {
            "@checked "
        } else {
            ""
        };
        format!(
            "{generics}{checked}({}) -> {}",
            self.function_argument_pack_summary(function),
            self.function_return_summary(function.returns)
        )
    }

    /// Renders a function as a named declaration.
    #[cfg(any())]
    fn named_function_summary(
        &mut self,
        name: &str,
        function: &FunctionType,
        options: &FunctionSummaryOptions,
    ) -> String {
        let generics = if options.hide_type_parameters {
            String::new()
        } else {
            generic_prefix(&function.generics, &function.generic_packs)
        };
        let arguments = self.named_function_argument_summary(function, options);
        let returns = self.named_function_return_summary(function.returns);
        format!("{name}{generics}({arguments}): {returns}")
    }

    /// Renders ordinary function arguments.
    fn function_argument_pack_summary(&mut self, function: &FunctionType) -> String {
        if !self.options.function_type_arguments {
            return self.pack_summary(function.arguments);
        }

        self.argument_pack_summary(function.arguments, &function.argument_names, false)
    }

    /// Renders function returns with Luau's single-return shorthand.
    fn function_return_summary(&mut self, returns: TypePackId) -> String {
        let normalized = self.arena.normalize_pack(returns);
        if normalized.types.len() == 1 && normalized.tail.is_none() {
            return self.type_summary(normalized.types[0]);
        }

        format!("({})", self.pack_summary(returns))
    }

    /// Renders named function returns using Luau's declaration spelling.
    #[cfg(any())]
    fn named_function_return_summary(&mut self, returns: TypePackId) -> String {
        let normalized = self.arena.normalize_pack(returns);
        let needs_wrap = normalized.types.len() > 1
            || (normalized.tail.is_some() && !normalized.types.is_empty())
            || (normalized.types.is_empty() && normalized.tail.is_none());

        if needs_wrap {
            format!("({})", self.pack_summary(returns))
        } else {
            self.pack_summary(returns)
        }
    }

    /// Renders named function argument lists.
    #[cfg(any())]
    fn named_function_argument_summary(
        &mut self,
        function: &FunctionType,
        options: &FunctionSummaryOptions,
    ) -> String {
        let names = effective_named_argument_names(function, options);
        let skipped = usize::from(function.has_self && options.hide_self_argument);
        self.argument_pack_summary_from_index(function.arguments, &names, true, skipped)
    }

    /// Renders a free type variable.
    fn free_type_summary(&mut self, variable: &TypeVariable) -> String {
        let Some(upper_bound) = variable.upper_bound else {
            return variable_summary("free", variable.level, &variable.name);
        };

        let name = variable
            .name
            .as_deref()
            .map(|name| format!("'{name}"))
            .unwrap_or_else(|| format!("free@{}", variable.level.0));

        if let Some(lower_bound) = variable.lower_bound {
            format!(
                "({} <: {} <: {})",
                self.type_summary(lower_bound),
                name,
                self.type_summary(upper_bound)
            )
        } else {
            format!("({name} <: {})", self.type_summary(upper_bound))
        }
    }

    fn negation_summary(&mut self, ty: TypeId) -> String {
        let ty = self.arena.follow(ty);
        if self.is_falsey_union(ty) {
            return "~(false?)".to_owned();
        }
        let summary = self.type_summary(ty);
        match self.arena.get(ty) {
            TypeKind::Union(_) | TypeKind::Intersection(_) => format!("~({summary})"),
            _ => format!("~{summary}"),
        }
    }

    fn is_falsey_union(&self, ty: TypeId) -> bool {
        let TypeKind::Union(options) = self.arena.get(self.arena.follow(ty)) else {
            return false;
        };
        if options.len() != 2 {
            return false;
        }
        let mut saw_nil = false;
        let mut saw_false = false;
        for option in options {
            match self.arena.get(self.arena.follow(*option)) {
                TypeKind::Primitive(PrimitiveType::Nil) => saw_nil = true,
                TypeKind::Singleton(SingletonType::Boolean(false)) => saw_false = true,
                _ => return false,
            }
        }
        saw_nil && saw_false
    }

    /// Renders a type-function instance, reducing concrete display-only cases
    /// without allocating new arena nodes.
    fn type_function_summary(&mut self, name: &str, arguments: &[TypeId]) -> String {
        if name == "keyof"
            && let [target] = arguments
        {
            match self.arena.get(self.arena.follow(*target)).clone() {
                TypeKind::Table(table) => return self.keyof_table_summary(&table),
                TypeKind::Never => return "never".to_owned(),
                TypeKind::Bound(_) => unreachable!("follow removes bound types"),
                _ => {}
            }
        }

        format!("{}<{}>", name, self.join_types(arguments, ", "))
    }

    fn keyof_table_summary(&mut self, table: &TableType) -> String {
        let mut parts =
            Vec::with_capacity(table.properties.len() + usize::from(table.indexer.is_some()));
        for name in table.properties.keys() {
            parts.push(format!("{name:?}"));
        }
        if let Some(indexer) = &table.indexer {
            let key = self.arena.follow(indexer.key);
            if matches!(
                self.arena.get(key),
                TypeKind::Primitive(PrimitiveType::String)
            ) {
                parts.retain(|part| !part.starts_with('"'));
            }
            parts.push(self.type_summary(key));
        }
        parts.sort();
        parts.dedup();
        if parts.is_empty() {
            "never".to_owned()
        } else {
            parts.join(" | ")
        }
    }

    /// Renders a table type.
    fn table_summary(&mut self, table: &TableType) -> String {
        if let Some(name) = table.name.as_ref().filter(|_| !self.options.expand_aliases) {
            if table.instantiated_type_params.is_empty()
                && table.instantiated_type_pack_params.is_empty()
            {
                return name.clone();
            }

            let mut params: Vec<String> = table
                .instantiated_type_params
                .iter()
                .map(|ty| self.type_summary(*ty))
                .collect();
            let pack_params = table.instantiated_type_pack_params.clone();
            // A single type-pack argument renders flat (`Y<string, number>`);
            // with multiple packs, each *list* pack is parenthesized to
            // disambiguate boundaries (`Y<(number, string), (boolean)>`), but a
            // bare variadic stays unparenthesized — its `...` already marks it
            // (`Y<number, number, ...number, (number, number, ...number)>`).
            let parenthesize = pack_params.len() >= 2;
            for pack in pack_params {
                let bare_variadic = matches!(
                    self.arena.get_pack(self.arena.follow_pack(pack)),
                    TypePackKind::Variadic { .. }
                );
                let rendered = self.pack_summary(pack);
                if parenthesize && !bare_variadic {
                    // Parenthesized packs keep their boundary even when empty
                    // (`Y<(), ()>`).
                    params.push(format!("({rendered})"));
                } else if rendered.is_empty() {
                    // A lone empty type-pack argument (`Packed<number>` where the
                    // alias's trailing `U...` resolves to `()`) is omitted —
                    // keeping it would render a stray `Packed<number, >`.
                    continue;
                } else {
                    params.push(rendered);
                }
            }
            return format!("{}<{}>", name, params.join(", "));
        }

        if table.properties.is_empty()
            && let Some(indexer) = &table.indexer
            && self.arena.get(indexer.key) == &TypeKind::Primitive(PrimitiveType::Number)
        {
            if indexer.read_only {
                return format!("{{read {}}}", self.type_summary(indexer.value));
            }
            return format!("{{{}}}", self.type_summary(indexer.value));
        }

        let (open, close) = table_braces(table.state);
        let mut parts = Vec::new();
        for (name, property) in &table.properties {
            if self.options.hide_error_properties && self.is_error_type(property.ty) {
                continue;
            }
            let prefix = if property.read_only {
                "read "
            } else if property.write_only {
                "write "
            } else {
                ""
            };
            parts.push(format!(
                "{prefix}{name}: {}",
                self.type_summary(property.ty)
            ));
        }
        if let Some(indexer) = &table.indexer {
            let prefix = if indexer.read_only { "read " } else { "" };
            parts.push(format!(
                "{prefix}[{}]: {}",
                self.type_summary(indexer.key),
                self.type_summary(indexer.value)
            ));
        }
        if parts.is_empty() {
            return format!("{open}  {close}");
        }

        let total = parts.len();
        if let Some(limit) = self.options.max_table_length {
            parts = truncate_table_parts(parts, open, close, limit);
            if parts.len() < total {
                parts.push(format!("... {} more ...", total - parts.len()));
            }
        }

        if self.options.use_line_breaks {
            format!("{open}\n    {}\n{close}", parts.join(",\n    "))
        } else {
            format!("{open} {} {close}", parts.join(", "))
        }
    }

    fn is_error_type(&self, ty: TypeId) -> bool {
        matches!(self.arena.get(self.arena.follow(ty)), TypeKind::Error)
    }

    /// Renders a joined list of types.
    fn join_types(&mut self, types: &[TypeId], separator: &str) -> String {
        if types.is_empty() {
            return "{}".to_owned();
        }
        types
            .iter()
            .map(|ty| self.type_summary(*ty))
            .collect::<Vec<_>>()
            .join(separator)
    }

    /// Renders a union or intersection with child parentheses where upstream
    /// rendering treats nested composite/function types as grouped operands.
    fn composite_summary(
        &mut self,
        id: TypeId,
        types: &[TypeId],
        separator: &str,
        kind: CompositeKind,
    ) -> String {
        if types.is_empty() {
            return "{}".to_owned();
        }
        if kind == CompositeKind::Union
            && let Some(optional) = self.optional_union_summary(types)
        {
            return optional;
        }

        let mut rendered = Vec::new();
        for ty in types {
            if kind == CompositeKind::Union && *ty == id {
                let _ = self.type_cycle_name(id);
                continue;
            }
            let child = self.type_summary(*ty);
            let child_is_cycle_name = self
                .type_cycle_names
                .get(ty)
                .is_some_and(|name| name == &child);
            let followed = self.arena.follow(*ty);
            let needs_parens = match (kind, self.arena.get(followed)) {
                _ if child_is_cycle_name => false,
                (_, TypeKind::Function(function)) => !is_top_function_type(self.arena, function),
                (CompositeKind::Union, TypeKind::Intersection(_))
                | (CompositeKind::Intersection, TypeKind::Union(_)) => true,
                _ => false,
            };
            if needs_parens {
                rendered.push(format!("({child})"));
            } else {
                rendered.push(child);
            }
        }
        rendered.sort();
        if kind == CompositeKind::Union {
            rendered.dedup();
        }
        let force_multiline = self.options.use_line_breaks
            && (rendered.len() > self.options.composite_types_single_line_limit
                || types
                    .iter()
                    .any(|ty| matches!(self.arena.get(*ty), TypeKind::Function(_))));
        if force_multiline {
            return rendered
                .into_iter()
                .enumerate()
                .map(|(index, item)| {
                    if index == 0 {
                        item
                    } else {
                        format!("{} {item}", separator.trim())
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
        rendered.join(separator)
    }

    fn optional_union_summary(&mut self, types: &[TypeId]) -> Option<String> {
        let [left, right] = types else {
            return None;
        };
        let value = match (self.is_nil_type(*left), self.is_nil_type(*right)) {
            (true, false) => *right,
            (false, true) => *left,
            _ => return None,
        };
        let summary = self.type_summary(value);
        let value_is_cycle_name = self
            .type_cycle_names
            .get(&value)
            .is_some_and(|name| name == &summary);
        let needs_parens = match self.arena.get(value) {
            _ if value_is_cycle_name => false,
            TypeKind::Function(function) => !is_top_function_type(self.arena, function),
            TypeKind::Union(_) | TypeKind::Intersection(_) => true,
            _ => return None,
        };
        Some(if needs_parens {
            format!("({summary})?")
        } else {
            format!("{summary}?")
        })
    }

    fn is_nil_type(&self, ty: TypeId) -> bool {
        matches!(
            self.arena.get(self.arena.follow(ty)),
            TypeKind::Primitive(PrimitiveType::Nil)
        )
    }

    /// Returns or assigns a recursive type name.
    fn type_cycle_name(&mut self, id: TypeId) -> String {
        if let Some(name) = self.type_cycle_names.get(&id) {
            return name.clone();
        }
        let name = format!("t{}", self.type_cycle_names.len() + 1);
        self.type_cycle_names.insert(id, name.clone());
        self.where_order.push(WhereEntry::Type(id));
        name
    }

    /// Returns or assigns a recursive type-pack name.
    fn pack_cycle_name(&mut self, id: TypePackId) -> String {
        if let Some(name) = self.pack_cycle_names.get(&id) {
            return name.clone();
        }
        let name = format!("tp{}", self.pack_cycle_names.len() + 1);
        self.pack_cycle_names.insert(id, name.clone());
        self.where_order.push(WhereEntry::Pack(id));
        name
    }

    /// Appends recursive `where` definitions to a rendered root.
    fn with_where_clause(&self, summary: String) -> String {
        let definitions = self
            .where_order
            .iter()
            .filter_map(|entry| match entry {
                WhereEntry::Type(id) => self
                    .type_cycle_names
                    .get(id)
                    .zip(self.type_cycle_definitions.get(id))
                    .map(|(name, definition)| format!("{name} = {definition}")),
                WhereEntry::Pack(id) => self
                    .pack_cycle_names
                    .get(id)
                    .zip(self.pack_cycle_definitions.get(id))
                    .map(|(name, definition)| format!("{name} = {definition}")),
            })
            .collect::<Vec<_>>();

        if definitions.is_empty() {
            summary
        } else {
            format!("{summary} where {}", definitions.join(" ; "))
        }
    }

    /// Renders a list pack.
    fn list_pack_summary(&mut self, types: &[TypeId], tail: Option<TypePackId>) -> String {
        let mut parts = types
            .iter()
            .map(|ty| self.type_summary(*ty))
            .collect::<Vec<_>>();
        if let Some(tail) = tail {
            parts.push(self.pack_summary(tail));
        }
        parts.join(", ")
    }

    /// Renders an argument pack with optional source argument names.
    fn argument_pack_summary(
        &mut self,
        pack: TypePackId,
        names: &[Option<String>],
        fill_missing_names: bool,
    ) -> String {
        self.argument_pack_summary_from_index(pack, names, fill_missing_names, 0)
    }

    /// Renders an argument pack with optional source argument names, skipping a
    /// prefix of concrete arguments when a named-function view hides `self`.
    fn argument_pack_summary_from_index(
        &mut self,
        pack: TypePackId,
        names: &[Option<String>],
        fill_missing_names: bool,
        skip: usize,
    ) -> String {
        let normalized = self.arena.normalize_pack(pack);
        let mut parts = Vec::new();
        for (index, ty) in normalized.types.iter().enumerate().skip(skip) {
            let ty = self.type_summary(*ty);
            match names.get(index).and_then(Option::as_ref) {
                Some(name) => parts.push(format!("{name}: {ty}")),
                None if fill_missing_names => parts.push(format!("_: {ty}")),
                None => parts.push(ty),
            }
        }
        if let Some(tail) = normalized.tail {
            let tail = self.pack_tail_summary(&tail);
            if fill_missing_names {
                let text = if matches!(tail.kind, PackTailRenderKind::Variadic) {
                    tail.text.trim_start_matches("...").to_owned()
                } else {
                    tail.text
                };
                parts.push(format!("...: {text}"));
            } else {
                parts.push(tail.text);
            }
        }
        parts.join(", ")
    }

    /// Renders a normalized pack tail.
    fn pack_tail_summary(&mut self, tail: &TypePackTail) -> PackTailRender {
        match tail {
            TypePackTail::Free { level, name } => PackTailRender {
                text: pack_variable_summary(*level, name),
                kind: PackTailRenderKind::Free,
            },
            TypePackTail::Generic(pack) => PackTailRender {
                text: generic_pack_summary(pack),
                kind: PackTailRenderKind::Generic,
            },
            TypePackTail::Variadic(ty) => PackTailRender {
                text: format!("...{}", self.type_summary(*ty)),
                kind: PackTailRenderKind::Variadic,
            },
            TypePackTail::Error => PackTailRender {
                text: "...*error-type*".to_owned(),
                kind: PackTailRenderKind::Error,
            },
            TypePackTail::Cycle(id) => PackTailRender {
                text: format!("<pack-cycle:{}>", id.index()),
                kind: PackTailRenderKind::Cycle,
            },
        }
    }
}

struct PackTailRender {
    text: String,
    kind: PackTailRenderKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PackTailRenderKind {
    Free,
    Generic,
    Variadic,
    Error,
    Cycle,
}

/// Renders a free variable name.
fn variable_summary(kind: &str, level: TypeLevel, name: &Option<String>) -> String {
    match name {
        Some(name) => format!("'{name}"),
        None => format!("{kind}@{}", level.0),
    }
}

/// Renders a free type-pack variable name.
fn pack_variable_summary(level: TypeLevel, name: &Option<String>) -> String {
    match name {
        Some(name) => format!("{name}..."),
        None => format!("freepack@{}...", level.0),
    }
}

/// Renders a generic type-pack parameter.
fn generic_pack_summary(pack: &GenericTypePack) -> String {
    format!("{}...", pack.name)
}

/// Returns Luau table delimiters for a table state.
fn table_braces(state: TableState) -> (&'static str, &'static str) {
    match state {
        TableState::Sealed => ("{", "}"),
        TableState::Unsealed | TableState::Generic | TableState::Free => ("{|", "|}"),
    }
}

/// Truncates table parts to fit an approximate rendered length.
fn truncate_table_parts(
    parts: Vec<String>,
    open: &str,
    close: &str,
    max_len: usize,
) -> Vec<String> {
    let mut kept = Vec::new();
    for part in parts {
        let candidate_len = format!(
            "{open} {} {close}",
            kept.iter()
                .chain([&part])
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        )
        .len();
        if !kept.is_empty() && candidate_len > max_len {
            break;
        }
        kept.push(part);
    }
    kept
}

/// Truncates a top-level type summary using Luau's visible marker.
fn truncate_type_summary(summary: String, max_len: Option<usize>) -> String {
    let Some(max_len) = max_len else {
        return summary;
    };
    if summary.len() <= max_len {
        return summary;
    }

    format!(
        "{}... *TRUNCATED*",
        summary.chars().take(max_len).collect::<String>()
    )
}

/// Renders a function generic parameter prefix.
fn generic_prefix(types: &[GenericType], packs: &[GenericTypePack]) -> String {
    if types.is_empty() && packs.is_empty() {
        return String::new();
    }

    let mut names = types
        .iter()
        .map(|generic| generic.name.clone())
        .collect::<Vec<_>>();
    names.extend(packs.iter().map(generic_pack_summary));
    format!("<{}>", names.join(", "))
}

/// Computes named-function argument names after applying override options.
#[cfg(any())]
fn effective_named_argument_names(
    function: &FunctionType,
    options: &FunctionSummaryOptions,
) -> Vec<Option<String>> {
    let mut names = function.argument_names.clone();
    for (index, name) in options.override_argument_names.iter().enumerate() {
        if index < names.len() {
            names[index] = Some(name.clone());
        } else {
            names.resize(index, None);
            names.push(Some(name.clone()));
        }
    }
    names
}
