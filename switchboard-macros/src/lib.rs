//! Switchboard-related proc macros.
//!
//! Current list (see function docs for details):
//!   - [`http_status_code_derive`] (for `#[derive(HttpStatusCode)]`)

use attribute_derive::FromAttr;
use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::spanned::Spanned;
use syn::{parse_macro_input, Data, DeriveInput, Fields};

// --- #[derive(HttpStatusCode)] -------------------------------------------------------------------

/// Simple derive macro that makes it easy to specify HTTP Status Codes for variants of enums.
///
/// Syntax:
///
/// ```rs
/// use serde::{Serialize, Deserialize};
/// use switchboard_macros::HttpStatusCode;
///
/// #[derive(HttpStatusCode, Serialize, Deserialize)]
/// enum MyResponse {
///     #[http(status = 200)]
///     Ok,
///     #[http(status = 403)]
///     Unauthorized,
///     #[http(status = 500)]
///     Database,
/// }
/// ```
///
/// Future work:
///   - support for `struct`s
///   - make sure that the implementation doesn't have any weird edge cases, since this is the
///       first proc macro I've written
#[proc_macro_derive(HttpStatusCode, attributes(http))]
pub fn http_status_code_derive(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    expand_http_status_code_derive(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

#[derive(FromAttr)]
#[attribute(ident = http)]
#[attribute(error(missing_field = "`{field}` was not specified"))]
struct HttpAttrs {
    #[attribute(example = "#[http(status = 404)")]
    status: u16,
}

// Inner function so that it's easier to wrap it with Error::into_compile_error
fn expand_http_status_code_derive(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let Data::Enum(numer) = input.data else {
        return Err(syn::Error::new(
            input.span(),
            "#[derive(HttpStatusCode)] only works on enums.",
        ));
    };
    let mut status_code_match_arms = vec![];
    let mut variant_idents = vec![];
    let mut variant_values = vec![];
    for variant in numer.variants {
        let attrs = HttpAttrs::from_attributes(&variant.attrs)?;
        let variant_ident = variant.ident;
        let variant_value = attrs.status;
        let variant_fields = match &variant.fields {
            Fields::Named(_) => {
                quote!({ .. })
            }
            Fields::Unnamed(_) => {
                quote!((..))
            }
            Fields::Unit => {
                quote!()
            }
        };
        status_code_match_arms.push(quote! {
            Self::#variant_ident #variant_fields => http::StatusCode::from_u16(#variant_value)
                .expect("Invalid HTTP Status Code (please check HttpStatusCode Derivation)")
        });
        variant_idents.push(variant_ident);
        variant_values.push(variant_value);
    }

    let name = input.ident;
    let name_test = format_ident!("__{name}_validity_tests");

    let trait_name = quote! { tml_switchboard_traits::JsonProxiedStatus };

    let output = quote! {
        impl #trait_name for #name {
            fn status_code(&self) -> http::StatusCode {
                match self {
                    #(#status_code_match_arms),*
                }
            }
        }
        #[cfg(test)]
        mod #name_test {
            #[test]
            fn #name_test() {
                #(http::StatusCode::from_u16(#variant_values)
                    .expect(&format!("--[[ Invalid HTTP Status Code (status = {}) for {}::{} ]]--. Error",
                        #variant_values, stringify!(#name), stringify!(#variant_idents)))
                 );*
                ;
            }
        }
    };

    Ok(output)
}
