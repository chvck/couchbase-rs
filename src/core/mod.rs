pub mod msg;

use crate::core::msg::Request;
use crate::util::ConnectionString;
use hyper::Client;
use hyper::body::to_bytes;

pub struct Core {}

impl Core {
    pub fn new(_connstr: ConnectionString) -> Self {
        Self {}
    }

    pub fn send<R: Request + Send + 'static>(&self, request: R) {
        match request.service_type() {
            ServiceType::Query =>  self.send_query(request),
            ServiceType::Kv => self.send_kv(request),
        }
    }

    // this needs to go away, just a mock...
    fn send_query<R: Request + Send + 'static>(&self, mut request: R) {
        tokio::spawn(async move {
            let client = Client::new();

            // set auth header: Authorization: Basic QWRtaW5pc3RyYXRvcjpwYXNzd29yZA==
            let res = client
                .get("http://localhost:8093/query/service".parse().unwrap())
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