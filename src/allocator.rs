use syn::parse::{Parse, ParseStream, Result};
use syn::spanned::Spanned;
use syn::{Attribute, Error, Expr, Ident, Meta, Token, Type};

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
    /// `#[allocator(Type => expr)]` or `#[unsafe(allocator(Type => expr))]` on the method.
    Explicit { ty: Type, expr: Expr },
    /// `#[allocator]` or `#[unsafe(allocator)]` on a function parameter.
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
/// * `#[unsafe(allocator(Type => expr))]`  → `is_unsafe = true`
///
/// The `#[unsafe(...)]` form is stripped by this proc macro before the compiler
/// validates attribute names, so custom `unsafe(allocator(...))` works fine.
///
/// Returns:
/// * `None`           – attribute is unrelated; leave it alone
/// * `Some(Ok(attr))` – recognised and parsed successfully
/// * `Some(Err(e))`   – recognised as an allocator attribute but malformed;
///                      the caller should strip the attribute and report the error
pub fn try_method_alloc(attr: &Attribute) -> Option<Result<AllocatorAttr>> {
    if attr.path().is_ident("allocator") {
        // #[allocator(Type => expr)] — safe path; bare #[allocator] (Meta::Path) is for params
        if let Meta::List(_) = &attr.meta {
            Some(attr.parse_args::<ExplicitArgs>().map(|args| AllocatorAttr {
                is_unsafe: false,
                source: AllocatorSource::Explicit { ty: args.ty, expr: args.expr },
            }))
        } else {
            None
        }
    } else if attr.path().is_ident("unsafe") {
        // #[unsafe(allocator(Type => expr))] — unsafe path
        // syn parses `unsafe` as an ident via Ident::parse_any; the proc macro
        // strips this attribute before the compiler validates the inner name.
        let inner: Meta = match attr.parse_args() {
            Ok(m) => m,
            Err(_) => return None, // not our attribute form
        };
        if let Meta::List(list) = inner {
            if list.path.is_ident("allocator") {
                let span = list.span();
                return Some(syn::parse2::<ExplicitArgs>(list.tokens).map(|args| AllocatorAttr {
                    is_unsafe: true,
                    source: AllocatorSource::Explicit { ty: args.ty, expr: args.expr },
                }).map_err(|_| Error::new(span, "malformed #[unsafe(allocator(...))]: expected `Type => expr`")));
            }
        }
        None
    } else {
        None
    }
}

// ── Parameter-level attribute parsing ───────────────────────────────────────

/// Check whether a parameter attribute is `#[allocator]` or `#[unsafe(allocator)]`.
///
/// Returns:
/// * `None`           – attribute is unrelated
/// * `Some(Ok(b))`    – recognised allocator marker; `b` is `is_unsafe`
/// * `Some(Err(e))`   – looks like an allocator marker but is malformed
pub fn try_param_alloc_marker(attr: &Attribute) -> Option<Result<bool>> {
    if attr.path().is_ident("allocator") {
        // `#[allocator]` – safe path (bare, no args)
        match &attr.meta {
            Meta::Path(_) => Some(Ok(false)),
            _ => None,
        }
    } else if attr.path().is_ident("unsafe") {
        // `#[unsafe(allocator)]` – unsafe path
        let inner: Meta = match attr.parse_args() {
            Ok(m) => m,
            Err(_) => return None,
        };
        match inner {
            Meta::Path(path) if path.is_ident("allocator") => Some(Ok(true)),
            other => {
                // Looks like #[unsafe(...)] but the inner path isn't `allocator` — not ours.
                // Only emit an error if it really looks like a misspelled allocator marker.
                let _ = other;
                None
            }
        }
    } else {
        None
    }
}

// ── Internal parse helpers ───────────────────────────────────────────────────

/// Parses the content of `#[allocator(...)]` on a method: `Type => Expr`
struct ExplicitArgs {
    ty: Type,
    expr: Expr,
}

impl Parse for ExplicitArgs {
    fn parse(input: ParseStream) -> Result<Self> {
        let ty: Type = input.parse()?;
        input.parse::<Token![=>]>()?;
        let expr: Expr = input.parse()?;
        Ok(ExplicitArgs { ty, expr })
    }
}
