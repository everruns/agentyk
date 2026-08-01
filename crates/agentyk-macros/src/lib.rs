//! Procedural macros for Agentyk's application-facing API.

use proc_macro::TokenStream;
use quote::quote;
use syn::{
    FnArg, Ident, ItemFn, LitStr, Pat, ReturnType, Token, Type, parse::Parse, parse::ParseStream,
    parse_macro_input,
};

struct ToolArgs {
    description: LitStr,
    name: Option<LitStr>,
}

impl Parse for ToolArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut description = None;
        let mut name = None;

        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            let value: LitStr = input.parse()?;
            match key.to_string().as_str() {
                "description" => description = Some(value),
                "name" => name = Some(value),
                _ => {
                    return Err(syn::Error::new(
                        key.span(),
                        "expected `description` or `name`",
                    ));
                }
            }
            if input.is_empty() {
                break;
            }
            input.parse::<Token![,]>()?;
        }

        Ok(Self {
            description: description
                .ok_or_else(|| syn::Error::new(input.span(), "missing `description = \"...\"`"))?,
            name,
        })
    }
}

/// Turn an async function into a value implementing `agentyk::Tool`.
///
/// Every ordinary parameter becomes a property in the generated JSON schema
/// and must implement `serde::de::DeserializeOwned` and
/// `schemars::JsonSchema`. `Option<T>` parameters may be omitted. A final
/// `&ToolContext` parameter is optional and receives the execution context
/// without appearing in the schema.
#[proc_macro_attribute]
pub fn tool(args: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(args as ToolArgs);
    let function = parse_macro_input!(item as ItemFn);
    expand_tool(args, function)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand_tool(args: ToolArgs, function: ItemFn) -> syn::Result<proc_macro2::TokenStream> {
    if function.sig.asyncness.is_none() {
        return Err(syn::Error::new_spanned(
            function.sig.fn_token,
            "an Agentyk tool must be an async function",
        ));
    }
    if !function.sig.generics.params.is_empty() || function.sig.generics.where_clause.is_some() {
        return Err(syn::Error::new_spanned(
            &function.sig.generics,
            "generic tool functions are not supported",
        ));
    }
    if matches!(function.sig.output, ReturnType::Default) {
        return Err(syn::Error::new_spanned(
            &function.sig,
            "an Agentyk tool must return ToolOutput",
        ));
    }

    let attributes = &function.attrs;
    let impl_attributes: Vec<_> = function
        .attrs
        .iter()
        .filter(|attribute| {
            attribute.path().is_ident("cfg")
                || attribute.path().is_ident("cfg_attr")
                || attribute.path().is_ident("allow")
                || attribute.path().is_ident("expect")
                || attribute.path().is_ident("warn")
                || attribute.path().is_ident("deny")
                || attribute.path().is_ident("forbid")
        })
        .collect();
    let visibility = &function.vis;
    let function_name = &function.sig.ident;
    let tool_name = args
        .name
        .unwrap_or_else(|| LitStr::new(&function_name.to_string(), function_name.span()));
    let description = args.description;
    let body = &function.block;

    let mut argument_names = Vec::new();
    let mut argument_types = Vec::new();
    let mut context_name = None;

    for (index, input) in function.sig.inputs.iter().enumerate() {
        let FnArg::Typed(argument) = input else {
            return Err(syn::Error::new_spanned(
                input,
                "tool functions cannot take `self`",
            ));
        };
        if is_tool_context(&argument.ty) {
            if index + 1 != function.sig.inputs.len() {
                return Err(syn::Error::new_spanned(
                    input,
                    "`&ToolContext` must be the final parameter",
                ));
            }
            let Pat::Ident(pattern) = argument.pat.as_ref() else {
                return Err(syn::Error::new_spanned(
                    &argument.pat,
                    "the ToolContext parameter must be a simple identifier",
                ));
            };
            context_name = Some(pattern.ident.clone());
            continue;
        }
        let Pat::Ident(pattern) = argument.pat.as_ref() else {
            return Err(syn::Error::new_spanned(
                &argument.pat,
                "tool parameters must be simple identifiers",
            ));
        };
        argument_names.push(pattern.ident.clone());
        argument_types.push((*argument.ty).clone());
    }

    let property_names: Vec<LitStr> = argument_names
        .iter()
        .map(|name| LitStr::new(&name.to_string(), name.span()))
        .collect();
    let deserialize = argument_names
        .iter()
        .zip(argument_types.iter())
        .zip(property_names.iter())
        .map(|((name, ty), property)| {
            if is_option(ty) {
                quote! {
                    let #name: #ty = match ::agentyk::__private::serde_json::from_value(
                        arguments.get(#property).cloned().unwrap_or(
                            ::agentyk::__private::serde_json::Value::Null,
                        ),
                    ) {
                        Ok(value) => value,
                        Err(error) => return ::agentyk::ToolOutput::error(
                            format!("invalid argument `{}`: {error}", #property),
                        ),
                    };
                }
            } else {
                quote! {
                    let #name: #ty = match arguments.get(#property).cloned() {
                        Some(value) => match ::agentyk::__private::serde_json::from_value(value) {
                            Ok(value) => value,
                            Err(error) => return ::agentyk::ToolOutput::error(
                                format!("invalid argument `{}`: {error}", #property),
                            ),
                        },
                        None => return ::agentyk::ToolOutput::error(
                            format!("missing required argument `{}`", #property),
                        ),
                    };
                }
            }
        });
    let context_binding = context_name.map(|name| quote!(let #name = context;));
    let schema_properties =
        argument_types
            .iter()
            .zip(property_names.iter())
            .map(|(ty, property)| {
                quote! {
                    properties.insert(
                        #property.to_owned(),
                        ::agentyk::__private::serde_json::to_value(
                            generator.subschema_for::<#ty>(),
                        ).expect("schemars always produces serializable schemas"),
                    );
                }
            });
    let required = argument_types
        .iter()
        .zip(property_names.iter())
        .filter(|(ty, _)| !is_option(ty))
        .map(|(_, property)| quote!(#property));
    Ok(quote! {
        #(#attributes)*
        #[doc = #description]
        #[allow(non_camel_case_types)]
        #visibility struct #function_name;

        #(#impl_attributes)*
        #[::agentyk::__private::async_trait]
        impl ::agentyk::Tool for #function_name {
            fn definition(&self) -> ::agentyk::ToolDefinition {
                let mut generator = ::agentyk::__private::schemars::SchemaGenerator::default();
                let mut properties = ::agentyk::__private::serde_json::Map::new();
                #(#schema_properties)*
                let definitions = generator.take_definitions(true);
                let mut parameters = ::agentyk::__private::serde_json::json!({
                    "type": "object",
                    "properties": properties,
                    "required": [#(#required),*],
                    "additionalProperties": false,
                });
                if !definitions.is_empty() {
                    parameters.as_object_mut()
                        .expect("tool parameters are always an object")
                        .insert(
                            "$defs".to_owned(),
                            ::agentyk::__private::serde_json::Value::Object(definitions),
                        );
                }
                ::agentyk::ToolDefinition::new(
                    #tool_name,
                    #description,
                    parameters,
                )
            }

            async fn execute(
                &self,
                arguments: ::agentyk::__private::serde_json::Value,
                context: &::agentyk::ToolContext,
            ) -> ::agentyk::ToolOutput {
                #(#deserialize)*
                #context_binding
                #body
            }
        }
    })
}

fn is_tool_context(ty: &Type) -> bool {
    let Type::Reference(reference) = ty else {
        return false;
    };
    let Type::Path(path) = reference.elem.as_ref() else {
        return false;
    };
    path.path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "ToolContext")
}

fn is_option(ty: &Type) -> bool {
    let Type::Path(path) = ty else {
        return false;
    };
    path.path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "Option")
}
