use crate::collection::Collection;
use crate::core::Core;
use std::sync::Arc;

pub struct Bucket {
    _name: String,
    core: Arc<Core>,
}

impl Bucket {
    pub(crate) fn new(name: String, core: Arc<Core>) -> Self {
        Self { _name: name, core }
    }

    pub fn default_collection(&self) -> Collection {
        Collection::new(self.core.clone())
    }
}
