//! Derive macros for Ruau's scoped embedding conversion traits.
//!
//! Named-field structs derive table conversions. Tuple structs and enums are
//! rejected. Use these macros through the `derive` feature on `ruau` or
//! `ruau-vm`.

use proc_macro::TokenStream;
use proc_macro_crate::{FoundCrate, crate_name};
use proc_macro2::Span;
use quote::quote;
use syn::{
    Attribute, Data, DeriveInput, Error, Field, Fields, GenericParam, Generics, Ident, LitStr,
    Path, Type, parse_macro_input, parse_quote,
};

/// Derives `IntoLua` for a named-field struct.
///
/// Fields are written into a new Lua table using their Rust field names as
/// string keys. Use `#[ruau(rename = "lua_key")]` on a field to choose a
/// different Lua key. Use `#[ruau(crate = "::path::to::vm")]` on the struct
/// when the conversion traits are available through a path other than
/// the manifest-discovered `ruau-vm` or `ruau` dependency.
#[proc_macro_derive(IntoLua, attributes(ruau))]
pub fn derive_into_lua(input: TokenStream) -> TokenStream {
    derive(input, Direction::Into)
}

/// Derives `FromLua` for a named-field struct.
///
/// The input value must be a Lua table. Each field is read with a raw table
/// lookup and converted through `FromLua`; conversion failures are annotated
/// with the field path.
#[proc_macro_derive(FromLua, attributes(ruau))]
pub fn derive_from_lua(input: TokenStream) -> TokenStream {
    derive(input, Direction::From)
}

#[derive(Clone, Copy)]
enum Direction {
    Into,
    From,
}

fn derive(input: TokenStream, direction: Direction) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(&input, direction)
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

fn expand(input: &DeriveInput, direction: Direction) -> syn::Result<proc_macro2::TokenStream> {
    let container = ContainerAttrs::parse(&input.attrs)?;
    let crate_path = container.crate_path()?;
    let fields = named_fields(input)?;
    let field_specs = fields
        .iter()
        .map(FieldSpec::parse)
        .collect::<syn::Result<Vec<_>>>()?;
    match direction {
        Direction::Into => expand_into_lua(input, &crate_path, &field_specs),
        Direction::From => expand_from_lua(input, &crate_path, &field_specs),
    }
}

fn expand_into_lua(
    input: &DeriveInput,
    crate_path: &Path,
    fields: &[FieldSpec],
) -> syn::Result<proc_macro2::TokenStream> {
    let name = &input.ident;
    let mut generics = with_scope_lifetime(&input.generics);
    {
        let where_clause = generics.make_where_clause();
        for field in fields {
            let ty = &field.ty;
            where_clause
                .predicates
                .push(parse_quote!(#ty: #crate_path::IntoLua<'__ruau>));
        }
    }
    let (impl_generics, _, where_clause) = generics.split_for_impl();
    let (_, ty_generics, _) = input.generics.split_for_impl();
    let writes = fields.iter().map(|field| {
        let ident = &field.ident;
        let key = &field.key;
        let path = field.path_lit();
        quote! {
            table
                .set(scope, #key, self.#ident)
                .map_err(|error| error.with_path(#path))?;
        }
    });
    Ok(quote! {
        impl #impl_generics #crate_path::IntoLua<'__ruau> for #name #ty_generics #where_clause {
            fn into_lua(
                self,
                scope: &#crate_path::Scope<'__ruau>,
            ) -> ::core::result::Result<#crate_path::ScopedValue<'__ruau>, #crate_path::RuntimeError> {
                let table = scope.create_table()?;
                #(#writes)*
                ::core::result::Result::Ok(#crate_path::ScopedValue::Table(table))
            }
        }
    })
}

fn expand_from_lua(
    input: &DeriveInput,
    crate_path: &Path,
    fields: &[FieldSpec],
) -> syn::Result<proc_macro2::TokenStream> {
    let name = &input.ident;
    let mut generics = with_scope_lifetime(&input.generics);
    {
        let where_clause = generics.make_where_clause();
        for field in fields {
            let ty = &field.ty;
            where_clause
                .predicates
                .push(parse_quote!(#ty: #crate_path::FromLua<'__ruau>));
        }
    }
    let (impl_generics, _, where_clause) = generics.split_for_impl();
    let (_, ty_generics, _) = input.generics.split_for_impl();
    let reads = fields.iter().map(|field| {
        let ident = &field.ident;
        let key = &field.key;
        let path = field.path_lit();
        quote! {
            #ident: table
                .get(scope, #key)
                .map_err(|error| error.with_path(#path))?
        }
    });
    Ok(quote! {
        impl #impl_generics #crate_path::FromLua<'__ruau> for #name #ty_generics #where_clause {
            fn from_lua(
                value: #crate_path::ScopedValue<'__ruau>,
                scope: &#crate_path::Scope<'__ruau>,
            ) -> ::core::result::Result<Self, #crate_path::RuntimeError> {
                let table = <#crate_path::Table<'__ruau> as #crate_path::FromLua<'__ruau>>::from_lua(value, scope)?;
                ::core::result::Result::Ok(Self {
                    #(#reads,)*
                })
            }
        }
    })
}

fn with_scope_lifetime(generics: &Generics) -> Generics {
    let mut generics = generics.clone();
    let lifetime = syn::Lifetime::new("'__ruau", Span::call_site());
    generics
        .params
        .insert(0, GenericParam::Lifetime(syn::LifetimeParam::new(lifetime)));
    generics
}

fn named_fields(
    input: &DeriveInput,
) -> syn::Result<&syn::punctuated::Punctuated<Field, syn::token::Comma>> {
    match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(fields) => Ok(&fields.named),
            Fields::Unnamed(_) | Fields::Unit => Err(Error::new_spanned(
                input,
                "Ruau conversion derives support only structs with named fields",
            )),
        },
        Data::Enum(_) | Data::Union(_) => Err(Error::new_spanned(
            input,
            "Ruau conversion derives support only structs with named fields",
        )),
    }
}

struct ContainerAttrs {
    crate_path: Option<Path>,
}

impl ContainerAttrs {
    fn parse(attrs: &[Attribute]) -> syn::Result<Self> {
        let mut crate_path = None;
        for attr in ruau_attrs(attrs) {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("crate") {
                    let value = meta.value()?;
                    let lit: LitStr = value.parse()?;
                    crate_path = Some(lit.parse()?);
                    Ok(())
                } else {
                    Err(meta.error("unsupported ruau container attribute"))
                }
            })?;
        }
        Ok(Self { crate_path })
    }

    fn crate_path(&self) -> syn::Result<Path> {
        if let Some(path) = &self.crate_path {
            return Ok(path.clone());
        }

        if let Some(path) = dependency_path("ruau-vm")? {
            return Ok(parse_path(&path));
        }

        if let Some(path) = dependency_path("ruau")? {
            return Ok(parse_path(&format!("{path}::vm")));
        }

        Err(Error::new(
            Span::call_site(),
            "could not find `ruau-vm` or `ruau` in Cargo.toml; add one as a dependency or use #[ruau(crate = \"::path\")]",
        ))
    }
}

fn dependency_path(package: &str) -> syn::Result<Option<String>> {
    match crate_name(package) {
        Ok(FoundCrate::Name(name)) => Ok(Some(format!("::{}", name.replace('-', "_")))),
        Ok(FoundCrate::Itself) => Ok(Some(match package {
            "ruau-vm" => "::ruau_vm".to_owned(),
            "ruau" => "::ruau".to_owned(),
            _ => "crate".to_owned(),
        })),
        Err(error) => {
            let missing = error.to_string().contains("Could not find")
                || error.to_string().contains("not found");
            if missing {
                Ok(None)
            } else {
                Err(Error::new(
                    Span::call_site(),
                    format!("failed to inspect Cargo.toml for `{package}`: {error}"),
                ))
            }
        }
    }
}

fn parse_path(path: &str) -> Path {
    syn::parse_str(path).expect("generated crate path parses")
}

struct FieldSpec {
    ident: Ident,
    ty: Type,
    key: LitStr,
}

impl FieldSpec {
    fn parse(field: &Field) -> syn::Result<Self> {
        let ident = field
            .ident
            .clone()
            .ok_or_else(|| Error::new_spanned(field, "expected a named field"))?;
        let mut key = ident.to_string();
        for attr in ruau_attrs(&field.attrs) {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("rename") {
                    let value = meta.value()?;
                    let lit: LitStr = value.parse()?;
                    key = lit.value();
                    Ok(())
                } else {
                    Err(meta.error("unsupported ruau field attribute"))
                }
            })?;
        }
        Ok(Self {
            ident,
            ty: field.ty.clone(),
            key: LitStr::new(
                &key,
                field
                    .ident
                    .as_ref()
                    .map_or_else(Span::call_site, Ident::span),
            ),
        })
    }

    fn path_lit(&self) -> LitStr {
        LitStr::new(&format!(".{}", self.key.value()), self.key.span())
    }
}

fn ruau_attrs(attrs: &[Attribute]) -> impl Iterator<Item = &Attribute> {
    attrs.iter().filter(|attr| attr.path().is_ident("ruau"))
}
