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

use bytes::Bytes;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug)]
#[non_exhaustive]
pub struct Request {
    pub(crate) method: http::Method,
    pub(crate) uri: String,
    pub(crate) auth: Option<Auth>,
    pub(crate) user_agent: Option<String>,
    pub(crate) content_type: Option<String>,
    pub(crate) body: Option<Bytes>,
    pub(crate) headers: HashMap<String, String>,
    pub(crate) unique_id: Option<String>,
    /// Whose permissions the server should apply, which is **not** the same
    /// question as who the request authenticates as.
    ///
    /// It sits beside [`Request::auth`] rather than inside it because the
    /// domain form needs both: the server authenticates the request from its
    /// credentials and reads the header only to decide whose rights to use. It
    /// was once an `Auth` variant, which left it occupying the slot the
    /// credentials needed and made a correct request unrepresentable.
    pub(crate) on_behalf_of: Option<OnBehalfOfInfo>,
}

impl Request {
    pub fn new(method: http::Method, uri: impl Into<String>) -> Self {
        Self {
            method,
            uri: uri.into(),
            auth: None,
            user_agent: None,
            content_type: None,
            body: None,
            headers: HashMap::new(),
            unique_id: None,
            on_behalf_of: None,
        }
    }

    pub fn auth(mut self, auth: impl Into<Option<Auth>>) -> Self {
        self.auth = auth.into();
        self
    }

    pub fn user_agent(mut self, user_agent: impl Into<Option<String>>) -> Self {
        self.user_agent = user_agent.into();
        self
    }

    pub fn content_type(mut self, content_type: impl Into<Option<String>>) -> Self {
        self.content_type = content_type.into();
        self
    }

    pub fn body(mut self, body: impl Into<Option<Bytes>>) -> Self {
        self.body = body.into();
        self
    }

    pub fn unique_id(mut self, unique_id: impl Into<Option<String>>) -> Self {
        self.unique_id = unique_id.into();
        self
    }

    pub fn add_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(key.into(), value.into());
        self
    }

    pub fn on_behalf_of(mut self, on_behalf_of: impl Into<Option<OnBehalfOfInfo>>) -> Self {
        self.on_behalf_of = on_behalf_of.into();
        self
    }
}

#[derive(PartialEq, Eq, Debug, Clone)]
pub struct BasicAuth {
    pub(crate) username: String,
    pub(crate) password: String,
}

impl BasicAuth {
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
        }
    }
}

#[derive(PartialEq, Eq, Debug, Clone)]
pub struct BearerAuth {
    pub(crate) token: String,
}

impl BearerAuth {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum Auth {
    BasicAuth(BasicAuth),
    BearerAuth(BearerAuth),
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize)]
#[non_exhaustive]
pub struct OnBehalfOfInfo {
    pub(crate) username: String,
    pub(crate) password_or_domain: OboPasswordOrDomain,
}

impl OnBehalfOfInfo {
    pub fn new(username: impl Into<String>, password_or_domain: OboPasswordOrDomain) -> Self {
        Self {
            username: username.into(),
            password_or_domain,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum OboPasswordOrDomain {
    Password(String),
    Domain(String),
}
