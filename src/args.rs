use crate::allocator::{AllocatorAttr, AllocatorSource};
use proc_macro2::Span;
use syn::parse::{Error, Parse, ParseStream, Result};
use syn::{Expr, Token, Type};

pub struct Args {
    /// `false` (default) – no `Send` bound on the returned future, matching Rust's native AFIT.
    /// `true`            – add `+ Send` (opt-in via `#[async_trait(Send)]`).
    pub is_send: bool,
    /// Optional default allocator applied to every method that does not carry its own
    /// `#[allocator(...)]` attribute.
    pub allocator: Option<AllocatorAttr>,
}

pub mod kw {
    syn::custom_keyword!(Send);
    syn::custom_keyword!(allocator);
}

impl Parse for Args {
    fn parse(input: ParseStream) -> Result<Self> {
        match try_parse(input) {
            Ok(args) if input.is_empty() => Ok(args),
            _ => Err(error()),
        }
    }
}

fn try_parse(input: ParseStream) -> Result<Args> {
    let mut is_send = false;
    let mut allocator = None;

    while !input.is_empty() {
        if input.peek(Token![?]) {
            // `?Send` — explicit "no Send" (same as the default, but self-documenting)
            input.parse::<Token![?]>()?;
            input.parse::<kw::Send>()?;
            is_send = false;
        } else if input.peek(kw::Send) {
            input.parse::<kw::Send>()?;
            is_send = true;
        } else if input.peek(kw::allocator) {
            input.parse::<kw::allocator>()?;
            let content;
            syn::parenthesized!(content in input);
            let ty: Type = content.parse()?;
            content.parse::<Token![=>]>()?;
            let expr: Expr = content.parse()?;
            allocator = Some(AllocatorAttr {
                is_unsafe: false,
                source: AllocatorSource::Explicit { ty, expr },
            });
        } else {
            return Err(input.error(
                "expected `Send`, `?Send`, or `allocator(Type => expr)`",
            ));
        }

        if !input.is_empty() {
            input.parse::<Token![,]>()?;
        }
    }

    Ok(Args { is_send, allocator })
}

fn error() -> Error {
    Error::new(
        Span::call_site(),
        "expected one of: \
         #[async_trait], \
         #[async_trait(Send)], \
         #[async_trait(?Send)], \
         #[async_trait(allocator(Type => expr))], \
         or combinations thereof",
    )
}
