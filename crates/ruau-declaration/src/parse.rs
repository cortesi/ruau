//! Validated declaration inspection.

use std::collections::{BTreeMap, BTreeSet};

use ruau_syntax::{Location, Stat, Type, parse};

/// Source form selected as the module root.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParsedRootKind {
    /// No module root is present.
    None,
    /// The exported `Module` alias is the root.
    ModuleAlias,
    /// One `declare <name>` global is the root.
    DeclaredGlobal,
}

/// One parsed type alias.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedAlias {
    /// Alias name.
    pub name: String,
    /// Whether the alias is exported.
    pub exported: bool,
    /// Inspected type shape.
    pub value: ParsedType,
}

/// One parsed class declaration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedClass {
    /// Class name.
    pub name: String,
    /// Fields and methods in declaration order.
    pub fields: Vec<ParsedField>,
}

/// One parsed global declaration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedGlobal {
    /// Global name.
    pub name: String,
    /// Declared type shape.
    pub value: ParsedType,
}

/// One root-level declared function.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedFunction {
    /// Function name.
    pub name: String,
}

/// One parsed record or class field.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedField {
    /// Field name.
    pub name: String,
    /// Field type shape.
    pub value: ParsedType,
    /// Whether declaration method syntax produced this field.
    pub method: bool,
}

/// Type forms used by declaration inspection queries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParsedType {
    /// Function type.
    Function,
    /// Record type with ordered fields.
    Record(Vec<ParsedField>),
    /// Unqualified named reference.
    Reference(String),
    /// Intersection members.
    Intersection(Vec<Self>),
    /// A valid type form that inspection does not need to expand.
    Other,
}

/// One located declaration inspection failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeclarationParseError {
    /// Human-readable failure detail.
    pub message: String,
    /// Source location, when available.
    pub location: Option<Location>,
}

impl std::fmt::Display for DeclarationParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(location) = self.location {
            write!(
                formatter,
                "{}:{}: {}",
                location.begin.line + 1,
                location.begin.column + 1,
                self.message
            )
        } else {
            formatter.write_str(&self.message)
        }
    }
}

impl std::error::Error for DeclarationParseError {}

/// A parsed declaration with stable inspection queries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedDeclaration {
    source: String,
    aliases: Vec<ParsedAlias>,
    classes: Vec<ParsedClass>,
    globals: Vec<ParsedGlobal>,
    functions: Vec<ParsedFunction>,
    root: Option<ParsedType>,
    root_kind: ParsedRootKind,
}

impl ParsedDeclaration {
    /// Returns the validated source text.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Returns aliases in declaration order.
    #[must_use]
    pub fn aliases(&self) -> &[ParsedAlias] {
        &self.aliases
    }

    /// Returns classes in declaration order.
    #[must_use]
    pub fn classes(&self) -> &[ParsedClass] {
        &self.classes
    }

    /// Returns globals in declaration order.
    #[must_use]
    pub fn globals(&self) -> &[ParsedGlobal] {
        &self.globals
    }

    /// Returns root-level declared functions in declaration order.
    #[must_use]
    pub fn functions(&self) -> &[ParsedFunction] {
        &self.functions
    }

    /// Returns the selected module-root source form.
    #[must_use]
    pub const fn root_kind(&self) -> ParsedRootKind {
        self.root_kind
    }

    /// Returns module-root function names in declaration order.
    #[must_use]
    pub fn root_function_names(&self) -> Vec<&str> {
        let aliases = self
            .aliases
            .iter()
            .map(|alias| (alias.name.as_str(), &alias.value))
            .collect::<BTreeMap<_, _>>();
        let mut names = Vec::new();
        if let Some(root) = &self.root {
            collect_functions(root, &aliases, &mut BTreeSet::new(), &mut names);
        }
        names
    }

    /// Returns class methods in class and declaration order.
    #[must_use]
    pub fn class_methods(&self) -> Vec<(&str, Vec<&str>)> {
        let aliases = self
            .aliases
            .iter()
            .map(|alias| (alias.name.as_str(), &alias.value))
            .collect::<BTreeMap<_, _>>();
        self.classes
            .iter()
            .map(|class| {
                let methods = class
                    .fields
                    .iter()
                    .filter(|field| {
                        field.method
                            || resolves_to_function(&field.value, &aliases, &mut BTreeSet::new())
                    })
                    .map(|field| field.name.as_str())
                    .collect();
                (class.name.as_str(), methods)
            })
            .collect()
    }
}

/// Parses one declaration into the inspection model.
///
/// The accepted root grammar consists of type aliases, declared classes,
/// declared globals, declared functions, and one final return statement.
/// A declaration can have at most one declared-global module root.
///
/// # Errors
/// Returns a located parser, unsupported-form, or duplicate-root error.
pub fn parse_declaration(source: &str) -> Result<ParsedDeclaration, DeclarationParseError> {
    let parsed = parse::parse_with_config(
        source,
        &parse::Config {
            allow_declaration_syntax: true,
            ..parse::Config::default()
        },
    );
    if let Some(error) = parsed.errors.first() {
        return Err(DeclarationParseError {
            message: error.message.clone(),
            location: Some(error.location),
        });
    }
    let Stat::Block { body, .. } = parsed.root else {
        return Err(DeclarationParseError {
            message: "declaration root is not a statement block".to_owned(),
            location: None,
        });
    };
    let mut declaration = ParsedDeclaration {
        source: source.to_owned(),
        aliases: Vec::new(),
        classes: Vec::new(),
        globals: Vec::new(),
        functions: Vec::new(),
        root: None,
        root_kind: ParsedRootKind::None,
    };
    let mut module_alias = None;
    for stat in body {
        match stat {
            Stat::TypeAlias {
                name,
                value,
                exported,
                ..
            } => {
                let name = name.as_str().to_owned();
                let value = lower_type(&value);
                if exported && name == "Module" {
                    module_alias = Some(value.clone());
                }
                declaration.aliases.push(ParsedAlias {
                    name,
                    exported,
                    value,
                });
            }
            Stat::DeclareClass { name, props, .. } => {
                declaration.classes.push(ParsedClass {
                    name: name.as_str().to_owned(),
                    fields: props
                        .into_iter()
                        .map(|field| ParsedField {
                            name: field.name.as_str().to_owned(),
                            value: lower_type(&field.declared_type),
                            method: field.is_method,
                        })
                        .collect(),
                });
            }
            Stat::DeclareGlobal {
                name,
                declared_type,
                location,
                ..
            } => {
                if declaration.root_kind == ParsedRootKind::DeclaredGlobal {
                    return Err(DeclarationParseError {
                        message: format!(
                            "multiple declared module roots including `{}`",
                            name.as_str()
                        ),
                        location,
                    });
                }
                let value = lower_type(&declared_type);
                declaration.root = Some(value.clone());
                declaration.root_kind = ParsedRootKind::DeclaredGlobal;
                declaration.globals.push(ParsedGlobal {
                    name: name.as_str().to_owned(),
                    value,
                });
            }
            Stat::DeclareFunction { name, .. } => {
                declaration.functions.push(ParsedFunction {
                    name: name.as_str().to_owned(),
                });
            }
            Stat::Return { .. } => {}
            other => {
                return Err(DeclarationParseError {
                    message: "unsupported statement in declaration inspection".to_owned(),
                    location: other.location(),
                });
            }
        }
    }
    if declaration.root.is_none() {
        declaration.root = module_alias;
        if declaration.root.is_some() {
            declaration.root_kind = ParsedRootKind::ModuleAlias;
        }
    }
    Ok(declaration)
}

fn lower_type(value: &Type) -> ParsedType {
    match value {
        Type::Reference {
            prefix: None, name, ..
        } => ParsedType::Reference(name.as_str().to_owned()),
        Type::Group { inner, .. } => lower_type(inner),
        Type::Intersection { types, .. } => {
            ParsedType::Intersection(types.iter().map(lower_type).collect())
        }
        Type::Function { .. } => ParsedType::Function,
        Type::Table { props, .. } => ParsedType::Record(
            props
                .iter()
                .map(|field| ParsedField {
                    name: field.name.as_str().to_owned(),
                    value: lower_type(&field.prop_type),
                    method: false,
                })
                .collect(),
        ),
        _ => ParsedType::Other,
    }
}

fn collect_functions<'a>(
    value: &'a ParsedType,
    aliases: &BTreeMap<&'a str, &'a ParsedType>,
    visiting: &mut BTreeSet<&'a str>,
    names: &mut Vec<&'a str>,
) {
    match value {
        ParsedType::Record(fields) => {
            for field in fields {
                if resolves_to_function(&field.value, aliases, &mut BTreeSet::new())
                    && !names.contains(&field.name.as_str())
                {
                    names.push(&field.name);
                }
            }
        }
        ParsedType::Reference(name) if visiting.insert(name) => {
            if let Some(alias) = aliases.get(name.as_str()) {
                collect_functions(alias, aliases, visiting, names);
            }
            visiting.remove(name.as_str());
        }
        ParsedType::Intersection(values) => {
            for value in values {
                collect_functions(value, aliases, visiting, names);
            }
        }
        ParsedType::Function | ParsedType::Other | ParsedType::Reference(_) => {}
    }
}

fn resolves_to_function<'a>(
    value: &'a ParsedType,
    aliases: &BTreeMap<&'a str, &'a ParsedType>,
    visiting: &mut BTreeSet<&'a str>,
) -> bool {
    match value {
        ParsedType::Function => true,
        ParsedType::Reference(name) if visiting.insert(name) => {
            let result = aliases
                .get(name.as_str())
                .is_some_and(|value| resolves_to_function(value, aliases, visiting));
            visiting.remove(name.as_str());
            result
        }
        ParsedType::Intersection(values) => values
            .iter()
            .any(|value| resolves_to_function(value, aliases, visiting)),
        ParsedType::Record(_) | ParsedType::Other | ParsedType::Reference(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspects_roots_aliases_classes_globals_and_functions() {
        let parsed = parse_declaration(
            r#"
export type Handler = (string) -> string
export type Module = { first: Handler, second: number }
declare class Handle
    close: (self: Handle) -> ()
end
declare demo: Module
declare function helper(value: string): string
return demo
"#,
        )
        .expect("declaration parses");
        assert_eq!(parsed.root_kind(), ParsedRootKind::DeclaredGlobal);
        assert_eq!(parsed.root_function_names(), ["first"]);
        assert_eq!(parsed.class_methods(), [("Handle", vec!["close"])]);
        assert_eq!(parsed.aliases().len(), 2);
        assert_eq!(parsed.globals()[0].name, "demo");
        assert_eq!(parsed.functions()[0].name, "helper");
    }

    #[test]
    fn reports_locations_and_rejects_duplicate_roots() {
        let parse_error = parse_declaration("declare broken:").expect_err("syntax must fail");
        assert!(parse_error.location.is_some());
        let duplicate = parse_declaration("declare a: {}\ndeclare b: {}")
            .expect_err("duplicate root must fail");
        assert!(duplicate.message.contains("multiple declared module roots"));
        assert!(duplicate.location.is_some());
    }
}
