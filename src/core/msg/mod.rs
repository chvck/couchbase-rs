pub mod kv;
pub mod query;

use crate::core::msg::kv::GetRequest;
use crate::core::msg::query::QueryRequest;
use crate::core::ServiceType;
use std::fmt::Debug;

#[derive(Debug)]
pub enum Request {
    Query(QueryRequest),
    Get(GetRequest),
}

impl Request {
    pub fn service_type(&self) -> ServiceType {
        match self {
            Self::Query(_) => ServiceType::Query,
            Self::Get(_) => ServiceType::Kv,
        }
    }
}
