use std::fmt::Display;

use proc_macro::TokenStream;
use proc_macro2::{Ident, Span};
use quote::{quote, ToTokens};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::token::Comma;

pub fn handler<T>(methods: &[T], mut ast: syn::ItemFn) -> TokenStream
where
    T: Display,
{
    let app_type = match ast.sig.inputs.first() {
        Some(syn::FnArg::Typed(syn::PatType { ty, .. })) => match **ty {
            syn::Type::Reference(ref reffed) => (*reffed.elem).clone(),
            _ => panic!("handler's first argument must be a reference"),
        },
        _ => panic!("handler argument lists must have &App as their first type"),
    };

    let app_type = match &app_type {
        syn::Type::Path(syn::TypePath { path, .. }) => Some(path),
        _ => None,
    }
    .and_then(|path| path.segments.first())
    .and_then(|segment| match &segment.ident {
        id if id == "Arc" => Some(&segment.arguments),
        _ => None,
    })
    .and_then(|args| match args {
        syn::PathArguments::AngleBracketed(inner) => Some(inner),
        _ => None,
    })
    .and_then(|args| match args.args.first() {
        Some(syn::GenericArgument::Type(ty)) => Some(ty.clone()),
        _ => None,
    })
    .unwrap_or(app_type);

    let mut method_patterns = proc_macro2::TokenStream::new();
    for (i, method) in methods.iter().enumerate() {
        let method = Ident::new(&method.to_string().to_ascii_uppercase(), Span::call_site());
        method_patterns.extend(if i > 0 {
            quote!( | &mendes::http::Method::#method)
        } else {
            quote!(&mendes::http::Method::#method)
        });
    }

    let mut done = false;
    let mut prefix = proc_macro2::TokenStream::new();
    let mut args = proc_macro2::TokenStream::new();
    for (i, arg) in ast.sig.inputs.iter_mut().enumerate() {
        let typed = match arg {
            syn::FnArg::Typed(typed) => typed,
            _ => panic!("did not expect receiver argument in handler"),
        };

        let mut special = false;
        let (pat, ty) = (&*typed.pat, &typed.ty);
        typed.attrs.retain(|attr| {
            if attr.path.is_ident("rest") {
                prefix.extend(quote!(
                    let #pat = <mendes::application::Rest<#ty> as mendes::FromContext<#app_type>>::from_context(
                        &cx.app, &cx.req, &mut cx.path, &mut cx.body,
                    )?.0;
                ));
                args.extend(quote!(#pat,));
                done = true;
                special = true;
                false
            } else if attr.path.is_ident("query") {
                prefix.extend(quote!(
                    let #pat = <mendes::application::Query<#ty> as mendes::FromContext<#app_type>>::from_context(
                        &cx.app, &cx.req, &mut cx.path, &mut cx.body,
                    )?.0;
                ));
                args.extend(quote!(#pat,));
                special = true;
                false
            } else {
                true
            }
        });

        if special {
            continue;
        } else if done {
            panic!("more arguments after #[rest] not allowed");
        }

        let name = match pat {
            syn::Pat::Wild(_) => syn::Pat::Ident(syn::PatIdent {
                ident: Ident::new(&format!("_{}", i), Span::call_site()),
                attrs: Vec::new(),
                mutability: None,
                subpat: None,
                by_ref: None,
            }),
            _ => pat.clone(),
        };

        prefix.extend(quote!(
            let #name = <#ty as mendes::FromContext<#app_type>>::from_context(
                &cx.app, &cx.req, &mut cx.path, &mut cx.body,
            )?;
        ));
        args.extend(quote!(#name,));
    }

    let name = ast.sig.ident.clone();
    let orig_vis = ast.vis.clone();
    ast.vis = nested_visibility(ast.vis);

    let handler = {
        let nested_vis = &ast.vis;
        let generics = &ast.sig.generics;
        let rtype = &ast.sig.output;
        let where_clause = &ast.sig.generics.where_clause;
        quote!(
            #nested_vis async fn handler#generics(
                cx: &mut mendes::application::Context<#app_type>
            ) #rtype #where_clause {
                match &cx.req.method {
                    #method_patterns => {}
                    _ => return Err(mendes::Error::MethodNotAllowed.into()),
                }
                #prefix
                call(#args).await
            }
        )
    };

    let call = {
        ast.sig.ident = Ident::new("call", Span::call_site());
        quote!(#ast)
    };

    quote!(#orig_vis mod #name {
        use super::*;
        #handler
        #call
    })
    .into()
}

fn nested_visibility(vis: syn::Visibility) -> syn::Visibility {
    match vis {
        cur @ syn::Visibility::Crate(_) | cur @ syn::Visibility::Public(_) => cur,
        syn::Visibility::Inherited => visibility("super"),
        cur @ syn::Visibility::Restricted(_) => {
            let inner = match &cur {
                syn::Visibility::Restricted(inner) => inner,
                _ => unreachable!(),
            };

            if inner.path.is_ident("self") {
                visibility("super")
            } else if inner.path.is_ident("super") {
                visibility("super::super")
            } else {
                cur
            }
        }
    }
}

fn visibility(path: &str) -> syn::Visibility {
    syn::Visibility::Restricted(syn::VisRestricted {
        pub_token: syn::Token![pub](Span::call_site()),
        paren_token: syn::token::Paren {
            span: Span::call_site(),
        },
        in_token: match path {
            "self" | "crate" | "super" => None,
            _ => Some(syn::Token![in](Span::call_site())),
        },
        path: Box::new(Ident::new(path, Span::call_site()).into()),
    })
}

pub fn route(mut ast: syn::ItemFn, root: bool) -> TokenStream {
    let (sig, stmts) = match ast.block.stmts.get_mut(0) {
        // If the first statement is an function, we'll assume this came from  #[async_trait]
        Some(syn::Stmt::Item(syn::Item::Fn(inner))) => (&inner.sig, &mut inner.block.stmts),
        _ => (&ast.sig, &mut ast.block.stmts),
    };

    let (idx, target, routes) = stmts
        .iter_mut()
        .enumerate()
        .find_map(|(i, stmt)| {
            let site = match stmt {
                syn::Stmt::Local(local) => local
                    .init
                    .as_mut()
                    .map(|init| MacroSite::Expr(&mut *init.1)),
                syn::Stmt::Expr(expr) => Some(MacroSite::Expr(expr)),
                syn::Stmt::Item(_) => Some(MacroSite::ItemStmt(stmt)),
                _ => None,
            }?;

            let mac = match site {
                MacroSite::Expr(syn::Expr::Macro(mac)) => &mut mac.mac,
                MacroSite::ItemStmt(syn::Stmt::Item(syn::Item::Macro(mac))) => &mut mac.mac,
                _ => return None,
            };

            let path = &mac.path;
            if !path.is_ident("path") && !path.is_ident("method") {
                return None;
            }

            let routes = Target::from_macro(&mac);
            Some((i, site, routes))
        })
        .expect("did not find 'path' or 'method' macro");

    let expr = syn::parse::<syn::Expr>(quote!(#routes).into()).unwrap();
    match target {
        MacroSite::Expr(dst) => {
            *dst = expr;
        }
        MacroSite::ItemStmt(stmt) => {
            *stmt = syn::Stmt::Expr(expr);
        }
    }

    let self_name = argument_name(sig, 0);
    let req_name = argument_name(sig, 1);
    if root {
        let self_name = self_name.unwrap();
        let req_name = req_name.unwrap();

        let prefix = syn::parse::<syn::Block>(
            quote!({
                use mendes::Application;
                use mendes::application::Responder;
                let app = #self_name.clone();
                let mut cx = mendes::Context::new(#self_name, #req_name);
            })
            .into(),
        )
        .unwrap();

        stmts.splice(idx..idx, prefix.stmts.into_iter());
        return ast.to_token_stream().into();
    }

    let cx_name = self_name.unwrap();
    let prefix = syn::parse::<syn::Block>(
        quote!({
            use mendes::Application;
            use mendes::application::Responder;
            let mut cx = #cx_name;
            let app = cx.app.clone();
        })
        .into(),
    )
    .unwrap();
    stmts.splice(idx..idx, prefix.stmts.into_iter());

    let name = ast.sig.ident.clone();
    let orig_vis = ast.vis.clone();
    ast.vis = nested_visibility(ast.vis);

    ast.sig.ident = Ident::new("handler", Span::call_site());
    quote!(#orig_vis mod #name {
        use super::*;
        #ast
    })
    .into()
}

enum MacroSite<'a> {
    Expr(&'a mut syn::Expr),
    ItemStmt(&'a mut syn::Stmt),
}

fn argument_name(sig: &syn::Signature, i: usize) -> Option<&syn::Ident> {
    let pat = match sig.inputs.iter().nth(i)? {
        syn::FnArg::Typed(arg) => &arg.pat,
        _ => return None,
    };

    match &**pat {
        syn::Pat::Ident(ident) => Some(&ident.ident),
        _ => None,
    }
}

#[allow(clippy::large_enum_variant)]
enum Target {
    Direct(syn::Expr),
    PathMap(PathMap),
    MethodMap(MethodMap),
}

impl Target {
    fn from_expr(expr: syn::Expr) -> Self {
        let mac = match expr {
            syn::Expr::Macro(mac) => mac,
            _ => return Target::Direct(expr),
        };

        Self::from_macro(&mac.mac)
    }

    fn from_macro(mac: &syn::Macro) -> Self {
        if mac.path.is_ident("path") {
            Target::PathMap(mac.parse_body().unwrap())
        } else if mac.path.is_ident("method") {
            Target::MethodMap(mac.parse_body().unwrap())
        } else {
            panic!("unknown macro used as dispatch target")
        }
    }
}

impl Parse for Target {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        Ok(Target::from_expr(input.parse::<syn::Expr>()?))
    }
}

impl quote::ToTokens for Target {
    fn to_tokens(&self, tokens: &mut proc_macro2::TokenStream) {
        match self {
            Target::Direct(expr) => quote!(
                #expr::handler(&mut cx).await.into_response(&*app, &cx.req)
            )
            .to_tokens(tokens),
            Target::MethodMap(map) => map.to_tokens(tokens),
            Target::PathMap(map) => map.to_tokens(tokens),
        }
    }
}

struct PathMap {
    routes: Vec<(Vec<syn::Attribute>, syn::Pat, Target)>,
}

impl Parse for PathMap {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut routes = vec![];
        while !input.is_empty() {
            if !routes.is_empty() {
                let _ = input.parse::<syn::Token![,]>();
                if input.is_empty() {
                    break;
                }
            }

            let attrs = input.call(syn::Attribute::parse_outer)?;
            let component = input.parse()?;
            input.parse::<syn::Token![=>]>()?;
            let target = input.parse()?;
            routes.push((attrs, component, target));
        }
        Ok(PathMap { routes })
    }
}

impl quote::ToTokens for PathMap {
    fn to_tokens(&self, tokens: &mut proc_macro2::TokenStream) {
        let mut route_tokens = proc_macro2::TokenStream::new();
        let mut wildcard = false;
        for (attrs, component, target) in self.routes.iter() {
            let mut rewind = false;
            if let syn::Pat::Wild(_) = component {
                wildcard = true;
                rewind = true;
            }

            attrs
                .iter()
                .for_each(|attr| attr.to_tokens(&mut route_tokens));
            if rewind {
                quote!(
                    #component => {
                        cx.rewind();
                        #target
                    }
                )
                .to_tokens(&mut route_tokens);
            } else {
                quote!(#component => #target,).to_tokens(&mut route_tokens);
            }
        }

        if !wildcard {
            route_tokens.extend(quote!(
                _ => ::mendes::Error::PathNotFound.into_response(&*app, &cx.req),
            ));
        }

        tokens.extend(quote!(match cx.next_path().as_deref() {
            #route_tokens
        }));
    }
}

struct MethodMap {
    routes: Vec<(Vec<syn::Attribute>, syn::Ident, Target)>,
}

impl Parse for MethodMap {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut routes = vec![];
        while !input.is_empty() {
            if !routes.is_empty() {
                let _ = input.parse::<syn::Token![,]>();
                if input.is_empty() {
                    break;
                }
            }

            let attrs = input.call(syn::Attribute::parse_outer)?;
            let component = input.parse()?;
            input.parse::<syn::Token![=>]>()?;
            let target = input.parse()?;
            routes.push((attrs, component, target));
        }
        Ok(MethodMap { routes })
    }
}

impl quote::ToTokens for MethodMap {
    fn to_tokens(&self, tokens: &mut proc_macro2::TokenStream) {
        let mut route_tokens = proc_macro2::TokenStream::new();
        let mut wildcard = false;
        for (attrs, component, target) in self.routes.iter() {
            if component == "_" {
                wildcard = true;
            }

            attrs
                .iter()
                .for_each(|attr| attr.to_tokens(&mut route_tokens));
            quote!(mendes::http::Method::#component => #target,).to_tokens(&mut route_tokens);
        }

        if !wildcard {
            route_tokens.extend(quote!(
                _ => ::mendes::Error::MethodNotAllowed.into_response(&*app, &cx.req),
            ));
        }

        tokens.extend(quote!(match cx.req.method {
            #route_tokens
        }));
    }
}

pub struct HandlerMethods {
    pub methods: Vec<syn::Ident>,
}

impl Parse for HandlerMethods {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let methods = Punctuated::<syn::Ident, Comma>::parse_terminated(input)?;
        Ok(Self {
            methods: methods.into_iter().collect(),
        })
    }
}
