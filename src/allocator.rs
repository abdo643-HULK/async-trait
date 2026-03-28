use syn::parse::{Parse, ParseStream, Result};
use syn::{Attribute, Expr, Ident, Meta, Token, Type};

/// Allocator configuration attached to a single async method.
#[derive(Clone)]
pub struct AllocatorAttr {
    /// `true`  → use `unsafe { Pin::new_unchecked(Box::new_in(fut, alloc)) }`
    /// `false` → use `Box::pin_in(fut, alloc)` (requires `A: 'static`)
    pub is_unsafe: bool,
    pub source: AllocatorSource,
}

#[derive(Clone)]
pub enum AllocatorSource {
    /// `#[allocator(Type => expr)]` or `#[allocator(unsafe, Type => expr)]` on the method.
    Explicit { ty: Type, expr: Expr },
    /// `#[allocator]` or `#[allocator(unsafe)]` on a function parameter.
    /// The type and identifier are inferred from the parameter itself.
    Param { ty: Type, ident: Ident },
}

impl AllocatorAttr {
    pub fn ty(&self) -> &Type {
        match &self.source {
            AllocatorSource::Explicit { ty, .. } | AllocatorSource::Param { ty, .. } => ty,
        }
    }

    pub fn expr_tokens(&self) -> proc_macro2::TokenStream {
        use quote::quote;
        match &self.source {
            AllocatorSource::Explicit { expr, .. } => quote!(#expr),
            AllocatorSource::Param { ident, .. } => quote!(#ident),
        }
    }
}

// ── Method-level attribute parsing ──────────────────────────────────────────

/// Attempt to read an allocator attribute from a single method-level `Attribute`.
///
/// Recognises:
/// * `#[allocator(Type => expr)]`          → `is_unsafe = false`
/// * `#[allocator(unsafe, Type => expr)]`  → `is_unsafe = true`
///
/// Returns `None` if the attribute is not an allocator attribute.
pub fn try_method_alloc(attr: &Attribute) -> Option<AllocatorAttr> {
    if !attr.path().is_ident("allocator") {
        return None;
    }
    match &attr.meta {
        // bare `#[allocator]` – that is the parameter-level marker, not a method-level one
        Meta::Path(_) => None,
        Meta::List(_) => {
            let args: ExplicitArgs = attr.parse_args().ok()?;
            Some(AllocatorAttr {
                is_unsafe: args.is_unsafe,
                source: AllocatorSource::Explicit {
                    ty: args.ty,
                    expr: args.expr,
                },
            })
        }
        _ => None,
    }
}

// ── Parameter-level attribute parsing ───────────────────────────────────────

/// Check whether a parameter attribute is `#[allocator]` or `#[allocator(unsafe)]`.
///
/// Returns `Some(is_unsafe)` when the attribute is the allocator marker, `None` otherwise.
pub fn try_param_alloc_marker(attr: &Attribute) -> Option<bool> {
    if !attr.path().is_ident("allocator") {
        return None;
    }
    match &attr.meta {
        // `#[allocator]` – safe path
        Meta::Path(_) => Some(false),
        // `#[allocator(unsafe)]` – unsafe path
        Meta::List(_) => {
            let is_kw: Result<Token![unsafe]> = attr.parse_args();
            if is_kw.is_ok() {
                Some(true)
            } else {
                // some other form – not a bare unsafe marker
                None
            }
        }
        _ => None,
    }
}

// ── Internal parse helpers ───────────────────────────────────────────────────

/// Parses the content of `#[allocator(...)]` on a method:
///   `[unsafe ,] Type => Expr`
struct ExplicitArgs {
    is_unsafe: bool,
    ty: Type,
    expr: Expr,
}

impl Parse for ExplicitArgs {
    fn parse(input: ParseStream) -> Result<Self> {
        // Optional leading `unsafe ,`
        let is_unsafe = if input.peek(Token![unsafe]) {
            input.parse::<Token![unsafe]>()?;
            input.parse::<Token![,]>()?;
            true
        } else {
            false
        };
        let ty: Type = input.parse()?;
        input.parse::<Token![=>]>()?;
        let expr: Expr = input.parse()?;
        Ok(ExplicitArgs { is_unsafe, ty, expr })
    }
}
