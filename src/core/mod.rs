pub mod msg;
pub mod auth;

use crate::core::msg::Request;
use crate::util::ConnectionString;
use hyper::{Client, Body};
use hyper::body::to_bytes;
use crate::core::auth::{Authenticator, PasswordAuthenticator};

pub struct Core {
    context: CoreContext,
}

impl Core {
    pub fn new(_connstr: ConnectionString) -> Self {
        let authenticator = PasswordAuthenticator::new("Administrator", "password");
        Self { context: CoreContext::new(Box::new(authenticator))}
    }

    pub fn send<R: Request + Send + 'static>(&self, request: R) {
        match request.service_type() {
            ServiceType::Query =>  self.send_query(request),
            ServiceType::Kv => self.send_kv(request),
        }
    }

    // this needs to go away, just a mock...
    fn send_query<R: Request + Send + 'static>(&self, mut request: R) {
        let client = Client::new();

        let http_request = hyper::Request::builder()
            .method("POST")
            .header(http::header::CONTENT_TYPE, "application/json")
            .uri("http://localhost:8093/query/service");
        let http_request = self.context.authenticator().auth_http_request(ServiceType::Query, http_request);

        // TODO: encode request body and pass it into here

        let http_request = http_request.body(Body::from(r#"{"statement": "select 1=1"}"#)).unwrap();

        println!("{:?}", http_request);

        tokio::spawn(async move {
            let res = client.request(http_request)
                .await
                .unwrap();

            println!("{:?}", res);

            // let body = hyper::body::aggregate(res.into_body()).await.unwrap();
            let r = to_bytes(res.into_body()).await.unwrap();
            let response = request.decode(r);
            request.succeed(response);
        });
    }

    // this needs to go away, just a mock...
    fn send_kv<R: Request + Send + 'static>(&self, _request: R) {
        unimplemented!()
    }
}

#[derive(Debug, PartialEq)]
pub enum ServiceType {
    Query,
    Kv,
}

pub struct CoreContext {
    authenticator: Box<dyn Authenticator>,
}

impl CoreContext {
    pub fn new(authenticator: Box<dyn Authenticator>) -> Self {
        Self { authenticator }
    }

    pub fn authenticator(&self) -> &Box<dyn Authenticator> {
        &self.authenticator
    }
}