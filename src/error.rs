pub enum CouchbaseError {
    Unknown(ErrorContext),
}

impl CouchbaseError {
    pub fn context(&self) -> &ErrorContext {
        match self {
            CouchbaseError::Unknown(ctx) => &ctx,
        }
    }
}

pub struct ErrorContext {}
