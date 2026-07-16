//! Proc-macro attribute support for the karmaio runtime.
//!
//! Provides `#[karmaio::main]` and `#[karmaio::test]`.
//! They build a `karmaio::RuntimeBuilder` and drive the async function with `Runtime::block_on`.
//!
//! Builder methods can be configured via attribute arguments, e.g.
//! `#[karmaio::main(blocking_threads = 64, driver_capacity = 2048)]` — each
//! `method = value` pair is forwarded to the `RuntimeBuilder`.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{ToTokens, quote};
use syn::{Expr, ItemFn, Meta, Token, parse_macro_input, punctuated::Punctuated};

/// Parsed `#[karmaio::main(...)]` / `#[karmaio::test(...)]` arguments.
///
/// Each entry is a `method = value` pair applied to the `RuntimeBuilder`.
struct RuntimeArgs {
    methods: Vec<(syn::Path, Expr)>,
}

impl syn::parse::Parse for RuntimeArgs {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let metas = Punctuated::<Meta, Token![,]>::parse_terminated(input)?;
        let mut methods = Vec::new();
        for meta in metas {
            match meta {
                Meta::NameValue(nv) => methods.push((nv.path, nv.value)),
                other => {
                    return Err(syn::Error::new_spanned(
                        other,
                        "expected `method = value`, e.g. `blocking_threads = 64`",
                    ));
                }
            }
        }
        Ok(RuntimeArgs { methods })
    }
}

/// Build a `RuntimeBuilder` expression from the parsed arguments.
fn build_builder(args: &RuntimeArgs) -> TokenStream2 {
    let mut builder = quote! { ::karmaio::RuntimeBuilder::new() };
    for (method, value) in &args.methods {
        builder = quote! { #builder.#method(#value) };
    }
    builder
}

/// Shared transform: strip `async`, wrap the body in `block_on`.
fn expand(attr: RuntimeArgs, input: ItemFn, is_test: bool) -> TokenStream {
    if input.sig.asyncness.is_none() {
        return syn::Error::new_spanned(
            input.sig.fn_token,
            "the `async` keyword is missing from the function declaration",
        )
        .to_compile_error()
        .into();
    }

    if !input.sig.inputs.is_empty() {
        let msg = if is_test {
            "test functions cannot accept arguments"
        } else {
            "the `main` function cannot accept arguments"
        };
        return syn::Error::new_spanned(&input.sig.inputs, msg)
            .to_compile_error()
            .into();
    }

    let mut input = input;
    input.sig.asyncness = None;

    let builder = build_builder(&attr);
    let body = &input.block;
    input.block = syn::parse2(quote! {
        {
            let mut rt = #builder
                .build()
                .expect("karmaio: failed to build runtime");
            rt.block_on(async move #body)
        }
    })
    .expect("karmaio: failed to build function body");

    if is_test && !input.attrs.iter().any(|a| a.path().is_ident("test")) {
        input.attrs.push(syn::parse_quote!(#[test]));
    }

    input.into_token_stream().into()
}

/// Define a karmaio runtime entrypoint.
///
/// Transforms an `async fn main` into a synchronous `fn main` that builds a
/// `RuntimeBuilder` and drives the future with `Runtime::block_on`.
///
/// # Examples
///
/// ```ignore
/// #[karmaio::main]
/// async fn main() {
///     let answer = async { 2 * 21 }.await;
///     assert_eq!(answer, 42);
/// }
/// ```
///
/// With runtime configuration:
///
/// ```ignore
/// #[karmaio::main(blocking_threads = 64, driver_capacity = 2048)]
/// async fn main() {
///     println!("configured karmaio runtime");
/// }
/// ```
#[proc_macro_attribute]
pub fn main(args: TokenStream, item: TokenStream) -> TokenStream {
    let attr = parse_macro_input!(args as RuntimeArgs);
    let input = parse_macro_input!(item as ItemFn);
    expand(attr, input, false)
}

/// Define an async test that runs on a karmaio runtime.
///
/// Equivalent to `#[test]` plus a `RuntimeBuilder`/`block_on` wrapper,
/// so the annotated `async fn` is executed on a karmaio runtime.
/// Accepts the same `method = value` configuration as `main`.
///
/// # Examples
///
/// ```ignore
/// #[karmaio::test]
/// async fn my_test() {
///     assert_eq!(1 + 1, 2);
/// }
/// ```
#[proc_macro_attribute]
pub fn test(args: TokenStream, item: TokenStream) -> TokenStream {
    let attr = parse_macro_input!(args as RuntimeArgs);
    let input = parse_macro_input!(item as ItemFn);
    expand(attr, input, true)
}
