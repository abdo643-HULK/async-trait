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
    /// `#[allocator(Type)]` — type only, no expression.
    /// Valid for trait methods without a default body.
    TypeOnly { ty: Type },
    /// `#[allocator(Type => expr)]` — full form.
    /// Valid for any method that has a body (default or impl).
    TypeAndExpr { ty: Type, expr: Expr },
    /// `#[allocator(=> expr)]` — expression only, type inferred from the trait requirement.
    /// Valid for impl methods only.
    ExprOnly { expr: Expr },
    /// `#[allocator]` or `#[unsafe(allocator)]` on a function parameter.
    /// The type and identifier are inferred from the parameter itself.
    Param { ty: Type, ident: Ident },
}

impl AllocatorAttr {
    /// Returns the allocator type if explicitly specified, or `None` for `ExprOnly`.
    pub fn ty(&self) -> Option<&Type> {
        match &self.source {
            AllocatorSource::TypeOnly { ty }
            | AllocatorSource::TypeAndExpr { ty, .. }
            | AllocatorSource::Param { ty, .. } => Some(ty),
            AllocatorSource::ExprOnly { .. } => None,
        }
    }

    /// Returns a token stream for the allocator expression, or `None` for `TypeOnly`.
    pub fn expr_tokens(&self) -> Option<proc_macro2::TokenStream> {
        use quote::quote;
        match &self.source {
            AllocatorSource::TypeAndExpr { expr, .. } | AllocatorSource::ExprOnly { expr } => {
                Some(quote!(#expr))
            }
            AllocatorSource::Param { ident, .. } => Some(quote!(#ident)),
            AllocatorSource::TypeOnly { .. } => None,
        }
    }
}

// ── Method-level attribute parsing ──────────────────────────────────────────

/// Attempt to read an allocator attribute from a single method-level `Attribute`.
///
/// Recognises:
/// * `#[allocator(Type)]`               → `TypeOnly`,    `is_unsafe = false`
/// * `#[allocator(Type => expr)]`       → `TypeAndExpr`, `is_unsafe = false`
/// * `#[allocator(=> expr)]`            → `ExprOnly`,    `is_unsafe = false`
/// * `#[unsafe(allocator(...))]`        → same three forms, `is_unsafe = true`
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
        // #[allocator(...)] — safe path; bare #[allocator] (Meta::Path) is for params only
        if let Meta::List(_) = &attr.meta {
            Some(attr.parse_args::<ExplicitArgs>().map(|args| AllocatorAttr {
                is_unsafe: false,
                source: args.into_source(),
            }))
        } else {
            None
        }
    } else if attr.path().is_ident("unsafe") {
        // #[unsafe(allocator(...))] — unsafe path
        let inner: Meta = match attr.parse_args() {
            Ok(m) => m,
            Err(_) => return None, // not our attribute form
        };
        if let Meta::List(list) = inner {
            if list.path.is_ident("allocator") {
                let span = list.span();
                return Some(
                    syn::parse2::<ExplicitArgs>(list.tokens)
                        .map(|args| AllocatorAttr {
                            is_unsafe: true,
                            source: args.into_source(),
                        })
                        .map_err(|_| {
                            Error::new(
                                span,
                                "malformed #[unsafe(allocator(...))]: \
                                 expected `Type`, `Type => expr`, or `=> expr`",
                            )
                        }),
                );
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
                let _ = other;
                None
            }
        }
    } else {
        None
    }
}

// ── Internal parse helpers ───────────────────────────────────────────────────

/// Parsed content of `#[allocator(...)]` on a method.
///
/// Three forms:
/// * `Type`          → `TypeOnly`
/// * `Type => Expr`  → `TypeAndExpr`
/// * `=> Expr`       → `ExprOnly`
enum ExplicitArgs {
    TypeOnly(Type),
    TypeAndExpr(Type, Expr),
    ExprOnly(Expr),
}

impl ExplicitArgs {
    fn into_source(self) -> AllocatorSource {
        match self {
            ExplicitArgs::TypeOnly(ty) => AllocatorSource::TypeOnly { ty },
            ExplicitArgs::TypeAndExpr(ty, expr) => AllocatorSource::TypeAndExpr { ty, expr },
            ExplicitArgs::ExprOnly(expr) => AllocatorSource::ExprOnly { expr },
        }
    }
}

impl Parse for ExplicitArgs {
    fn parse(input: ParseStream) -> Result<Self> {
        if input.peek(Token![=>]) {
            // `=> expr` — expression-only form
            input.parse::<Token![=>]>()?;
            let expr: Expr = input.parse()?;
            Ok(ExplicitArgs::ExprOnly(expr))
        } else {
            let ty: Type = input.parse()?;
            if input.peek(Token![=>]) {
                // `Type => expr` — full form
                input.parse::<Token![=>]>()?;
                let expr: Expr = input.parse()?;
                Ok(ExplicitArgs::TypeAndExpr(ty, expr))
            } else {
                // `Type` — type-only form
                Ok(ExplicitArgs::TypeOnly(ty))
            }
        }
    }
}
