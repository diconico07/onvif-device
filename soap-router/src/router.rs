use std::{collections::HashMap, convert::Infallible, future::Future, io::Write, pin::Pin};

use axum::{
    body::{boxed, Body, Bytes},
    extract::FromRequest,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::BufMut;
use futures::{stream::FuturesOrdered, StreamExt};
use tower_service::Service;
use xmltree::Element;

use crate::fault::SoapFault;

pub struct SoapRequest {
    pub headers: xmltree::Element,
    pub body: xmltree::Element,
}
pub struct SoapMessage(pub xmltree::Element);

impl Default for SoapMessage {
    fn default() -> Self {
        Self::new()
    }
}

impl SoapMessage {
    pub fn new() -> Self {
        let mut env = Element::new("Enveloppe");
        env.namespace = Some("http://www.w3.org/2003/05/soap-envelope".to_string());
        let mut namespaces = xmltree::Namespace::empty();
        namespaces.put("xml", "http://www.w3.org/XML/1998/namespace");
        namespaces.put("env", "http://www.w3.org/2003/05/soap-envelope");
        env.namespaces = Some(namespaces);
        let mut body = Element::new("Body");
        body.prefix = Some("env".to_string());
        env.children.push(xmltree::XMLNode::Element(body));
        Self(env)
    }

    pub fn get_body(&self) -> &xmltree::Element {
        self.0
            .get_child(("Body", "http://www.w3.org/2003/05/soap-envelope"))
            .unwrap()
    }

    pub fn get_headers(&self) -> Option<&xmltree::Element> {
        self.0
            .get_child(("Header", "http://www.w3.org/2003/05/soap-envelope"))
    }

    pub fn get_mut_body(&mut self) -> &mut xmltree::Element {
        self.0
            .get_mut_child(("Body", "http://www.w3.org/2003/05/soap-envelope"))
            .unwrap()
    }

    pub fn get_mut_headers(&mut self) -> &mut xmltree::Element {
        if self.get_headers().is_none() {
            let mut h = Element::new("Headers");
            h.prefix = Some("env".to_string());
            self.0.children.insert(0, xmltree::XMLNode::Element(h));
        }
        self.0
            .get_mut_child(("Header", "http://www.w3.org/2003/05/soap-envelope"))
            .unwrap()
    }
}

impl From<xmltree::Element> for SoapMessage {
    fn from(value: xmltree::Element) -> Self {
        Self(value)
    }
}

impl From<SoapMessage> for xmltree::Element {
    fn from(val: SoapMessage) -> xmltree::Element {
        val.0
    }
}

impl<Y, Z> From<(Y, Z)> for SoapMessage
where
    Y: Into<SoapMessage>,
    Z: Into<SoapMessage>,
{
    fn from(val: (Y, Z)) -> SoapMessage {
        SoapMessage(merge_soap_enveloppe(val.0.into().0, val.1.into().0))
    }
}

type BoxedSoapFuture = Pin<Box<dyn Future<Output = Result<SoapMessage, SoapFault>> + Send>>;
type BoxedSoapHandlerService = tower::util::BoxCloneService<SoapRequest, SoapMessage, SoapFault>;

#[derive(Clone)]
struct SoapHandlerService<S, H>
where
    H: SoapHandler<S>,
{
    state: S,
    handler: H,
}

impl<S, H> SoapHandlerService<S, H>
where
    H: SoapHandler<S>,
{
    fn new(handler: H, state: S) -> Self {
        Self { state, handler }
    }
}

impl<S, H> Service<SoapRequest> for SoapHandlerService<S, H>
where
    H: SoapHandler<S>,
    S: Clone,
{
    type Error = SoapFault;
    type Response = SoapMessage;

    type Future = BoxedSoapFuture;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        Ok(()).into()
    }

    fn call(&mut self, req: SoapRequest) -> Self::Future {
        self.handler.clone().call(&req, self.state.clone())
    }
}

pub trait SoapHandler<S>: 'static + Send + Sync + Clone {
    fn call(self, req: &SoapRequest, state: S) -> BoxedSoapFuture;
}

impl<F, Fut, Res, S> SoapHandler<S> for F
where
    F: FnOnce() -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<Res, SoapFault>> + Send,
    Res: Into<SoapMessage>,
{
    fn call(self, _req: &SoapRequest, _state: S) -> BoxedSoapFuture {
        Box::pin(async move { Ok(self().await?.into()) })
    }
}

#[derive(Clone)]
pub struct SoapRouter<S>
where
    S: Send + Sync + 'static,
{
    state: S,
    routes: HashMap<(String, String), BoxedSoapHandlerService>,
}

impl<S> SoapRouter<S>
where
    S: Clone + Send + Sync,
{
    pub fn new(state: S) -> Self {
        SoapRouter {
            state,
            routes: HashMap::default(),
        }
    }

    pub fn add_operation<H>(mut self, namespace: String, element_name: String, handler: H) -> Self
    where
        H: SoapHandler<S> + 'static + Send + Sync,
        S: Send + Sync + 'static,
    {
        self.routes.insert(
            (namespace, element_name),
            BoxedSoapHandlerService::new(SoapHandlerService::new(handler, self.state.clone())),
        );
        self
    }

    async fn parse_request(&self, req: Request<Body>) -> Result<SoapMessage, String> {
        let state = self.state.clone();
        let body = Bytes::from_request(req, &state).await.unwrap();
        let xml_body = xmltree::Element::parse(body.as_ref()).unwrap();
        if xml_body.name != "Envelope"
            && xml_body.namespace != Some("http://www.w3.org/2003/05/soap-envelope".to_string())
        {
            return Err("Not a SOAP message".to_string());
        }
        if xml_body
            .get_child(("Body", "http://www.w3.org/2003/05/soap-envelope"))
            .is_none()
        {
            return Err("Malformed SOAP Message".to_string());
        }
        Ok(xml_body.into())
    }

    async fn call_internal(&self, req: Request<Body>) -> Result<Response, Infallible> {
        let soap_req = match self.parse_request(req).await {
            Ok(r) => r,
            Err(_) => {
                let body = boxed(Body::default());
                return Ok(Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(body)
                    .unwrap());
            }
        };
        let soap_body = soap_req.get_body();
        let soap_headers = match soap_req.get_headers() {
            None => {
                let mut e = Element::new("Header");
                e.namespace = Some("http://www.w3.org/2003/05/soap-envelope".to_string());
                e
            }
            Some(h) => h.clone(),
        };
        let mut fut = FuturesOrdered::new();
        for elem in soap_body.children.iter() {
            let elem = elem.as_element();
            if elem.is_none() {
                continue;
            }
            let elem = elem.unwrap();
            if let Some(handler) = self.routes.get(&(
                elem.namespace.clone().unwrap_or_default(),
                elem.name.clone(),
            )) {
                fut.push_back(handler.clone().call(SoapRequest {
                    headers: soap_headers.clone(),
                    body: elem.clone(),
                }));
            }
        }
        if fut.is_empty() {
            // Handle operations not found
            todo!()
        }
        let soap_reponses: Vec<xmltree::Element> = fut.map(|e| e.unwrap().0).collect().await;

        let merged_response = soap_reponses
            .into_iter()
            .reduce(merge_soap_enveloppe)
            .unwrap();

        let mut buf = vec![].writer();
        merged_response.write(buf.by_ref()).unwrap();
        Ok(buf.into_inner().into_response())
    }
}

fn merge_soap_enveloppe(mut accumulator: Element, element: Element) -> Element {
    for child in element.children {
        match child.as_element() {
            None => accumulator.children.push(child),
            Some(e) => {
                let acc_child = match e.namespace.clone() {
                    None => accumulator.get_mut_child(e.name.clone()),
                    Some(n) => accumulator.get_mut_child((e.name.clone(), n)),
                };
                match acc_child {
                    None => accumulator.children.push(child),
                    Some(a) => {
                        if a.attributes == e.attributes {
                            a.children.extend(e.children.iter().cloned())
                        } else {
                            accumulator.children.push(child);
                        }
                    }
                }
            }
        }
    }
    accumulator
}

impl<S> Service<Request<Body>> for SoapRouter<S>
where
    S: Clone + Send + Sync + 'static,
{
    type Response = Response;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let cs = self.clone();
        Box::pin(async move { cs.call_internal(req).await })
    }

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        Ok(()).into()
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn test_merge_xml() {
        let xml1_raw = r#"<?xml version="1.0" ?>
        <soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope" xmlns:m="http://www.example.org">
            <soap:Header>
            </soap:Header>
            <soap:Body>
                <m:GetStockPrice>
                    <m:StockName>T</m:StockName>
                </m:GetStockPrice>
            </soap:Body>
        </soap:Envelope>
        "#;

        let xml2_raw = r#"<?xml version="1.0" ?>
        <soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope" xmlns:m="http://www.example.org">
            <soap:Header>
            </soap:Header>
            <soap:Body>
                <m:GetStockPrice>
                    <m:StockName>Y</m:StockName>
                </m:GetStockPrice>
            </soap:Body>
        </soap:Envelope>
        "#;
        let expected_raw = r#"<?xml version="1.0"?>
        <soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope" xmlns:m="http://www.example.org">
            <soap:Header>
            </soap:Header>
            <soap:Body>
                <m:GetStockPrice>
                    <m:StockName>T</m:StockName>
                </m:GetStockPrice>
                <m:GetStockPrice>
                    <m:StockName>Y</m:StockName>
                </m:GetStockPrice>
            </soap:Body>
        </soap:Envelope>
        "#;

        let xml1 = Element::parse(xml1_raw.as_bytes()).unwrap();
        let xml2 = Element::parse(xml2_raw.as_bytes()).unwrap();
        let expected = Element::parse(expected_raw.as_bytes()).unwrap();

        assert_eq!(merge_soap_enveloppe(xml1, xml2), expected)
    }

    #[tokio::test]
    async fn test_router() {
        let mut router = SoapRouter::new(())
            .add_operation("http://www.example.org".to_string(), "GetStockPrice".to_string(), || { async move { 
                let xml1_raw = r#"<?xml version="1.0" ?>
        <soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope" xmlns:m="http://www.example.org">
            <soap:Header>
            </soap:Header>
            <soap:Body>
                <m:GetStockPriceResponse>
                    <m:StockPrice>3.60</m:StockPrice>
                </m:GetStockPriceResponse>
            </soap:Body>
        </soap:Envelope>
        "#;
        Ok(Element::parse(xml1_raw.as_bytes()).unwrap())
            }});

        let in_raw = r#"<?xml version="1.0"?>
            <soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope" xmlns:m="http://www.example.org">
                <soap:Header>
                </soap:Header>
                <soap:Body>
                    <m:GetStockPrice>
                        <m:StockName>T</m:StockName>
                    </m:GetStockPrice>
                    <m:GetStockPrice>
                        <m:StockName>Y</m:StockName>
                    </m:GetStockPrice>
                </soap:Body>
            </soap:Envelope>
            "#;
        let expected_raw = r#"<?xml version="1.0"?>
        <soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope" xmlns:m="http://www.example.org">
            <soap:Header>
            </soap:Header>
            <soap:Body>
                <m:GetStockPriceResponse>
                    <m:StockPrice>3.60</m:StockPrice>
                </m:GetStockPriceResponse>
                <m:GetStockPriceResponse>
                    <m:StockPrice>3.60</m:StockPrice>
                </m:GetStockPriceResponse>
            </soap:Body>
        </soap:Envelope>
        "#;
        let req: Request<Body> = Request::builder()
            .uri("/")
            .body(in_raw.as_bytes().into())
            .unwrap();
        let resp = router.call(req).await.unwrap();

        assert!(resp.status().is_success());

        let body = hyper::body::to_bytes(resp.into_body()).await.unwrap();
        let xml_body = Element::parse(body.as_ref()).unwrap();
        let expected = Element::parse(expected_raw.as_bytes()).unwrap();
        assert_eq!(xml_body, expected)
    }
}
