use crate::core::msg::Request;
use crate::core::{CoreContext, ServiceType};

use hyper::body::to_bytes;
use hyper::{Body, Client};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

/// Represents a Node in a couchbase cluster
pub struct Node {
    hostname: String,
    context: Arc<CoreContext>,
}

impl Node {
    pub fn new(hostname: String, context: Arc<CoreContext>) -> Self {
        Self { hostname, context }
    }

    pub fn send<R: Request + Send + 'static>(&self, request: R) {
        match request.service_type() {
            ServiceType::Query => self.send_query(request),
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
        let http_request = self
            .context
            .authenticator()
            .auth_http_request(ServiceType::Query, http_request);

        // TODO: encode request body and pass it into here

        let http_request = http_request
            .body(Body::from(r#"{"statement": "select 1=1"}"#))
            .unwrap();

        //println!("{:?}", http_request);

        tokio::spawn(async move {
            let res = client.request(http_request).await.unwrap();

            // println!("{:?}", res);

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

pub struct RoundRobinLocator {
    counter: AtomicU64,
}

impl RoundRobinLocator {
    pub fn dispatch<R: Request + Send + 'static>(&self, request: R, nodes: &[Node]) {
        // perform the real checks and use the counter offset
        for n in nodes {
            n.send(request);
            break;
        }
    }
}

impl Default for RoundRobinLocator {
    fn default() -> Self {
        // todo: should be initalized with a random from 0..1024
        Self {
            counter: AtomicU64::new(0),
        }
    }
}

pub struct KeyValueLocator {}

impl KeyValueLocator {
    pub fn dispatch<R: Request + Send + 'static>(&self, request: R, nodes: &[Node]) {
        unimplemented!()
    }
}

impl Default for KeyValueLocator {
    fn default() -> Self {
        Self {}
    }
}
