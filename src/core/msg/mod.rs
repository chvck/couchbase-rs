pub mod kv;

use bytes::Bytes;

pub trait Request {
    type Item;

    fn encode(&self) -> Bytes;

    fn decode(&self, input: Bytes) -> Self::Item;

    fn succeed(&mut self, response: Self::Item);
}

pub trait Response {}
