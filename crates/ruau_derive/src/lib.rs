//! Procedural macros for `ruau`.

#![allow(clippy::absolute_paths, clippy::missing_docs_in_private_items)]

#[cfg(feature = "macros")]
use {
    crate::chunk::Chunk,
    proc_macro::{TokenStream, TokenTree},
    proc_macro_error2::proc_macro_error,
    proc_macro2::TokenStream as TokenStream2,
    quote::quote,
};

#[cfg(feature = "macros")]
fn to_ident(tt: &TokenTree) -> TokenStream2 {
    let s: TokenStream = tt.clone().into();
    s.into()
}

#[cfg(feature = "macros")]
#[proc_macro]
#[proc_macro_error]
/// Capture Rust variables inside an inline Luau chunk.
pub fn chunk(input: TokenStream) -> TokenStream {
    let chunk = Chunk::new(input);

    let source = chunk.source();

    let caps_len = chunk.captures().len();
    let caps = chunk.captures().iter().map(|cap| {
        let cap_name = cap.as_rust().to_string();
        let cap = to_ident(cap.as_rust());
        quote! { env.raw_set(#cap_name, #cap)?; }
    });

    let wrapped_code = quote! {{
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

            // Add captured variables
            #(#caps)*

            env.set_metatable(Some(meta))?;
            Ok(env)
        };

        InnerChunk(Cell::new(Some(make_env)))
    }};

    wrapped_code.into()
}

#[cfg(feature = "macros")]
#[proc_macro_derive(FromLuau)]
/// Derive `ruau::FromLuau` for a Rust type.
pub fn from_luau(input: TokenStream) -> TokenStream {
    from_luau::from_luau(input)
}

#[cfg(feature = "macros")]
mod chunk;
#[cfg(feature = "macros")]
mod from_luau;
#[cfg(feature = "macros")]
mod token;
