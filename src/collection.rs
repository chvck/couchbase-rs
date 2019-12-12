use crate::core::msg::kv::{GetRequest, GetResponse};
use crate::core::msg::Request;
use crate::core::Core;
use crate::kv::{GetOptions, GetResult};
use std::future::Future;
use std::rc::Rc;
use bytes::Bytes;

pub struct Collection {
    core: Rc<Core>,
}

impl Collection {
    pub(crate) fn new(core: Rc<Core>) -> Self {
        Self { core }
    }

    pub async fn get<S: Into<String>>(&self, id: S, options: Option<GetOptions>) -> GetResult {
        let (sender, receiver) = futures::channel::oneshot::channel();
        let request = GetRequest::new(sender, id.into());
        self.core.send(request);
        match receiver.await {
            Ok(r) => GetResult::new(r.cas(), 0, r.content().clone(), None),
            Err(_) => panic!("Do something with this error!"),
        }
    }
}
