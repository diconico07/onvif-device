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

type SoapMessage = xmltree::Element;
type SoapError = String;
type BoxedSoapFuture = Pin<Box<dyn Future<Output = Result<SoapMessage, SoapError>> + Send>>;
type BoxedSoapHandlerService = tower::util::BoxCloneService<SoapMessage, SoapMessage, SoapError>;

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

impl<S, H> Service<SoapMessage> for SoapHandlerService<S, H>
where
    H: SoapHandler<S>,
    S: Clone,
{
    type Error = SoapError;
    type Response = SoapMessage;

    type Future = BoxedSoapFuture;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        Ok(()).into()
    }

    fn call(&mut self, req: SoapMessage) -> Self::Future {
        self.handler.clone().call(&req, self.state.clone())
    }
}

pub trait SoapHandler<S>: 'static + Send + Sync + Clone {
    fn call(self, req: &SoapMessage, state: S) -> BoxedSoapFuture;
}

impl<F, Fut, Res, S> SoapHandler<S> for F
where
    F: FnOnce() -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<Res, SoapError>> + Send,
    Res: Into<SoapMessage>,
{
    fn call(self, _req: &SoapMessage, _state: S) -> BoxedSoapFuture {
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

    async fn parse_request(&self, req: Request<Body>) -> Result<xmltree::Element, String> {
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
        Ok(xml_body)
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
        let soap_body = soap_req
            .get_child(("Body", "http://www.w3.org/2003/05/soap-envelope"))
            .unwrap();
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
                fut.push_back(handler.clone().call(soap_req.clone()));
            }
        }
        if fut.is_empty() {
            // Handle operations not found
            todo!()
        }
        let soap_reponses: Vec<xmltree::Element> = fut.map(|e| e.unwrap()).collect().await;

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
