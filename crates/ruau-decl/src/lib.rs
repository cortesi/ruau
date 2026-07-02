//! Typed Luau declaration authoring model.
//!
//! Builds, validates, and renders `.d.luau` declarations without depending on
//! the parser, typechecker, or VM. Hosts use this when declarations are a
//! product of Rust data rather than handwritten strings: build aliases,
//! globals, functions, and classes, call [`Builder::finish`], then render the
//! resulting declaration for `ruau_typecheck::builtins::DeclSource::Text`.
//! Canopy-style host surfaces follow exactly that render-to-text flow.

#![warn(missing_docs)]

use std::{
    borrow::Cow,
    collections::{BTreeSet, HashMap},
};

mod error;

pub use error::{Error, Errors};

/// Owned or static Luau declaration text.
pub type Text = Cow<'static, str>;

/// A Luau type expression.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Ty {
    /// `boolean`.
    Boolean,
    /// `number`.
    Number,
    /// `string`.
    String,
    /// `nil`.
    Nil,
    /// `any`.
    Any,
    /// Reference to an alias, class, or external type.
    Named(Text),
    /// String-literal singleton, such as `"up"`.
    Literal(Text),
    /// Optional type, rendered as `T?`.
    Optional(Box<Self>),
    /// Array-like table, rendered as `{T}`.
    Array(Box<Self>),
    /// Map-like table, rendered as `{ [K]: V }`.
    Map(Box<Self>, Box<Self>),
    /// Union type, rendered as `A | B`.
    Union(Vec<Self>),
    /// Intersection type, rendered as `A & B`.
    Intersection(Vec<Self>),
    /// Structural table type.
    Table(Vec<Field>),
    /// Function type.
    Function(Box<FnSig>),
}

impl Ty {
    /// Builds a named type reference.
    #[must_use]
    pub fn named(name: impl Into<Text>) -> Self {
        Self::Named(name.into())
    }

    /// Builds a union of string literal singletons.
    #[must_use]
    pub fn literals(values: impl IntoIterator<Item = impl Into<Text>>) -> Self {
        Self::union(values.into_iter().map(|value| Self::Literal(value.into())))
    }

    /// Builds a structural table type.
    #[must_use]
    pub fn table(fields: impl IntoIterator<Item = Field>) -> Self {
        Self::Table(fields.into_iter().collect())
    }

    /// Builds a union, flattening nested unions, deduplicating members, and
    /// folding `nil` into an optional wrapper when possible.
    #[must_use]
    pub fn union(tys: impl IntoIterator<Item = Self>) -> Self {
        let mut out = Vec::new();
        let mut saw_nil = false;
        for ty in tys {
            match ty {
                Self::Union(tys) => {
                    for ty in tys {
                        push_union_member(&mut out, &mut saw_nil, ty);
                    }
                }
                ty => push_union_member(&mut out, &mut saw_nil, ty),
            }
        }
        match (out.len(), saw_nil) {
            (0, true) => Self::Nil,
            (0, false) => Self::Union(Vec::new()),
            (1, true) => out.pop().expect("length checked").optional(),
            (_, true) => Self::Optional(Box::new(Self::Union(out))),
            (1, false) => out.pop().expect("length checked"),
            (_, false) => Self::Union(out),
        }
    }

    /// Builds a map table type.
    #[must_use]
    pub fn map(key: Self, value: Self) -> Self {
        Self::Map(Box::new(key), Box::new(value))
    }

    /// Builds a function type.
    #[must_use]
    pub fn func(sig: FnSig) -> Self {
        Self::Function(Box::new(sig))
    }

    /// Returns an optional version of this type.
    #[must_use]
    pub fn optional(self) -> Self {
        match self {
            Self::Optional(_) => self,
            Self::Union(members) => Self::Optional(Box::new(Self::Union(members))),
            ty => Self::Optional(Box::new(ty)),
        }
    }

    /// Returns an array table containing this type.
    #[must_use]
    pub fn array(self) -> Self {
        Self::Array(Box::new(self))
    }

    /// Renders this type expression.
    #[must_use]
    pub fn render(&self) -> String {
        self.render_with_prec(Prec::Lowest, 0)
    }

    fn render_with_prec(&self, parent: Prec, indent: usize) -> String {
        match self {
            Self::Boolean => "boolean".to_owned(),
            Self::Number => "number".to_owned(),
            Self::String => "string".to_owned(),
            Self::Nil => "nil".to_owned(),
            Self::Any => "any".to_owned(),
            Self::Named(name) => name.to_string(),
            Self::Literal(value) => quote_luau_string(value),
            Self::Optional(ty) => {
                let rendered = if matches!(
                    ty.as_ref(),
                    Self::Function(_) | Self::Union(_) | Self::Intersection(_)
                ) {
                    format!("({})", ty.render_with_prec(Prec::Lowest, indent))
                } else {
                    ty.render_with_prec(Prec::Optional, indent)
                };
                format!("{rendered}?")
            }
            Self::Array(ty) => format!("{{{}}}", ty.render_with_prec(Prec::Lowest, indent)),
            Self::Map(key, value) => format!(
                "{{ [{}]: {} }}",
                key.render_with_prec(Prec::Lowest, indent),
                value.render_with_prec(Prec::Lowest, indent)
            ),
            Self::Union(tys) => render_joined_type(tys, " | ", Prec::Union, parent, indent),
            Self::Intersection(tys) => {
                render_joined_type(tys, " & ", Prec::Intersection, parent, indent)
            }
            Self::Table(fields) => render_table(fields, indent),
            Self::Function(sig) => {
                let rendered = sig.render_type(indent);
                parenthesize_if(Prec::Function < parent, rendered)
            }
        }
    }

    fn collect_named_refs<'a>(&'a self, refs: &mut Vec<&'a str>) {
        match self {
            Self::Named(name) => refs.push(name),
            Self::Optional(ty) | Self::Array(ty) => ty.collect_named_refs(refs),
            Self::Map(key, value) => {
                key.collect_named_refs(refs);
                value.collect_named_refs(refs);
            }
            Self::Union(tys) | Self::Intersection(tys) => {
                for ty in tys {
                    ty.collect_named_refs(refs);
                }
            }
            Self::Table(fields) => {
                for field in fields {
                    field.ty.collect_named_refs(refs);
                }
            }
            Self::Function(sig) => sig.collect_named_refs(refs),
            Self::Boolean
            | Self::Number
            | Self::String
            | Self::Nil
            | Self::Any
            | Self::Literal(_) => {}
        }
    }
}

/// A field in a table type or class declaration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Field {
    /// Field name.
    pub name: Text,
    /// Field type.
    pub ty: Ty,
    /// Optional documentation text.
    pub doc: Option<Text>,
}

impl Field {
    /// Builds a field.
    #[must_use]
    pub fn new(name: impl Into<Text>, ty: Ty) -> Self {
        Self {
            name: name.into(),
            ty,
            doc: None,
        }
    }

    /// Attaches documentation.
    #[must_use]
    pub fn doc(mut self, doc: impl Into<Text>) -> Self {
        self.doc = Some(doc.into());
        self
    }
}

/// A function parameter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Param {
    /// Parameter name.
    pub name: Text,
    /// Parameter type.
    pub ty: Ty,
    /// Optional documentation text.
    pub doc: Option<Text>,
}

impl Param {
    /// Builds a parameter.
    #[must_use]
    pub fn new(name: impl Into<Text>, ty: Ty) -> Self {
        Self {
            name: name.into(),
            ty,
            doc: None,
        }
    }

    /// Attaches documentation.
    #[must_use]
    pub fn doc(mut self, doc: impl Into<Text>) -> Self {
        self.doc = Some(doc.into());
        self
    }
}

impl From<(&'static str, Ty)> for Param {
    fn from((name, ty): (&'static str, Ty)) -> Self {
        Self::new(name, ty)
    }
}

/// A function signature.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FnSig {
    /// Positional parameters.
    pub params: Vec<Param>,
    /// Variadic tail parameter type.
    pub varargs: Option<Ty>,
    /// Return pack. An empty pack renders as `()`.
    pub returns: Vec<Ty>,
}

impl FnSig {
    /// Builds an empty function signature.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a parameter.
    #[must_use]
    pub fn param(mut self, param: impl Into<Param>) -> Self {
        self.params.push(param.into());
        self
    }

    /// Sets the variadic tail parameter.
    #[must_use]
    pub fn varargs(mut self, ty: Ty) -> Self {
        self.varargs = Some(ty);
        self
    }

    /// Appends one return value to the return pack.
    #[must_use]
    pub fn ret(mut self, ty: Ty) -> Self {
        self.returns.push(ty);
        self
    }

    fn render_type(&self, indent: usize) -> String {
        format!(
            "({}) -> {}",
            self.render_params(ParamContext::FunctionType, indent),
            self.render_returns(indent)
        )
    }

    fn render_declaration_tail(&self, indent: usize) -> String {
        format!(
            "({}): {}",
            self.render_params(ParamContext::Declaration, indent),
            self.render_returns(indent)
        )
    }

    fn render_class_method_tail(&self, indent: usize) -> String {
        let params = self.render_params(ParamContext::Declaration, indent);
        let params = if params.is_empty() {
            "self".to_owned()
        } else {
            format!("self, {params}")
        };
        format!("({params}): {}", self.render_returns(indent))
    }

    fn render_params(&self, context: ParamContext, indent: usize) -> String {
        let mut params = Vec::new();
        for param in &self.params {
            params.push(format!(
                "{}: {}",
                param.name,
                param.ty.render_with_prec(Prec::Lowest, indent)
            ));
        }
        if let Some(varargs) = &self.varargs {
            let ty = varargs.render_with_prec(Prec::Lowest, indent);
            params.push(match context {
                ParamContext::Declaration => format!("...: {ty}"),
                ParamContext::FunctionType => format!("...{ty}"),
            });
        }
        params.join(", ")
    }

    fn render_returns(&self, indent: usize) -> String {
        match self.returns.as_slice() {
            [] => "()".to_owned(),
            [ty] => ty.render_with_prec(Prec::Lowest, indent),
            tys => {
                let rendered = tys
                    .iter()
                    .map(|ty| ty.render_with_prec(Prec::Lowest, indent))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("({rendered})")
            }
        }
    }

    fn collect_named_refs<'a>(&'a self, refs: &mut Vec<&'a str>) {
        for param in &self.params {
            param.ty.collect_named_refs(refs);
        }
        if let Some(varargs) = &self.varargs {
            varargs.collect_named_refs(refs);
        }
        for ty in &self.returns {
            ty.collect_named_refs(refs);
        }
    }
}

/// An `export type Name = T` item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Alias {
    /// Alias name.
    pub name: Text,
    /// Alias body.
    pub ty: Ty,
    /// Optional documentation text.
    pub doc: Option<Text>,
}

impl Alias {
    /// Builds an alias.
    #[must_use]
    pub fn new(name: impl Into<Text>, ty: Ty) -> Self {
        Self {
            name: name.into(),
            ty,
            doc: None,
        }
    }

    /// Attaches documentation.
    #[must_use]
    pub fn doc(mut self, doc: impl Into<Text>) -> Self {
        self.doc = Some(doc.into());
        self
    }
}

/// A `declare name: T` item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Global {
    /// Global name.
    pub name: Text,
    /// Global type.
    pub ty: Ty,
    /// Optional documentation text.
    pub doc: Option<Text>,
}

impl Global {
    /// Builds a global declaration.
    #[must_use]
    pub fn new(name: impl Into<Text>, ty: Ty) -> Self {
        Self {
            name: name.into(),
            ty,
            doc: None,
        }
    }

    /// Attaches documentation.
    #[must_use]
    pub fn doc(mut self, doc: impl Into<Text>) -> Self {
        self.doc = Some(doc.into());
        self
    }
}

/// A `declare function name(...): R` item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Func {
    /// Function name.
    pub name: Text,
    /// Function signature.
    pub sig: FnSig,
    /// Optional documentation text.
    pub doc: Option<Text>,
}

impl Func {
    /// Builds a function declaration.
    #[must_use]
    pub fn new(name: impl Into<Text>, sig: FnSig) -> Self {
        Self {
            name: name.into(),
            sig,
            doc: None,
        }
    }

    /// Attaches documentation.
    #[must_use]
    pub fn doc(mut self, doc: impl Into<Text>) -> Self {
        self.doc = Some(doc.into());
        self
    }
}

/// A method inside a class declaration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Method {
    /// Method name.
    pub name: Text,
    /// Method signature.
    pub sig: FnSig,
    /// Optional documentation text.
    pub doc: Option<Text>,
}

impl Method {
    /// Builds a class method.
    #[must_use]
    pub fn new(name: impl Into<Text>, sig: FnSig) -> Self {
        Self {
            name: name.into(),
            sig,
            doc: None,
        }
    }

    /// Attaches documentation.
    #[must_use]
    pub fn doc(mut self, doc: impl Into<Text>) -> Self {
        self.doc = Some(doc.into());
        self
    }
}

/// A `declare class Name ... end` item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Class {
    /// Class name.
    pub name: Text,
    /// Class fields.
    pub fields: Vec<Field>,
    /// Class methods.
    pub methods: Vec<Method>,
    /// Optional documentation text.
    pub doc: Option<Text>,
}

impl Class {
    /// Builds an empty class declaration.
    #[must_use]
    pub fn new(name: impl Into<Text>) -> Self {
        Self {
            name: name.into(),
            fields: Vec::new(),
            methods: Vec::new(),
            doc: None,
        }
    }

    /// Appends a class field.
    #[must_use]
    pub fn field(mut self, field: Field) -> Self {
        self.fields.push(field);
        self
    }

    /// Appends a class method.
    #[must_use]
    pub fn method(mut self, method: Method) -> Self {
        self.methods.push(method);
        self
    }

    /// Attaches documentation.
    #[must_use]
    pub fn doc(mut self, doc: impl Into<Text>) -> Self {
        self.doc = Some(doc.into());
        self
    }

    /// Renders just this class declaration.
    #[must_use]
    pub fn render(&self) -> String {
        render_class(self)
    }
}

/// Ordered declaration builder with finish-time validation.
#[derive(Debug, Default)]
pub struct Builder {
    items: Vec<BuildItem>,
}

impl Builder {
    /// Builds an empty declaration builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a banner comment section to the rendered output.
    pub fn section(&mut self, title: impl Into<Text>) {
        self.items.push(BuildItem::Section(title.into()));
    }

    /// Adds an alias declaration.
    pub fn alias(&mut self, alias: Alias) {
        self.items.push(BuildItem::Item(Item::Alias(alias)));
    }

    /// Adds a class declaration.
    pub fn class(&mut self, class: Class) {
        self.items.push(BuildItem::Item(Item::Class(class)));
    }

    /// Adds a global declaration.
    pub fn global(&mut self, global: Global) {
        self.items.push(BuildItem::Item(Item::Global(global)));
    }

    /// Adds a function declaration.
    pub fn function(&mut self, func: Func) {
        self.items.push(BuildItem::Item(Item::Func(func)));
    }

    /// Declares a type name that is provided by another module or preamble.
    pub fn extern_ty(&mut self, name: impl Into<Text>) {
        self.items.push(BuildItem::Extern(name.into()));
    }

    /// Returns whether the builder already contains `name`.
    ///
    /// This includes aliases, classes, globals, functions, and external type
    /// declarations queued so far.
    #[must_use]
    pub fn contains_name(&self, name: &str) -> bool {
        self.items.iter().any(|item| match item {
            BuildItem::Section(_) => false,
            BuildItem::Extern(existing) => existing == name,
            BuildItem::Item(item) => item.name() == name,
        })
    }

    /// Validates and returns a renderable declaration module.
    ///
    /// # Errors
    /// Returns every validation error found in the builder: invalid
    /// identifiers, conflicting items, and unresolved named type references.
    pub fn finish(self) -> Result<DeclModule, Errors> {
        let mut errors = Vec::new();
        let mut registered: HashMap<String, Item> = HashMap::new();
        let mut type_names = BTreeSet::new();
        let mut output = Vec::new();

        for item in &self.items {
            if let BuildItem::Extern(name) = item {
                validate_item_name(&mut errors, "extern type", name);
                type_names.insert(name.to_string());
            }
        }

        for item in self.items {
            match item {
                BuildItem::Section(title) => output.push(ModuleItem::Section(title)),
                BuildItem::Extern(_) => {}
                BuildItem::Item(item) => {
                    validate_item(&mut errors, &item);
                    if item.defines_type() {
                        type_names.insert(item.name().to_owned());
                    }
                    let key = item.name().to_owned();
                    if let Some(first) = registered.get(&key) {
                        if first != &item {
                            errors.push(Error::ConflictingItem {
                                name: key,
                                first: first.render_single_line(),
                                second: item.render_single_line(),
                            });
                        }
                    } else {
                        registered.insert(key, item.clone());
                        output.push(ModuleItem::Item(item));
                    }
                }
            }
        }

        for item in &output {
            if let ModuleItem::Item(item) = item {
                validate_refs(&mut errors, item, &type_names);
            }
        }

        if errors.is_empty() {
            Ok(DeclModule { items: output })
        } else {
            Err(Errors::new(errors))
        }
    }
}

/// A validated Luau declaration module.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeclModule {
    items: Vec<ModuleItem>,
}

impl DeclModule {
    /// Renders the declaration module.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        let mut first = true;
        for item in &self.items {
            if !first {
                out.push('\n');
            }
            first = false;
            match item {
                ModuleItem::Section(title) => {
                    out.push_str(&format!("-- ===== {title} =====\n"));
                }
                ModuleItem::Item(item) => {
                    out.push_str(&item.render());
                    out.push('\n');
                }
            }
        }
        out
    }
}

/// Declaration source accepted by embedding APIs.
///
/// Text declarations keep hand-authored `.d.luau` snippets cheap to pass
/// through, while model declarations let hosts build structured declarations
/// once and render them at the API boundary.
#[derive(Clone, Copy, Debug)]
pub enum DeclSource<'a> {
    /// Borrowed `.d.luau` text.
    Text(&'a str),
    /// Borrowed declaration model.
    Model(&'a DeclModule),
}

impl<'a> DeclSource<'a> {
    /// Renders the source, borrowing text declarations and owning model output.
    #[must_use]
    pub fn render(&self) -> Cow<'a, str> {
        match self {
            Self::Text(source) => Cow::Borrowed(source),
            Self::Model(module) => Cow::Owned(module.render()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum BuildItem {
    Section(Text),
    Extern(Text),
    Item(Item),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ModuleItem {
    Section(Text),
    Item(Item),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Item {
    Alias(Alias),
    Global(Global),
    Func(Func),
    Class(Class),
}

impl Item {
    fn name(&self) -> &str {
        match self {
            Self::Alias(item) => &item.name,
            Self::Global(item) => &item.name,
            Self::Func(item) => &item.name,
            Self::Class(item) => &item.name,
        }
    }

    fn defines_type(&self) -> bool {
        matches!(self, Self::Alias(_) | Self::Class(_))
    }

    fn render(&self) -> String {
        match self {
            Self::Alias(alias) => render_alias(alias),
            Self::Global(global) => render_global(global),
            Self::Func(func) => render_func(func),
            Self::Class(class) => render_class(class),
        }
    }

    fn render_single_line(&self) -> String {
        self.render()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum Prec {
    Lowest,
    Union,
    Intersection,
    Optional,
    Function,
}

#[derive(Clone, Copy)]
enum ParamContext {
    Declaration,
    FunctionType,
}

fn push_union_member(out: &mut Vec<Ty>, saw_nil: &mut bool, ty: Ty) {
    if matches!(ty, Ty::Nil) {
        *saw_nil = true;
    } else if !out.iter().any(|existing| existing == &ty) {
        out.push(ty);
    }
}

fn parenthesize_if(condition: bool, rendered: String) -> String {
    if condition {
        format!("({rendered})")
    } else {
        rendered
    }
}

fn render_joined_type(
    tys: &[Ty],
    separator: &str,
    prec: Prec,
    parent: Prec,
    indent: usize,
) -> String {
    let rendered = tys
        .iter()
        .map(|ty| ty.render_with_prec(prec, indent))
        .collect::<Vec<_>>()
        .join(separator);
    parenthesize_if(prec < parent, rendered)
}

fn render_table(fields: &[Field], indent: usize) -> String {
    if fields.is_empty() {
        return "{}".to_owned();
    }
    let current = " ".repeat(indent);
    let inner = " ".repeat(indent + 4);
    let mut out = String::from("{\n");
    for field in fields {
        render_doc(&mut out, field.doc.as_deref(), indent + 4);
        out.push_str(&inner);
        out.push_str(&render_field_name(&field.name));
        out.push_str(": ");
        out.push_str(&field.ty.render_with_prec(Prec::Lowest, indent + 4));
        out.push_str(",\n");
    }
    out.push_str(&current);
    out.push('}');
    out
}

fn render_alias(alias: &Alias) -> String {
    let mut out = String::new();
    render_doc(&mut out, alias.doc.as_deref(), 0);
    out.push_str(&format!(
        "export type {} = {}",
        alias.name,
        alias.ty.render()
    ));
    out
}

fn render_global(global: &Global) -> String {
    let mut out = String::new();
    render_doc(&mut out, global.doc.as_deref(), 0);
    out.push_str(&format!("declare {}: {}", global.name, global.ty.render()));
    out
}

fn render_func(func: &Func) -> String {
    let mut out = String::new();
    render_doc(&mut out, func.doc.as_deref(), 0);
    render_param_docs(&mut out, &func.sig, 0);
    out.push_str(&format!(
        "declare function {}{}",
        func.name,
        func.sig.render_declaration_tail(0)
    ));
    out
}

fn render_class(class: &Class) -> String {
    let mut out = String::new();
    render_doc(&mut out, class.doc.as_deref(), 0);
    out.push_str(&format!("declare class {}\n", class.name));
    for field in &class.fields {
        render_doc(&mut out, field.doc.as_deref(), 4);
        out.push_str("    ");
        out.push_str(&field.name);
        out.push_str(": ");
        out.push_str(&field.ty.render_with_prec(Prec::Lowest, 4));
        out.push('\n');
    }
    for method in &class.methods {
        render_doc(&mut out, method.doc.as_deref(), 4);
        render_param_docs(&mut out, &method.sig, 4);
        out.push_str("    function ");
        out.push_str(&method.name);
        out.push_str(&method.sig.render_class_method_tail(4));
        out.push('\n');
    }
    out.push_str("end");
    out
}

fn render_param_docs(out: &mut String, sig: &FnSig, indent: usize) {
    for param in &sig.params {
        if let Some(doc) = &param.doc {
            render_doc_line(out, &format!("@param {} {doc}", param.name), indent);
        }
    }
    if let Some(varargs) = &sig.varargs {
        let _ = varargs;
    }
}

fn render_doc(out: &mut String, doc: Option<&str>, indent: usize) {
    let Some(doc) = doc else {
        return;
    };
    for paragraph in doc.lines() {
        if paragraph.trim().is_empty() {
            render_doc_line(out, "", indent);
        } else {
            for line in wrap_doc(paragraph.trim(), 92usize.saturating_sub(indent)) {
                render_doc_line(out, &line, indent);
            }
        }
    }
}

fn render_doc_line(out: &mut String, line: &str, indent: usize) {
    out.push_str(&" ".repeat(indent));
    if line.is_empty() {
        out.push_str("---\n");
    } else {
        out.push_str("--- ");
        out.push_str(line);
        out.push('\n');
    }
}

fn wrap_doc(text: &str, width: usize) -> Vec<String> {
    if text.len() <= width {
        return vec![text.to_owned()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let pending_len = if current.is_empty() {
            word.len()
        } else {
            current.len() + 1 + word.len()
        };
        if pending_len > width && !current.is_empty() {
            lines.push(current);
            current = word.to_owned();
        } else {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn render_field_name(name: &str) -> String {
    if is_luau_identifier(name) {
        name.to_owned()
    } else {
        format!("[{}]", quote_luau_string(name))
    }
}

fn quote_luau_string(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => out.push_str(&format!("\\u{{{:x}}}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn validate_item(errors: &mut Vec<Error>, item: &Item) {
    match item {
        Item::Alias(alias) => validate_item_name(errors, "alias", &alias.name),
        Item::Global(global) => validate_item_name(errors, "global", &global.name),
        Item::Func(func) => {
            validate_item_name(errors, "function", &func.name);
            validate_sig_names(errors, &format!("function {}", func.name), &func.sig);
        }
        Item::Class(class) => {
            validate_item_name(errors, "class", &class.name);
            for field in &class.fields {
                validate_identifier(errors, &format!("class {} field", class.name), &field.name);
            }
            for method in &class.methods {
                validate_identifier(
                    errors,
                    &format!("class {} method", class.name),
                    &method.name,
                );
                validate_sig_names(
                    errors,
                    &format!("class {} method {}", class.name, method.name),
                    &method.sig,
                );
            }
        }
    }
}

fn validate_sig_names(errors: &mut Vec<Error>, location: &str, sig: &FnSig) {
    for param in &sig.params {
        validate_identifier(errors, &format!("{location} parameter"), &param.name);
    }
}

fn validate_item_name(errors: &mut Vec<Error>, kind: &str, name: &str) {
    validate_identifier(errors, kind, name);
}

fn validate_identifier(errors: &mut Vec<Error>, location: &str, name: &str) {
    if !is_luau_identifier(name) {
        errors.push(Error::InvalidIdentifier {
            location: location.to_owned(),
            name: name.to_owned(),
        });
    }
}

fn validate_refs(errors: &mut Vec<Error>, item: &Item, type_names: &BTreeSet<String>) {
    let mut refs = Vec::new();
    match item {
        Item::Alias(alias) => alias.ty.collect_named_refs(&mut refs),
        Item::Global(global) => global.ty.collect_named_refs(&mut refs),
        Item::Func(func) => func.sig.collect_named_refs(&mut refs),
        Item::Class(class) => {
            for field in &class.fields {
                field.ty.collect_named_refs(&mut refs);
            }
            for method in &class.methods {
                method.sig.collect_named_refs(&mut refs);
            }
        }
    }
    for name in refs {
        if !type_names.contains(name) {
            errors.push(Error::UnknownType {
                location: item.name().to_owned(),
                name: name.to_owned(),
            });
        }
    }
}

fn is_luau_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }
    if !chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric()) {
        return false;
    }
    !LUAU_KEYWORDS.contains(&name)
}

const LUAU_KEYWORDS: &[&str] = &[
    "and", "break", "class", "continue", "do", "else", "elseif", "end", "export", "false", "for",
    "function", "if", "in", "local", "nil", "not", "or", "repeat", "return", "self", "then",
    "true", "type", "until", "while",
];

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use ruau_ast::parse::{ParseConfig, parse_file_with};

    use super::*;

    fn assert_parses(source: &str) {
        let result = parse_file_with(
            source,
            &ParseConfig {
                allow_declaration_syntax: true,
                ..ParseConfig::upstream_default()
            },
        );
        assert!(
            result.errors.is_empty(),
            "rendered declaration did not parse:\n{source}\n{:?}",
            result.errors
        );
    }

    #[test]
    fn renders_docs_tables_functions_and_classes() {
        let mut builder = Builder::new();
        builder.extern_ty("NodeId");
        builder.alias(
            Alias::new(
                "OpenOpts",
                Ty::table([
                    Field::new("path", Ty::String).doc("Path to open."),
                    Field::new("line", Ty::Number.optional()).doc("One-based line."),
                    Field::new("has-hyphen", Ty::Boolean),
                ]),
            )
            .doc("Options controlling open."),
        );
        builder.class(
            Class::new("Editor")
                .field(Field::new("id", Ty::named("NodeId")))
                .method(Method::new(
                    "insert",
                    FnSig::new()
                        .param(Param::new("text", Ty::String).doc("Text to insert."))
                        .ret(Ty::Boolean),
                )),
        );
        builder.global(Global::new(
            "editor",
            Ty::table([Field::new(
                "open",
                Ty::func(
                    FnSig::new()
                        .param(("opts", Ty::named("OpenOpts")))
                        .ret(Ty::Boolean),
                ),
            )]),
        ));

        let rendered = builder.finish().expect("declaration is valid").render();
        assert!(rendered.contains("export type OpenOpts = {"));
        assert!(rendered.contains("[\"has-hyphen\"]: boolean"));
        assert!(rendered.contains("declare class Editor"));
        assert_parses(&rendered);
    }

    #[test]
    fn validates_all_errors() {
        let mut builder = Builder::new();
        builder.alias(Alias::new("bad-name", Ty::named("Missing")));
        builder.alias(Alias::new("OpenOpts", Ty::String));
        builder.alias(Alias::new("OpenOpts", Ty::Number));
        builder.function(Func::new(
            "call",
            FnSig::new().param(("end", Ty::named("MissingAgain"))),
        ));

        let errors = builder.finish().expect_err("invalid declaration fails");
        assert_eq!(errors.errors().len(), 5);
        assert!(errors.errors().iter().any(
            |error| matches!(error, Error::ConflictingItem { name, .. } if name == "OpenOpts")
        ));
    }

    #[test]
    fn builder_reports_queued_names_before_finish() {
        let mut builder = Builder::new();
        builder.section("Core");
        builder.extern_ty("NodeId");
        builder.alias(Alias::new("AliasName", Ty::String));
        builder.class(Class::new("Widget"));
        builder.global(Global::new("widget", Ty::named("Widget")));
        builder.function(Func::new(
            "make_widget",
            FnSig::new().ret(Ty::named("Widget")),
        ));

        for name in ["NodeId", "AliasName", "Widget", "widget", "make_widget"] {
            assert!(builder.contains_name(name), "{name} should be present");
        }
        assert!(!builder.contains_name("missing"));
    }

    #[test]
    fn union_dedups_and_folds_nil_to_optional() {
        let ty = Ty::union([Ty::String, Ty::Nil, Ty::String]);
        assert_eq!(ty.render(), "string?");

        let ty = Ty::union([Ty::String, Ty::Number, Ty::Nil]);
        assert_eq!(ty.render(), "(string | number)?");
    }

    proptest! {
        #[test]
        fn rendered_literal_union_alias_parses(values in prop::collection::vec("[a-z]{1,8}", 1..8)) {
            let mut builder = Builder::new();
            builder.alias(Alias::new("Choice", Ty::literals(values)));
            let rendered = builder.finish().expect("generated declaration is valid").render();
            assert_parses(&rendered);
        }
    }
}
