use serde_json::Value;
use snafu::Snafu;
use std::collections::HashMap;
use std::fmt::Display;
use std::fmt::{Debug, Error, Formatter};

#[derive(Debug, Snafu)]
pub enum CouchbaseError {
    #[snafu(display("Request timed out: {}", ctx))]
    RequestTimeout { ctx: ErrorContext },
    #[snafu(display("Unexpected internal error: {}", ctx))]
    Unexpected { ctx: ErrorContext },
}

pub type Result<T, E = CouchbaseError> = std::result::Result<T, E>;

#[derive(Debug)]
pub struct ErrorContext {
    inner: HashMap<String, Value>,
}

impl ErrorContext {
    pub fn empty() -> Self {
        Self {
            inner: HashMap::new(),
        }
    }

    pub fn from_map(input: HashMap<String, Value>) -> Self {
        Self { inner: input }
    }

    pub fn from_message(input: &str) -> Self {
        let mut map = HashMap::new();
        map.insert("msg".into(), Value::String(input.into()));
        Self::from_map(map)
    }
}

impl Display for ErrorContext {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), Error> {
        write!(
            f,
            "{}",
            serde_json::to_string(&self.inner).unwrap_or_else(|_| "".into())
        )
    }
}
