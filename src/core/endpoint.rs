use crate::core::msg::{Request};
use crate::core::{ServiceType, CoreContext};
use hyper::client::HttpConnector;
use hyper::{Client};
use hyper::body::to_bytes;
use std::sync::Arc;

pub trait Endpoint {
    fn send(&self, request: Request);
}

pub struct QueryEndpoint {
    hostname: String,
    port: usize,
    client: Arc<hyper::Client<HttpConnector>>,
    context: Arc<CoreContext>,
}

impl QueryEndpoint {

    pub fn new(hostname: String, port: usize, context: Arc<CoreContext>) -> Self {
        Self { hostname, port, client: Arc::new(Client::new()), context }
    }

}

impl Endpoint for QueryEndpoint {
    fn send(&self, request: Request) {
        match request {
            Request::Query(mut req) => {
                let builder = hyper::Request::builder();
                let builder = self
                    .context
                    .authenticator()
                    .auth_http_request(ServiceType::Query, builder);

                let http_request = req.encode(&self.hostname, self.port,builder);

                let c = self.client.clone();
                tokio::spawn(async move {
                    let res = c.request(http_request).await.unwrap();
                    let r = to_bytes(res.into_body()).await.unwrap();
                    let response = req.decode(r);
                    req.succeed(response);
                });
            },
            _ => panic!("Unknown request for query endpoint, this is a bug...")
        }
    }
}

/*
pub struct KeyValueEndpoint {

}


impl KeyValueEndpoint {
    pub fn new() -> Self {
        Self {}
    }
}

impl Endpoint for KeyValueEndpoint {
    fn send(&self, request: Box<dyn Request<Item = dyn Response>>) {
        unimplemented!()
    }
}
*/