use std::{
    collections::{HashMap, HashSet},
    io::Write,
};

use axum::response::IntoResponse;
use bytes::BufMut;
use url::Url;
use xmltree::Element;

use crate::router::SoapMessage;

#[derive(Debug)]
pub struct SoapFault {
    code: SoapFaultCode,
    sub_codes: Vec<(url::Url, String)>,
    reason: HashMap<isolang::Language, String>,
    detail: Option<xmltree::Element>,
}

#[derive(strum_macros::Display, Debug)]
pub enum SoapFaultCode {
    VersionMismatch,
    MustUnderstand,
    DataEncodingUnknown,
    Sender,
    Receiver,
}

impl std::error::Error for SoapFault {}

impl SoapFault {
    pub fn new(
        code: SoapFaultCode,
        sub_codes: Vec<(url::Url, String)>,
        reason: HashMap<isolang::Language, String>,
        detail: Option<xmltree::Element>,
    ) -> Self {
        // Reason should not be empty
        if reason.is_empty() {
            panic!("Given an empty Soap Fault Reason")
        }

        Self {
            code,
            sub_codes,
            reason,
            detail,
        }
    }
}

#[derive(Default)]
struct PrefixGenerator {
    prev: Vec<u8>,
}

impl PrefixGenerator {
    fn next(&mut self) -> String {
        let mut to_add = vec![];
        while let Some(last) = self.prev.pop() {
            if last == b'z' {
                to_add.push(b'a');
            } else {
                self.prev.push(last + 1);
                break;
            }
        }
        if self.prev.is_empty() {
            self.prev.push(b'a');
        }
        self.prev.extend(to_add);
        String::from_utf8(self.prev.clone()).unwrap()
    }
}

impl From<SoapFault> for SoapMessage {
    fn from(val: SoapFault) -> SoapMessage {
        let mut env = Element::new("Enveloppe");
        env.namespace = Some("http://www.w3.org/2003/05/soap-envelope".to_string());
        let mut namespaces = xmltree::Namespace::empty();
        namespaces.put("xml", "http://www.w3.org/XML/1998/namespace");
        namespaces.put("env", "http://www.w3.org/2003/05/soap-envelope");
        let mut pfgen = PrefixGenerator::default();
        let code_namespaces: HashMap<Url, String> = val
            .sub_codes
            .iter()
            .map(|(u, _)| u.clone())
            .collect::<HashSet<url::Url>>()
            .into_iter()
            .map(|u| (u, pfgen.next()))
            .collect();

        code_namespaces.iter().for_each(|(uri, prefix)| {
            namespaces.put(prefix, uri.as_str());
        });

        env.namespaces = Some(namespaces);

        let mut body = Element::new("Body");
        body.prefix = Some("env".to_string());

        let mut fault = Element::new("Fault");
        fault.prefix = Some("env".to_string());

        let mut code = Element::new("Fault");
        code.prefix = Some("env".to_string());

        let mut value = Element::new("Value");
        value.prefix = Some("env".to_string());
        value
            .children
            .push(xmltree::XMLNode::Text(format!("env:{}", val.code)));
        code.children.push(xmltree::XMLNode::Element(value));

        let code = val
            .sub_codes
            .into_iter()
            .rev()
            .fold(code, |acc, (ns, val)| {
                let mut subcode = Element::new("Subcode");
                subcode.prefix = Some("env".to_string());
                let mut value = Element::new("Value");
                value.prefix = Some("env".to_string());
                value.children.push(xmltree::XMLNode::Text(format!(
                    "{}:{}",
                    code_namespaces.get(&ns).as_ref().unwrap(),
                    val
                )));
                subcode.children.push(xmltree::XMLNode::Element(value));
                subcode.children.push(xmltree::XMLNode::Element(acc));
                subcode
            });

        fault.children.push(xmltree::XMLNode::Element(code));

        let mut reason = Element::new("Reason");
        reason.prefix = Some("env".to_string());
        let reason = val.reason.into_iter().fold(reason, |mut acc, (ln, val)| {
            let mut text = Element::new("Text");
            text.prefix = Some("env".to_string());
            text.attributes
                .insert("xml:lang".to_string(), ln.to_639_3().to_string());
            text.children.push(xmltree::XMLNode::Text(val));
            acc.children.push(xmltree::XMLNode::Element(text));
            acc
        });

        fault.children.push(xmltree::XMLNode::Element(reason));

        if let Some(det) = val.detail {
            fault.children.push(xmltree::XMLNode::Element(det));
        }
        body.children.push(xmltree::XMLNode::Element(fault));
        env.children.push(xmltree::XMLNode::Element(body));
        SoapMessage(env)
    }
}

impl std::fmt::Display for SoapFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "SOAP Fault <{}.>: {}",
            self.code,
            self.reason
                .get(&isolang::Language::Eng)
                .unwrap_or(self.reason.values().next().unwrap())
        ))
    }
}

impl IntoResponse for SoapFault {
    fn into_response(self) -> axum::response::Response {
        let xml_body = Into::<SoapMessage>::into(self).0;
        let mut buf = vec![].writer();
        xml_body.write(buf.by_ref()).unwrap();
        buf.into_inner().into_response()
    }
}
