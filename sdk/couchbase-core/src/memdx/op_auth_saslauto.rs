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

use std::cmp::PartialEq;

use crate::memdx::auth_mechanism::AuthMechanism;
use crate::memdx::dispatcher::Dispatcher;
use crate::memdx::error::Error;
use crate::memdx::error::Result;
use crate::memdx::op_auth_saslbyname::{
    OpSASLAuthByNameEncoder, OpsSASLAuthByName, PendingSASLAuth, PendingSASLStep, SASLAuthStep,
};
use crate::memdx::pendingop::{
    discard_op_with_deadline, dispatch_op_with_deadline, recv_op_with_deadline, StandardPendingOp,
};
use crate::memdx::request::SASLListMechsRequest;
use crate::memdx::response::SASLListMechsResponse;
use tokio::time::Instant;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Credentials {
    UserPass { username: String, password: String },
    JwtToken(String),
}

impl Credentials {
    pub fn user_pass(&self) -> Result<(&str, &str)> {
        match self {
            Credentials::UserPass { username, password } => {
                Ok((username.as_str(), password.as_str()))
            }
            _ => Err(Error::new_invalid_argument_error(
                "credentials do not contain username/password",
                None,
            )),
        }
    }

    pub fn jwt(&self) -> Result<&str> {
        match self {
            Credentials::JwtToken(token) => Ok(token.as_str()),
            _ => Err(Error::new_invalid_argument_error(
                "credentials do not contain jwt",
                None,
            )),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct SASLAuthAutoOptions {
    pub credentials: Credentials,

    pub enabled_mechs: Vec<AuthMechanism>,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct SASLListMechsOptions {}

pub trait OpSASLAutoEncoder: OpSASLAuthByNameEncoder {
    fn sasl_list_mechs<D>(
        &self,
        dispatcher: &D,
        request: SASLListMechsRequest,
    ) -> impl std::future::Future<Output = Result<StandardPendingOp<SASLListMechsResponse>>>
    where
        D: Dispatcher;
}

/// An authentication exchange with mechanism negotiation, already on the wire.
///
/// `SASLListMechs` goes out alongside an optimistic attempt with the caller's first choice
/// of mechanism, rather than before it. The list is only ever needed to decide whether a
/// *failed* attempt is worth retrying with something else, so asking for it up front and
/// waiting would cost a round trip on every connection that authenticates, to answer a
/// question that almost never gets asked.
pub(crate) struct PendingSASLAuthAuto {
    credentials: Credentials,
    enabled_mechs: Vec<AuthMechanism>,

    /// Outstanding until the first batch of responses is read.
    list_mechs: Option<StandardPendingOp<SASLListMechsResponse>>,
    server_mechs: Vec<AuthMechanism>,

    attempted_mech: AuthMechanism,
    /// Set once the fallback mechanism has been tried; a second failure is terminal.
    retried: bool,

    in_flight: InFlight,
}

/// The request of the exchange that is currently unanswered.
enum InFlight {
    Auth(PendingSASLAuth),
    Step(PendingSASLStep),
}

impl InFlight {
    async fn discard(self, deadline: Instant) {
        match self {
            InFlight::Auth(auth) => auth.discard(deadline).await,
            InFlight::Step(step) => step.discard(deadline).await,
        }
    }
}

/// The state of an exchange once the responses in flight have been read.
///
/// `Continue` hands the driver straight back to the caller, so the value is moved once and
/// dropped; the size difference between the variants buys nothing to pay for with a box.
#[allow(clippy::large_enum_variant)]
pub(crate) enum SASLAuthAutoProgress {
    /// The connection is authenticated.
    Done,
    /// A further round trip is already on the wire; call `resolve` again.
    Continue(PendingSASLAuthAuto),
}

impl PendingSASLAuthAuto {
    /// Whether the exchange needs a further round trip after the one in flight.
    ///
    /// A caller that wants to pipeline work which depends on being authenticated uses this
    /// to find the batch that will finish the job.
    pub(crate) fn needs_another_round_trip(&self) -> bool {
        match &self.in_flight {
            InFlight::Auth(auth) => auth.needs_another_round_trip(),
            InFlight::Step(_) => false,
        }
    }

    /// Read what is outstanding away, for a caller that is abandoning the exchange.
    pub(crate) async fn discard(self, deadline: Instant) {
        if let Some(op) = self.list_mechs {
            discard_op_with_deadline(deadline, op).await;
        }
        self.in_flight.discard(deadline).await;
    }

    /// Read what is outstanding. When the exchange is not finished, the next request is
    /// dispatched before returning, so the caller can pipeline behind that one too.
    pub(crate) async fn resolve<E, D>(
        mut self,
        encoder: &E,
        dispatcher: &D,
        deadline: Instant,
    ) -> Result<SASLAuthAutoProgress>
    where
        E: OpSASLAutoEncoder,
        D: Dispatcher,
    {
        // The list was written before the attempt, so its response comes back first. Read
        // it before judging the attempt, because whether a failure is retryable depends on
        // what the server said it offers.
        if let Some(op) = self.list_mechs.take() {
            match recv_op_with_deadline(deadline, op).await {
                Ok(resp) => self.server_mechs = resp.available_mechs,
                Err(e) => {
                    self.in_flight.discard(deadline).await;
                    return Err(e);
                }
            }
        }

        let auth = match self.in_flight {
            // The last request of a mechanism's exchange. There is nothing left to fall
            // back to, so its answer is the answer.
            InFlight::Step(step) => {
                return step
                    .resolve(deadline)
                    .await
                    .map(|()| SASLAuthAutoProgress::Done);
            }
            InFlight::Auth(auth) => auth,
        };

        let e = match auth.resolve(encoder, dispatcher, deadline).await {
            Ok(SASLAuthStep::Done) => return Ok(SASLAuthAutoProgress::Done),
            Ok(SASLAuthStep::Continue(step)) => {
                self.in_flight = InFlight::Step(step);
                return Ok(SASLAuthAutoProgress::Continue(self));
            }
            Err(e) => e,
        };

        if e.is_cancellation_error() {
            return Err(e);
        }

        // There is no obvious way to differentiate between a mechanism being unsupported
        // and the credentials being wrong. So for now we just assume any error should be
        // ignored if our list-mechs doesn't include the mechanism we used.
        // If the server supports the mechanism we tried, it means this error is 'real'.
        // One fallback is all we get: if the mechanism the server told us it supports also
        // fails, the credentials are the only remaining explanation.
        if self.retried || self.server_mechs.contains(&self.attempted_mech) {
            return Err(e);
        }

        let next_mech = self
            .enabled_mechs
            .iter()
            .find(|item| self.server_mechs.contains(item))
            .cloned();

        let next_mech = match next_mech {
            Some(mech) => mech,
            None => {
                return Err(Error::new_message_error("no supported mechanisms found"));
            }
        };

        let attempt = OpsSASLAuthByName {}
            .dispatch(
                encoder,
                dispatcher,
                deadline,
                &self.credentials,
                next_mech.clone(),
            )
            .await?;

        self.attempted_mech = next_mech;
        self.retried = true;
        self.in_flight = InFlight::Auth(attempt);

        Ok(SASLAuthAutoProgress::Continue(self))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct OpsSASLAuthAuto {}

impl OpsSASLAuthAuto {
    /// Write `SASLListMechs` and the first authentication attempt, without waiting for
    /// either reply.
    pub(crate) async fn dispatch<E, D>(
        &self,
        encoder: &E,
        dispatcher: &D,
        deadline: Instant,
        opts: SASLAuthAutoOptions,
    ) -> Result<PendingSASLAuthAuto>
    where
        E: OpSASLAutoEncoder,
        D: Dispatcher,
    {
        if opts.enabled_mechs.is_empty() {
            return Err(Error::new_invalid_argument_error(
                "no enabled mechanisms",
                "enabled_mechanisms".to_string(),
            ));
        }

        let list_mechs = dispatch_op_with_deadline(
            deadline,
            encoder.sasl_list_mechs(dispatcher, SASLListMechsRequest {}),
        )
        .await?;

        // This unwrap is safe, we know it can't be None.
        let attempted_mech = opts.enabled_mechs.first().unwrap().clone();

        let attempt = match (OpsSASLAuthByName {})
            .dispatch(
                encoder,
                dispatcher,
                deadline,
                &opts.credentials,
                attempted_mech.clone(),
            )
            .await
        {
            Ok(attempt) => attempt,
            Err(e) => {
                discard_op_with_deadline(deadline, list_mechs).await;
                return Err(e);
            }
        };

        Ok(PendingSASLAuthAuto {
            credentials: opts.credentials,
            enabled_mechs: opts.enabled_mechs,
            list_mechs: Some(list_mechs),
            server_mechs: Vec::new(),
            attempted_mech,
            retried: false,
            in_flight: InFlight::Auth(attempt),
        })
    }

    pub async fn sasl_auth_auto<E, D>(
        &self,
        encoder: &E,
        dispatcher: &D,
        deadline: Instant,
        opts: SASLAuthAutoOptions,
    ) -> Result<()>
    where
        E: OpSASLAutoEncoder,
        D: Dispatcher,
    {
        let mut pending = self.dispatch(encoder, dispatcher, deadline, opts).await?;

        loop {
            match pending.resolve(encoder, dispatcher, deadline).await? {
                SASLAuthAutoProgress::Done => return Ok(()),
                SASLAuthAutoProgress::Continue(next) => pending = next,
            }
        }
    }
}
