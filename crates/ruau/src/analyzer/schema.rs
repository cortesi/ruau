//! Declaration schema extraction for checked host interfaces.

use std::collections::BTreeMap;

use super::{
    AnalysisError, ArgumentSchema, CallableSchema, ClassSchema, FieldSchema, ModuleRoot,
    ModuleSchema, NamespaceSchema, TypeAliasSchema, TypeSlice,
};
use crate::resolver::SourceSpan;

/// Extracts the top-level module declaration and class method declarations from `.d.luau` source.
///
/// This is a Rust-side contract parser for host declaration files, not a full Luau parser. The
/// supported surface is the declaration shape Ruau exposes to embedders: top-level module tables,
/// exported type aliases, classes, fields, methods, docs, and source spans. Compatibility should be
/// expanded through focused fixture tests; if those fixtures grow into general Luau grammar parsing,
/// move the extractor closer to the Luau parser through the C shim instead of widening ad hoc
/// string parsing here.
pub fn extract_module_schema(source: &str) -> Result<ModuleSchema, AnalysisError> {
    let source_map = SourceMap::new(source);
    let stripped = mask_comments(source);
    let mut schema = ModuleSchema {
        module_description: top_module_description(source),
        type_aliases: collect_export_type_aliases(source, &stripped, &source_map)?,
        ..ModuleSchema::default()
    };
    if let Some(module_alias) = schema.type_aliases.get("Module") {
        let type_start = stripped.find(&module_alias.ty.source).unwrap_or_default();
        let (namespace, _) =
            parse_namespace_type(&module_alias.ty.source, type_start, &source_map, source)?;
        schema.root = Some(ModuleRoot {
            name: "Module".to_owned(),
            namespace,
            span: module_alias.span,
        });
    }
    let mut cursor = stripped.as_str();

    while let Some(declare_at) = next_top_level_declare(cursor) {
        let declaration_start = stripped.len() - cursor.len() + declare_at;
        cursor = &cursor[declare_at + "declare ".len()..];
        let cursor_start = stripped.len() - cursor.len();
        let (trimmed_cursor, trimmed_start) = trim_start_with_offset(cursor, cursor_start);
        cursor = trimmed_cursor;

        if let Some(after) = cursor.strip_prefix("class ") {
            let class_source_start = trimmed_start + "class ".len();
            let (name, body, body_start, rest, class_end) =
                read_class_block(after, class_source_start)?;
            let mut class = parse_class_body(source, body, body_start, &source_map);
            class.span = Some(source_map.span(declaration_start, class_end));
            schema.classes.insert(name.to_owned(), class);
            cursor = rest;
            continue;
        }

        if let Some((name, after_name)) = read_identifier(cursor) {
            let after_colon = trim_start(after_name);
            if let Some(after_colon) = after_colon.strip_prefix(':') {
                let namespace_start = stripped.len() - after_colon.len();
                let (trimmed_namespace, namespace_start) =
                    trim_start_with_offset(after_colon, namespace_start);
                let (namespace, rest) =
                    parse_namespace_type(trimmed_namespace, namespace_start, &source_map, source)?;
                if let Some(existing) = schema.root.as_ref() {
                    if existing.name != "Module" {
                        return Err(AnalysisError::ModuleSchema(format!(
                            "multiple module-root declarations: `{}` and `{name}`",
                            existing.name
                        )));
                    }
                    cursor = rest;
                    continue;
                }
                schema.root = Some(ModuleRoot {
                    name: name.to_owned(),
                    namespace,
                    span: Some(source_map.span(declaration_start, namespace_start)),
                });
                cursor = rest;
            } else {
                cursor = after_name;
            }
            continue;
        }

        cursor = skip_to_newline(cursor);
    }

    Ok(schema)
}

fn collect_export_type_aliases(
    raw_source: &str,
    source: &str,
    source_map: &SourceMap,
) -> Result<BTreeMap<String, TypeAliasSchema>, AnalysisError> {
    let mut aliases = BTreeMap::new();
    let lines = line_records(source);
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index].text;
        let trimmed = line.trim_start();
        if !trimmed.starts_with("export type ") {
            index += 1;
            continue;
        }

        let mut block = String::new();
        let mut balance = 0_i32;
        let mut block_index = index;
        while block_index < lines.len() {
            let current = lines[block_index].text;
            update_type_balance(current, &mut balance);
            block.push_str(current);
            block.push('\n');
            block_index += 1;
            if balance <= 0
                && !is_export_type_continuation(lines.get(block_index).map(|line| line.text))
            {
                break;
            }
        }

        let start = lines[index].start;
        let end = lines[block_index - 1].end;
        let raw_block = &raw_source[start..end];
        let docs = doc_block_before(raw_source, start, source_map);
        let alias = parse_type_alias(&block, raw_block, start, end, docs, source_map)?;
        aliases.insert(alias.name.clone(), alias);
        index = block_index;
    }
    Ok(aliases)
}

fn parse_type_alias(
    source: &str,
    raw_source: &str,
    start: usize,
    end: usize,
    docs: Option<String>,
    source_map: &SourceMap,
) -> Result<TypeAliasSchema, AnalysisError> {
    let trimmed = source.trim();
    let rest = trimmed
        .strip_prefix("export type ")
        .ok_or_else(|| module_schema_error("expected exported type alias"))?
        .trim_start();
    let (name, after_name) =
        read_identifier(rest).ok_or_else(|| module_schema_error("expected exported type name"))?;
    let after_params = skip_type_params(after_name)?;
    let ty = after_params
        .trim_start()
        .strip_prefix('=')
        .ok_or_else(|| module_schema_error(format!("export type `{name}` is missing `=`")))?
        .trim();
    Ok(TypeAliasSchema {
        name: name.to_owned(),
        ty: TypeSlice {
            source: ty.to_owned(),
            span: raw_source
                .find(ty)
                .map(|relative| source_map.span(start + relative, start + relative + ty.len())),
        },
        source: raw_source.to_owned(),
        span: Some(source_map.span(start, end)),
        docs,
    })
}

fn skip_type_params(source: &str) -> Result<&str, AnalysisError> {
    let source = source.trim_start();
    if !source.starts_with('<') {
        return Ok(source);
    }

    let mut depth = 0_i32;
    for (index, character) in source.char_indices() {
        match character {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(&source[index + character.len_utf8()..]);
                }
            }
            _ => {}
        }
    }

    Err(module_schema_error(
        "unbalanced generic parameters in export type",
    ))
}

fn is_export_type_continuation(line: Option<&str>) -> bool {
    let Some(line) = line else {
        return false;
    };
    let trimmed = line.trim_start();
    !trimmed.is_empty()
        && (line.starts_with(' ') || line.starts_with('\t'))
        && !trimmed.starts_with("declare ")
        && !trimmed.starts_with("export type ")
}

fn update_type_balance(line: &str, balance: &mut i32) {
    for character in line.chars() {
        match character {
            '{' | '(' | '[' => *balance += 1,
            '}' | ')' | ']' => *balance -= 1,
            _ => {}
        }
    }
}

fn next_top_level_declare(source: &str) -> Option<usize> {
    let mut at_line_start = true;
    for (index, character) in source.char_indices() {
        if at_line_start && source[index..].starts_with("declare ") {
            return Some(index);
        }
        at_line_start = character == '\n';
    }
    None
}

fn read_class_block(
    source: &str,
    source_start: usize,
) -> Result<(&str, &str, usize, &str, usize), AnalysisError> {
    let (name, after_name) =
        read_identifier(source).ok_or_else(|| module_schema_error("expected class name"))?;
    let mut cursor = trim_start(after_name);

    if let Some(rest) = cursor.strip_prefix("extends ") {
        let after_extends = trim_start(rest);
        let (_parent, after_parent) = read_identifier(after_extends)
            .ok_or_else(|| module_schema_error("expected parent class name"))?;
        cursor = trim_start(after_parent);
    }

    let body_start = cursor;
    let body_start_offset = source_start + source.len() - body_start.len();
    let end_offset = find_keyword_end(cursor)
        .ok_or_else(|| module_schema_error(format!("class `{name}` is missing `end`")))?;
    let body = &body_start[..end_offset];
    let rest = &body_start[end_offset + "end".len()..];
    let class_end = body_start_offset + end_offset + "end".len();

    Ok((name, body, body_start_offset, rest, class_end))
}

fn parse_class_body(
    raw_source: &str,
    body: &str,
    body_start: usize,
    source_map: &SourceMap,
) -> ClassSchema {
    let mut methods = Vec::new();
    let mut method_signatures = BTreeMap::new();
    let mut fields = BTreeMap::new();
    for line_record in line_records(body) {
        let raw_line = line_record.text;
        let line_start = body_start + line_record.start;
        let line_end = body_start + line_record.end;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with("--") {
            continue;
        }
        let trimmed_start = line_start + raw_line.len() - raw_line.trim_start().len();
        let docs = doc_block_before(raw_source, trimmed_start, source_map);

        if let Some(rest) = line.strip_prefix("function ")
            && let Some((name, after)) = read_identifier(rest)
            && trim_start(after).starts_with('(')
        {
            methods.push(name.to_owned());
            let callable_start = trimmed_start + "function ".len() + name.len() + after.len()
                - trim_start(after).len();
            if let Ok(mut callable) =
                parse_colon_callable(trim_start(after), callable_start, true, source_map)
            {
                callable.span = Some(source_map.span(trimmed_start, line_end));
                callable.docs = docs;
                method_signatures.insert(name.to_owned(), callable);
            }
            continue;
        }

        if let Some((name, after)) = read_identifier(line) {
            let trimmed = trim_start(after);
            if let Some(rest) = trimmed.strip_prefix(':')
                && trim_start(rest).starts_with('(')
            {
                methods.push(name.to_owned());
                let callable_start = trimmed_start + name.len() + trimmed.len() - rest.len()
                    + rest.len()
                    - trim_start(rest).len();
                if let Ok(mut callable) =
                    parse_arrow_callable(trim_start(rest), callable_start, true, source_map)
                {
                    callable.span = Some(source_map.span(trimmed_start, line_end));
                    callable.docs = docs;
                    method_signatures.insert(name.to_owned(), callable);
                }
            } else if let Some(rest) = trimmed.strip_prefix(':') {
                let ty_start = line_end - rest.len() + rest.len() - rest.trim_start().len();
                let ty_source = rest.trim();
                fields.insert(
                    name.to_owned(),
                    FieldSchema {
                        name: name.to_owned(),
                        ty: TypeSlice {
                            source: ty_source.to_owned(),
                            span: Some(source_map.span(ty_start, ty_start + ty_source.len())),
                        },
                        span: Some(source_map.span(trimmed_start, line_end)),
                        docs,
                    },
                );
            }
        }
    }
    ClassSchema {
        methods,
        method_signatures,
        fields,
        span: None,
    }
}

fn parse_namespace_type<'a>(
    source: &'a str,
    source_start: usize,
    source_map: &SourceMap,
    doc_source: &str,
) -> Result<(NamespaceSchema, &'a str), AnalysisError> {
    let source = trim_start(source);
    let after_brace = source
        .strip_prefix('{')
        .ok_or_else(|| module_schema_error("expected `{`"))?;
    let inside_start = source_start + source.len() - after_brace.len();

    let close_at = find_matching_brace(after_brace)?;
    let inside = &after_brace[..close_at];
    let rest = &after_brace[close_at + 1..];

    Ok((
        parse_namespace_body(inside, inside_start, source_map, doc_source)?,
        rest,
    ))
}

fn parse_namespace_body(
    body: &str,
    body_start: usize,
    source_map: &SourceMap,
    doc_source: &str,
) -> Result<NamespaceSchema, AnalysisError> {
    let mut namespace = NamespaceSchema::default();
    for (entry_offset, entry) in split_top_level_commas_with_offsets(body) {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        let entry_start = body_start + entry_offset + entry.len() - entry.trim_start().len();
        let entry_end = body_start + entry_offset + entry.trim_end().len();

        let (key, after_key) = read_identifier(trimmed)
            .ok_or_else(|| module_schema_error(format!("missing key: `{trimmed}`")))?;
        let after_colon = trim_start(after_key);
        let value = after_colon
            .strip_prefix(':')
            .ok_or_else(|| module_schema_error(format!("missing `:` after `{key}`")))?
            .trim();

        if value.starts_with('(') {
            namespace.functions.push(key.to_owned());
            let value_start = entry_end - value.len();
            let mut callable = parse_arrow_callable(value, value_start, false, source_map)?;
            callable.span = Some(source_map.span(entry_start, entry_end));
            callable.docs = doc_block_before(doc_source, entry_start, source_map);
            namespace.callables.insert(key.to_owned(), callable);
        } else if value.starts_with('{') {
            let value_start = entry_end - value.len();
            let (child, _) = parse_namespace_type(value, value_start, source_map, doc_source)?;
            namespace.children.insert(key.to_owned(), child);
        }
    }
    Ok(namespace)
}

fn parse_arrow_callable(
    source: &str,
    source_start: usize,
    drop_self: bool,
    source_map: &SourceMap,
) -> Result<CallableSchema, AnalysisError> {
    let close_at = find_matching_paren(source)?;
    let args = parse_args(
        &source[1..close_at],
        source_start + 1,
        drop_self,
        source_map,
    )?;
    let after = trim_start(&source[close_at + 1..]);
    let returns = after
        .strip_prefix("->")
        .ok_or_else(|| module_schema_error(format!("missing `->` in callable `{source}`")))?
        .trim();
    let return_start = source_start + source.len() - returns.len();
    Ok(CallableSchema {
        args,
        returns: TypeSlice {
            source: returns.to_owned(),
            span: Some(source_map.span(return_start, return_start + returns.len())),
        },
        method: drop_self,
        span: None,
        docs: None,
    })
}

fn parse_colon_callable(
    source: &str,
    source_start: usize,
    drop_self: bool,
    source_map: &SourceMap,
) -> Result<CallableSchema, AnalysisError> {
    let close_at = find_matching_paren(source)?;
    let args = parse_args(
        &source[1..close_at],
        source_start + 1,
        drop_self,
        source_map,
    )?;
    let after = trim_start(&source[close_at + 1..]);
    let returns = after
        .strip_prefix(':')
        .ok_or_else(|| module_schema_error(format!("missing return type in callable `{source}`")))?
        .trim();
    let return_start = source_start + source.len() - returns.len();
    Ok(CallableSchema {
        args,
        returns: TypeSlice {
            source: returns.to_owned(),
            span: Some(source_map.span(return_start, return_start + returns.len())),
        },
        method: drop_self,
        span: None,
        docs: None,
    })
}

fn parse_args(
    source: &str,
    source_start: usize,
    drop_self: bool,
    source_map: &SourceMap,
) -> Result<Vec<ArgumentSchema>, AnalysisError> {
    let mut args = Vec::new();
    for (arg_offset, raw_arg) in split_top_level_commas_with_offsets(source) {
        let raw_arg = raw_arg.trim();
        if raw_arg.is_empty() {
            continue;
        }
        let arg_start = source_start + arg_offset;
        let arg_start = arg_start + raw_arg.len() - raw_arg.trim_start().len();
        let arg_end = arg_start + raw_arg.len();
        if drop_self && args.is_empty() && raw_arg == "self" {
            continue;
        }
        let (name, after_name) = read_identifier(raw_arg)
            .ok_or_else(|| module_schema_error(format!("malformed argument `{raw_arg}`")))?;
        if drop_self && args.is_empty() && name == "self" {
            continue;
        }
        let ty = after_name
            .trim_start()
            .strip_prefix(':')
            .ok_or_else(|| module_schema_error(format!("argument `{name}` is missing a type")))?
            .trim();
        let ty_start = arg_end - ty.len();
        args.push(ArgumentSchema {
            name: name.to_owned(),
            ty: TypeSlice {
                source: ty.to_owned(),
                span: Some(source_map.span(ty_start, ty_start + ty.len())),
            },
            optional: name.ends_with('?') || ty.ends_with('?'),
            span: Some(source_map.span(arg_start, arg_end)),
        });
    }
    Ok(args)
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

#[derive(Debug, Clone, Copy)]
struct LineRecord<'a> {
    text: &'a str,
    start: usize,
    end: usize,
}

fn line_records(source: &str) -> Vec<LineRecord<'_>> {
    let mut lines = Vec::new();
    let mut start = 0;
    for line in source.split_inclusive('\n') {
        let end = start + line.trim_end_matches('\n').len();
        lines.push(LineRecord {
            text: &source[start..end],
            start,
            end,
        });
        start += line.len();
    }
    if source.is_empty() {
        lines.push(LineRecord {
            text: "",
            start: 0,
            end: 0,
        });
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

    fn span(&self, start: usize, end: usize) -> SourceSpan {
        let (line, column) = self.position(start);
        let (end_line, end_column) = self.position(end);
        SourceSpan {
            line,
            column,
            end_line,
            end_column,
        }
    }

    fn line_index(&self, offset: usize) -> usize {
        match self.line_offsets.binary_search(&offset) {
            Ok(index) => index,
            Err(index) => index.saturating_sub(1),
        }
    }

    fn position(&self, offset: usize) -> (u32, u32) {
        let line = self.line_index(offset);
        let line_start = self.line_offsets[line];
        (line as u32, offset.saturating_sub(line_start) as u32)
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

fn mask_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if index + 1 < bytes.len() && bytes[index] == b'-' && bytes[index + 1] == b'-' {
            if index + 3 < bytes.len() && bytes[index + 2] == b'[' && bytes[index + 3] == b'[' {
                index += 4;
                while index + 1 < bytes.len() && !(bytes[index] == b']' && bytes[index + 1] == b']')
                {
                    output.push(if bytes[index] == b'\n' { b'\n' } else { b' ' });
                    index += 1;
                }
                if index + 1 < bytes.len() {
                    index += 2;
                }
                continue;
            }

            while index < bytes.len() && bytes[index] != b'\n' {
                output.push(b' ');
                index += 1;
            }
            continue;
        }

        output.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(output).unwrap_or_default()
}

fn read_identifier(source: &str) -> Option<(&str, &str)> {
    let mut end = 0;
    for (index, character) in source.char_indices() {
        let ok = if index == 0 {
            character == '_' || character.is_ascii_alphabetic()
        } else {
            character == '_' || character.is_ascii_alphanumeric()
        };
        if ok {
            end = index + character.len_utf8();
        } else {
            break;
        }
    }

    (end != 0).then_some((&source[..end], &source[end..]))
}

fn trim_start(source: &str) -> &str {
    source.trim_start()
}

fn trim_start_with_offset(source: &str, start: usize) -> (&str, usize) {
    let trimmed = source.trim_start();
    (trimmed, start + source.len() - trimmed.len())
}

fn skip_to_newline(source: &str) -> &str {
    if let Some(index) = source.find('\n') {
        &source[index + 1..]
    } else {
        ""
    }
}

fn find_matching_brace(source: &str) -> Result<usize, AnalysisError> {
    let bytes = source.as_bytes();
    let mut depth = 1_i32;
    let mut paren_depth = 0_i32;
    for (index, byte) in bytes.iter().copied().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(index);
                }
            }
            b'(' => paren_depth += 1,
            b')' => paren_depth -= 1,
            _ => {}
        }
        if depth < 0 || paren_depth < 0 {
            return Err(module_schema_error(
                "unbalanced punctuation in namespace body",
            ));
        }
    }
    Err(module_schema_error("unterminated namespace body"))
}

fn find_matching_paren(source: &str) -> Result<usize, AnalysisError> {
    let after_open = source
        .strip_prefix('(')
        .ok_or_else(|| module_schema_error(format!("expected `(` in callable `{source}`")))?;
    find_matching_after_open(after_open, b'(', b')')
        .map(|index| index + 1)
        .map_err(module_schema_error)
}

fn find_matching_after_open(source: &str, open: u8, close: u8) -> Result<usize, &'static str> {
    let bytes = source.as_bytes();
    let mut depth = 1_i32;
    for (index, byte) in bytes.iter().copied().enumerate() {
        if byte == open {
            depth += 1;
        } else if byte == close {
            depth -= 1;
            if depth == 0 {
                return Ok(index);
            }
        }
        if depth < 0 {
            return Err("unbalanced punctuation");
        }
    }
    Err("unterminated declaration")
}

fn split_top_level_commas_with_offsets(body: &str) -> Vec<(usize, &str)> {
    let mut output = Vec::new();
    let mut depth = 0_i32;
    let mut start = 0;
    for (index, character) in body.char_indices() {
        match character {
            '{' | '(' | '[' => depth += 1,
            '}' | ')' | ']' => depth -= 1,
            ',' if depth == 0 => {
                output.push((start, &body[start..index]));
                start = index + 1;
            }
            _ => {}
        }
    }
    output.push((start, &body[start..]));
    output
}

fn find_keyword_end(source: &str) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut index = 0;
    while index + 3 <= bytes.len() {
        if &bytes[index..index + 3] == b"end" {
            let before_ok = index == 0 || !is_ident_byte(bytes[index - 1]);
            let after_ok = index + 3 == bytes.len() || !is_ident_byte(bytes[index + 3]);
            if before_ok && after_ok {
                return Some(index);
            }
        }
        index += 1;
    }
    None
}

fn is_ident_byte(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric()
}

fn module_schema_error(message: impl Into<String>) -> AnalysisError {
    AnalysisError::ModuleSchema(message.into())
}
