use crate::core::msg::{Request, Response};
use crate::core::ServiceType;
use bytes::Bytes;
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
}

impl Request for QueryRequest {
    type Item = QueryResponse;

    fn encode(&self) -> Bytes {
        unimplemented!()
    }

    fn decode(&self, input: Bytes) -> Self::Item {
        QueryResponse::new(input)
    }

    fn succeed(&mut self, response: Self::Item) {
        let sender = self.sender.take().unwrap();
        sender.send(response).expect("Could not send! - fix me.");
    }

    fn service_type(&self) -> ServiceType {
        ServiceType::Query
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

impl Response for QueryResponse {}
