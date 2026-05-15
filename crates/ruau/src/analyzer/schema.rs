//! Declaration schema extraction for checked host interfaces.

use serde_json::Value;

use super::{
    AnalysisError, ArgumentSchema, CallableSchema, ClassSchema, FieldSchema, ModuleRoot,
    ModuleSchema, NamespaceSchema, TypeAliasSchema, TypeSlice, native::parse_ast_json,
};
use crate::resolver::SourceSpan;

/// Extracts the top-level module declaration and class declarations from `.d.luau` source.
///
/// Parsing is delegated to the native Luau parser through the analysis shim. This Rust layer maps
/// the Luau AST shape into ruau's public module schema and preserves source slices, spans, and docs
/// from the original declaration text.
pub fn extract_module_schema(source: &str) -> Result<ModuleSchema, AnalysisError> {
    let ast_json = parse_ast_json(source)?;
    let ast: Value = serde_json::from_str(&ast_json)
        .map_err(|error| module_schema_error(format!("invalid Luau AST JSON: {error}")))?;
    let source_map = SourceMap::new(source);
    let body = ast
        .get("body")
        .and_then(Value::as_array)
        .ok_or_else(|| module_schema_error("Luau AST root is missing a statement body"))?;

    let mut schema = ModuleSchema {
        module_description: top_module_description(source),
        ..ModuleSchema::default()
    };

    for statement in body {
        match node_type(statement) {
            Some("AstStatTypeAlias") if bool_field(statement, "exported") => {
                let alias = parse_type_alias(statement, source, &source_map)?;
                if alias.name == "Module" {
                    let namespace = table_namespace(
                        field(statement, "value")?,
                        source,
                        &source_map,
                        NamespaceContext::Module,
                    )?;
                    schema.root = Some(ModuleRoot {
                        name: "Module".to_owned(),
                        namespace,
                        span: alias.span,
                    });
                }
                schema.type_aliases.insert(alias.name.clone(), alias);
            }
            Some("AstStatDeclareClass") => {
                let name = str_field(statement, "name")?.to_owned();
                let class = parse_class(statement, source, &source_map)?;
                schema.classes.insert(name, class);
            }
            _ if declare_global_type(statement).is_some() => {
                if matches!(
                    schema.root.as_ref().map(|root| root.name.as_str()),
                    Some("Module")
                ) {
                    continue;
                }

                let name = str_field(statement, "name")?.to_owned();
                let namespace = table_namespace(
                    declare_global_type(statement).expect("checked above"),
                    source,
                    &source_map,
                    NamespaceContext::Module,
                )?;
                if let Some(existing) = schema.root.as_ref() {
                    return Err(AnalysisError::ModuleSchema(format!(
                        "multiple module-root declarations: `{}` and `{name}`",
                        existing.name
                    )));
                }
                schema.root = Some(ModuleRoot {
                    name,
                    namespace,
                    span: location_field(statement).ok(),
                });
            }
            _ => {}
        }
    }

    Ok(schema)
}

fn parse_type_alias(
    node: &Value,
    source: &str,
    source_map: &SourceMap,
) -> Result<TypeAliasSchema, AnalysisError> {
    let name = str_field(node, "name")?.to_owned();
    let span = location_field(node).ok();
    let ty = type_slice(field(node, "value")?, source, source_map)?;
    let source_text = span
        .as_ref()
        .and_then(|span| source_map.slice(source, span))
        .unwrap_or_default()
        .to_owned();
    let docs = docs_before_span(source, source_map, span.as_ref());

    Ok(TypeAliasSchema {
        name,
        ty,
        source: source_text,
        span,
        docs,
    })
}

#[derive(Clone, Copy)]
enum NamespaceContext {
    Module,
    Class,
}

fn parse_class(
    node: &Value,
    source: &str,
    source_map: &SourceMap,
) -> Result<ClassSchema, AnalysisError> {
    let mut class = ClassSchema {
        span: location_field(node).ok(),
        ..ClassSchema::default()
    };
    let props = field(node, "props")?
        .as_array()
        .ok_or_else(|| module_schema_error("declared class props must be an array"))?;

    for prop in props {
        let name = str_field(prop, "name")?.to_owned();
        let ty = field(prop, "luauType")?;
        let span = location_field(prop).ok();
        let docs = docs_before_span(source, source_map, span.as_ref());

        if node_type(ty) == Some("AstTypeFunction") {
            class.methods.push(name.clone());
            let mut callable = parse_callable(ty, source, source_map, NamespaceContext::Class)?;
            callable.span = span;
            callable.docs = docs;
            class.method_signatures.insert(name, callable);
        } else {
            class.fields.insert(
                name.clone(),
                FieldSchema {
                    name,
                    ty: type_slice(ty, source, source_map)?,
                    span,
                    docs,
                },
            );
        }
    }

    Ok(class)
}

fn table_namespace(
    node: &Value,
    source: &str,
    source_map: &SourceMap,
    context: NamespaceContext,
) -> Result<NamespaceSchema, AnalysisError> {
    if node_type(node) != Some("AstTypeTable") {
        return Err(module_schema_error(
            "expected module schema to be a table type",
        ));
    }

    let mut namespace = NamespaceSchema::default();
    let props = field(node, "props")?
        .as_array()
        .ok_or_else(|| module_schema_error("namespace props must be an array"))?;
    if let Some(indexer) = field(node, "indexer").ok().filter(|value| !value.is_null()) {
        let indexer_source = location_field(indexer)
            .ok()
            .and_then(|span| source_map.slice(source, &span))
            .unwrap_or_default();
        if !indexer_source.trim_start().starts_with('[') && indexer_source.contains(':') {
            // Luau represents `name: Type` scalar table entries as indexers. They are not
            // callable module members, matching the old schema extractor's behavior.
        } else if !indexer_source.trim_start().starts_with('[')
            && let Some(name) = field(indexer, "resultType")
                .ok()
                .and_then(|value| value.get("name"))
                .and_then(Value::as_str)
        {
            return Err(module_schema_error(format!("missing `:` after `{name}`")));
        } else {
            return Err(module_schema_error("namespace indexers are not supported"));
        }
    }

    for prop in props {
        let name = str_field(prop, "name")?.to_owned();
        let ty = field(prop, "propType")?;
        match node_type(ty) {
            Some("AstTypeFunction") => {
                namespace.functions.push(name.clone());
                let mut callable = parse_callable(ty, source, source_map, context)?;
                callable.span = location_field(prop).ok();
                callable.docs = docs_before_span(source, source_map, callable.span.as_ref());
                namespace.callables.insert(name, callable);
            }
            Some("AstTypeTable") => {
                let child = table_namespace(ty, source, source_map, context)?;
                namespace.children.insert(name, child);
            }
            _ => {}
        }
    }

    Ok(namespace)
}

fn parse_callable(
    node: &Value,
    source: &str,
    source_map: &SourceMap,
    context: NamespaceContext,
) -> Result<CallableSchema, AnalysisError> {
    let arg_types = field(field(node, "argTypes")?, "types")?
        .as_array()
        .ok_or_else(|| module_schema_error("callable argTypes.types must be an array"))?;
    let arg_names = field(node, "argNames")?
        .as_array()
        .ok_or_else(|| module_schema_error("callable argNames must be an array"))?;
    let mut args = Vec::new();

    for (index, ty) in arg_types.iter().enumerate() {
        let name_node = arg_names
            .get(index)
            .and_then(|value| if value.is_null() { None } else { Some(value) });
        if matches!(context, NamespaceContext::Class) && index == 0 && name_node.is_none() {
            continue;
        }
        let name = name_node
            .and_then(|value| value.get("name"))
            .and_then(Value::as_str)
            .ok_or_else(|| module_schema_error("callable arguments must be named"))?;
        if matches!(context, NamespaceContext::Class) && index == 0 && name == "self" {
            continue;
        }

        let ty = type_slice(ty, source, source_map)?;
        let name_span = name_node.and_then(|value| location_field(value).ok());
        let span = name_span
            .as_ref()
            .zip(ty.span.as_ref())
            .map(|(name_span, ty_span)| merge_span(name_span, ty_span))
            .or(ty.span);
        args.push(ArgumentSchema {
            name: name.to_owned(),
            optional: name.ends_with('?') || ty.source.ends_with('?'),
            ty,
            span,
        });
    }

    Ok(CallableSchema {
        args,
        returns: type_slice(field(node, "returnTypes")?, source, source_map)?,
        method: matches!(context, NamespaceContext::Class),
        span: None,
        docs: None,
    })
}

fn type_slice(
    node: &Value,
    source: &str,
    source_map: &SourceMap,
) -> Result<TypeSlice, AnalysisError> {
    let span = location_field(node).ok();
    let mut source_text = span
        .as_ref()
        .and_then(|span| source_map.slice(source, span))
        .unwrap_or_default()
        .trim()
        .to_owned();

    if source_text.is_empty()
        && node_type(node) == Some("AstTypePackExplicit")
        && field(field(node, "typeList")?, "types")?
            .as_array()
            .is_some_and(Vec::is_empty)
    {
        source_text = "()".to_owned();
    }

    Ok(TypeSlice {
        source: source_text,
        span,
    })
}

pub(super) fn checker_source_for_interface(
    schema: &ModuleSchema,
    source: &str,
) -> Result<String, AnalysisError> {
    let mut output = String::new();
    for alias in schema.type_aliases.values() {
        if alias.name != "Module" {
            output.push_str(alias.source.trim_end());
            output.push_str("\n\n");
        }
    }

    for (class, schema) in &schema.classes {
        output.push_str("export type ");
        output.push_str(class);
        output.push_str(" = {\n");
        for (field, schema) in &schema.fields {
            output.push_str("    ");
            output.push_str(field);
            output.push_str(": ");
            output.push_str(&schema.ty.source);
            output.push_str(",\n");
        }
        for (method, callable) in &schema.method_signatures {
            output.push_str("    ");
            output.push_str(method);
            output.push_str(": ");
            write_callable_type(&mut output, callable, Some(class));
            output.push_str(",\n");
        }
        output.push_str("}\n\n");
    }

    if let Some(alias) = schema.type_aliases.get("Module") {
        output.push_str(alias.source.trim_end());
        output.push_str("\n\n");
    } else if let Some(root) = &schema.root {
        output.push_str("export type Module = ");
        write_namespace_type(&mut output, &root.namespace, 0);
        output.push_str("\n\n");
    }

    if schema.root.is_some() {
        output.push_str("return (nil :: any) :: Module\n");
    } else if output.is_empty() {
        output.push_str(source);
    }

    Ok(output)
}

fn write_namespace_type(output: &mut String, namespace: &NamespaceSchema, depth: usize) {
    output.push_str("{\n");
    for (name, callable) in &namespace.callables {
        write_indent(output, depth + 1);
        output.push_str(name);
        output.push_str(": ");
        write_callable_type(output, callable, None);
        output.push_str(",\n");
    }
    for (name, child) in &namespace.children {
        write_indent(output, depth + 1);
        output.push_str(name);
        output.push_str(": ");
        write_namespace_type(output, child, depth + 1);
        output.push_str(",\n");
    }
    write_indent(output, depth);
    output.push('}');
}

fn write_callable_type(output: &mut String, callable: &CallableSchema, self_type: Option<&str>) {
    output.push('(');
    let mut first = true;
    if let Some(self_type) = self_type {
        output.push_str("self: ");
        output.push_str(self_type);
        first = false;
    }
    for arg in &callable.args {
        if !first {
            output.push_str(", ");
        }
        output.push_str(&arg.name);
        output.push_str(": ");
        output.push_str(&arg.ty.source);
        first = false;
    }
    output.push_str(") -> ");
    output.push_str(&callable.returns.source);
}

fn write_indent(output: &mut String, depth: usize) {
    for _ in 0..depth {
        output.push_str("    ");
    }
}

fn node_type(node: &Value) -> Option<&str> {
    node.get("type").and_then(Value::as_str)
}

fn declare_global_type(node: &Value) -> Option<&Value> {
    let ty = node.get("type")?;
    (ty.is_object() && node.get("nameLocation").is_some()).then_some(ty)
}

fn bool_field(node: &Value, key: &str) -> bool {
    node.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn str_field<'a>(node: &'a Value, key: &str) -> Result<&'a str, AnalysisError> {
    node.get(key).and_then(Value::as_str).ok_or_else(|| {
        module_schema_error(format!("Luau AST node is missing string field `{key}`"))
    })
}

fn field<'a>(node: &'a Value, key: &str) -> Result<&'a Value, AnalysisError> {
    node.get(key)
        .ok_or_else(|| module_schema_error(format!("Luau AST node is missing field `{key}`")))
}

fn location_field(node: &Value) -> Result<SourceSpan, AnalysisError> {
    let location = str_field(node, "location")?;
    parse_location(location)
}

fn parse_location(location: &str) -> Result<SourceSpan, AnalysisError> {
    let (begin, end) = location
        .split_once(" - ")
        .ok_or_else(|| module_schema_error(format!("invalid Luau AST location `{location}`")))?;
    let (line, column) = parse_position(begin)?;
    let (end_line, end_column) = parse_position(end)?;
    Ok(SourceSpan {
        line,
        column,
        end_line,
        end_column,
    })
}

fn parse_position(position: &str) -> Result<(u32, u32), AnalysisError> {
    let (line, column) = position
        .split_once(',')
        .ok_or_else(|| module_schema_error(format!("invalid Luau AST position `{position}`")))?;
    let line = line
        .parse()
        .map_err(|_| module_schema_error(format!("invalid Luau AST line `{line}`")))?;
    let column = column
        .parse()
        .map_err(|_| module_schema_error(format!("invalid Luau AST column `{column}`")))?;
    Ok((line, column))
}

fn merge_span(start: &SourceSpan, end: &SourceSpan) -> SourceSpan {
    SourceSpan {
        line: start.line,
        column: start.column,
        end_line: end.end_line,
        end_column: end.end_column,
    }
}

fn docs_before_span(
    source: &str,
    source_map: &SourceMap,
    span: Option<&SourceSpan>,
) -> Option<String> {
    span.and_then(|span| source_map.offset(span.line, span.column))
        .and_then(|offset| doc_block_before(source, offset, source_map))
}

#[derive(Debug, Clone, Copy)]
struct LineRecord<'a> {
    text: &'a str,
}

fn line_records(source: &str) -> Vec<LineRecord<'_>> {
    let mut lines = Vec::new();
    let mut start = 0;
    for line in source.split_inclusive('\n') {
        let end = start + line.trim_end_matches('\n').len();
        lines.push(LineRecord {
            text: &source[start..end],
        });
        start += line.len();
    }
    if source.is_empty() {
        lines.push(LineRecord { text: "" });
    }
    lines
}

#[derive(Debug)]
struct SourceMap {
    line_offsets: Vec<usize>,
}

impl SourceMap {
    fn new(source: &str) -> Self {
        let mut line_offsets = vec![0];
        for (index, byte) in source.bytes().enumerate() {
            if byte == b'\n' {
                line_offsets.push(index + 1);
            }
        }
        Self { line_offsets }
    }

    fn offset(&self, line: u32, column: u32) -> Option<usize> {
        let line_start = *self.line_offsets.get(line as usize)?;
        Some(line_start.saturating_add(column as usize))
    }

    fn slice<'a>(&self, source: &'a str, span: &SourceSpan) -> Option<&'a str> {
        let start = self.offset(span.line, span.column)?;
        let end = self.offset(span.end_line, span.end_column)?;
        source.get(start..end)
    }

    fn line_index(&self, offset: usize) -> usize {
        match self.line_offsets.binary_search(&offset) {
            Ok(index) => index,
            Err(index) => index.saturating_sub(1),
        }
    }
}

fn top_module_description(source: &str) -> Option<String> {
    let mut docs = Vec::new();
    let lines = line_records(source);
    let mut index = 0;
    while index < lines.len() {
        let trimmed = lines[index].text.trim_start();
        if trimmed.is_empty() {
            break;
        }
        if let Some(text) = trimmed.strip_prefix("--[[") {
            let mut block = Vec::new();
            let mut current = text;
            loop {
                if let Some(end) = current.find("]]") {
                    block.push(current[..end].trim().to_owned());
                    break;
                }
                block.push(current.trim().to_owned());
                index += 1;
                if index >= lines.len() {
                    break;
                }
                current = lines[index].text;
            }
            docs.push(block.join("\n").trim().to_owned());
            index += 1;
            continue;
        }
        if let Some(text) = trimmed.strip_prefix("--") {
            docs.push(text.trim_start().to_owned());
            index += 1;
            continue;
        }
        break;
    }
    (!docs.is_empty()).then(|| docs.join("\n"))
}

fn doc_block_before(
    source: &str,
    declaration_start: usize,
    source_map: &SourceMap,
) -> Option<String> {
    let lines = line_records(source);
    let line_index = source_map.line_index(declaration_start);
    if line_index == 0 {
        return None;
    }

    let mut docs = Vec::new();
    let mut index = line_index;
    let mut kind: Option<DocCommentKind> = None;
    while index > 0 {
        index -= 1;
        let trimmed = lines[index].text.trim_start();
        if trimmed.is_empty() {
            break;
        }
        if trimmed.ends_with("]]") {
            if kind
                .replace(DocCommentKind::Block)
                .is_some_and(|kind| kind != DocCommentKind::Block)
            {
                break;
            }
            let mut block = Vec::new();
            loop {
                let current = lines[index].text.trim_start();
                if let Some(start) = current.find("--[[") {
                    let text = &current[start + "--[[".len()..];
                    let text = text.strip_suffix("]]").unwrap_or(text);
                    block.push(text.trim().to_owned());
                    break;
                }
                let text = current.strip_suffix("]]").unwrap_or(current);
                block.push(text.trim().to_owned());
                if index == 0 {
                    break;
                }
                index -= 1;
            }
            block.reverse();
            docs.extend(block);
            break;
        }
        if let Some(text) = trimmed.strip_prefix("--") {
            if kind
                .replace(DocCommentKind::Line)
                .is_some_and(|kind| kind != DocCommentKind::Line)
            {
                break;
            }
            docs.push(text.trim_start().to_owned());
            continue;
        }
        break;
    }
    docs.reverse();
    (!docs.is_empty()).then(|| docs.join("\n"))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DocCommentKind {
    Line,
    Block,
}

fn module_schema_error(message: impl Into<String>) -> AnalysisError {
    AnalysisError::ModuleSchema(message.into())
}
