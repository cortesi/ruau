use proc_macro::TokenStream;
use quote::quote;
use syn::{DeriveInput, parse_macro_input};

pub fn from_luau(input: TokenStream) -> TokenStream {
    let DeriveInput { ident, generics, .. } = parse_macro_input!(input as DeriveInput);

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
