use bytes::Bytes;
use std::time::Duration;

#[derive(Debug)]
pub struct GetResult {
    cas: u64,
    flags: i32,
    expiry: Option<Duration>,
    content: Bytes,
}

impl GetResult {
    pub fn new(cas: u64, flags: i32, content: Bytes, expiry: Option<Duration>) -> Self {
        Self {
            cas,
            flags,
            content,
            expiry,
        }
    }

    pub fn cas(&self) -> u64 {
        self.cas
    }

    pub fn expiry(&self) -> &Option<Duration> {
        &self.expiry
    }
}

pub struct GetOptions {}
