use crate::bucket::Bucket;
use crate::core::Core;
use crate::util::ConnectionString;
use std::rc::Rc;
use crate::query::{QueryOptions, QueryResult};

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

    async fn query<S>(&self, tatement: S, options: Option<QueryOptions>) -> QueryResult where S: Into<String> {
        unimplemented!()
    }
}
