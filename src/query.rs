use std::time::Duration;

pub struct QueryOptions {
    pub(crate) timeout: Option<Duration>,
}

impl QueryOptions {
    fn new() -> Self {
        Self { timeout: None }
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

impl Default for QueryOptions {
    fn default() -> Self {
        QueryOptions::new()
    }
}

#[derive(Debug)]
pub struct QueryResult {}

impl QueryResult {
    pub async fn meta_data() -> QueryMetaData {
        unimplemented!()
    }
}

pub struct QueryMetaData {}
