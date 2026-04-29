//! Procedural macros for `ruau`.

#![allow(clippy::absolute_paths, clippy::missing_docs_in_private_items)]

use proc_macro::TokenStream;
use proc_macro_error2::proc_macro_error;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{DeriveInput, parse_macro_input};

use crate::chunk::Chunk;

mod chunk;
mod token;

#[proc_macro]
#[proc_macro_error]
/// Capture Rust variables inside an inline Luau chunk.
pub fn chunk(input: TokenStream) -> TokenStream {
    let chunk = Chunk::new(input);

    let source = chunk.source();
    let captures = chunk.captures();
    let caps_len = captures.len();
    let caps = captures.iter().map(|cap| {
        let cap_name = cap.to_string();
        let cap_ts: TokenStream2 = TokenStream::from(cap.clone()).into();
        quote! { env.raw_set(#cap_name, #cap_ts)?; }
    });

    quote! {{
        use ruau::{AsChunk, ChunkMode, Luau, Result, Table};
        use ::std::borrow::Cow;
        use ::std::cell::Cell;
        use ::std::io::Result as IoResult;

        struct InnerChunk<F: FnOnce(&Luau) -> Result<Table>>(Cell<Option<F>>);

        impl<F> AsChunk for InnerChunk<F>
        where
            F: FnOnce(&Luau) -> Result<Table>,
        {
            fn environment(&self, lua: &Luau) -> Result<Option<Table>> {
                if #caps_len > 0 {
                    if let Some(make_env) = self.0.take() {
                        return make_env(lua).map(Some);
                    }
                }
                Ok(None)
            }

            fn mode(&self) -> Option<ChunkMode> {
                Some(ChunkMode::Text)
            }

            fn source<'a>(&self) -> IoResult<Cow<'a, [u8]>> {
                Ok(Cow::Borrowed((#source).as_bytes()))
            }
        }

        let make_env = move |lua: &Luau| -> Result<Table> {
            let globals = lua.globals();
            let env = lua.create_table()?;
            let meta = lua.create_table()?;
            meta.raw_set("__index", &globals)?;
            meta.raw_set("__newindex", &globals)?;

            #(#caps)*

            env.set_metatable(Some(meta))?;
            Ok(env)
        };

        InnerChunk(Cell::new(Some(make_env)))
    }}
    .into()
}

#[proc_macro_derive(FromLuau)]
/// Derive `ruau::FromLuau` for a Rust type.
pub fn from_luau(input: TokenStream) -> TokenStream {
    let DeriveInput {
        ident, generics, ..
    } = parse_macro_input!(input as DeriveInput);

    let ident_str = ident.to_string();
    let (impl_generics, ty_generics, _) = generics.split_for_impl();
    let where_clause = match &generics.where_clause {
        Some(where_clause) => quote! { #where_clause, Self: 'static + Clone },
        None => quote! { where Self: 'static + Clone },
    };

    quote! {
        impl #impl_generics ::ruau::FromLuau for #ident #ty_generics #where_clause {
            #[inline]
            fn from_luau(value: ::ruau::Value, _: &::ruau::Luau) -> ::ruau::Result<Self> {
                match value {
                    ::ruau::Value::UserData(ud) => Ok(ud.borrow::<Self>()?.clone()),
                    _ => Err(::ruau::Error::FromLuauConversionError {
                        from: value.type_name(),
                        to: #ident_str.to_string(),
                        message: None,
                    }),
                }
            }
        }
    }
    .into()
}
