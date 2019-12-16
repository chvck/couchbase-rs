use crate::bucket::Bucket;
use crate::core::auth::{Authenticator, PasswordAuthenticator};
use crate::core::msg::query::QueryRequest;
use crate::core::Core;
use crate::error::{CouchbaseError, ErrorContext, Result};
use crate::query::{QueryOptions, QueryResult};
use crate::util::ConnectionString;
use std::sync::Arc;
use std::time::Duration;
use tokio::time;

pub struct Cluster {
    core: Arc<Core>,
}

impl Cluster {
    pub fn connect<CS, S>(connection_string: CS, username: S, password: S) -> Self
    where
        CS: Into<ConnectionString>,
        S: Into<String>,
    {
        let authenticator = Box::new(PasswordAuthenticator::new(username, password));
        Self::connect_with_options(connection_string, ClusterOptions { authenticator })
    }

    pub fn connect_with_options<CS>(_connection_string: CS, options: ClusterOptions) -> Self
    where
        CS: Into<ConnectionString>,
    {
        let authenticator = options.authenticator;
        Self {
            core: Arc::new(Core::new(authenticator)),
        }
    }

    pub fn bucket<S>(&self, name: S) -> Bucket
    where
        S: Into<String>,
    {
        Bucket::new(name.into(), self.core.clone())
    }

    pub async fn query<S>(&self, statement: S, options: Option<QueryOptions>) -> Result<QueryResult>
    where
        S: Into<String>,
    {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let request = QueryRequest::new(sender, statement.into());

        let user_timeout = options
            .and_then(|o| o.timeout)
            .unwrap_or(Duration::from_secs(2));
        let timeout = time::timeout(user_timeout, receiver);

        self.core.send(request);

        match timeout.await {
            Ok(f) => match f {
                Ok(r) => {
                    // println!("--> {:?}", r);
                    Ok(QueryResult {})
                }
                Err(_e) => Err(CouchbaseError::Unexpected {
                    ctx: ErrorContext::from_message("Sender dropped"),
                }),
            },
            Err(_) => Err(CouchbaseError::RequestTimeout {
                ctx: ErrorContext::empty(),
            }),
        }
    }
}

pub struct ClusterOptions {
    authenticator: Box<dyn Authenticator>,
}
