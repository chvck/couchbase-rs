pub mod msg;

use crate::core::msg::kv::GetRequest;
use crate::core::msg::{Request, Response};
use crate::util::ConnectionString;
use bytes::{BufMut, BytesMut};

pub struct Core {}

impl Core {
    pub fn new(connstr: ConnectionString) -> Self {
        Self {}
    }

    pub fn send<R: Request>(&self, mut request: R) {
        let mut res = BytesMut::new();
        res.put_u32(1337);

        let mut response = request.decode(res.freeze());
        request.succeed(response);
    }
}
