use crate::core::msg::{Request, Response};
use bytes::Bytes;
use futures::channel::oneshot::Sender;
use std::borrow::Cow;

pub struct GetRequest {
    id: String,
    sender: Option<Sender<GetResponse>>,
}

impl GetRequest {
    pub fn new(sender: Sender<GetResponse>, id: String) -> Self {
        Self {
            sender: Some(sender),
            id,
        }
    }
}

impl Request for GetRequest {
    type Item = GetResponse;

    fn encode(&self) -> Bytes {
        unimplemented!()
    }

    fn decode(&self, input: Bytes) -> Self::Item {
        GetResponse {
            cas: 1234,
            content: input,
        }
    }

    fn succeed(&mut self, response: Self::Item) {
        let mut sender = self.sender.take().unwrap();
        sender.send(response);
    }
}

pub struct GetResponse {
    cas: u64,
    content: Bytes,
}

impl GetResponse {
    pub fn new(cas: u64, content: Bytes) -> Self {
        Self { cas, content }
    }

    pub fn cas(&self) -> u64 {
        self.cas
    }

    pub fn content(&self) -> &Bytes {
        &self.content
    }
}

impl Response for GetResponse {}
