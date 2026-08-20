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

//! The GSI queryport protocol — scanning a secondary index on a Couchbase
//! indexer node.
//!
//! A peer of [`memdx`](crate::memdx): it owns the indexing service's wire
//! format, request and response types, error taxonomy and HTTP surface, and it
//! owns nothing about *which* node to talk to. Pooling, topology and routing
//! live above it, the way `kvclientpool` and `vbucketrouter` live above
//! `memdx`.
//!
//! **One structural difference from `memdx`, and it shapes everything here.**
//! `memdx` multiplexes — many operations in flight on one socket, correlated by
//! opaque. Queryport does not. There is no request id on the wire; a request
//! owns its connection until the server sends a terminator, and ordering *is*
//! the correlation. So there is no opaque map and no dispatcher, and a
//! [`Client`](client::Client) serves one request at a time.
//!
//! ### Layers
//!
//! | Module | Owns |
//! |---|---|
//! | [`packet_codec`] | The six-byte frame, its checksum, and the end-of-response marker |
//! | [`proto`] | The protobuf schema, and the parse that turns it into domain types and errors |
//! | [`error`] | The taxonomy every layer lifts into |
//! | [`span`] | The bounds of a scan |
//! | [`status`] | The indexing service's HTTP surface: what indexes exist, and where |
//! | [`client`] | One authenticated connection, and the operations on it |
//! | [`scan`] | A streaming scan, and the connection it owns while it runs |

pub mod client;
pub mod error;
pub mod packet_codec;
pub mod proto;
pub mod scan;
pub mod span;
pub mod status;

#[cfg(test)]
mod client_test;
/// The scripted queryport server. `pub(crate)` rather than private to
/// `indexerx`, because the layers above it — pooling, and the streams a scan
/// hands out — are the ones whose behaviour only shows up against a server that
/// ends a scan the way a real one does.
#[cfg(test)]
pub(crate) mod test_server;

pub use client::{Client, ConnectOptions};
pub use error::{AuthCode, Error, ErrorKind, ServerError};
pub use packet_codec::{CodecError, Frame, PacketCodec, ProtocolError};
pub use proto::{
    Consistency, DataEncoding, ParseError, Projection, Response, ScanEntry, ScanOptions,
};
pub use scan::ScanStream;
pub use span::{Filter, Inclusion, Scan};
pub use status::{IndexStatus, Indexing};
