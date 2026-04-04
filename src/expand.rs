use crate::allocator::{self, AllocatorAttr, AllocatorSource};
use crate::args::Args;
use crate::bound::{has_bound, InferredBound, Supertraits};
use crate::lifetime::{AddLifetimeToImplTrait, CollectLifetimes};
use crate::parse::Item;
use crate::receiver::{has_self_in_block, has_self_in_sig, mut_pat, ReplaceSelf};
use crate::verbatim::VerbatimFn;
use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote, quote_spanned, ToTokens};
use std::collections::BTreeSet as Set;
use std::mem;
use syn::punctuated::Punctuated;
use syn::visit_mut::{self, VisitMut};
use syn::{
    parse_quote, parse_quote_spanned, Attribute, Block, Error, Expr, FnArg, GenericArgument,
    GenericParam, Generics, Ident, ImplItem, Lifetime, LifetimeParam, Pat, PatIdent,
    PathArguments, Receiver, Result, ReturnType, Signature, Token, TraitItem, Type, TypeInfer,
    TypePath, WhereClause,
};

impl ToTokens for Item {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        match self {
            Item::Trait(item) => item.to_tokens(tokens),
            Item::Impl(item) => item.to_tokens(tokens),
        }
    }
}

#[derive(Clone, Copy)]
enum Context<'a> {
    Trait {
        generics: &'a Generics,
        supertraits: &'a Supertraits,
    },
    Impl {
        impl_generics: &'a Generics,
        associated_type_impl_traits: &'a Set<Ident>,
    },
}

impl Context<'_> {
    fn lifetimes<'a>(&'a self, used: &'a [Lifetime]) -> impl Iterator<Item = &'a LifetimeParam> {
        let generics = match self {
            Context::Trait { generics, .. } => generics,
            Context::Impl { impl_generics, .. } => impl_generics,
        };
        generics.params.iter().filter_map(move |param| {
            if let GenericParam::Lifetime(param) = param {
                if used.contains(&param.lifetime) {
                    return Some(param);
                }
            }
            None
        })
    }
}

// ── Error accumulator ────────────────────────────────────────────────────────

struct Errors(Option<Error>);

impl Errors {
    fn append(&mut self, error: Error) {
        match &mut self.0 {
            Some(e) => e.combine(error),
            None => self.0 = Some(error),
        }
    }
}

impl Extend<Error> for Errors {
    fn extend<T: IntoIterator<Item = Error>>(&mut self, iter: T) {
        let mut iter = iter.into_iter();
        if let Some(mut e) = self.0.take().or_else(|| iter.next()) {
            e.extend(iter);
            self.0 = Some(e);
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Expand a `#[async_trait]` item.
///
/// Returns the modified item (in place) and a `TokenStream` of extra items that
/// should be emitted *before* the transformed item in the output (currently the
/// `#[diagnostic::on_unimplemented]`-annotated helper module for Send traits).
pub fn expand(input: &mut Item, args: &Args) -> (TokenStream, Result<()>) {
    let mut errors = Errors(None);
    let mut extra = TokenStream::new();
    expand_inner(input, args, &mut errors, &mut extra);
    let result = match errors.0 {
        Some(e) => Err(e),
        None => Ok(()),
    };
    (extra, result)
}

fn expand_inner(input: &mut Item, args: &Args, errors: &mut Errors, extra: &mut TokenStream) {
    match input {
        Item::Trait(input) => {
            let context = Context::Trait {
                generics: &input.generics,
                supertraits: &input.supertraits,
            };

            // For Send traits, emit a companion diagnostic module so that the
            // `where Self: AsyncTraitSend[Sync] + 'async_trait` bound fires
            // `#[diagnostic::on_unimplemented]` with a helpful message instead of
            // the generic "cannot be sent between threads safely" error.
            let diag_helper: Option<Ident> = if !args.local {
                let mod_ident = format_ident!("__async_trait_diag_{}", input.ident);
                let trait_name = input.ident.to_string();
                extra.extend(diagnostic_send_module(&mod_ident, &trait_name));
                Some(mod_ident)
            } else {
                None
            };

            for inner in &mut input.items {
                if let TraitItem::Fn(method) = inner {
                    let sig = &mut method.sig;
                    if sig.asyncness.is_some() {
                        let (send_override, mut method_alloc) =
                            extract_method_config(&mut method.attrs, args.local, errors);
                        if method_alloc.is_none() {
                            method_alloc = extract_param_alloc(sig, errors);
                        }
                        let is_local = method_is_local(args.local, send_override);
                        let effective_alloc =
                            method_alloc.as_ref().or(args.allocator.as_ref());

                        // Validate that the method-level allocator uses the correct form.
                        // `Param` sources have no form to validate; args.allocator is always
                        // TypeAndExpr (enforced by the attribute parser in args.rs).
                        if let Some(ref a) = method_alloc {
                            let has_body = method.default.is_some();
                            match &a.source {
                                AllocatorSource::ExprOnly { expr } => {
                                    errors.append(Error::new(
                                        syn::spanned::Spanned::span(expr),
                                        "allocator type is required in trait declarations; \
                                         use `#[allocator(Type)]` or `#[allocator(Type => expr)]`",
                                    ));
                                }
                                AllocatorSource::TypeAndExpr { expr, .. } if !has_body => {
                                    errors.append(Error::new(
                                        syn::spanned::Spanned::span(expr),
                                        "expression is unused (trait method has no body); \
                                         use `#[allocator(Type)]`",
                                    ));
                                }
                                AllocatorSource::TypeOnly { ty } if has_body => {
                                    errors.append(Error::new(
                                        syn::spanned::Spanned::span(ty),
                                        "expression required for default method body; \
                                         use `#[allocator(Type => expr)]`",
                                    ));
                                }
                                _ => {}
                            }
                        }

                        let block = &mut method.default;
                        let mut has_self = has_self_in_sig(sig);
                        method.attrs.push(parse_quote!(#[must_use]));
                        if let Some(block) = block {
                            has_self |= has_self_in_block(block);
                            transform_block(context, sig, block, effective_alloc);
                            method.attrs.push(lint_suppress_with_body());
                        } else {
                            method.attrs.push(lint_suppress_without_body());
                        }
                        let has_default = method.default.is_some();
                        transform_sig(
                            context,
                            sig,
                            has_self,
                            has_default,
                            is_local,
                            effective_alloc,
                            diag_helper.as_ref(),
                        );
                    } else {
                        check_async_trait_not_allowed(&mut method.attrs, errors);
                    }
                }
            }
        }
        Item::Impl(input) => {
            let mut associated_type_impl_traits = Set::new();
            for inner in &input.items {
                if let ImplItem::Type(assoc) = inner {
                    if let Type::ImplTrait(_) = assoc.ty {
                        associated_type_impl_traits.insert(assoc.ident.clone());
                    }
                }
            }

            let context = Context::Impl {
                impl_generics: &input.generics,
                associated_type_impl_traits: &associated_type_impl_traits,
            };
            for inner in &mut input.items {
                match inner {
                    ImplItem::Fn(method) if method.sig.asyncness.is_some() => {
                        let sig = &mut method.sig;
                        let block = &mut method.block;
                        let has_self = has_self_in_sig(sig);

                        let (send_override, mut method_alloc) =
                            extract_method_config(&mut method.attrs, args.local, errors);
                        if method_alloc.is_none() {
                            method_alloc = extract_param_alloc(sig, errors);
                        }
                        let is_local = method_is_local(args.local, send_override);
                        // Unit-struct sugar: `#[allocator(Type)]` in an impl method body
                        // is treated as `#[allocator(Type => Type)]`, i.e. the type token
                        // itself is used as the allocator expression.  This works for unit
                        // structs (Global, System, …); non-unit structs produce a natural
                        // compiler error ("struct has fields that need initialisation").
                        let synthetic_alloc: Option<AllocatorAttr> =
                            if let Some(ref a) = method_alloc {
                                if let AllocatorSource::TypeOnly { ty } = &a.source {
                                    let ty = ty.clone();
                                    let expr: Expr = syn::parse2(quote!(#ty))
                                        .expect("type path is a valid expr");
                                    Some(AllocatorAttr {
                                        is_unsafe: a.is_unsafe,
                                        source: AllocatorSource::TypeAndExpr { ty, expr },
                                    })
                                } else {
                                    None
                                }
                            } else {
                                None
                            };
                        let mut effective_alloc: Option<&AllocatorAttr> =
                            if let Some(ref s) = synthetic_alloc {
                                Some(s)
                            } else {
                                method_alloc.as_ref().or(args.allocator.as_ref())
                            };

                        // `ExprOnly` is always an error in impl context (the type cannot be
                        // inferred from the trait declaration).  Clear effective_alloc so
                        // both transforms fall back to Box::pin for clean IDE recovery.
                        if let Some(AllocatorSource::ExprOnly { expr }) =
                            method_alloc.as_ref().map(|a| &a.source)
                        {
                            errors.append(Error::new(
                                syn::spanned::Spanned::span(expr),
                                "allocator type cannot be inferred from the trait \
                                 declaration; use `#[allocator(Type => expr)]`",
                            ));
                            effective_alloc = None;
                        }

                        transform_block(context, sig, block, effective_alloc);
                        transform_sig(
                            context,
                            sig,
                            has_self,
                            false,
                            is_local,
                            effective_alloc,
                            None, // impl methods: no diagnostic helper needed
                        );
                        method.attrs.push(lint_suppress_with_body());
                    }
                    ImplItem::Fn(method) => {
                        check_async_trait_not_allowed(&mut method.attrs, errors);
                    }
                    ImplItem::Verbatim(tokens) => {
                        let mut method = match syn::parse2::<VerbatimFn>(tokens.clone()) {
                            Ok(method) if method.sig.asyncness.is_some() => method,
                            _ => continue,
                        };
                        let sig = &mut method.sig;
                        let has_self = has_self_in_sig(sig);

                        // Verbatim items have no attrs vec to extract from, use defaults.
                        let is_local = args.local;
                        let effective_alloc = args.allocator.as_ref();

                        transform_sig(
                            context,
                            sig,
                            has_self,
                            false,
                            is_local,
                            effective_alloc,
                            None, // verbatim items are in impl context; no helper
                        );
                        method.attrs.push(lint_suppress_with_body());
                        *tokens = quote!(#method);
                    }
                    _ => {}
                }
            }
        }
    }
}

// ── Per-method configuration extraction ─────────────────────────────────────

/// Resolve whether a specific method should be local (no Send) given the trait-level default
/// and an optional per-method override.
fn method_is_local(trait_is_local: bool, send_override: Option<bool>) -> bool {
    send_override.map(|s| !s).unwrap_or(trait_is_local)
}

/// Extract and strip per-method `#[async_trait(Send)]`, `#[async_trait(?Send)]`, and
/// `#[allocator(...)]` attributes from `attrs`.
///
/// Returns `(send_override, method_alloc)` where:
/// - `send_override` is `Some(true)` for `#[async_trait(Send)]`,
///   `Some(false)` for `#[async_trait(?Send)]`, `None` if absent.
/// - `method_alloc` is the parsed `AllocatorAttr` if present.
///
/// Errors are appended to `errors` rather than returned.
fn extract_method_config(
    attrs: &mut Vec<Attribute>,
    trait_is_local: bool,
    errors: &mut Errors,
) -> (Option<bool>, Option<AllocatorAttr>) {
    let mut send_override: Option<bool> = None;
    let mut method_alloc: Option<AllocatorAttr> = None;

    let old = mem::take(attrs);
    *attrs = old
        .into_iter()
        .filter(|attr| {
            // `#[async_trait(Send)]` or `#[async_trait(?Send)]` on the method
            if attr.path().is_ident("async_trait") {
                let span = syn::spanned::Spanned::span(attr);
                if send_override.is_some() {
                    errors.append(Error::new(span, "duplicate #[async_trait] on method"));
                    return false;
                }
                match attr.parse_args::<MethodSendArg>() {
                    Ok(arg) => {
                        let override_is_local = !arg.is_send;
                        if override_is_local == trait_is_local {
                            errors.append(Error::new(
                                span,
                                if trait_is_local {
                                    "redundant #[async_trait(?Send)] on method"
                                } else {
                                    "redundant #[async_trait(Send)] on method"
                                },
                            ));
                        } else {
                            send_override = Some(arg.is_send);
                        }
                    }
                    Err(e) => errors.append(Error::new(span, e)),
                }
                return false;
            }
            // `#[allocator(Type => expr)]` or `#[unsafe(allocator(Type => expr))]`
            match allocator::try_method_alloc(attr) {
                Some(Ok(alloc)) => {
                    method_alloc = Some(alloc);
                    false
                }
                Some(Err(e)) => {
                    errors.append(e);
                    false
                }
                None => true,
            }
        })
        .collect();

    (send_override, method_alloc)
}

/// Scan `sig.inputs` for a parameter annotated with `#[allocator]` or `#[unsafe(allocator)]`.
/// Strips the attribute from the parameter and returns the corresponding `AllocatorAttr`.
fn extract_param_alloc(sig: &mut Signature, errors: &mut Errors) -> Option<AllocatorAttr> {
    for arg in sig.inputs.iter_mut() {
        if let FnArg::Typed(typed) = arg {
            let mut found: Option<bool> = None;
            let old_attrs = mem::take(&mut typed.attrs);
            typed.attrs = old_attrs
                .into_iter()
                .filter(|attr| {
                    match allocator::try_param_alloc_marker(attr) {
                        Some(Ok(is_unsafe)) => {
                            found = Some(is_unsafe);
                            false
                        }
                        Some(Err(e)) => {
                            errors.append(e);
                            false
                        }
                        None => true,
                    }
                })
                .collect();

            if let Some(is_unsafe) = found {
                if let Pat::Ident(PatIdent { ident, .. }) = &*typed.pat {
                    return Some(AllocatorAttr {
                        is_unsafe,
                        source: AllocatorSource::Param {
                            ty: (*typed.ty).clone(),
                            ident: ident.clone(),
                        },
                    });
                }
            }
        }
    }
    None
}

/// Minimal parser for the `Send` / `?Send` argument inside `#[async_trait(...)]` on a method.
struct MethodSendArg {
    is_send: bool,
}

impl syn::parse::Parse for MethodSendArg {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        if input.peek(Token![?]) {
            input.parse::<Token![?]>()?;
            input.parse::<crate::args::kw::Send>()?;
            Ok(MethodSendArg { is_send: false })
        } else {
            input.parse::<crate::args::kw::Send>()?;
            Ok(MethodSendArg { is_send: true })
        }
    }
}

// ── Attribute checks ─────────────────────────────────────────────────────────

/// Strip any `#[async_trait(...)]` attributes from a non-async method and report them as errors.
fn check_async_trait_not_allowed(attrs: &mut Vec<Attribute>, errors: &mut Errors) {
    let old = mem::take(attrs);
    *attrs = old
        .into_iter()
        .filter(|attr| {
            if attr.path().is_ident("async_trait") {
                errors.append(Error::new(
                    syn::spanned::Spanned::span(attr),
                    "#[async_trait] attribute is not allowed on non-async methods",
                ));
                false
            } else {
                true
            }
        })
        .collect();
}

// ── Lint suppression attributes ──────────────────────────────────────────────

fn lint_suppress_with_body() -> Attribute {
    parse_quote! {
        #[allow(
            elided_named_lifetimes,
            clippy::async_yields_async,
            clippy::diverging_sub_expression,
            clippy::let_unit_value,
            clippy::needless_arbitrary_self_type,
            clippy::no_effect_underscore_binding,
            clippy::shadow_same,
            clippy::type_complexity,
            clippy::type_repetition_in_bounds,
            clippy::used_underscore_binding
        )]
    }
}

fn lint_suppress_without_body() -> Attribute {
    parse_quote! {
        #[allow(
            elided_named_lifetimes,
            clippy::type_complexity,
            clippy::type_repetition_in_bounds
        )]
    }
}

// ── Diagnostic helper module ─────────────────────────────────────────────────

/// Generate a private module containing `#[diagnostic::on_unimplemented]`-annotated
/// helper traits for the `Self: Send [+ Sync] + 'async_trait` bounds.
///
/// The module is emitted as a sibling of the `#[async_trait(Send)]` trait so that
/// `transform_sig` can reference `self::<mod_ident>::AsyncTraitSend[Sync]` in the
/// where clause.  This fires E0277 with a context-aware message instead of the
/// default "cannot be sent between threads safely".
///
/// Two helper traits are generated:
/// * `AsyncTraitSend`     — supertrait `Send` only (for `self` / `Arc<Self>` receivers)
/// * `AsyncTraitSendSync` — supertraits `Send + Sync` (for `&self` / `&mut self` receivers)
fn diagnostic_send_module(mod_ident: &Ident, trait_name: &str) -> TokenStream {
    // Build notes as String so quote! emits them as string literals (via ToTokens for String),
    // allowing the trait name to be embedded without using concat!() which is not a literal.
    let note_send = format!(
        "all implementations of `{trait_name}` must be `Send`; \
         `#[async_trait]` wraps async fn return types in \
         `Pin<Box<dyn Future<Output = ...> + Send + '_>>`"
    );
    let note_send_sync = format!(
        "all implementations of `{trait_name}` must be `Send + Sync`; \
         `&self` receivers require `Send + Sync` so that `&Self` is `Send`"
    );
    let note_opt_out = "use `#[async_trait(?Send)]` to allow non-`Send` implementations";

    quote! {
        #[doc(hidden)]
        #[allow(non_snake_case, dead_code)]
        mod #mod_ident {
            #[diagnostic::on_unimplemented(
                message = "`{Self}` cannot implement this async trait method because it is not `Send`",
                note = #note_send,
                note = #note_opt_out,
            )]
            pub trait AsyncTraitSend: ::core::marker::Send {}

            #[diagnostic::do_not_recommend]
            impl<T: ::core::marker::Send> AsyncTraitSend for T {}

            #[diagnostic::on_unimplemented(
                message = "`{Self}` cannot implement this async trait method because it is not `Send + Sync`",
                note = #note_send_sync,
                note = #note_opt_out,
            )]
            pub trait AsyncTraitSendSync: ::core::marker::Send + ::core::marker::Sync {}

            #[diagnostic::do_not_recommend]
            impl<T: ::core::marker::Send + ::core::marker::Sync> AsyncTraitSendSync for T {}
        }
    }
}

// ── Signature transformation ─────────────────────────────────────────────────

// Input:
//     async fn f<T>(&self, x: &T) -> Ret;
//
// Output (no Send, no custom allocator):
//     fn f<'life0, 'life1, 'async_trait, T>(
//         &'life0 self,
//         x: &'life1 T,
//     ) -> Pin<Box<dyn Future<Output = Ret> + 'async_trait>>
//     where
//         'life0: 'async_trait,
//         'life1: 'async_trait,
//         T: 'async_trait,
//         Self: 'async_trait;
//
// With Send + custom allocator:
//     -> Pin<Box<dyn Future<Output = Ret> + Send + 'async_trait, MyAlloc>>
fn transform_sig(
    context: Context,
    sig: &mut Signature,
    has_self: bool,
    has_default: bool,
    is_local: bool,
    alloc: Option<&AllocatorAttr>,
    send_helper: Option<&Ident>,
) {
    sig.fn_token.span = sig.asyncness.take().unwrap().span;

    let (ret_arrow, ret) = match &sig.output {
        ReturnType::Default => (quote!(->), quote!(())),
        ReturnType::Type(arrow, ret) => (quote!(#arrow), quote!(#ret)),
    };

    let mut lifetimes = CollectLifetimes::new();
    for arg in &mut sig.inputs {
        match arg {
            FnArg::Receiver(arg) => lifetimes.visit_receiver_mut(arg),
            FnArg::Typed(arg) => lifetimes.visit_type_mut(&mut arg.ty),
        }
    }

    for param in &mut sig.generics.params {
        match param {
            GenericParam::Type(param) => {
                let param_name = &param.ident;
                let span = match param.colon_token.take() {
                    Some(colon_token) => colon_token.span,
                    None => param_name.span(),
                };
                if param.attrs.is_empty() {
                    let bounds = mem::take(&mut param.bounds);
                    where_clause_or_default(&mut sig.generics.where_clause)
                        .predicates
                        .push(parse_quote_spanned!(span=> #param_name: 'async_trait + #bounds));
                } else {
                    param.bounds.push(parse_quote!('async_trait));
                }
            }
            GenericParam::Lifetime(param) => {
                let param_name = &param.lifetime;
                let span = match param.colon_token.take() {
                    Some(colon_token) => colon_token.span,
                    None => param_name.span(),
                };
                if param.attrs.is_empty() {
                    let bounds = mem::take(&mut param.bounds);
                    where_clause_or_default(&mut sig.generics.where_clause)
                        .predicates
                        .push(parse_quote_spanned!(span=> #param: 'async_trait + #bounds));
                } else {
                    param.bounds.push(parse_quote!('async_trait));
                }
            }
            GenericParam::Const(_) => {}
        }
    }

    for param in context.lifetimes(&lifetimes.explicit) {
        let param = &param.lifetime;
        let span = param.span();
        where_clause_or_default(&mut sig.generics.where_clause)
            .predicates
            .push(parse_quote_spanned!(span=> #param: 'async_trait));
    }

    if sig.generics.lt_token.is_none() {
        sig.generics.lt_token = Some(Token![<](sig.ident.span()));
    }
    if sig.generics.gt_token.is_none() {
        sig.generics.gt_token = Some(Token![>](sig.paren_token.span.join()));
    }

    for elided in lifetimes.elided {
        sig.generics.params.push(parse_quote!(#elided));
        where_clause_or_default(&mut sig.generics.where_clause)
            .predicates
            .push(parse_quote_spanned!(elided.span()=> #elided: 'async_trait));
    }

    sig.generics.params.push(parse_quote!('async_trait));

    if has_self {
        let bounds: &[InferredBound] = if is_local {
            &[]
        } else if let Some(receiver) = sig.receiver() {
            match receiver.ty.as_ref() {
                // self: &Self
                Type::Reference(ty) if ty.mutability.is_none() => &[InferredBound::Sync],
                // self: Arc<Self>
                Type::Path(ty)
                    if {
                        let segment = ty.path.segments.last().unwrap();
                        segment.ident == "Arc"
                            && match &segment.arguments {
                                PathArguments::AngleBracketed(arguments) => {
                                    arguments.args.len() == 1
                                        && match &arguments.args[0] {
                                            GenericArgument::Type(Type::Path(arg)) => {
                                                arg.path.is_ident("Self")
                                            }
                                            _ => false,
                                        }
                                }
                                _ => false,
                            }
                    } =>
                {
                    &[InferredBound::Sync, InferredBound::Send]
                }
                _ => &[InferredBound::Send],
            }
        } else {
            &[InferredBound::Send]
        };

        let filtered: Vec<&InferredBound> = bounds
            .iter()
            .filter(|bound| match context {
                Context::Trait { supertraits, .. } => {
                    has_default && !has_bound(supertraits, bound)
                }
                Context::Impl { .. } => false,
            })
            .collect();

        // When a diagnostic helper module is available (Send trait) and there are
        // Send/Sync bounds to add, replace the raw `Send [+ Sync]` bounds with the
        // helper trait so that `#[diagnostic::on_unimplemented]` fires with a more
        // informative error message.
        let needs_sync = filtered.iter().any(|b| matches!(b, InferredBound::Sync));
        let predicate = if !filtered.is_empty() {
            if let Some(helper_mod) = send_helper {
                let helper_trait: Ident = if needs_sync {
                    format_ident!("AsyncTraitSendSync")
                } else {
                    format_ident!("AsyncTraitSend")
                };
                parse_quote! { Self: self::#helper_mod::#helper_trait + 'async_trait }
            } else {
                parse_quote! { Self: #(#filtered +)* 'async_trait }
            }
        } else {
            parse_quote! { Self: 'async_trait }
        };

        where_clause_or_default(&mut sig.generics.where_clause)
            .predicates
            .push(predicate);
    }

    for (i, arg) in sig.inputs.iter_mut().enumerate() {
        match arg {
            FnArg::Receiver(receiver) => {
                if receiver.reference.is_none() {
                    receiver.mutability = None;
                }
            }
            FnArg::Typed(arg) => {
                if match *arg.ty {
                    Type::Reference(_) => false,
                    _ => true,
                } {
                    if let Pat::Ident(pat) = &mut *arg.pat {
                        pat.by_ref = None;
                        pat.mutability = None;
                    } else {
                        let positional = positional_arg(i, &arg.pat);
                        let m = mut_pat(&mut arg.pat);
                        arg.pat = parse_quote!(#m #positional);
                    }
                }
                AddLifetimeToImplTrait.visit_type_mut(&mut arg.ty);
            }
        }
    }

    let bounds = if is_local {
        quote!('async_trait)
    } else {
        quote!(::core::marker::Send + 'async_trait)
    };

    // Determine the allocator type token to use in the Box second type parameter.
    // `None` → omit the second parameter entirely (no allocator, or ExprOnly error-recovery).
    // `Some(tok)` → emit `, tok`.
    let alloc_ty_tokens: Option<TokenStream> = alloc.and_then(|a| a.ty().map(|ty| quote!(#ty)));

    sig.output = match alloc_ty_tokens {
        None => parse_quote! {
            #ret_arrow ::core::pin::Pin<Box<
                dyn ::core::future::Future<Output = #ret> + #bounds
            >>
        },
        Some(alloc_ty) => parse_quote! {
            #ret_arrow ::core::pin::Pin<::std::boxed::Box<
                dyn ::core::future::Future<Output = #ret> + #bounds,
                #alloc_ty
            >>
        },
    };
}

// ── Block transformation ─────────────────────────────────────────────────────

// Input:
//     async fn f<T>(&self, x: &T, (a, b): (A, B)) -> Ret {
//         self + x + a + b
//     }
//
// Output (no allocator):
//     Box::pin(async move {
//         let ___ret: Ret = {
//             let __self = self;
//             let x = x;
//             let (a, b) = __arg1;
//
//             __self + x + a + b
//         };
//
//         ___ret
//     })
//
// Output (with allocator `MyAlloc => MyAlloc::new()`):
//     {
//         let __pin_allocator = MyAlloc::new();
//         Box::pin_in(async move { ... }, __pin_allocator)
//     }
fn transform_block(
    context: Context,
    sig: &mut Signature,
    block: &mut Block,
    alloc: Option<&AllocatorAttr>,
) {
    // For a Param allocator, the parameter must NOT be re-bound inside the async block
    // (it gets consumed before the block via __pin_allocator).
    let alloc_param_ident: Option<&Ident> = alloc.and_then(|a| {
        if let AllocatorSource::Param { ident, .. } = &a.source {
            Some(ident)
        } else {
            None
        }
    });

    let mut replace_self = false;
    let decls = sig
        .inputs
        .iter()
        .enumerate()
        .map(|(i, arg)| match arg {
            FnArg::Receiver(Receiver {
                self_token,
                mutability,
                ..
            }) => {
                replace_self = true;
                let ident = Ident::new("__self", self_token.span);
                quote!(let #mutability #ident = #self_token;)
            }
            FnArg::Typed(arg) => {
                // If there is a #[cfg(...)] attribute that selectively enables
                // the parameter, forward it to the variable.
                let attrs = arg.attrs.iter().filter(|attr| attr.path().is_ident("cfg"));

                if let Type::Reference(_) = *arg.ty {
                    quote!()
                } else if let Pat::Ident(PatIdent {
                    ident, mutability, ..
                }) = &*arg.pat
                {
                    // Skip re-binding the allocator parameter: it is consumed before
                    // the async block.
                    if alloc_param_ident == Some(ident) {
                        return quote!();
                    }
                    quote! {
                        #(#attrs)*
                        let #mutability #ident = #ident;
                    }
                } else {
                    let pat = &arg.pat;
                    let ident = positional_arg(i, pat);
                    if let Pat::Wild(_) = **pat {
                        quote! {
                            #(#attrs)*
                            let #ident = #ident;
                        }
                    } else {
                        quote! {
                            #(#attrs)*
                            let #pat = {
                                let #ident = #ident;
                                #ident
                            };
                        }
                    }
                }
            }
        })
        .collect::<Vec<_>>();

    if replace_self {
        ReplaceSelf.visit_block_mut(block);
    }

    let let_ret = match &mut sig.output {
        ReturnType::Default => quote! {
            #(#decls)*
            let _: () = #block;
        },
        ReturnType::Type(_, ret) => {
            if contains_associated_type_impl_trait(context, ret) {
                if decls.is_empty() {
                    let stmts = &block.stmts;
                    quote!(#(#stmts)*)
                } else {
                    quote!(#(#decls)* #block)
                }
            } else {
                let mut ret = ret.clone();
                replace_impl_trait_with_infer(&mut ret);
                quote! {
                    if let ::core::option::Option::Some(__ret) = ::core::option::Option::None::<#ret> {
                        #[allow(unreachable_code)]
                        return __ret;
                    }
                    #(#decls)*
                    let __ret: #ret = #block;
                    #[allow(unreachable_code)]
                    __ret
                }
            }
        }
    };

    let span = sig.asyncness.unwrap().span;

    let box_pin = match alloc {
        None => {
            quote_spanned!(span=> Box::pin(async move { #let_ret }))
        }
        Some(a) => {
            match a.expr_tokens() {
                None => {
                    // TypeOnly in impl context — error already reported by validate_alloc_form;
                    // emit a fallback Box::pin for IDE error recovery.
                    quote_spanned!(span=> Box::pin(async move { #let_ret }))
                }
                Some(alloc_expr) => {
                    if a.is_unsafe {
                        quote_spanned!(span=> {
                            let __pin_allocator = #alloc_expr;
                            unsafe {
                                ::core::pin::Pin::new_unchecked(
                                    ::std::boxed::Box::new_in(
                                        async move { #let_ret },
                                        __pin_allocator,
                                    )
                                )
                            }
                        })
                    } else {
                        // ExprOnly: no explicit type — skip the 'static lifetime check
                        // (the type is inferred, so we cannot inspect its lifetimes here).
                        let lifetime_error = a.ty().and_then(|alloc_ty| {
                            if has_non_static_lifetime(alloc_ty) {
                                Some(syn::Error::new(
                                    syn::spanned::Spanned::span(alloc_ty),
                                    "allocator type has a non-`'static` lifetime; \
                                     `Box::pin_in` requires `A: 'static` \
                                     — use `#[unsafe(allocator(...))]` to opt out of that \
                                     bound (you must guarantee the allocator outlives the \
                                     pinned future)",
                                ).to_compile_error())
                            } else {
                                None
                            }
                        });
                        if let Some(err) = lifetime_error {
                            err
                        } else {
                            quote_spanned!(span=> {
                                let __pin_allocator = #alloc_expr;
                                ::std::boxed::Box::pin_in(
                                    async move { #let_ret },
                                    __pin_allocator,
                                )
                            })
                        }
                    }
                }
            }
        }
    };

    block.stmts = parse_quote!(#box_pin);
}

// ── Utilities ────────────────────────────────────────────────────────────────

/// Returns `true` if `ty` contains any lifetime that is not `'static`.
///
/// Works by scanning the token stream of the type for `'ident` pairs where the
/// ident is not `"static"`. This is a syntactic check — it catches explicit
/// lifetime annotations like `BumpAlloc<'arena>` or `&'a Arena`, but not
/// implicit lifetimes on bare type names.
fn has_non_static_lifetime(ty: &Type) -> bool {
    use proc_macro2::TokenTree;
    use quote::ToTokens;

    fn scan(tokens: proc_macro2::TokenStream) -> bool {
        let mut iter = tokens.into_iter().peekable();
        while let Some(tt) = iter.next() {
            match tt {
                TokenTree::Punct(ref p) if p.as_char() == '\'' => {
                    if let Some(TokenTree::Ident(ident)) = iter.peek() {
                        if ident != "static" {
                            return true;
                        }
                    }
                }
                TokenTree::Group(g) => {
                    if scan(g.stream()) {
                        return true;
                    }
                }
                _ => {}
            }
        }
        false
    }

    let mut ts = proc_macro2::TokenStream::new();
    ty.to_tokens(&mut ts);
    scan(ts)
}

fn positional_arg(i: usize, pat: &Pat) -> Ident {
    let span = syn::spanned::Spanned::span(pat).resolved_at(Span::mixed_site());
    format_ident!("__arg{}", i, span = span)
}

fn contains_associated_type_impl_trait(context: Context, ret: &mut Type) -> bool {
    struct AssociatedTypeImplTraits<'a> {
        set: &'a Set<Ident>,
        contains: bool,
    }

    impl<'a> VisitMut for AssociatedTypeImplTraits<'a> {
        fn visit_type_path_mut(&mut self, ty: &mut TypePath) {
            if ty.qself.is_none()
                && ty.path.segments.len() == 2
                && ty.path.segments[0].ident == "Self"
                && self.set.contains(&ty.path.segments[1].ident)
            {
                self.contains = true;
            }
            visit_mut::visit_type_path_mut(self, ty);
        }
    }

    match context {
        Context::Trait { .. } => false,
        Context::Impl {
            associated_type_impl_traits,
            ..
        } => {
            let mut visit = AssociatedTypeImplTraits {
                set: associated_type_impl_traits,
                contains: false,
            };
            visit.visit_type_mut(ret);
            visit.contains
        }
    }
}

fn where_clause_or_default(clause: &mut Option<WhereClause>) -> &mut WhereClause {
    clause.get_or_insert_with(|| WhereClause {
        where_token: Default::default(),
        predicates: Punctuated::new(),
    })
}

fn replace_impl_trait_with_infer(ty: &mut Type) {
    struct ReplaceImplTraitWithInfer;

    impl VisitMut for ReplaceImplTraitWithInfer {
        fn visit_type_mut(&mut self, ty: &mut Type) {
            if let Type::ImplTrait(impl_trait) = ty {
                *ty = Type::Infer(TypeInfer {
                    underscore_token: Token![_](impl_trait.impl_token.span),
                });
            }
            visit_mut::visit_type_mut(self, ty);
        }
    }

    ReplaceImplTraitWithInfer.visit_type_mut(ty);
}
