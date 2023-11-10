use proc_macro::TokenStream;
use quote::quote;
use syn;

#[proc_macro_derive(SoapBody)]
pub fn derive_soap_boady_fn(input: TokenStream) -> TokenStream {
    // Construct a representation of Rust code as a syntax tree
    // that we can manipulate
    let ast = syn::parse(input).unwrap();

    // Build the trait implementation
    impl_derive_soap_body(&ast)
}

#[proc_macro_derive(SoapHeader)]
pub fn derive_soap_header_fn(input: TokenStream) -> TokenStream {
    // Construct a representation of Rust code as a syntax tree
    // that we can manipulate
    let ast = syn::parse(input).unwrap();

    // Build the trait implementation
    impl_derive_soap_header(&ast)
}

fn impl_derive_soap_header(ast: &syn::DeriveInput) -> TokenStream {
    let struct_name = &ast.ident;
    
    let gen = quote! {
        impl std::convert::TryFrom<soap_router::router::SoapRequest> for #struct_name {
            type Error = soap_router::fault::SoapFault;

            fn try_from(value: soap_router::router::SoapRequest) -> std::Result<Self, Self::Error> {
                let buf = vec![].writer();
                value.body.write(buf.by_ref()).unwrap();
                let buf = buf.into_inner();
                yaserde::de::from_reader(buf)
            }
        }

        impl std::convert::Into<soap_router::router::SoapMessage> for #struct_name {
            fn into(self) -> soap_router::router::SoapMessage {
                let buf = vec![].writer();
                let buf = yaserde::ser::serialize_with_writer(&value, buf, Default::default()).unwrap();
                let buf = buf.into_inner();
                let elem = xmltree::Element::parse(buf).unwrap();
                let mut msg = soap_router::router::SoapMessage::new();
                msg.get_mut_headers().children.push(xmltree::XMLNode::Element(elem));
                msg
            }
        }
    };
    gen.into()
}

fn impl_derive_soap_body(ast: &syn::DeriveInput) -> TokenStream {
    let struct_name = &ast.ident;
    
    let gen = quote! {
        impl std::convert::TryFrom<soap_router::router::SoapRequest> for #struct_name {
            type Error = soap_router::fault::SoapFault;

            fn try_from(value: soap_router::router::SoapRequest) -> std::Result<Self, Self::Error> {
                let buf = vec![].writer();
                value.body.write(buf.by_ref()).unwrap();
                let buf = buf.into_inner();
                yaserde::de::from_reader(buf)
            }
        }

        impl std::convert::Into<soap_router::router::SoapMessage> for #struct_name {
            fn into(self) -> soap_router::router::SoapMessage {
                let buf = vec![].writer();
                let buf = yaserde::ser::serialize_with_writer(&value, buf, Default::default()).unwrap();
                let buf = buf.into_inner();
                let elem = xmltree::Element::parse(buf).unwrap();
                let mut msg = soap_router::router::SoapMessage::new();
                msg.get_mut_body().children.push(xmltree::XMLNode::Element(elem));
                msg
            }
        }
    };
    gen.into()
}
