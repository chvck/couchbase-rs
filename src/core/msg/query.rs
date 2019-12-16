use bytes::Bytes;
use http::request::Builder;
use hyper::Body;
use tokio::sync::oneshot::Sender;

#[derive(Debug)]
pub struct QueryRequest {
    statement: String,
    sender: Option<Sender<QueryResponse>>,
}

impl QueryRequest {
    pub fn new<S: Into<String>>(sender: Sender<QueryResponse>, statement: S) -> Self {
        Self {
            sender: Some(sender),
            statement: statement.into(),
        }
    }

    pub fn decode(&self, input: Bytes) -> QueryResponse {
        QueryResponse::new(input)
    }

    pub fn succeed(&mut self, response: QueryResponse) {
        let sender = self.sender.take().unwrap();
        sender.send(response).expect("Could not send! - fix me.");
    }

    pub fn encode(&self, hostname: &str, port: usize, request: Builder) -> http::Request<Body> {
        request
            .method("POST")
            .header(http::header::CONTENT_TYPE, "application/json")
            .uri(&format!("http://{}:{}/query/service", hostname, port))
            .body(Body::from(r#"{"statement": "select 1=1"}"#))
            .unwrap()
    }
}

#[derive(Debug)]
pub struct QueryResponse {
    content: Bytes,
}

impl QueryResponse {
    pub fn new(content: Bytes) -> Self {
        Self { content }
    }
}
