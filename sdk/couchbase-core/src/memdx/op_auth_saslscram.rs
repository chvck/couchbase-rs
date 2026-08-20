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

use hmac::Hmac;
use sha1::Sha1;
use sha2::{Sha256, Sha512};

use crate::memdx::auth_mechanism::AuthMechanism;
use crate::memdx::dispatcher::Dispatcher;
use crate::memdx::error::Error;
use crate::memdx::error::Result;
use crate::memdx::op_auth_saslplain::OpSASLPlainEncoder;
use crate::memdx::pendingop::StandardPendingOp;
use crate::memdx::request::SASLStepRequest;
use crate::memdx::response::SASLStepResponse;
use crate::scram::Client;

pub trait OpSASLScramEncoder: OpSASLPlainEncoder {
    fn sasl_step<D>(
        &self,
        dispatcher: &D,
        request: SASLStepRequest,
    ) -> impl std::future::Future<Output = Result<StandardPendingOp<SASLStepResponse>>>
    where
        D: Dispatcher;
}

/// A SCRAM client bound to one of the hashes the mechanism can negotiate.
///
/// A SCRAM exchange spans two round trips, so the client has to be carried from the one
/// that sends the client-first message to the one that answers the server's challenge.
/// Holding the hash as an enum keeps `Client`'s digest type parameters from leaking into
/// everything that carries it.
pub(crate) enum ScramClient {
    Sha1(Client<Hmac<Sha1>, Sha1>),
    Sha256(Client<Hmac<Sha256>, Sha256>),
    Sha512(Client<Hmac<Sha512>, Sha512>),
}

impl ScramClient {
    /// `None` when `mech` is not one of the SCRAM mechanisms.
    pub(crate) fn new(mech: &AuthMechanism, username: &str, password: &str) -> Option<Self> {
        let user = username.to_string();
        let pass = password.to_string();

        match mech {
            AuthMechanism::ScramSha1 => Some(ScramClient::Sha1(Client::new(user, pass, None))),
            AuthMechanism::ScramSha256 => Some(ScramClient::Sha256(Client::new(user, pass, None))),
            AuthMechanism::ScramSha512 => Some(ScramClient::Sha512(Client::new(user, pass, None))),
            _ => None,
        }
    }

    /// The client-first message.
    pub(crate) fn client_first(&mut self) -> Result<Vec<u8>> {
        let payload = match self {
            ScramClient::Sha1(client) => client.step1(),
            ScramClient::Sha256(client) => client.step1(),
            ScramClient::Sha512(client) => client.step1(),
        };

        payload
            .map_err(|e| Error::new_protocol_error("failed to perform initial sasl step").with(e))
    }

    /// The client-final message, answering the server-first message in `challenge`.
    pub(crate) fn client_final(&mut self, challenge: &[u8]) -> Result<Vec<u8>> {
        let payload = match self {
            ScramClient::Sha1(client) => client.step2(challenge),
            ScramClient::Sha256(client) => client.step2(challenge),
            ScramClient::Sha512(client) => client.step2(challenge),
        };

        payload.map_err(|e| Error::new_protocol_error("failed to perform second sasl step").with(e))
    }
}
