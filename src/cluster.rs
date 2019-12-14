use crate::bucket::Bucket;
use crate::core::Core;
use crate::util::ConnectionString;
use std::rc::Rc;
use crate::query::{QueryOptions, QueryResult};
use crate::core::msg::query::QueryRequest;
use crate::error::{Result, CouchbaseError, ErrorContext};
use std::time::Duration;
use tokio::time;

pub struct Cluster {
    core: Rc<Core>,
}

impl Cluster {
    pub fn connect<S>(connection_string: S) -> Self
    where
        S: Into<ConnectionString>,
    {
        Self {
            core: Rc::new(Core::new(connection_string.into())),
        }
    }

    pub fn bucket<S>(&self, name: S) -> Bucket
    where
        S: Into<String>,
    {
        Bucket::new(name.into(), self.core.clone())
    }

    pub async fn query<S>(&self, statement: S, options: Option<QueryOptions>) -> Result<QueryResult> where S: Into<String> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let request = QueryRequest::new(sender, statement.into());

        let user_timeout = options.and_then(|o| o.timeout).unwrap_or( Duration::from_secs(2));
        let timeout = time::timeout(user_timeout, receiver);

        self.core.send(request);

        match timeout.await {
            Ok(f) => match f {
                Ok(r) => {
                    println!("--> {:?}", r);
                    Ok(QueryResult {})
                },
                Err(_e) => Err(CouchbaseError::Unexpected { ctx: ErrorContext::from_message("Sender dropped") })
            },
            Err(_) => Err(CouchbaseError::RequestTimeout { ctx: ErrorContext::empty() })
        }
    }
}
