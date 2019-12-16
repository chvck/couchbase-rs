use crate::core::msg::Request;
use crate::core::{CoreContext, ServiceType};

use crate::core::endpoint::{Endpoint, QueryEndpoint};
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

/// Represents a Node in a couchbase cluster
pub struct Node {
    _hostname: String,
    _context: Arc<CoreContext>,
    endpoints: HashMap<ServiceType, Box<dyn Endpoint>>,
}

impl Node {
    pub fn new(hostname: String, context: Arc<CoreContext>) -> Self {
        let mut endpoints: HashMap<ServiceType, Box<dyn Endpoint>> = HashMap::new();
        endpoints.insert(
            ServiceType::Query,
            Box::new(QueryEndpoint::new(hostname.clone(), 8093, context.clone())),
        );

        Self {
            _hostname: hostname,
            _context: context,
            endpoints,
        }
    }

    pub fn send(&self, request: Request) {
        self.endpoints
            .get(&request.service_type())
            .expect("endpoint not found")
            .send(request);
    }
}

pub struct RoundRobinLocator {
    _counter: AtomicU64,
}

impl RoundRobinLocator {
    pub fn dispatch(&self, request: Request, nodes: &[Node]) {
        // perform the real checks and use the counter offset
        for n in nodes {
            n.send(request);
            break;
        }
    }
}

impl Default for RoundRobinLocator {
    fn default() -> Self {
        // todo: should be initalized with a random from 0..1024
        Self {
            _counter: AtomicU64::new(0),
        }
    }
}

pub struct KeyValueLocator {}

impl KeyValueLocator {
    pub fn dispatch(&self, _request: Request, _nodes: &[Node]) {
        unimplemented!()
    }
}

impl Default for KeyValueLocator {
    fn default() -> Self {
        Self {}
    }
}
