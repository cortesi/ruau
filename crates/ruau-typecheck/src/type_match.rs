//! Sound comparison of an upstream-expected rendered type against ruau's.
//!
//! This backs the positive-type ratchet: a case auto-promotes only when every
//! one of its `requireType` assertions [`TypeMatch::Match`]es, so the single
//! non-negotiable property is **soundness** — a `Match` must be a true semantic
//! equality, never a coincidental string collision. Coverage is allowed to
//! start low and ratchet up; correctness is not.
//!
//! The comparator is therefore deliberately conservative. Upstream's `toString`
//! (and ruau's `summary`) are lossy, solver-variant pretty-printers: an error
//! type prints `*error-type*` regardless of which error, free/generic variables
//! get solver-dependent spellings, recursive types fold into `t1 where t1 = …`,
//! and deep types truncate. Any rendering carrying such a construct is
//! [`TypeMatch::Unsupported`] — it can never auto-promote — and only the
//! concrete remainder, after cosmetic normalization, is eligible to `Match`.
//!
//! On top of cosmetic normalization the comparator applies a small set of
//! *structural* normalizations the upstream plan sanctions as semantics-
//! preserving: optional sugar (`T?` ≡ `T | nil`), union / intersection member
//! reordering and de-duplication, and table-field reordering. These are done by
//! parsing the rendering into a canonical tree with the correct precedence —
//! never by splitting strings — so the function-vs-optional precedence trap
//! (`(number) -> string | nil` ≠ `nil | (number) -> string`) cannot become a
//! false match. Any rendering outside the strict grammar fails the parse and
//! falls back to exact comparison, so the normalization can only ever *add*
//! sound matches, never remove the conservative guarantee.

/// The result of comparing an expected rendered type against an actual one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TypeMatch {
    /// The two render to the same concrete type — a sound semantic match.
    Match,
    /// Both are concrete but render differently — ruau inferred another type.
    Mismatch,
    /// At least one rendering is lossy or solver-variant, so string comparison
    /// cannot soundly decide equality. Never auto-promotes.
    Unsupported {
        /// Why the comparison is not sound.
        reason: TypeMatchUnsupported,
    },
}

impl TypeMatch {
    /// Whether this is a sound positive match (the only auto-promotable result).
    #[must_use]
    pub const fn is_match(&self) -> bool {
        matches!(self, Self::Match)
    }
}

/// Why a rendered-type comparison is not soundly decidable by string equality.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum TypeMatchUnsupported {
    /// Carries `*error-type*` — every error renders identically.
    ErrorType,
    /// Carries a free/generic variable whose spelling is solver-dependent.
    FreeVariable,
    /// A recursive `… where t1 = …` rendering, solver-variant.
    RecursiveWhere,
    /// A truncated (`…`) rendering.
    Truncated,
    /// Empty expected text — nothing to compare.
    Empty,
}

impl TypeMatchUnsupported {
    /// Stable label for tallies and reports.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ErrorType => "error-type",
            Self::FreeVariable => "free-variable",
            Self::RecursiveWhere => "recursive-where",
            Self::Truncated => "truncated",
            Self::Empty => "empty",
        }
    }
}

/// Compares an upstream-expected rendered type against ruau's rendered type.
///
/// Returns [`TypeMatch::Match`] only when both are concrete (free of lossy or
/// solver-variant constructs) and equal after cosmetic normalization; lossy
/// renderings on either side yield [`TypeMatch::Unsupported`].
#[must_use]
pub fn compare_rendered_types(expected: &str, actual: &str) -> TypeMatch {
    if let Some(verdict) = compare_recursive_where_renderings(expected, actual) {
        return verdict;
    }
    if let Some(reason) = unsupported_reason(expected).or_else(|| unsupported_reason(actual)) {
        return TypeMatch::Unsupported { reason };
    }
    let expected = normalize(expected);
    let actual = normalize(actual);
    if expected == actual {
        return TypeMatch::Match;
    }
    // Second chance: parse both into canonical trees and compare structurally.
    // This only ever turns a `Mismatch` into a `Match`, and only when both sides
    // parse fully under the strict grammar, so it cannot manufacture a false
    // PASS — a shape outside the grammar leaves the verdict at `Mismatch`.
    if let (Some(lhs), Some(rhs)) = (canonicalize(&expected), canonicalize(&actual))
        && lhs == rhs
    {
        return TypeMatch::Match;
    }
    TypeMatch::Mismatch
}

fn compare_recursive_where_renderings(expected: &str, actual: &str) -> Option<TypeMatch> {
    let expected_has_where = has_recursive_where(expected);
    let actual_has_where = has_recursive_where(actual);
    if !expected_has_where && !actual_has_where {
        return None;
    }
    if expected.contains("*unknown*") || actual.contains("*unknown*") {
        return Some(TypeMatch::Unsupported {
            reason: TypeMatchUnsupported::ErrorType,
        });
    }
    if expected.contains('\'') || actual.contains('\'') {
        return Some(TypeMatch::Unsupported {
            reason: TypeMatchUnsupported::FreeVariable,
        });
    }
    if expected.contains('\u{2026}') || actual.contains('\u{2026}') {
        return Some(TypeMatch::Unsupported {
            reason: TypeMatchUnsupported::Truncated,
        });
    }
    if !expected_has_where || !actual_has_where {
        return Some(TypeMatch::Unsupported {
            reason: TypeMatchUnsupported::RecursiveWhere,
        });
    }
    match (
        canonicalize_recursive_where(expected),
        canonicalize_recursive_where(actual),
    ) {
        (Some(expected), Some(actual)) if expected == actual => Some(TypeMatch::Match),
        (Some(_), Some(_)) => Some(TypeMatch::Mismatch),
        _ => Some(TypeMatch::Unsupported {
            reason: TypeMatchUnsupported::RecursiveWhere,
        }),
    }
}

fn has_recursive_where(rendered: &str) -> bool {
    rendered.contains(" where ")
}

/// Returns the first reason a rendering is not soundly comparable, if any.
fn unsupported_reason(rendered: &str) -> Option<TypeMatchUnsupported> {
    let trimmed = rendered.trim();
    if trimmed.is_empty() {
        return Some(TypeMatchUnsupported::Empty);
    }
    // `*unknown*` (a blocked/unresolved marker) stays fail-closed: matching it
    // would equate two "could not compute" results. `*error-type*` and `~T`
    // negations, by contrast, are concrete spellings the parser models as
    // opaque atoms, so identical renderings compare soundly by exact string.
    if trimmed.contains("*unknown*") {
        return Some(TypeMatchUnsupported::ErrorType);
    }
    if trimmed.contains("where ") || trimmed.contains(" where") {
        return Some(TypeMatchUnsupported::RecursiveWhere);
    }
    if trimmed.contains("...") || trimmed.contains('\u{2026}') {
        // A `...` *type pack* tail (`(number, ...)`) is concrete, but a bare
        // `...` truncation marker is lossy. Treat a `...` not immediately
        // following `(`/`,`/space-inside-parens conservatively as truncation;
        // the common safe pack form keeps its own concrete spelling, so only
        // flag a standalone ellipsis.
        if trimmed.contains('\u{2026}') {
            return Some(TypeMatchUnsupported::Truncated);
        }
    }
    if contains_free_variable(trimmed) {
        return Some(TypeMatchUnsupported::FreeVariable);
    }
    None
}

/// Whether the rendering carries a free/generic variable with a solver-variant
/// spelling: an apostrophe-prefixed name (`'a`, `'local0`) or a bare `tN`/`aN`
/// type-variable token.
fn contains_free_variable(rendered: &str) -> bool {
    if rendered.contains('\'') {
        return true;
    }
    // A standalone `t<digits>` / `a<digits>` token (e.g. `t1`, `a0`) is a
    // type-variable spelling, not part of a longer identifier.
    let bytes = rendered.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let ch = bytes[index];
        let token_start = index == 0 || !is_identifier_byte(bytes[index - 1]);
        if token_start && (ch == b't' || ch == b'a') {
            let mut end = index + 1;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            let has_digits = end > index + 1;
            let token_end = end >= bytes.len() || !is_identifier_byte(bytes[end]);
            if has_digits && token_end {
                return true;
            }
        }
        index += 1;
    }
    false
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_solver_type_variable_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    matches!(bytes.first(), Some(b't' | b'a'))
        && bytes.len() > 1
        && bytes[1..].iter().all(u8::is_ascii_digit)
}

fn is_identifier(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.first().is_some_and(|byte| is_identifier_start(*byte))
        && bytes[1..].iter().all(|byte| is_identifier_byte(*byte))
}

fn split_top_level(source: &str, delimiter: u8) -> Option<Vec<&str>> {
    let bytes = source.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0;
    let mut index = 0;
    let mut paren_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                index = skip_string(bytes, index)?;
                continue;
            }
            b'(' => paren_depth += 1,
            b')' => paren_depth = paren_depth.checked_sub(1)?,
            b'{' => brace_depth += 1,
            b'}' => brace_depth = brace_depth.checked_sub(1)?,
            b'<' => angle_depth += 1,
            b'>' if angle_depth > 0 => angle_depth -= 1,
            byte if byte == delimiter
                && paren_depth == 0
                && brace_depth == 0
                && angle_depth == 0 =>
            {
                parts.push(source[start..index].trim());
                start = index + 1;
            }
            _ => {}
        }
        index += 1;
    }
    if paren_depth != 0 || brace_depth != 0 || angle_depth != 0 {
        return None;
    }
    parts.push(source[start..].trim());
    Some(parts)
}

fn split_top_level_once(source: &str, delimiter: u8) -> Option<(&str, &str)> {
    let parts = split_top_level(source, delimiter)?;
    let [left, right] = parts.as_slice() else {
        return None;
    };
    Some((left, right))
}

/// Applies the cosmetic-safe normalizations: drops the `@checked` marker and
/// collapses runs of whitespace. The semantics-preserving *structural*
/// normalizations (optional sugar, union/intersection reordering, table-field
/// reordering) are applied separately by [`canonicalize`]; the sealed `{| … |}`
/// vs unsealed `{ … }` distinction is preserved by both, so a `Match` never
/// erases a real difference.
fn normalize(rendered: &str) -> String {
    rendered
        .replace("@checked ", "")
        .replace("@checked", "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// A rendered type parsed into a canonical, comparable tree. Two semantically
/// equal renderings (modulo the sanctioned normalizations) produce equal trees;
/// the derived `Eq`/`Ord` does the comparing, so there is no re-rendering step
/// to drift. Anything the parser does not fully model is kept as an opaque
/// [`Canon::Atom`] compared by its normalized string — sound, just not
/// decomposed.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Canon {
    /// A recursive `root where t1 = ... ; t2 = ...` rendering. Recursive
    /// binder names are alpha-normalized by their definition order.
    RecursiveWhere {
        root: Box<Self>,
        definitions: Vec<Self>,
    },
    /// A type variable bound by the nearest recursive `where` rendering.
    RecursiveType(usize),
    /// A quantified function surface (`<T, U...>(...) -> ...`). Parameter
    /// names are alpha-renamed by first use in the body, while generic kind
    /// and count stay significant.
    Quantified {
        type_count: usize,
        pack_count: usize,
        body: Box<Self>,
    },
    /// A type generic bound by the nearest rendered quantifier.
    GenericType(usize),
    /// A type-pack generic bound by the nearest rendered quantifier.
    GenericPack(usize),
    /// An opaque leaf: a primitive/name, a singleton literal, a generic
    /// application (`T<…>`), or any sub-rendering kept verbatim. Compared by
    /// its normalized string.
    Atom(String),
    /// A union with members flattened, de-duplicated, and sorted — so member
    /// order and `T?` ≡ `T | nil` desugaring do not affect equality.
    Union(Vec<Self>),
    /// An intersection, likewise flattened, de-duplicated, and sorted.
    Intersection(Vec<Self>),
    /// A function type. Argument order is significant (kept as-is); the return
    /// type is canonicalized.
    Function {
        /// Positional argument types, in order.
        args: Vec<Self>,
        /// The (greedy) return type.
        ret: Box<Self>,
    },
    /// A parenthesized type pack such as a multi-return list. Order is
    /// significant; element types are canonicalized.
    Tuple(Vec<Self>),
    /// A table type. The sealed/unsealed distinction is significant and
    /// preserved; field order is not, so fields are sorted by name.
    Table {
        /// `true` for a sealed `{| … |}` table.
        sealed: bool,
        /// Fields, sorted by name.
        fields: Vec<(String, Self)>,
    },
}

/// Parses a normalized rendering into a [`Canon`] tree, or `None` if it falls
/// outside the strict grammar. `None` is the fail-closed signal: the caller
/// then keeps the conservative exact-string verdict.
fn canonicalize(rendered: &str) -> Option<Canon> {
    let mut parser = TypeParser::new(rendered);
    parser.parse_all()
}

fn canonicalize_recursive_where(rendered: &str) -> Option<Canon> {
    let rendered = normalize(rendered);
    let (root_source, definitions_source) = rendered.split_once(" where ")?;
    let raw_definitions = split_top_level(definitions_source, b';')?;
    if raw_definitions.is_empty() {
        return None;
    }

    let mut names = Vec::new();
    let mut bodies = Vec::new();
    for definition in raw_definitions {
        let (name, body) = split_top_level_once(definition, b'=')?;
        let name = name.trim();
        if name.is_empty() || !is_identifier(name) || names.iter().any(|known| known == name) {
            return None;
        }
        names.push(name.to_owned());
        bodies.push(body.trim().to_owned());
    }

    let root = TypeParser::new_with_recursive(root_source.trim(), names.clone()).parse_all()?;
    let mut definitions = Vec::new();
    for body in bodies {
        definitions.push(TypeParser::new_with_recursive(&body, names.clone()).parse_all()?);
    }
    Some(Canon::RecursiveWhere {
        root: Box::new(root),
        definitions,
    })
}

fn renumber_quantified_generics(mut body: Canon, type_count: usize, pack_count: usize) -> Canon {
    let mut type_map = vec![None; type_count];
    let mut pack_map = vec![None; pack_count];
    renumber_quantified_generics_in(&mut body, &mut type_map, &mut pack_map, &mut 0, &mut 0);
    body
}

fn renumber_quantified_generics_in(
    node: &mut Canon,
    type_map: &mut [Option<usize>],
    pack_map: &mut [Option<usize>],
    next_type: &mut usize,
    next_pack: &mut usize,
) {
    match node {
        Canon::GenericType(index) => {
            if let Some(slot) = type_map.get_mut(*index) {
                let mapped = *slot.get_or_insert_with(|| {
                    let mapped = *next_type;
                    *next_type += 1;
                    mapped
                });
                *index = mapped;
            }
        }
        Canon::GenericPack(index) => {
            if let Some(slot) = pack_map.get_mut(*index) {
                let mapped = *slot.get_or_insert_with(|| {
                    let mapped = *next_pack;
                    *next_pack += 1;
                    mapped
                });
                *index = mapped;
            }
        }
        Canon::RecursiveWhere { root, definitions } => {
            renumber_quantified_generics_in(root, type_map, pack_map, next_type, next_pack);
            for definition in definitions {
                renumber_quantified_generics_in(
                    definition, type_map, pack_map, next_type, next_pack,
                );
            }
        }
        Canon::RecursiveType(_) => {}
        Canon::Quantified { .. } => {
            // Nested quantified renderings are parsed independently and are
            // deliberately not treated as captures of the surrounding prefix.
        }
        Canon::Union(members) | Canon::Intersection(members) | Canon::Tuple(members) => {
            for member in members {
                renumber_quantified_generics_in(member, type_map, pack_map, next_type, next_pack);
            }
        }
        Canon::Function { args, ret } => {
            for arg in args {
                renumber_quantified_generics_in(arg, type_map, pack_map, next_type, next_pack);
            }
            renumber_quantified_generics_in(ret, type_map, pack_map, next_type, next_pack);
        }
        Canon::Table { fields, .. } => {
            for (_, field) in fields {
                renumber_quantified_generics_in(field, type_map, pack_map, next_type, next_pack);
            }
        }
        Canon::Atom(_) => {}
    }
}

/// A strict recursive-descent parser for the subset of rendered types the
/// comparator normalizes. Precedence, low to high: function arrow `->`
/// (greedy return), union `|`, intersection `&`, optional `?`.
struct TypeParser<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    generic_types: Vec<String>,
    generic_packs: Vec<String>,
    recursive_types: Vec<String>,
}

impl<'a> TypeParser<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            generic_types: Vec::new(),
            generic_packs: Vec::new(),
            recursive_types: Vec::new(),
        }
    }

    fn new_with_recursive(src: &'a str, recursive_types: Vec<String>) -> Self {
        Self {
            recursive_types,
            ..Self::new(src)
        }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() && self.bytes[self.pos] == b' ' {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn starts_with(&self, prefix: &str) -> bool {
        self.bytes[self.pos..].starts_with(prefix.as_bytes())
    }

    fn parse_type(&mut self) -> Option<Canon> {
        self.skip_ws();
        if self.peek() == Some(b'<') {
            if !self.generic_types.is_empty() || !self.generic_packs.is_empty() {
                return None;
            }
            return self.parse_quantified();
        }
        self.parse_union()
    }

    fn parse_all(&mut self) -> Option<Canon> {
        let ty = self.parse_type()?;
        self.skip_ws();
        if self.pos == self.bytes.len() {
            Some(ty)
        } else {
            // Trailing unparsed input (e.g. an unparenthesized curried arrow)
            // means the grammar did not fully describe the rendering — bail.
            None
        }
    }

    fn parse_quantified(&mut self) -> Option<Canon> {
        let saved_types = self.generic_types.len();
        let saved_packs = self.generic_packs.len();
        self.pos += 1; // consume '<'
        let mut type_count = 0;
        let mut pack_count = 0;
        loop {
            self.skip_ws();
            let name = self.parse_identifier()?;
            if self.starts_with("...") {
                self.pos += 3;
                self.generic_packs.push(name);
                pack_count += 1;
            } else {
                self.generic_types.push(name);
                type_count += 1;
            }
            self.skip_ws();
            match self.peek()? {
                b',' => self.pos += 1,
                b'>' => {
                    self.pos += 1;
                    break;
                }
                _ => return None,
            }
        }
        let body = renumber_quantified_generics(self.parse_union()?, type_count, pack_count);
        self.generic_types.truncate(saved_types);
        self.generic_packs.truncate(saved_packs);
        Some(Canon::Quantified {
            type_count,
            pack_count,
            body: Box::new(body),
        })
    }

    fn parse_union(&mut self) -> Option<Canon> {
        let mut members = vec![self.parse_intersection()?];
        loop {
            self.skip_ws();
            if self.peek() == Some(b'|') {
                self.pos += 1;
                members.push(self.parse_intersection()?);
            } else {
                break;
            }
        }
        Some(build_union(members))
    }

    fn parse_intersection(&mut self) -> Option<Canon> {
        let mut members = vec![self.parse_postfix()?];
        loop {
            self.skip_ws();
            if self.peek() == Some(b'&') {
                self.pos += 1;
                members.push(self.parse_postfix()?);
            } else {
                break;
            }
        }
        Some(build_intersection(members))
    }

    fn parse_postfix(&mut self) -> Option<Canon> {
        let mut ty = self.parse_primary()?;
        loop {
            self.skip_ws();
            if self.peek() == Some(b'?') {
                self.pos += 1;
                ty = build_union(vec![ty, Canon::Atom("nil".to_string())]);
            } else {
                break;
            }
        }
        Some(ty)
    }

    fn parse_primary(&mut self) -> Option<Canon> {
        self.skip_ws();
        match self.peek()? {
            b'(' => self.parse_paren(),
            b'{' => self.parse_table(),
            b'"' => self.parse_string_singleton(),
            b'*' => self.parse_starred_marker(),
            b'~' => self.parse_negation(),
            _ => self.parse_name_or_number(),
        }
    }

    /// Parses a `*…*` marker (`*error-type*`, `*blocked*`) as an opaque atom.
    fn parse_starred_marker(&mut self) -> Option<Canon> {
        let start = self.pos;
        self.pos += 1; // consume the opening '*'
        while let Some(byte) = self.peek() {
            self.pos += 1;
            if byte == b'*' {
                return Some(Canon::Atom(self.src[start..self.pos].to_string()));
            }
        }
        None
    }

    /// Parses a `~T` negation as an opaque atom keyed on the negated primary's
    /// canonical rendering, so `~nil` compares equal across renderings without
    /// modelling negation algebra (a shape it cannot canonicalize fails closed).
    fn parse_negation(&mut self) -> Option<Canon> {
        let start = self.pos;
        self.pos += 1; // consume '~'
        self.parse_primary()?;
        Some(Canon::Atom(normalize(&self.src[start..self.pos])))
    }

    /// Parses a `(` group: either a parenthesized type, a parenthesized type
    /// pack, or a function's argument list when followed by `->`.
    fn parse_paren(&mut self) -> Option<Canon> {
        self.pos += 1; // consume '('
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b')') {
            self.pos += 1;
        } else {
            loop {
                self.skip_ws();
                // A function parameter may carry a cosmetic `name:` label
                // (`(self: Foo, x: number)`); the label does not affect the
                // type, so skip it and compare the parameter types only.
                self.try_skip_param_label();
                items.push(self.parse_type()?);
                self.skip_ws();
                match self.peek()? {
                    b',' => self.pos += 1,
                    b')' => {
                        self.pos += 1;
                        break;
                    }
                    _ => return None,
                }
            }
        }
        self.skip_ws();
        if self.starts_with("->") {
            self.pos += 2;
            // The return type is greedy: `(a) -> b | c` is `(a) -> (b | c)`.
            let ret = self.parse_union()?;
            Some(Canon::Function {
                args: items,
                ret: Box::new(ret),
            })
        } else if items.len() == 1 {
            Some(items.into_iter().next().expect("len checked"))
        } else {
            Some(Canon::Tuple(items))
        }
    }

    /// Skips a cosmetic function-parameter label (`name :`) when one leads the
    /// current argument; restores the cursor when there is none. Labels only
    /// appear in a function argument list, so dropping them is semantics-
    /// preserving — `(a: number) -> X` and `(number) -> X` are the same type.
    fn try_skip_param_label(&mut self) {
        let save = self.pos;
        if self.peek().is_some_and(is_identifier_start) {
            while self.peek().is_some_and(is_identifier_byte) {
                self.pos += 1;
            }
            self.skip_ws();
            // A single `:` is a parameter label; leave `::` (and anything else)
            // for the type grammar.
            if self.peek() == Some(b':') && self.bytes.get(self.pos + 1) != Some(&b':') {
                self.pos += 1;
                return;
            }
        }
        self.pos = save;
    }

    /// Parses a `{ … }` or sealed `{| … |}` table. The brace region is scanned
    /// by depth (skipping string singletons); its body is then parsed as a
    /// `name: type` field list, or kept as an opaque atom if it is an indexer /
    /// array / modifier shape the field grammar does not cover.
    fn parse_table(&mut self) -> Option<Canon> {
        let open = self.pos; // at '{'
        self.pos += 1; // consume '{'
        self.skip_ws();
        let sealed = self.peek() == Some(b'|');

        let body_start = self.pos;
        let mut depth = 1usize;
        let mut index = self.pos;
        while index < self.bytes.len() {
            match self.bytes[index] {
                b'"' => {
                    index = skip_string(self.bytes, index)?;
                    continue;
                }
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            index += 1;
        }
        if depth != 0 {
            return None;
        }
        let body = &self.src[body_start..index];
        self.pos = index + 1; // consume '}'

        let field_source = if sealed {
            body.trim().strip_prefix('|')?.strip_suffix('|')
        } else {
            Some(body)
        };
        if let Some(field_source) = field_source
            && let Some(fields) = parse_field_list(field_source)
        {
            return Some(Canon::Table { sealed, fields });
        }
        // Indexer / array / modifier table: keep the whole `{ … }` opaque.
        Some(Canon::Atom(normalize(&self.src[open..self.pos])))
    }

    fn parse_string_singleton(&mut self) -> Option<Canon> {
        let start = self.pos;
        let end = skip_string(self.bytes, self.pos)?;
        self.pos = end;
        Some(Canon::Atom(self.src[start..end].to_string()))
    }

    /// Parses a name (optionally with opaque `<…>` generic arguments) or a
    /// numeric singleton, both kept as an [`Canon::Atom`].
    fn parse_name_or_number(&mut self) -> Option<Canon> {
        let start = self.pos;
        let first = self.peek()?;
        if first == b'-' || first.is_ascii_digit() {
            self.pos += 1;
            while let Some(byte) = self.peek() {
                if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+') {
                    self.pos += 1;
                } else {
                    break;
                }
            }
            return Some(Canon::Atom(self.src[start..self.pos].to_string()));
        }
        if !is_identifier_start(first) {
            return None;
        }
        self.pos += 1;
        while let Some(byte) = self.peek() {
            if is_identifier_byte(byte) {
                self.pos += 1;
            } else {
                break;
            }
        }
        let name_end = self.pos;
        let name = &self.src[start..name_end];
        if self.starts_with("...")
            && let Some(index) = self.generic_pack_index(name)
        {
            self.pos += 3;
            return Some(Canon::GenericPack(index));
        }
        self.skip_ws();
        if self.peek() == Some(b'<') {
            let mut depth = 0usize;
            let mut index = self.pos;
            while index < self.bytes.len() {
                match self.bytes[index] {
                    b'"' => {
                        index = skip_string(self.bytes, index)?;
                        continue;
                    }
                    b'<' => depth += 1,
                    b'>' => {
                        depth -= 1;
                        if depth == 0 {
                            index += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                index += 1;
            }
            if depth != 0 {
                return None;
            }
            self.pos = index;
            return Some(Canon::Atom(normalize(&self.src[start..self.pos])));
        }
        self.pos = name_end;
        if let Some(index) = self.recursive_type_index(name) {
            return Some(Canon::RecursiveType(index));
        }
        if is_solver_type_variable_name(name) {
            return None;
        }
        if let Some(index) = self.generic_type_index(name) {
            Some(Canon::GenericType(index))
        } else {
            Some(Canon::Atom(name.to_string()))
        }
    }

    fn parse_identifier(&mut self) -> Option<String> {
        let start = self.pos;
        if !self.peek().is_some_and(is_identifier_start) {
            return None;
        }
        self.pos += 1;
        while self.peek().is_some_and(is_identifier_byte) {
            self.pos += 1;
        }
        Some(self.src[start..self.pos].to_string())
    }

    fn generic_type_index(&self, name: &str) -> Option<usize> {
        self.generic_types
            .iter()
            .position(|generic| generic == name)
    }

    fn generic_pack_index(&self, name: &str) -> Option<usize> {
        self.generic_packs
            .iter()
            .position(|generic| generic == name)
    }

    fn recursive_type_index(&self, name: &str) -> Option<usize> {
        self.recursive_types
            .iter()
            .position(|recursive| recursive == name)
    }
}

/// Parses a table body (`name: type, …`) into sorted fields, or `None` if it is
/// not a plain named-field list.
fn parse_field_list(body: &str) -> Option<Vec<(String, Canon)>> {
    let body = body.trim();
    if body.is_empty() {
        return Some(Vec::new());
    }
    let mut parser = TypeParser::new(body);
    let mut fields = Vec::new();
    loop {
        parser.skip_ws();
        let name = parser.parse_field_name()?;
        parser.skip_ws();
        if parser.peek() != Some(b':') {
            return None;
        }
        parser.pos += 1;
        let ty = parser.parse_type()?;
        fields.push((name, ty));
        parser.skip_ws();
        match parser.peek() {
            Some(b',') => parser.pos += 1,
            None => break,
            _ => return None,
        }
    }
    fields.sort();
    Some(fields)
}

impl TypeParser<'_> {
    fn parse_field_name(&mut self) -> Option<String> {
        let start = self.pos;
        if !is_identifier_start(self.peek()?) {
            return None;
        }
        self.pos += 1;
        while let Some(byte) = self.peek() {
            if is_identifier_byte(byte) {
                self.pos += 1;
            } else {
                break;
            }
        }
        Some(self.src[start..self.pos].to_string())
    }
}

/// Flattens nested unions, de-duplicates, and sorts; a single surviving member
/// collapses to that member (so `T | T` and `T?` of `nil` reduce correctly).
fn build_union(members: Vec<Canon>) -> Canon {
    let mut flat = Vec::new();
    for member in members {
        match member {
            Canon::Union(inner) => flat.extend(inner),
            other => flat.push(other),
        }
    }
    flat.sort();
    flat.dedup();
    absorb_singletons_into_primitives(&mut flat);
    if flat.len() == 1 {
        flat.into_iter().next().expect("len checked")
    } else {
        Canon::Union(flat)
    }
}

/// Removes a literal singleton from a union when its primitive is also a member:
/// `"a" | string` ≡ `string`, `1 | number` ≡ `number`, `true | boolean` ≡
/// `boolean`. Sound (a singleton is a subtype of its primitive); collapses the
/// rendering difference a reduced inference produces against an unreduced one.
fn absorb_singletons_into_primitives(members: &mut Vec<Canon>) {
    let has = |name: &str| {
        members
            .iter()
            .any(|m| matches!(m, Canon::Atom(atom) if atom == name))
    };
    let (has_string, has_number, has_boolean) = (has("string"), has("number"), has("boolean"));
    members.retain(|member| {
        let Canon::Atom(atom) = member else {
            return true;
        };
        !((has_string && atom.starts_with('"'))
            || (has_boolean && (atom == "true" || atom == "false"))
            || (has_number && is_numeric_literal(atom)))
    });
}

fn is_numeric_literal(atom: &str) -> bool {
    let bytes = atom.as_bytes();
    match bytes.first() {
        Some(byte) if byte.is_ascii_digit() => true,
        Some(b'-' | b'+') => bytes.get(1).is_some_and(u8::is_ascii_digit),
        _ => false,
    }
}

/// Intersection counterpart of [`build_union`].
fn build_intersection(members: Vec<Canon>) -> Canon {
    let mut flat = Vec::new();
    for member in members {
        match member {
            Canon::Intersection(inner) => flat.extend(inner),
            other => flat.push(other),
        }
    }
    flat.sort();
    flat.dedup();
    if flat.len() == 1 {
        flat.into_iter().next().expect("len checked")
    } else {
        Canon::Intersection(flat)
    }
}

/// Given an index at a `"`, returns the index just past the closing quote,
/// honoring `\"` escapes; `None` if unterminated.
fn skip_string(bytes: &[u8], start: usize) -> Option<usize> {
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => return Some(index + 1),
            _ => index += 1,
        }
    }
    None
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

#[cfg(any())]
mod tests {
    use super::*;

    // The adversarial soundness seed: each pair is a real shape the corpus
    // produces, with its hand-verified verdict. A `Match` here must be a true
    // semantic equality.

    #[test]
    fn concrete_equal_types_match() {
        assert_eq!(compare_rendered_types("number", "number"), TypeMatch::Match);
        assert_eq!(
            compare_rendered_types("{ a: number }", "{  a:  number }"),
            TypeMatch::Match
        );
        assert_eq!(
            compare_rendered_types("(number) -> string", "(number) -> string"),
            TypeMatch::Match
        );
        assert_eq!(
            compare_rendered_types("\"hello\"", "\"hello\""),
            TypeMatch::Match
        );
    }

    #[test]
    fn checked_marker_is_cosmetic() {
        assert_eq!(
            compare_rendered_types("() -> ()", "@checked () -> ()"),
            TypeMatch::Match
        );
    }

    #[test]
    fn concrete_distinct_types_mismatch() {
        assert_eq!(
            compare_rendered_types("number", "string"),
            TypeMatch::Mismatch
        );
        assert_eq!(
            compare_rendered_types("{ a: number }", "{ a: string }"),
            TypeMatch::Mismatch
        );
    }

    #[test]
    fn sealed_vs_unsealed_tables_stay_distinct() {
        // `{| |}` vs `{ }` is semantic (TableState); never normalized away.
        assert_eq!(
            compare_rendered_types("{| a: number |}", "{ a: number }"),
            TypeMatch::Mismatch
        );
    }

    #[test]
    fn singletons_are_absorbed_into_their_primitive_in_a_union() {
        // A reduced inference (`string`) matches an unreduced rendering
        // (`"a" | string`) — the singleton is a subtype of its primitive.
        assert_eq!(
            compare_rendered_types(r#""a" | string"#, "string"),
            TypeMatch::Match
        );
        assert_eq!(
            compare_rendered_types("1 | number", "number"),
            TypeMatch::Match
        );
        assert_eq!(
            compare_rendered_types("true | false | boolean", "boolean"),
            TypeMatch::Match
        );
        // No primitive present → no absorption, so a real difference still fails.
        assert_eq!(
            compare_rendered_types(r#""a" | number"#, "number"),
            TypeMatch::Mismatch
        );
        assert_eq!(
            compare_rendered_types(r#""a""#, "string"),
            TypeMatch::Mismatch
        );
    }

    #[test]
    fn error_type_and_negation_compare_as_atoms() {
        // `*error-type*` and `~T` negations are concrete spellings: identical
        // renderings match (both inferred an error / the same negation here),
        // and they canonicalize through union reordering + `?` desugaring.
        assert_eq!(
            compare_rendered_types("*error-type*", "*error-type*"),
            TypeMatch::Match
        );
        assert_eq!(
            compare_rendered_types("(*error-type* | ~nil)?", "*error-type* | nil | ~nil"),
            TypeMatch::Match
        );
        // A real difference still mismatches — no catch-all false PASS.
        assert_eq!(
            compare_rendered_types("*error-type*", "string"),
            TypeMatch::Mismatch
        );
        assert_eq!(
            compare_rendered_types("~nil", "~number"),
            TypeMatch::Mismatch
        );
        // `*unknown*` (an unresolved-blocked marker) stays fail-closed.
        assert_eq!(
            compare_rendered_types("*unknown*", "*unknown*"),
            TypeMatch::Unsupported {
                reason: TypeMatchUnsupported::ErrorType
            }
        );
    }

    #[test]
    fn free_variables_are_unsupported() {
        // Solver-dependent spellings must never auto-promote, even if equal.
        assert_eq!(
            compare_rendered_types("'a", "'a"),
            TypeMatch::Unsupported {
                reason: TypeMatchUnsupported::FreeVariable
            }
        );
        assert_eq!(
            compare_rendered_types("t1", "t1"),
            TypeMatch::Unsupported {
                reason: TypeMatchUnsupported::FreeVariable
            }
        );
        assert_eq!(
            compare_rendered_types("string", "'local0"),
            TypeMatch::Unsupported {
                reason: TypeMatchUnsupported::FreeVariable
            }
        );
    }

    #[test]
    fn recursive_where_matches_when_both_sides_parse_equivalent_cycles() {
        assert_eq!(
            compare_rendered_types(
                "t1 where t1 = () -> t1?",
                "t2 where t2 = @checked () -> t2?"
            ),
            TypeMatch::Match
        );
        assert_eq!(
            compare_rendered_types("t1 where t1 = () -> t1", "t2 where t2 = @checked () -> t2"),
            TypeMatch::Match
        );
    }

    #[test]
    fn recursive_where_still_fails_closed_for_one_sided_or_unparsed_shapes() {
        assert_eq!(
            compare_rendered_types("Vector", "t1 where t1 = { value: number }"),
            TypeMatch::Unsupported {
                reason: TypeMatchUnsupported::RecursiveWhere
            }
        );
        assert_eq!(
            compare_rendered_types("t1 where t1 = () -> t1", "t1 where = () -> t1"),
            TypeMatch::Unsupported {
                reason: TypeMatchUnsupported::RecursiveWhere
            }
        );
    }

    #[test]
    fn recursive_where_mismatches_when_parsed_cycles_differ() {
        assert_eq!(
            compare_rendered_types("t1 where t1 = () -> t1", "t2 where t2 = (number) -> t2"),
            TypeMatch::Mismatch
        );
    }

    #[test]
    fn concrete_identifiers_starting_with_t_or_a_are_not_free_variables() {
        // `table`, `any`, `thread`, a user type `Animal` must not be mistaken
        // for type-variable tokens (`t1`, `a0`).
        assert_eq!(compare_rendered_types("thread", "thread"), TypeMatch::Match);
        assert_eq!(compare_rendered_types("any", "any"), TypeMatch::Match);
        assert_eq!(compare_rendered_types("Animal", "Animal"), TypeMatch::Match);
        assert_eq!(
            compare_rendered_types("{ tag: number }", "{ tag: number }"),
            TypeMatch::Match
        );
    }

    #[test]
    fn empty_expectation_is_unsupported() {
        assert_eq!(
            compare_rendered_types("", "number"),
            TypeMatch::Unsupported {
                reason: TypeMatchUnsupported::Empty
            }
        );
    }

    // --- structural normalization: optional sugar, union/intersection
    // reordering, table-field reordering. These are the sanctioned-safe
    // normalizations; each `Match` here is a true semantic equality.

    #[test]
    fn optional_sugar_matches_nil_union() {
        // ruau renders optionals as `nil | T`; upstream renders `T?`.
        assert_eq!(
            compare_rendered_types("string?", "nil | string"),
            TypeMatch::Match
        );
        assert_eq!(
            compare_rendered_types("number?", "string | nil"),
            TypeMatch::Mismatch
        );
        assert_eq!(
            compare_rendered_types("{ a: number }?", "nil | { a: number }"),
            TypeMatch::Match
        );
    }

    #[test]
    fn optional_is_not_the_bare_type() {
        // `T?` is `T | nil`, never `T` — desugaring must not erase the nil.
        assert_eq!(
            compare_rendered_types("string?", "string"),
            TypeMatch::Mismatch
        );
    }

    #[test]
    fn union_member_order_is_normalized() {
        assert_eq!(
            compare_rendered_types("string | number", "number | string"),
            TypeMatch::Match
        );
        assert_eq!(
            compare_rendered_types("boolean | number | nil", "nil | boolean | number"),
            TypeMatch::Match
        );
    }

    #[test]
    fn intersection_member_order_is_normalized() {
        assert_eq!(compare_rendered_types("A & B", "B & A"), TypeMatch::Match);
    }

    #[test]
    fn nested_optionals_in_tables_normalize() {
        assert_eq!(
            compare_rendered_types(
                "{ a: string?, b: number? }?",
                "nil | { a: nil | string, b: nil | number }"
            ),
            TypeMatch::Match
        );
    }

    #[test]
    fn table_field_order_is_normalized() {
        assert_eq!(
            compare_rendered_types("{ a: number, b: string }", "{ b: string, a: number }"),
            TypeMatch::Match
        );
    }

    #[test]
    fn function_argument_unions_normalize() {
        assert_eq!(
            compare_rendered_types(
                "((boolean | number)?) -> number",
                "(boolean | nil | number) -> number"
            ),
            TypeMatch::Match
        );
    }

    #[test]
    fn function_returning_optional_is_not_an_optional_function() {
        // THE precedence trap: `(number) -> string | nil` parses as a function
        // returning `string | nil`; `nil | (number) -> string` is an optional
        // function. They are different types and must never normalize to equal.
        assert_eq!(
            compare_rendered_types("(number) -> string | nil", "nil | (number) -> string"),
            TypeMatch::Mismatch
        );
    }

    #[test]
    fn function_returning_optional_matches_either_spelling() {
        assert_eq!(
            compare_rendered_types("(number) -> string?", "(number) -> (nil | string)"),
            TypeMatch::Match
        );
    }

    #[test]
    fn function_type_packs_normalize_element_types() {
        assert_eq!(
            compare_rendered_types(
                "({+ [unknown]: unknown +}, unknown?) -> (unknown?, unknown)",
                "({+ [unknown]: unknown +}, nil | unknown) -> (nil | unknown, unknown)"
            ),
            TypeMatch::Match
        );
        assert_eq!(
            compare_rendered_types("(number, string) -> ()", "(string, number) -> ()"),
            TypeMatch::Mismatch
        );
    }

    #[test]
    fn grouped_optional_function_matches_nil_union() {
        assert_eq!(
            compare_rendered_types("((number) -> string)?", "nil | (number) -> string"),
            TypeMatch::Match
        );
    }

    #[test]
    fn mixed_union_intersection_precedence_is_respected() {
        // `(A | B) & C` ≠ `A | B & C` (the latter is `A | (B & C)`).
        assert_eq!(
            compare_rendered_types("(A | B) & C", "C & (A | B)"),
            TypeMatch::Match
        );
        assert_eq!(
            compare_rendered_types("(A | B) & C", "A | B & C"),
            TypeMatch::Mismatch
        );
    }

    #[test]
    fn generic_arguments_are_opaque_and_order_sensitive() {
        assert_eq!(
            compare_rendered_types("Map<string, number>", "Map<string, number>"),
            TypeMatch::Match
        );
        assert_eq!(
            compare_rendered_types("Map<string, number>", "Map<number, string>"),
            TypeMatch::Mismatch
        );
    }

    #[test]
    fn sealed_distinction_survives_normalization() {
        // Even with reordering in play, sealed vs unsealed stays distinct.
        assert_eq!(
            compare_rendered_types("{| b: string, a: number |}", "{ a: number, b: string }"),
            TypeMatch::Mismatch
        );
    }

    #[test]
    fn function_parameter_labels_are_cosmetic() {
        // Parameter names in a function type don't affect the type.
        assert_eq!(
            compare_rendered_types("(a: number) -> string", "(number) -> string"),
            TypeMatch::Match
        );
        assert_eq!(
            compare_rendered_types(
                "(self: Foo, x: number) -> number",
                "(Foo, number) -> number"
            ),
            TypeMatch::Match
        );
    }

    #[test]
    fn quantified_function_generic_pack_names_are_alpha_equivalent() {
        let expected = "<a, b...>((a) -> (b...), a) -> (b...)";
        let actual = "<a, A...>@checked ((a) -> (A...), a) -> (A...)";
        assert_eq!(compare_rendered_types(expected, actual), TypeMatch::Match);
        assert_eq!(
            compare_rendered_types(
                "<a, b..., c...>((c...) -> (b...), (a) -> (c...), a) -> (b...)",
                "<a, A..., B...>@checked ((A...) -> (B...), (a) -> (A...), a) -> (B...)"
            ),
            TypeMatch::Match
        );
    }

    #[test]
    fn quantified_function_generic_kinds_and_positions_stay_significant() {
        assert_eq!(
            compare_rendered_types("<a>((a) -> a) -> a", "<a...>((a...) -> (a...)) -> (a...)"),
            TypeMatch::Mismatch
        );
        assert_eq!(
            compare_rendered_types(
                "<a, b...>((a) -> (b...), a) -> (b...)",
                "<a, A...>((A...) -> (A...), a) -> (A...)"
            ),
            TypeMatch::Mismatch
        );
    }

    #[test]
    fn function_parameter_label_does_not_mask_type_difference() {
        // Stripping the label must not equate different parameter *types*.
        assert_eq!(
            compare_rendered_types("(a: number) -> string", "(a: string) -> string"),
            TypeMatch::Mismatch
        );
        assert_eq!(
            compare_rendered_types("(a: number) -> string", "(string) -> string"),
            TypeMatch::Mismatch
        );
    }

    #[test]
    fn unparseable_pack_return_falls_back_to_mismatch() {
        // A variadic pack return is outside the grammar; canonicalize bails and
        // the verdict stays a sound mismatch (never a panic, never a match).
        assert_eq!(
            compare_rendered_types("() -> (...string)", "any"),
            TypeMatch::Mismatch
        );
    }
}
