//! `#[mcp_tool]` proc-macro for `simulacra-mcp-server-sdk` (S041).
//!
//! Wraps a function `fn name(args: ArgsType) -> Result<Ret, Err>` so that:
//!
//! 1. The original function is preserved verbatim for direct calls.
//! 2. A registration record is submitted to the SDK's `inventory` registry,
//!    populated at static-init time. The record carries the tool name,
//!    description, a `schemars`-derived JSON Schema *builder*, and a dispatch
//!    closure that deserializes JSON, calls the function, and serializes the
//!    return.
//!
//! Usage:
//!
//! ```ignore
//! use simulacra_mcp_server_sdk::mcp_tool;
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(Deserialize, schemars::JsonSchema)]
//! struct EchoArgs { query: String }
//!
//! #[derive(Serialize)]
//! struct EchoOut { echoed: serde_json::Value }
//!
//! #[mcp_tool(description = "Echo input back")]
//! fn echo(args: EchoArgs) -> Result<EchoOut, String> {
//!     Ok(EchoOut { echoed: serde_json::json!({ "query": args.query }) })
//! }
//! ```

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{
    FnArg, ItemFn, LitStr, Meta, PatType, Token, Type, parse::Parser, parse_macro_input,
    punctuated::Punctuated, spanned::Spanned,
};

/// Parsed contents of `#[mcp_tool(description = "...")]`.
#[derive(Default)]
struct ToolAttrs {
    description: Option<String>,
}

fn parse_attrs(args: TokenStream) -> syn::Result<ToolAttrs> {
    let mut out = ToolAttrs::default();
    if args.is_empty() {
        return Ok(out);
    }

    let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
    let metas = parser.parse(args)?;

    for meta in metas {
        match meta {
            Meta::NameValue(nv) if nv.path.is_ident("description") => {
                if let syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(s),
                    ..
                }) = nv.value
                {
                    out.description = Some(s.value());
                } else {
                    return Err(syn::Error::new(
                        nv.path.span(),
                        "`description` must be a string literal",
                    ));
                }
            }
            other => {
                return Err(syn::Error::new(
                    other.span(),
                    "unsupported #[mcp_tool] attribute (expected `description = \"…\"`)",
                ));
            }
        }
    }

    Ok(out)
}

/// Annotate a free function as an MCP tool.
#[proc_macro_attribute]
pub fn mcp_tool(args: TokenStream, input: TokenStream) -> TokenStream {
    let attrs = match parse_attrs(args) {
        Ok(a) => a,
        Err(e) => return e.to_compile_error().into(),
    };

    let func = parse_macro_input!(input as ItemFn);
    let fn_ident = func.sig.ident.clone();
    let tool_name = fn_ident.to_string();
    let description = attrs.description.unwrap_or_default();

    // Identify the single argument's type. We require exactly one positional
    // argument (the deserialized args struct). A zero-arg or multi-arg function
    // does not match the v1 MCP tool shape.
    let arg_ty: Type = match func.sig.inputs.len() {
        1 => match func.sig.inputs.first().unwrap() {
            FnArg::Typed(PatType { ty, .. }) => (**ty).clone(),
            FnArg::Receiver(r) => {
                return syn::Error::new(r.span(), "#[mcp_tool] cannot be used on methods")
                    .to_compile_error()
                    .into();
            }
        },
        n => {
            return syn::Error::new(
                func.sig.span(),
                format!("#[mcp_tool] requires exactly one argument (the args struct); found {n}"),
            )
            .to_compile_error()
            .into();
        }
    };

    // Hidden idents we generate, namespaced by the tool name to avoid
    // collisions across multiple #[mcp_tool] functions in the same module.
    let dispatch_ident = format_ident!("__simulacra_mcp_dispatch_{}", fn_ident);
    let schema_ident = format_ident!("__simulacra_mcp_schema_{}", fn_ident);
    let description_lit = LitStr::new(&description, proc_macro2::Span::call_site());
    let tool_name_lit = LitStr::new(&tool_name, proc_macro2::Span::call_site());

    let expanded = quote! {
        #func

        #[doc(hidden)]
        fn #dispatch_ident(args_json: &str) -> ::std::result::Result<::std::string::String, ::std::string::String> {
            let parsed: #arg_ty = ::simulacra_mcp_server_sdk::__private::serde_json::from_str(args_json)
                .map_err(|e| ::std::format!("invalid arguments: {}", e))?;
            let out = #fn_ident(parsed).map_err(|e| ::std::format!("{:?}", e))?;
            ::simulacra_mcp_server_sdk::__private::serde_json::to_string(&out)
                .map_err(|e| ::std::format!("serialize failed: {}", e))
        }

        #[doc(hidden)]
        fn #schema_ident() -> ::std::string::String {
            let schema = ::simulacra_mcp_server_sdk::__private::schemars::schema_for!(#arg_ty);
            ::simulacra_mcp_server_sdk::__private::serde_json::to_string(&schema)
                .unwrap_or_else(|_| "{}".to_string())
        }

        ::simulacra_mcp_server_sdk::__private::inventory::submit! {
            ::simulacra_mcp_server_sdk::__private::ToolEntry {
                name: #tool_name_lit,
                description: #description_lit,
                schema: #schema_ident,
                dispatch: #dispatch_ident,
            }
        }
    };

    expanded.into()
}
