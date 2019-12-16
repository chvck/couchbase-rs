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
use crate::core::msg::Request;

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
        Self::connect_with_options(
            connection_string,
            ClusterOptions {
                authenticator,
                config: ClusterConfig::default(),
            },
        )
    }

    pub fn connect_with_options<CS>(_connection_string: CS, options: ClusterOptions) -> Self
    where
        CS: Into<ConnectionString>,
    {
        let authenticator = options.authenticator;
        let config = options.config;
        Self {
            core: Arc::new(Core::new(authenticator, config)),
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
        let request = Request::Query(QueryRequest::new(sender, statement.into()));

        let user_timeout = options
            .and_then(|o| o.timeout)
            .unwrap_or_else(|| self.core.config().timeout_config().query_timeout().clone());
        let timeout = time::timeout(user_timeout, receiver);

        self.core.send(request);

        match timeout.await {
            Ok(f) => match f {
                Ok(_r) => {
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
    config: ClusterConfig,
}

#[derive(Default)]
pub struct ClusterConfig {
    timeout_config: TimeoutConfig,
}

impl ClusterConfig {
    pub fn timeout_config(&self) -> &TimeoutConfig {
        &self.timeout_config
    }

}
pub struct TimeoutConfig {
    kv_timeout: Duration,
    query_timeout: Duration,
}

impl TimeoutConfig {
    pub fn kv_timeout(&self) -> &Duration {
        &self.kv_timeout
    }
    pub fn query_timeout(&self) -> &Duration {
        &self.query_timeout
    }
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        TimeoutConfig {
            kv_timeout: Duration::from_millis(2500),
            query_timeout: Duration::from_secs(75),
        }
    }
}
