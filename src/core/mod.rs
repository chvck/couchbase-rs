pub mod auth;
pub mod msg;
mod node;

use crate::core::auth::{Authenticator, PasswordAuthenticator};
use crate::core::msg::Request;
use crate::core::node::{KeyValueLocator, Node, RoundRobinLocator};
use crate::util::ConnectionString;
use std::sync::Arc;

pub struct Core {
    context: Arc<CoreContext>,
    nodes: Vec<Node>,
    query_node_locator: RoundRobinLocator,
    key_value_locator: KeyValueLocator,
}

impl Core {
    pub fn new(authenticator: Box<dyn Authenticator>) -> Self {
        let context = Arc::new(CoreContext::new(authenticator));

        let nodes = vec![Node::new("127.0.0.1".into(), context.clone())];
        Self {
            context,
            nodes,
            query_node_locator: RoundRobinLocator::default(),
            key_value_locator: KeyValueLocator::default(),
        }
    }

    pub fn send<R: Request + Send + 'static>(&self, request: R) {
        match request.service_type() {
            ServiceType::Query => self.query_node_locator.dispatch(request, &self.nodes),
            ServiceType::Kv => self.key_value_locator.dispatch(request, &self.nodes),
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum ServiceType {
    Query,
    Kv,
}

pub struct CoreContext {
    authenticator: Box<dyn Authenticator>,
}

impl CoreContext {
    pub fn new(authenticator: Box<dyn Authenticator>) -> Self {
        Self { authenticator }
    }

    pub fn authenticator(&self) -> &Box<dyn Authenticator> {
        &self.authenticator
    }
}
