/*
 *
 *  * Copyright (c) 2025 Couchbase, Inc.
 *  *
 *  * Licensed under the Apache License, Version 2.0 (the "License");
 *  * you may not use this file except in compliance with the License.
 *  * You may obtain a copy of the License at
 *  *
 *  *    http://www.apache.org/licenses/LICENSE-2.0
 *  *
 *  * Unless required by applicable law or agreed to in writing, software
 *  * distributed under the License is distributed on an "AS IS" BASIS,
 *  * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 *  * See the License for the specific language governing permissions and
 *  * limitations under the License.
 *
 */

use std::future::{poll_fn, Future};
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::time::{timeout_at, Instant};

use crate::memdx::client::{OpaqueMap, ResponseReceiver, SenderContext};
use crate::memdx::client_response::ClientResponse;
use crate::memdx::error::CancellationErrorKind;
use crate::memdx::error::{Error, Result};
use crate::memdx::response::TryFromClientResponse;

pub struct ClientPendingOp {
    opaque: u32,
    receiver: ResponseReceiver,
    opaque_map: Arc<Mutex<OpaqueMap>>,

    completed: AtomicBool,
}

impl ClientPendingOp {
    pub(crate) fn new(
        opaque: u32,
        opaque_map: Arc<Mutex<OpaqueMap>>,
        receiver: ResponseReceiver,
    ) -> Self {
        ClientPendingOp {
            opaque,
            opaque_map,
            receiver,
            completed: AtomicBool::new(false),
        }
    }

    pub async fn recv(&mut self) -> Result<ClientResponse> {
        match &mut self.receiver {
            ResponseReceiver::OneShot(slot) => {
                let Some(receiver) = slot.as_mut() else {
                    // The single reply has already been read.
                    return Err(Error::new_cancelled_error(
                        CancellationErrorKind::RequestCancelled,
                    ));
                };

                // Polled through a `&mut`, never moved out of the slot: `recv` has to
                // survive being dropped mid-await by a `timeout_at` and then called again,
                // and taking the receiver first would lose it on that path.
                let received = poll_fn(|cx| Pin::new(&mut *receiver).poll(cx)).await;

                // tokio's one-shot receiver panics if it is polled after completing, so it
                // goes now that it has.
                *slot = None;
                self.completed.store(true, Ordering::SeqCst);

                match received {
                    Ok(response) => response,
                    // The sender was dropped without answering, which only happens once the
                    // client is gone.
                    Err(_) => Err(Error::new_cancelled_error(
                        CancellationErrorKind::RequestCancelled,
                    )),
                }
            }
            // A persistent operation is never `completed`: more replies may follow every
            // one that arrives, and it is the caller dropping the op that ends it.
            ResponseReceiver::Streaming(receiver) => match receiver.recv().await {
                Some(response) => response,
                None => Err(Error::new_cancelled_error(
                    CancellationErrorKind::RequestCancelled,
                )),
            },
        }
    }

    pub async fn cancel(&mut self, e: CancellationErrorKind) -> bool {
        let context = self.cancel_op();

        if let Some(context) = context {
            // Delivered to ourselves -- we still hold the receiver -- because a caller that
            // cancels on a deadline reads the cancellation back out through `recv`.
            let _ = context
                .sender
                .send(Err(Error::new_cancelled_error(e)))
                .await;

            true
        } else {
            false
        }
    }

    fn cancel_op(&mut self) -> Option<SenderContext> {
        if self.completed.load(Ordering::SeqCst) {
            return None;
        }

        let requests: Arc<Mutex<OpaqueMap>> = Arc::clone(&self.opaque_map);
        let mut map = requests.lock().unwrap();

        map.remove(&self.opaque)
    }
}

impl Drop for ClientPendingOp {
    fn drop(&mut self) {
        // We don't need to send a cancellation error on the sender here, we own the receiver
        // and if we've been dropped then the receiver is gone with us.
        self.cancel_op();
    }
}

pub struct StandardPendingOp<TryFromClientResponse> {
    wrapped: ClientPendingOp,
    _target: PhantomData<TryFromClientResponse>,
}

impl<T: TryFromClientResponse> StandardPendingOp<T> {
    pub(crate) fn new(op: ClientPendingOp) -> Self {
        Self {
            wrapped: op,
            _target: PhantomData,
        }
    }

    pub(crate) fn opaque(&self) -> u32 {
        self.wrapped.opaque
    }
}

impl<T: TryFromClientResponse> StandardPendingOp<T> {
    pub async fn recv(&mut self) -> Result<T> {
        let packet = self.wrapped.recv().await?;

        T::try_from(packet)
    }

    pub async fn cancel(&mut self, e: CancellationErrorKind) -> bool {
        self.wrapped.cancel(e).await
    }
}

/// Write an operation's request, giving up if it is not on the wire by `deadline`.
///
/// The response is deliberately left unread. On a request/response protocol that is the
/// whole difference between a batch of operations costing one round trip and costing one
/// each: the caller can keep dispatching while the replies are still in the air.
pub(super) async fn dispatch_op_with_deadline<F, T>(
    deadline: Instant,
    fut: F,
) -> Result<StandardPendingOp<T>>
where
    F: Future<Output = Result<StandardPendingOp<T>>>,
    T: TryFromClientResponse,
{
    match timeout_at(deadline, fut).await {
        Ok(op) => op,
        Err(_e) => Err(Error::new_cancelled_error(CancellationErrorKind::Timeout)),
    }
}

/// Read a dispatched operation's response, cancelling it if `deadline` passes.
pub(super) async fn recv_op_with_deadline<T>(
    deadline: Instant,
    mut op: StandardPendingOp<T>,
) -> Result<T>
where
    T: TryFromClientResponse,
{
    match timeout_at(deadline, op.recv()).await {
        Ok(res) => res,
        Err(_e) => {
            if op.cancel(CancellationErrorKind::Timeout).await {
                return Err(Error::new_cancelled_error(CancellationErrorKind::Timeout));
            };

            op.recv().await
        }
    }
}

/// Read a dispatched operation's response and throw it away.
///
/// Dropping a dispatched op instead unregisters its opaque, so the reply that is already
/// on its way back arrives with nowhere to go and is logged as an orphan. Any path that
/// abandons an operation it has already written has to come through here.
pub(super) async fn discard_op_with_deadline<T>(deadline: Instant, op: StandardPendingOp<T>)
where
    T: TryFromClientResponse,
{
    let _ = recv_op_with_deadline(deadline, op).await;
}

pub(super) async fn run_op_future_with_deadline<F, T>(deadline: Instant, fut: F) -> Result<T>
where
    F: Future<Output = Result<StandardPendingOp<T>>>,
    T: TryFromClientResponse,
{
    let op = dispatch_op_with_deadline(deadline, fut).await?;

    recv_op_with_deadline(deadline, op).await
}
