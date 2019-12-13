use crate::core::msg::{Request, Response};
use bytes::Bytes;
use futures::channel::oneshot::Sender;
use std::borrow::Cow;

#[derive(Debug)]
pub struct GetRequest<'a> {
    id: Cow<'a, str>,
    sender: Option<Sender<GetResponse>>,
}

impl<'a> GetRequest<'a> {
    pub fn new<S: Into<Cow<'a, str>>>(sender: Sender<GetResponse>, id: S) -> Self {
        Self {
            sender: Some(sender),
            id: id.into(),
        }
    }
}

impl<'a> Request for GetRequest<'a> {
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

#[derive(Debug)]
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
