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

pub struct GetOptions {
    pub(crate) timeout: Option<Duration>,
}

impl GetOptions {
    fn new() -> Self {
        Self { timeout: None }
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

impl Default for GetOptions {
    fn default() -> Self {
        GetOptions::new()
    }
}
