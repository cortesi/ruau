//! `.d.luau` structure extraction for module manifests.
//!
//! This recognises the declaration shapes emitted by `verber-protocol` and
//! similar hand-written definition files without attempting full type checking.

use std::collections::BTreeMap;

/// Aggregated schema extracted from one `.d.luau`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModuleSchema {
    /// Top-level declared module-root global, if any.
    pub root: Option<ModuleRoot>,
    /// `declare class` declarations.
    pub classes: BTreeMap<String, ClassSchema>,
}

/// Top-level `declare <name>: { ... }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleRoot {
    /// Global name (matches `Manifest.name`).
    pub name: String,
    /// Function and namespace shape rooted at the module table.
    pub namespace: NamespaceSchema,
}

/// One namespace level: function names + nested child namespaces.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NamespaceSchema {
    /// Function-typed members callable directly at this level.
    pub functions: Vec<String>,
    /// Nested namespace members, name to schema.
    pub children: BTreeMap<String, Self>,
}

/// Method names declared inside a `declare class ... end` block.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClassSchema {
    /// Method names.
    pub methods: Vec<String>,
}

/// Errors returned while extracting a module schema.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ModuleSchemaError {
    /// The source declares more than one top-level module-root global.
    #[error("multiple module-root declarations: `{first}` and `{second}`")]
    MultipleRoots {
        /// Name of the first declared root.
        first: String,
        /// Name of the second declared root.
        second: String,
    },
    /// Source contained unbalanced punctuation or malformed declarations.
    #[error("malformed source: {0}")]
    Malformed(String),
}

/// Walks `source` and returns the extracted `ModuleSchema`.
pub fn extract_module_schema(source: &str) -> Result<ModuleSchema, ModuleSchemaError> {
    let stripped = strip_comments(source);
    let mut schema = ModuleSchema::default();
    let mut cursor = stripped.as_str();

    while let Some(declare_at) = next_top_level_declare(cursor) {
        cursor = &cursor[declare_at + "declare ".len()..];
        cursor = trim_start(cursor);

        if cursor.starts_with("class ") {
            let after = &cursor["class ".len()..];
            let (name, body, rest) = read_class_block(after)?;
            schema
                .classes
                .insert(name.to_string(), parse_class_body(body));
            cursor = rest;
            continue;
        }

        if let Some((name, after_name)) = read_identifier(cursor) {
            let after_colon = trim_start(after_name);
            if let Some(after_colon) = after_colon.strip_prefix(':') {
                let (namespace, rest) = parse_namespace_type(trim_start(after_colon))?;
                if let Some(existing) = schema.root.as_ref() {
                    return Err(ModuleSchemaError::MultipleRoots {
                        first: existing.name.clone(),
                        second: name.to_string(),
                    });
                }
                schema.root = Some(ModuleRoot {
                    name: name.to_string(),
                    namespace,
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

/// Locates the next `declare ` token at column 0.
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

/// Reads one `declare class ... end` block.
fn read_class_block(source: &str) -> Result<(&str, &str, &str), ModuleSchemaError> {
    let (name, after_name) = read_identifier(source)
        .ok_or_else(|| ModuleSchemaError::Malformed("expected class name".into()))?;
    let mut cursor = trim_start(after_name);

    if let Some(rest) = cursor.strip_prefix("extends ") {
        let after_extends = trim_start(rest);
        let (_parent, after_parent) = read_identifier(after_extends)
            .ok_or_else(|| ModuleSchemaError::Malformed("expected parent class name".into()))?;
        cursor = trim_start(after_parent);
    }

    let body_start = cursor;
    let end_offset = find_keyword_end(cursor)
        .ok_or_else(|| ModuleSchemaError::Malformed(format!("class `{name}` is missing `end`")))?;
    let body = &body_start[..end_offset];
    let rest = &body_start[end_offset + "end".len()..];

    Ok((name, body, rest))
}

/// Parses a class body into a `ClassSchema`.
fn parse_class_body(body: &str) -> ClassSchema {
    let mut methods = Vec::new();
    for raw_line in body.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with("--") {
            continue;
        }

        if let Some(rest) = line.strip_prefix("function ")
            && let Some((name, after)) = read_identifier(rest)
            && trim_start(after).starts_with('(')
        {
            methods.push(name.to_string());
            continue;
        }

        if let Some((name, after)) = read_identifier(line) {
            let trimmed = trim_start(after);
            if let Some(rest) = trimmed.strip_prefix(':')
                && trim_start(rest).starts_with('(')
            {
                methods.push(name.to_string());
            }
        }
    }
    ClassSchema { methods }
}

/// Parses a `{ key: type, ... }` namespace type starting at `source`.
fn parse_namespace_type(source: &str) -> Result<(NamespaceSchema, &str), ModuleSchemaError> {
    let source = trim_start(source);
    let after_brace = source
        .strip_prefix('{')
        .ok_or_else(|| ModuleSchemaError::Malformed("expected `{`".into()))?;

    let close_at = find_matching_brace(after_brace)?;
    let inside = &after_brace[..close_at];
    let rest = &after_brace[close_at + 1..];

    Ok((parse_namespace_body(inside)?, rest))
}

/// Parses key/value pairs inside a namespace body.
fn parse_namespace_body(body: &str) -> Result<NamespaceSchema, ModuleSchemaError> {
    let mut namespace = NamespaceSchema::default();
    let mut entries = split_top_level_commas(body);
    for entry in entries.drain(..) {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }

        let (key, after_key) = read_identifier(trimmed)
            .ok_or_else(|| ModuleSchemaError::Malformed(format!("missing key: `{trimmed}`")))?;
        let after_colon = trim_start(after_key);
        let value = after_colon
            .strip_prefix(':')
            .ok_or_else(|| ModuleSchemaError::Malformed(format!("missing `:` after `{key}`")))?
            .trim();

        if value.starts_with('(') {
            namespace.functions.push(key.to_string());
        } else if value.starts_with('{') {
            let (child, _) = parse_namespace_type(value)?;
            namespace.children.insert(key.to_string(), child);
        }
    }
    Ok(namespace)
}

/// Strips Luau line and block comments by replacing them with spaces.
fn strip_comments(source: &str) -> String {
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

/// Reads one Luau identifier and returns it with the remaining suffix.
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

    if end == 0 {
        None
    } else {
        Some((&source[..end], &source[end..]))
    }
}

/// Trims leading whitespace.
fn trim_start(source: &str) -> &str {
    source.trim_start()
}

/// Skips to and past the next newline character.
fn skip_to_newline(source: &str) -> &str {
    if let Some(index) = source.find('\n') {
        &source[index + 1..]
    } else {
        ""
    }
}

/// Finds the matching `}` for a `{` at index 0 of `source`.
fn find_matching_brace(source: &str) -> Result<usize, ModuleSchemaError> {
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
            return Err(ModuleSchemaError::Malformed(
                "unbalanced punctuation in namespace body".into(),
            ));
        }
    }
    Err(ModuleSchemaError::Malformed(
        "unterminated namespace body".into(),
    ))
}

/// Splits `body` on top-level commas only.
fn split_top_level_commas(body: &str) -> Vec<&str> {
    let mut output = Vec::new();
    let mut depth = 0_i32;
    let mut start = 0;
    for (index, character) in body.char_indices() {
        match character {
            '{' | '(' | '[' => depth += 1,
            '}' | ')' | ']' => depth -= 1,
            ',' if depth == 0 => {
                output.push(&body[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    output.push(&body[start..]);
    output
}

/// Finds the position of a standalone `end` keyword.
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

/// Returns true if `byte` could be part of a Luau identifier.
fn is_ident_byte(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::{ModuleSchemaError, extract_module_schema};

    #[test]
    fn extracts_module_root_and_functions() {
        let source = r#"
declare fs: {
    read: (path: string) -> string,
    write: (path: string, content: string) -> (),
    list: (path: string) -> {string},
}
"#;
        let schema = extract_module_schema(source).expect("schema");
        let root = schema.root.expect("root");
        assert_eq!("fs", root.name);
        assert_eq!(vec!["read", "write", "list"], root.namespace.functions);
        assert!(root.namespace.children.is_empty());
    }

    #[test]
    fn extracts_nested_namespaces() {
        let source = r#"
declare aws: {
    s3: {
        list_buckets: () -> {string},
    },
    ec2: {
        describe_instances: () -> {string},
    },
}
"#;
        let schema = extract_module_schema(source).expect("schema");
        let root = schema.root.expect("root");
        let s3 = root.namespace.children.get("s3").expect("s3");
        let ec2 = root.namespace.children.get("ec2").expect("ec2");
        assert_eq!(vec!["list_buckets"], s3.functions);
        assert_eq!(vec!["describe_instances"], ec2.functions);
    }

    #[test]
    fn extracts_class_methods() {
        let source = r#"
declare class Page
    goto: (self, url: string) -> ()
    function click(self, sel: string): boolean
end
"#;
        let schema = extract_module_schema(source).expect("schema");
        let class = schema.classes.get("Page").expect("class");
        assert_eq!(vec!["goto", "click"], class.methods);
    }

    #[test]
    fn extracts_module_and_classes_together() {
        let source = r#"
declare class Store
    get: (self, key: string) -> string
    set: (self, key: string, value: string) -> ()
end

declare class Txn
    set: (self, key: string, value: string) -> ()
    commit: (self) -> ()
end

declare kv: {
    open: (name: string) -> Store,
}
"#;
        let schema = extract_module_schema(source).expect("schema");
        let root = schema.root.expect("root");
        assert_eq!("kv", root.name);
        assert_eq!(vec!["open"], root.namespace.functions);
        assert_eq!(
            vec!["get", "set"],
            schema.classes.get("Store").expect("store").methods
        );
        assert_eq!(
            vec!["set", "commit"],
            schema.classes.get("Txn").expect("txn").methods
        );
    }

    #[test]
    fn rejects_two_module_roots() {
        let source = r#"
declare fs: { f: () -> () }
declare fs2: { g: () -> () }
"#;
        let error = extract_module_schema(source).expect_err("schema should fail");
        match error {
            ModuleSchemaError::MultipleRoots { first, second } => {
                assert_eq!("fs", first);
                assert_eq!("fs2", second);
            }
            other => panic!("expected MultipleRoots, got {other:?}"),
        }
    }

    #[test]
    fn ignores_comments_and_plain_fields() {
        let source = r#"
-- module-root with comment
declare fs: {
    version: string,
    -- inline
    read: (path: string) -> string,
}
"#;
        let schema = extract_module_schema(source).expect("schema");
        let root = schema.root.expect("root");
        assert_eq!(vec!["read"], root.namespace.functions);
    }
}
