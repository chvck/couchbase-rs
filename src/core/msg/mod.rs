pub mod kv;
pub mod query;

use crate::core::ServiceType;
use bytes::Bytes;

pub trait Request {
    type Item;

    fn encode(&self) -> Bytes;

    fn decode(&self, input: Bytes) -> Self::Item;

    fn succeed(&mut self, response: Self::Item);

    fn service_type(&self) -> ServiceType;
}

pub trait Response {}
