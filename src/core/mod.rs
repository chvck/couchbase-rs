pub mod auth;
mod endpoint;
pub mod msg;
mod node;

use crate::cluster::ClusterConfig;
use crate::core::auth::Authenticator;
use crate::core::msg::Request;
use crate::core::node::{KeyValueLocator, Node, RoundRobinLocator};
use std::sync::Arc;

pub struct Core {
    config: ClusterConfig,
    _context: Arc<CoreContext>,
    nodes: Vec<Node>,
    query_node_locator: RoundRobinLocator,
    key_value_locator: KeyValueLocator,
}

impl Core {
    pub fn new(authenticator: Box<dyn Authenticator>, config: ClusterConfig) -> Self {
        let context = Arc::new(CoreContext::new(authenticator));

        let nodes = vec![Node::new("127.0.0.1".into(), context.clone())];
        Self {
            config,
            _context: context,
            nodes,
            query_node_locator: RoundRobinLocator::default(),
            key_value_locator: KeyValueLocator::default(),
        }
    }

    pub fn send(&self, request: Request) {
        match request.service_type() {
            ServiceType::Query => self.query_node_locator.dispatch(request, &self.nodes),
            ServiceType::Kv => self.key_value_locator.dispatch(request, &self.nodes),
        }
    }

    pub fn config(&self) -> &ClusterConfig {
        &self.config
    }
}

#[derive(Debug, PartialEq, Hash, Eq)]
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
