//! Paths into type and type-pack graphs.

use serde::{Deserialize, Serialize};

use super::{Arena, TypeId, TypeKind, TypePackId, TypePackKind};

impl Arena {
    /// Traverses a path from a type or type-pack root and returns the type at
    /// the destination.
    #[must_use]
    pub(crate) fn traverse_path_for_type(
        &self,
        root: impl Into<TypePathRoot>,
        path: &TypePath,
    ) -> Option<TypeId> {
        match self.traverse_path(root.into(), path)? {
            TypePathValue::Type(id) => Some(id),
            TypePathValue::Pack(_) => None,
        }
    }

    /// Traverses a path from a type or type-pack root and returns the type pack
    /// at the destination.
    ///
    /// Pack slices allocate a fresh pack node, matching upstream's use of a
    /// scratch arena for traversal results.
    pub(crate) fn traverse_path_for_pack(
        &mut self,
        root: impl Into<TypePathRoot>,
        path: &TypePath,
    ) -> Option<TypePackId> {
        match self.traverse_path_mut(root.into(), path)? {
            TypePathValue::Type(_) => None,
            TypePathValue::Pack(id) => Some(id),
        }
    }

    fn traverse_path(&self, root: TypePathRoot, path: &TypePath) -> Option<TypePathValue> {
        let mut value = TypePathValue::from(root);
        for component in path.components() {
            value = self.traverse_component(value, component)?;
        }
        Some(value)
    }

    fn traverse_path_mut(&mut self, root: TypePathRoot, path: &TypePath) -> Option<TypePathValue> {
        let mut value = TypePathValue::from(root);
        for component in path.components() {
            value = match (value, component) {
                (TypePathValue::Pack(pack), TypePathComponent::PackSlice { start }) => {
                    TypePathValue::Pack(self.slice_pack(pack, *start)?)
                }
                (value, component) => self.traverse_component(value, component)?,
            };
        }
        Some(value)
    }

    fn traverse_component(
        &self,
        value: TypePathValue,
        component: &TypePathComponent,
    ) -> Option<TypePathValue> {
        match (value, component) {
            (TypePathValue::Type(id), TypePathComponent::Property { name, .. }) => self
                .property_type(id, name, &mut Vec::new())
                .map(TypePathValue::Type),
            (TypePathValue::Type(id), TypePathComponent::Index { index }) => {
                let id = self.follow(id);
                match self.get(id) {
                    TypeKind::Union(options) | TypeKind::Intersection(options) => {
                        options.get(*index).copied().map(TypePathValue::Type)
                    }
                    TypeKind::TypeFunctionInstance { arguments, .. } => {
                        arguments.get(*index).copied().map(TypePathValue::Type)
                    }
                    _ => None,
                }
            }
            (TypePathValue::Pack(id), TypePathComponent::Index { index }) => self
                .normalize_pack(id)
                .types
                .get(*index)
                .copied()
                .map(TypePathValue::Type),
            (TypePathValue::Type(id), TypePathComponent::TypeField(field)) => self
                .traverse_type_field(id, *field)
                .map(TypePathValue::Type),
            (TypePathValue::Pack(id), TypePathComponent::TypeField(TypeField::Variadic)) => {
                let id = self.follow_pack(id);
                match self.get_pack(id) {
                    TypePackKind::Variadic { ty } => Some(TypePathValue::Type(*ty)),
                    _ => None,
                }
            }
            (TypePathValue::Type(id), TypePathComponent::PackField(field)) => self
                .traverse_pack_field(id, *field)
                .map(TypePathValue::Pack),
            (TypePathValue::Pack(id), TypePathComponent::PackField(PackField::Tail)) => {
                let id = self.follow_pack(id);
                match self.get_pack(id) {
                    TypePackKind::List {
                        tail: Some(tail), ..
                    } => Some(TypePathValue::Pack(*tail)),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn traverse_type_field(&self, id: TypeId, field: TypeField) -> Option<TypeId> {
        match (self.get(self.follow(id)), field) {
            (TypeKind::Table(table), TypeField::IndexLookup) => {
                table.indexer.as_ref().map(|indexer| indexer.key)
            }
            (TypeKind::Table(table), TypeField::IndexResult) => {
                table.indexer.as_ref().map(|indexer| indexer.value)
            }
            (TypeKind::Metatable { table, .. }, TypeField::Table) => Some(*table),
            (TypeKind::Metatable { metatable, .. }, TypeField::Metatable) => Some(*metatable),
            (TypeKind::Free(variable), TypeField::UpperBound) => variable.upper_bound,
            (TypeKind::Free(variable), TypeField::LowerBound) => variable.lower_bound,
            (TypeKind::Negation(ty), TypeField::Negated) => Some(*ty),
            _ => None,
        }
    }

    fn traverse_pack_field(&self, id: TypeId, field: PackField) -> Option<TypePackId> {
        let TypeKind::Function(function) = self.get(self.follow(id)) else {
            return None;
        };
        match field {
            PackField::Arguments => Some(function.arguments),
            PackField::Returns => Some(function.returns),
            PackField::Tail => None,
        }
    }

    fn property_type(&self, id: TypeId, name: &str, active: &mut Vec<TypeId>) -> Option<TypeId> {
        let id = self.follow(id);
        if active.contains(&id) {
            return None;
        }
        active.push(id);
        let result = match self.get(id) {
            TypeKind::Table(table) => table.properties.get(name).map(|property| property.ty),
            TypeKind::Metatable {
                table, metatable, ..
            } => self
                .property_type(*table, name, active)
                .or_else(|| self.metatable_index_property(*metatable, name, active)),
            _ => None,
        };
        active.pop();
        result
    }

    fn metatable_index_property(
        &self,
        metatable: TypeId,
        name: &str,
        active: &mut Vec<TypeId>,
    ) -> Option<TypeId> {
        let TypeKind::Table(table) = self.get(self.follow(metatable)) else {
            return None;
        };
        let index = table.properties.get("__index")?.ty;
        if matches!(self.get(self.follow(index)), TypeKind::Metatable { .. }) {
            return None;
        }
        self.property_type(index, name, active)
    }

    fn slice_pack(&mut self, id: TypePackId, start: usize) -> Option<TypePackId> {
        let normalized = self.normalize_pack(id);
        if start > normalized.types.len() {
            return None;
        }

        let tail = self.alloc_optional_pack_tail(normalized.tail);

        Some(self.alloc_pack(TypePackKind::List {
            types: normalized.types[start..].to_vec(),
            tail,
        }))
    }
}

/// Immutable path into a type or type pack.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TypePath {
    /// Path components.
    components: Vec<TypePathComponent>,
}

/// Root value for type-path traversal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TypePathRoot {
    /// Start from a type handle.
    Type(TypeId),
    /// Start from a type-pack handle.
    Pack(TypePackId),
}

#[cfg(any())]
impl From<TypeId> for TypePathRoot {
    fn from(value: TypeId) -> Self {
        Self::Type(value)
    }
}

#[cfg(any())]
impl From<TypePackId> for TypePathRoot {
    fn from(value: TypePackId) -> Self {
        Self::Pack(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TypePathValue {
    Type(TypeId),
    Pack(TypePackId),
}

impl From<TypePathRoot> for TypePathValue {
    fn from(value: TypePathRoot) -> Self {
        match value {
            TypePathRoot::Type(id) => Self::Type(id),
            TypePathRoot::Pack(id) => Self::Pack(id),
        }
    }
}

impl TypePath {
    /// Creates an empty path.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            components: Vec::new(),
        }
    }

    /// Creates a path from components.
    #[must_use]
    pub fn from_components(components: Vec<TypePathComponent>) -> Self {
        Self { components }
    }

    /// Returns true when the path has no components.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.components.is_empty()
    }

    /// Returns the path components.
    #[must_use]
    pub fn components(&self) -> &[TypePathComponent] {
        &self.components
    }

    /// Returns true when the path is currently comparing function arguments.
    #[must_use]
    pub fn ends_in_function_arguments(&self) -> bool {
        matches!(
            self.components.last(),
            Some(TypePathComponent::PackField(PackField::Arguments))
        )
    }

    /// Returns a new path with `other` appended.
    #[must_use]
    #[cfg(any())]
    pub fn append(&self, other: &Self) -> Self {
        let mut components = Vec::with_capacity(self.components.len() + other.components.len());
        components.extend_from_slice(&self.components);
        components.extend_from_slice(&other.components);
        Self { components }
    }

    /// Returns a new path with one component appended.
    #[must_use]
    pub fn push(&self, component: TypePathComponent) -> Self {
        let mut components = self.components.clone();
        components.push(component);
        Self { components }
    }

    /// Returns a new path with the final component removed.
    #[must_use]
    #[cfg(any())]
    pub fn pop(&self) -> Self {
        let mut components = self.components.clone();
        components.pop();
        Self { components }
    }

    /// Renders the machine-oriented upstream-style path string.
    #[must_use]
    pub fn render(&self) -> String {
        self.components
            .iter()
            .map(TypePathComponent::render)
            .collect::<String>()
    }

    /// Renders the human-oriented path prefix used in diagnostics.
    #[must_use]
    pub fn render_human(&self) -> String {
        if let [TypePathComponent::Property { name, access }, rest @ ..] =
            self.components.as_slice()
        {
            let action = match access {
                PropertyAccess::Read | PropertyAccess::ReadWrite => "accessing",
                PropertyAccess::Write => "writing to",
            };
            let mut rendered = format!("{action} `{name}`");
            if !rest.is_empty() {
                rendered.push(' ');
                rendered.push_str(&render_human_tail(rest));
            }
            return rendered;
        }

        if let [TypePathComponent::PackSlice { start }] = self.components.as_slice() {
            return format!("the portion of the type pack starting at index {start} to the end");
        }

        self.render()
    }
}

/// One component of a [`TypePath`].
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum TypePathComponent {
    /// Named property access.
    Property {
        /// Property name.
        name: String,
        /// Access direction.
        access: PropertyAccess,
    },
    /// Index into a union, intersection, or pack.
    Index {
        /// Zero-based index.
        index: usize,
    },
    /// Field on a type.
    TypeField(TypeField),
    /// Field on a type pack.
    PackField(PackField),
    /// Slice of a type pack from `start` to the tail.
    PackSlice {
        /// Zero-based start index.
        start: usize,
    },
}

impl TypePathComponent {
    /// Creates a read property component.
    #[must_use]
    pub fn read_property(name: impl Into<String>) -> Self {
        Self::Property {
            name: name.into(),
            access: PropertyAccess::Read,
        }
    }

    /// Creates a write property component.
    #[must_use]
    pub fn write_property(name: impl Into<String>) -> Self {
        Self::Property {
            name: name.into(),
            access: PropertyAccess::Write,
        }
    }

    /// Creates a read/write property component.
    #[must_use]
    pub fn property(name: impl Into<String>) -> Self {
        Self::Property {
            name: name.into(),
            access: PropertyAccess::ReadWrite,
        }
    }

    /// Renders this component in upstream-style path notation.
    fn render(&self) -> String {
        match self {
            Self::Property { name, access } => format!("[{} {name:?}]", access.as_str()),
            Self::Index { index } => format!("[{index}]"),
            Self::TypeField(field) => field.render().to_owned(),
            Self::PackField(field) => field.render().to_owned(),
            Self::PackSlice { start } => format!("[{start}:]"),
        }
    }
}

/// Property access direction.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PropertyAccess {
    /// Read-only access.
    Read,
    /// Write-only access.
    Write,
    /// Access direction not yet known.
    #[default]
    ReadWrite,
}

impl PropertyAccess {
    /// Upstream-style access label.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::ReadWrite => "read/write",
        }
    }
}

/// Type-field path component.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TypeField {
    /// Table indexer lookup/key type.
    IndexLookup,
    /// Table indexer value/result type.
    IndexResult,
    /// Table portion of a metatable-wrapped type.
    Table,
    /// Metatable portion.
    Metatable,
    /// Upper bound of a free variable.
    UpperBound,
    /// Lower bound of a free variable.
    LowerBound,
    /// Negated type operand.
    Negated,
    /// Variadic pack element.
    Variadic,
}

impl TypeField {
    /// Upstream-style render fragment.
    const fn render(self) -> &'static str {
        match self {
            Self::IndexLookup => ".indexer().key",
            Self::IndexResult => ".indexer().value",
            Self::Table => ".table()",
            Self::Metatable => ".metatable()",
            Self::UpperBound => ".upperBound()",
            Self::LowerBound => ".lowerBound()",
            Self::Negated => ".negated()",
            Self::Variadic => ".variadic()",
        }
    }
}

/// Type-pack field path component.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PackField {
    /// Function argument pack.
    Arguments,
    /// Function return pack.
    Returns,
    /// Tail pack after the fixed prefix.
    Tail,
}

impl PackField {
    /// Upstream-style render fragment.
    const fn render(self) -> &'static str {
        match self {
            Self::Arguments => ".arguments()",
            Self::Returns => ".returns()",
            Self::Tail => ".tail()",
        }
    }
}

/// Builder for immutable [`TypePath`] values.
#[derive(Clone, Debug, Default)]
#[cfg(any())]
pub struct TypePathBuilder {
    /// Components accumulated so far.
    components: Vec<TypePathComponent>,
}

#[cfg(any())]
impl TypePathBuilder {
    /// Creates an empty builder.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            components: Vec::new(),
        }
    }

    /// Appends a read/write property.
    #[must_use]
    pub fn property(mut self, name: impl Into<String>) -> Self {
        self.components.push(TypePathComponent::property(name));
        self
    }

    /// Appends a read property.
    #[must_use]
    pub fn read_property(mut self, name: impl Into<String>) -> Self {
        self.components.push(TypePathComponent::read_property(name));
        self
    }

    /// Appends a write property.
    #[must_use]
    pub fn write_property(mut self, name: impl Into<String>) -> Self {
        self.components
            .push(TypePathComponent::write_property(name));
        self
    }

    /// Appends an index.
    #[must_use]
    pub fn index(mut self, index: usize) -> Self {
        self.components.push(TypePathComponent::Index { index });
        self
    }

    /// Appends a type field.
    #[must_use]
    pub fn type_field(mut self, field: TypeField) -> Self {
        self.components.push(TypePathComponent::TypeField(field));
        self
    }

    /// Appends the metatable field.
    #[must_use]
    pub fn metatable(self) -> Self {
        self.type_field(TypeField::Metatable)
    }

    /// Appends the lower-bound field.
    #[must_use]
    pub fn lower_bound(self) -> Self {
        self.type_field(TypeField::LowerBound)
    }

    /// Appends the upper-bound field.
    #[must_use]
    pub fn upper_bound(self) -> Self {
        self.type_field(TypeField::UpperBound)
    }

    /// Appends the table-indexer key field.
    #[must_use]
    pub fn index_key(self) -> Self {
        self.type_field(TypeField::IndexLookup)
    }

    /// Appends the table-indexer value field.
    #[must_use]
    pub fn index_value(self) -> Self {
        self.type_field(TypeField::IndexResult)
    }

    /// Appends the negated-type field.
    #[must_use]
    pub fn negated(self) -> Self {
        self.type_field(TypeField::Negated)
    }

    /// Appends the variadic element field.
    #[must_use]
    pub fn variadic(self) -> Self {
        self.type_field(TypeField::Variadic)
    }

    /// Appends a pack field.
    #[must_use]
    pub fn pack_field(mut self, field: PackField) -> Self {
        self.components.push(TypePathComponent::PackField(field));
        self
    }

    /// Appends the function-arguments pack field.
    #[must_use]
    pub fn arguments(self) -> Self {
        self.pack_field(PackField::Arguments)
    }

    /// Appends the function-returns pack field.
    #[must_use]
    pub fn returns(self) -> Self {
        self.pack_field(PackField::Returns)
    }

    /// Appends the tail pack field.
    #[must_use]
    pub fn tail(self) -> Self {
        self.pack_field(PackField::Tail)
    }

    /// Appends a pack slice.
    #[must_use]
    pub fn pack_slice(mut self, start: usize) -> Self {
        self.components.push(TypePathComponent::PackSlice { start });
        self
    }

    /// Builds the immutable path.
    #[must_use]
    pub fn build(self) -> TypePath {
        TypePath::from_components(self.components)
    }
}

/// Renders a human path tail.
fn render_human_tail(components: &[TypePathComponent]) -> String {
    if components == [TypePathComponent::TypeField(TypeField::Metatable)] {
        "has the metatable portion as ".to_owned()
    } else {
        format!(
            "has path {} as ",
            TypePath::from_components(components.to_vec()).render()
        )
    }
}
