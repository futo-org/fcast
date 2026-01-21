use proc_macro::TokenStream;
use quote::quote;
use syn::{DeriveInput, parse_macro_input};

#[proc_macro_derive(GetTypeString)]
pub fn derive_get_type_string(input: TokenStream) -> TokenStream {
    let file = syn::parse(input.clone()).unwrap();
    let struct_def = prettyplease::unparse(&file);
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    let expanded = quote! {
        impl #name {
            pub fn type_string() -> &'static str {
                #struct_def
            }
        }
    };

    TokenStream::from(expanded)
}
