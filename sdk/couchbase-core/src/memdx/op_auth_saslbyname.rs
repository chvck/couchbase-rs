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

use crate::memdx::auth_mechanism::AuthMechanism;
use crate::memdx::dispatcher::Dispatcher;
use crate::memdx::error::Error;
use crate::memdx::error::Result;
use crate::memdx::op_auth_saslauto::Credentials;
use crate::memdx::op_auth_sasloauthbearer::OpsSASLOAuthBearer;
use crate::memdx::op_auth_saslplain::{OpSASLPlainEncoder, OpsSASLAuthPlain};
use crate::memdx::op_auth_saslscram::{OpSASLScramEncoder, ScramClient};
use crate::memdx::pendingop::{
    discard_op_with_deadline, dispatch_op_with_deadline, recv_op_with_deadline, StandardPendingOp,
};
use crate::memdx::request::{SASLAuthRequest, SASLStepRequest};
use crate::memdx::response::{SASLAuthResponse, SASLStepResponse};
use tokio::time::Instant;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct SASLAuthByNameOptions {
    pub credentials: Credentials,

    pub auth_mechanism: AuthMechanism,

    pub deadline: Instant,
}

pub trait OpSASLAuthByNameEncoder: OpSASLScramEncoder + OpSASLPlainEncoder {}

/// An authentication attempt whose first request has been written but whose response has
/// not been read yet.
///
/// Keeping the two halves apart is what lets the caller put other requests on the wire
/// behind this one instead of paying a round trip for it alone.
pub(crate) struct PendingSASLAuth {
    auth_mechanism: AuthMechanism,
    op: StandardPendingOp<SASLAuthResponse>,
    /// Carried between the two round trips of a SCRAM exchange. `None` for the mechanisms
    /// that finish in one.
    scram: Option<ScramClient>,
}

/// What is left of an authentication attempt once the response in flight has been read.
pub(crate) enum SASLAuthStep {
    /// The server accepted the credentials.
    Done,
    /// The mechanism asked to continue, and the follow-up is already on the wire.
    Continue(PendingSASLStep),
}

/// The second round trip of a multi-step mechanism, already dispatched.
pub(crate) struct PendingSASLStep {
    op: StandardPendingOp<SASLStepResponse>,
}

impl PendingSASLAuth {
    /// Whether the mechanism needs a further round trip after the one in flight.
    ///
    /// This decides where a caller can put work that depends on being authenticated: it
    /// may ride along with the last request of the exchange, but not with an earlier one.
    pub(crate) fn needs_another_round_trip(&self) -> bool {
        self.scram.is_some()
    }

    /// Read the response and, when the mechanism asks to continue, write the follow-up
    /// before returning so the caller can pipeline behind that too.
    pub(crate) async fn resolve<E, D>(
        self,
        encoder: &E,
        dispatcher: &D,
        deadline: Instant,
    ) -> Result<SASLAuthStep>
    where
        E: OpSASLAuthByNameEncoder,
        D: Dispatcher,
    {
        let resp = recv_op_with_deadline(deadline, self.op).await?;

        if !resp.needs_more_steps {
            return Ok(SASLAuthStep::Done);
        }

        let mut scram = match self.scram {
            Some(scram) => scram,
            // PLAIN and OAUTHBEARER have nothing to continue with, so being asked to means
            // the server and the client disagree about the mechanism in use.
            None => {
                return Err(Error::new_protocol_error(
                    "server did not accept auth when the client expected",
                ));
            }
        };

        let req = SASLStepRequest {
            payload: scram.client_final(&resp.payload)?,
            auth_mechanism: self.auth_mechanism,
        };

        let op = dispatch_op_with_deadline(deadline, encoder.sasl_step(dispatcher, req)).await?;

        Ok(SASLAuthStep::Continue(PendingSASLStep { op }))
    }

    /// Read the response away, for a caller that is abandoning the attempt.
    pub(crate) async fn discard(self, deadline: Instant) {
        discard_op_with_deadline(deadline, self.op).await;
    }
}

impl PendingSASLStep {
    pub(crate) async fn resolve(self, deadline: Instant) -> Result<()> {
        let resp = recv_op_with_deadline(deadline, self.op).await?;

        if resp.needs_more_steps {
            return Err(Error::new_protocol_error(
                "server did not accept auth when the client expected",
            ));
        }

        Ok(())
    }

    pub(crate) async fn discard(self, deadline: Instant) {
        discard_op_with_deadline(deadline, self.op).await;
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct OpsSASLAuthByName {}

impl OpsSASLAuthByName {
    /// Write the first request of an authentication attempt without waiting for its reply.
    pub(crate) async fn dispatch<E, D>(
        &self,
        encoder: &E,
        dispatcher: &D,
        deadline: Instant,
        credentials: &Credentials,
        auth_mechanism: AuthMechanism,
    ) -> Result<PendingSASLAuth>
    where
        E: OpSASLAuthByNameEncoder,
        D: Dispatcher,
    {
        let (payload, scram) = match &auth_mechanism {
            AuthMechanism::Plain => {
                let (username, password) = credentials.user_pass()?;

                (OpsSASLAuthPlain::payload(username, password), None)
            }
            AuthMechanism::ScramSha1 | AuthMechanism::ScramSha256 | AuthMechanism::ScramSha512 => {
                let (username, password) = credentials.user_pass()?;

                // This unwrap is safe, we know the mechanism is a SCRAM one.
                let mut scram = ScramClient::new(&auth_mechanism, username, password).unwrap();

                (scram.client_first()?, Some(scram))
            }
            AuthMechanism::OAuthBearer => {
                let token = credentials.jwt()?;

                (OpsSASLOAuthBearer::payload(token), None)
            }
        };

        let req = SASLAuthRequest {
            payload,
            auth_mechanism: auth_mechanism.clone(),
        };

        let op = dispatch_op_with_deadline(deadline, encoder.sasl_auth(dispatcher, req)).await?;

        Ok(PendingSASLAuth {
            auth_mechanism,
            op,
            scram,
        })
    }

    pub async fn sasl_auth_by_name<E, D>(
        &self,
        encoder: &E,
        dispatcher: &D,
        opts: SASLAuthByNameOptions,
    ) -> Result<()>
    where
        E: OpSASLAuthByNameEncoder,
        D: Dispatcher,
    {
        let pending = self
            .dispatch(
                encoder,
                dispatcher,
                opts.deadline,
                &opts.credentials,
                opts.auth_mechanism,
            )
            .await?;

        match pending.resolve(encoder, dispatcher, opts.deadline).await? {
            SASLAuthStep::Done => Ok(()),
            SASLAuthStep::Continue(step) => step.resolve(opts.deadline).await,
        }
    }
}
