use crate::core::msg::kv::GetRequest;
use crate::core::Core;
use crate::error::{CouchbaseError, ErrorContext, Result};
use crate::kv::{GetOptions, GetResult};
use std::borrow::Cow;
use std::sync::Arc;
use tokio::time;
use crate::core::msg::Request;

pub struct Collection {
    core: Arc<Core>,
}

impl Collection {
    pub(crate) fn new(core: Arc<Core>) -> Self {
        Self { core }
    }

    pub async fn get<'a, S: Into<Cow<'a, str>>>(
        &self,
        id: S,
        options: Option<GetOptions>,
    ) -> Result<GetResult> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let request = Request::Get(GetRequest::new(sender, id.into()));

        let user_timeout = options
            .and_then(|o| o.timeout)
            .unwrap_or_else(|| self.core.config().timeout_config().kv_timeout().clone());
        let timeout = time::timeout(user_timeout, receiver);

        self.core.send(request);

        match timeout.await {
            Ok(f) => match f {
                Ok(r) => Ok(GetResult::new(r.cas(), 0, r.content().clone(), None)),
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
