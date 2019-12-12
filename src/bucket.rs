use crate::collection::Collection;
use crate::core::Core;
use std::rc::Rc;

pub struct Bucket {
    name: String,
    core: Rc<Core>,
}

impl Bucket {
    pub(crate) fn new(name: String, core: Rc<Core>) -> Self {
        Self { name, core }
    }

    pub fn default_collection(&self) -> Collection {
        Collection::new(self.core.clone())
    }
}
