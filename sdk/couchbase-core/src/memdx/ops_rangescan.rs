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

//! Range scan: create a scan on one vbucket, continue it, cancel it.
//!
//! A whole-collection scan is one scan per vbucket -- placement is by hash of
//! the key rather than by key order, so there is no pruning to be had and the
//! unit of work is a fan-out of `num_vbuckets` scans rather than an operation.
//! That shape is why the two allocation decisions below are worth their
//! comments: both are paid once per vbucket per drain round, so on a
//! 1024-vbucket bucket they are paid a thousand times for one logical scan.
//!
//! `RangeScanContinue` is the crate's only **multi-response** operation: the
//! server answers one continue with a stream of packets and terminates the
//! stream with `RangeScanMore` (ask again) or `RangeScanComplete` (done). It is
//! therefore the only operation dispatched with `is_persistent = true`.
//!
//! Lifted from cbcore-rs, whose `docs/allocation-costs.md` is the study behind
//! the two optimisations here.

use std::time::Duration;

use base64::display::Base64Display;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::{Buf, Bytes};
use serde::{Serialize, Serializer};

use crate::memdx::client_response::ClientResponse;
use crate::memdx::datatype::DataTypeFlag;
use crate::memdx::dispatcher::Dispatcher;
use crate::memdx::error::{Error, Result, ServerError, ServerErrorKind};
use crate::memdx::ext_frame_code::ExtReqFrameCode;
use crate::memdx::extframe;
use crate::memdx::magic::Magic;
use crate::memdx::opcode::OpCode;
use crate::memdx::ops_crud::OpsCrud;
use crate::memdx::packet::RequestPacket;
use crate::memdx::pendingop::StandardPendingOp;
use crate::memdx::response::{TraceAttributes, TryFromClientResponse};
use crate::memdx::status::Status;

/// The scan uuid's width, fixed by the protocol.
const SCAN_UUID_LEN: usize = 16;

/// A `RangeScanContinue` extras block: scan uuid, max count, timeout, max bytes.
const CONTINUE_EXTRAS_LEN: usize = SCAN_UUID_LEN + 4 + 4 + 4;

/// The fixed-width part of a full (not keys-only) scan item: flags, expiry,
/// seqno, cas, datatype.
const ITEM_HEADER_LEN: usize = 4 + 4 + 8 + 8 + 1;

/// How much to reserve for the encoded create body.
///
/// A representative document measures ~150 bytes and `serde_json::to_vec`
/// starts at 128, so serialising into a fresh `Vec` always reallocs. This is
/// deliberately well above the common case rather than tight to it: the bounds
/// are base64 of *document keys*, so the body has no fixed upper size, and one
/// allocation of 1 KiB costs the same as one of 128 bytes.
const BODY_RESERVE: usize = 1024;

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RangeScanCreateRangeScanConfig<'a> {
    pub start: Option<&'a [u8]>,
    pub end: Option<&'a [u8]>,
    pub exclusive_start: Option<&'a [u8]>,
    pub exclusive_end: Option<&'a [u8]>,
}

#[derive(Debug, Clone)]
pub struct RangeScanCreateRandomSamplingConfig {
    pub seed: u64,
    pub samples: u64,
}

#[derive(Debug, Clone)]
pub struct RangeScanCreateSnapshotRequirements {
    pub vb_uuid: u64,
    pub seq_no: u64,
    pub seq_no_exists: bool,
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone)]
pub struct RangeScanConfig<'a> {
    pub collection_id: u32,
    pub keys_only: bool,
    pub range: Option<RangeScanCreateRangeScanConfig<'a>>,
    pub sampling: Option<RangeScanCreateRandomSamplingConfig>,
    pub snapshot: Option<RangeScanCreateSnapshotRequirements>,
}

#[derive(Debug, Clone)]
pub struct RangeScanCreateRequest<'a> {
    pub vbucket_id: u16,
    pub config: RangeScanConfig<'a>,
    pub on_behalf_of: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct RangeScanContinueRequest<'a> {
    pub scan_uuid: &'a [u8],
    pub vbucket_id: u16,
    pub max_count: u32,
    pub max_bytes: u32,
    pub timeout: Option<Duration>,
    pub on_behalf_of: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct RangeScanCancelRequest<'a> {
    pub scan_uuid: &'a [u8],
    pub vbucket_id: u16,
    pub on_behalf_of: Option<&'a str>,
}

// ---------------------------------------------------------------------------
// The create body
// ---------------------------------------------------------------------------

/// A `u64` the server expects as a JSON string.
///
/// **`collect_str` rather than `to_string`.** serde_json formats a `Display`
/// straight into its writer, so the string never exists as a `String` -- which
/// was one of three allocations this body made before it had written a byte.
fn ser_u64_str<S: Serializer>(v: &u64, s: S) -> std::result::Result<S::Ok, S::Error> {
    s.collect_str(v)
}

/// A collection id as lower-case hex, by the same route.
fn ser_hex_u32<S: Serializer>(v: &Option<u32>, s: S) -> std::result::Result<S::Ok, S::Error> {
    struct Hex(u32);
    impl std::fmt::Display for Hex {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{:x}", self.0)
        }
    }
    match v {
        Some(v) => s.collect_str(&Hex(*v)),
        None => s.serialize_none(),
    }
}

/// Bytes as base64, streamed through `Base64Display` rather than encoded into a
/// `String` first.
fn ser_base64<S: Serializer>(v: &Option<&[u8]>, s: S) -> std::result::Result<S::Ok, S::Error> {
    match v {
        Some(v) => s.collect_str(&Base64Display::new(v, &BASE64)),
        None => s.serialize_none(),
    }
}

#[derive(Serialize)]
struct RangeScanConfigJson<'a> {
    #[serde(
        serialize_with = "ser_hex_u32",
        skip_serializing_if = "Option::is_none"
    )]
    collection: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    key_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    range: Option<RangeScanRangeJson<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sampling: Option<RangeScanSampleJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot_requirements: Option<RangeScanSnapshotJson>,
}

/// **Borrowed, not owned.** The bounds are serialised straight from the
/// caller's slices; nothing is copied on the way to the wire.
#[derive(Serialize)]
struct RangeScanRangeJson<'a> {
    #[serde(serialize_with = "ser_base64", skip_serializing_if = "Option::is_none")]
    start: Option<&'a [u8]>,
    #[serde(serialize_with = "ser_base64", skip_serializing_if = "Option::is_none")]
    end: Option<&'a [u8]>,
    #[serde(serialize_with = "ser_base64", skip_serializing_if = "Option::is_none")]
    excl_start: Option<&'a [u8]>,
    #[serde(serialize_with = "ser_base64", skip_serializing_if = "Option::is_none")]
    excl_end: Option<&'a [u8]>,
}

#[derive(Serialize)]
struct RangeScanSampleJson {
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<u64>,
    samples: u64,
}

#[derive(Serialize)]
struct RangeScanSnapshotJson {
    #[serde(serialize_with = "ser_u64_str")]
    vb_uuid: u64,
    seqno: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    seqno_exists: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_ms: Option<u64>,
}

fn invalid_argument(msg: &'static str, arg: &'static str) -> Error {
    Error::new_invalid_argument_error(msg, arg.to_string())
}

impl RangeScanConfig<'_> {
    /// Serialise the scan config as the create's JSON body.
    ///
    /// The validation is not incidental: a body the server rejects costs a
    /// round trip to find out.
    fn encode(&self) -> Result<Vec<u8>> {
        if self.range.is_some() && self.sampling.is_some() {
            return Err(invalid_argument(
                "only one of range and sampling can be set",
                "config",
            ));
        }
        if self.range.is_none() && self.sampling.is_none() {
            return Err(invalid_argument(
                "one of range and sampling must be set",
                "config",
            ));
        }

        let range_json = if let Some(range) = &self.range {
            if range.start.is_some() && range.exclusive_start.is_some() {
                return Err(invalid_argument(
                    "only one of start and exclusive start within range can be set",
                    "range",
                ));
            }
            if range.end.is_some() && range.exclusive_end.is_some() {
                return Err(invalid_argument(
                    "only one of end and exclusive end within range can be set",
                    "range",
                ));
            }
            if range.start.is_none() && range.exclusive_start.is_none() {
                return Err(invalid_argument(
                    "one of start and exclusive start within range must be set",
                    "range",
                ));
            }
            if range.end.is_none() && range.exclusive_end.is_none() {
                return Err(invalid_argument(
                    "one of end and exclusive end within range must be set",
                    "range",
                ));
            }

            Some(RangeScanRangeJson {
                start: range.start,
                end: range.end,
                excl_start: range.exclusive_start,
                excl_end: range.exclusive_end,
            })
        } else {
            None
        };

        let sampling_json = if let Some(sampling) = &self.sampling {
            if sampling.samples == 0 {
                return Err(invalid_argument(
                    "samples within sampling must be set",
                    "sampling",
                ));
            }
            Some(RangeScanSampleJson {
                seed: if sampling.seed != 0 {
                    Some(sampling.seed)
                } else {
                    None
                },
                samples: sampling.samples,
            })
        } else {
            None
        };

        let snapshot_json = if let Some(snapshot) = &self.snapshot {
            if snapshot.vb_uuid == 0 {
                return Err(invalid_argument(
                    "vbuuid within snapshot must be set",
                    "snapshot",
                ));
            }
            if snapshot.seq_no == 0 {
                return Err(invalid_argument(
                    "seqno within snapshot must be set",
                    "snapshot",
                ));
            }
            Some(RangeScanSnapshotJson {
                vb_uuid: snapshot.vb_uuid,
                seqno: snapshot.seq_no,
                seqno_exists: if snapshot.seq_no_exists {
                    Some(true)
                } else {
                    None
                },
                timeout_ms: snapshot.timeout.map(|t| t.as_millis() as u64),
            })
        } else {
            None
        };

        let config_json = RangeScanConfigJson {
            collection: if self.collection_id != 0 {
                Some(self.collection_id)
            } else {
                None
            },
            key_only: if self.keys_only { Some(true) } else { None },
            range: range_json,
            sampling: sampling_json,
            snapshot_requirements: snapshot_json,
        };

        // Presized and written through, rather than `to_vec`'s 128-byte start
        // and the realloc this document always needs.
        let mut out = Vec::with_capacity(BODY_RESERVE);
        serde_json::to_writer(&mut out, &config_json)
            .map_err(|e| Error::new_protocol_error(format!("json encoding failed: {e}")))?;
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RangeScanCreateResponse {
    pub scan_uuid: Bytes,
    pub server_duration: Option<Duration>,
}

impl TryFromClientResponse for RangeScanCreateResponse {
    fn try_from(resp: ClientResponse) -> Result<Self> {
        let packet = resp.packet();

        match packet.status {
            Status::Success => {}
            // A vbucket holding nothing in range answers `KeyNotFound` rather
            // than opening an empty scan.
            Status::KeyNotFound => {
                return Err(server_error(&packet, ServerErrorKind::KeyNotFound));
            }
            // The requested seqno is not in the vbucket's history.
            Status::NotStored => {
                return Err(server_error(&packet, ServerErrorKind::NotStored));
            }
            Status::RangeError => {
                return Err(server_error(&packet, ServerErrorKind::RangeError));
            }
            Status::RangeScanVBUUIDNotEqual => {
                return Err(server_error(
                    &packet,
                    ServerErrorKind::RangeScanVBUUIDNotEqual,
                ));
            }
            _ => return Err(OpsCrud::decode_common_error(&packet)),
        }

        let server_duration = match &packet.framing_extras {
            Some(f) => extframe::decode_res_ext_frames(f)?,
            None => None,
        };

        let scan_uuid = packet.value.unwrap_or_default();
        if scan_uuid.len() != SCAN_UUID_LEN {
            return Err(Error::new_protocol_error(format!(
                "range scan create returned a {} byte scan uuid, expected {SCAN_UUID_LEN}",
                scan_uuid.len()
            )));
        }

        Ok(RangeScanCreateResponse {
            scan_uuid,
            server_duration,
        })
    }
}

impl TraceAttributes for RangeScanCreateResponse {
    fn server_duration(&self) -> Option<Duration> {
        self.server_duration
    }
}

/// Where a continue's stream of packets has got to.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum RangeScanIterState {
    /// More packets are coming for *this* continue. Keep reading.
    Ongoing,
    /// This continue is finished and the scan is not: send another.
    NeedsContinue,
    /// The scan is finished. The server has released it; do not cancel.
    Complete,
}

#[derive(Debug)]
pub struct RangeScanContinueResponse {
    pub items: RangeScanItemIter,
    pub stream_state: RangeScanIterState,
    pub server_duration: Option<Duration>,
}

impl TryFromClientResponse for RangeScanContinueResponse {
    fn try_from(resp: ClientResponse) -> Result<Self> {
        let packet = resp.packet();

        let stream_state = match packet.status {
            Status::Success => RangeScanIterState::Ongoing,
            Status::RangeScanMore => RangeScanIterState::NeedsContinue,
            Status::RangeScanComplete => RangeScanIterState::Complete,
            Status::KeyNotFound => {
                return Err(server_error(&packet, ServerErrorKind::KeyNotFound));
            }
            Status::RangeScanCancelled => {
                return Err(server_error(&packet, ServerErrorKind::RangeScanCancelled));
            }
            _ => return Err(OpsCrud::decode_common_error(&packet)),
        };

        let extras = packet.extras.as_ref().ok_or_else(|| {
            Error::new_protocol_error("range scan continue response had no extras")
        })?;
        if extras.len() != 4 {
            return Err(Error::new_protocol_error(format!(
                "range scan continue response had {} extras bytes, expected 4",
                extras.len()
            )));
        }
        let includes_content = u32::from_be_bytes([extras[0], extras[1], extras[2], extras[3]]);

        let server_duration = match &packet.framing_extras {
            Some(f) => extframe::decode_res_ext_frames(f)?,
            None => None,
        };

        let value = packet.value.unwrap_or_default();
        let items = if includes_content == 0 {
            RangeScanItemIter::KeyOnly(RangeScanKeyOnlyItemIter { data: value })
        } else {
            RangeScanItemIter::Full(RangeScanFullItemIter { data: value })
        };

        Ok(RangeScanContinueResponse {
            items,
            stream_state,
            server_duration,
        })
    }
}

/// What one continue achieved, once its whole stream of packets has been read.
///
/// `more` and `complete` are exclusive and both can be false: a continue whose
/// last packet was `Success` has neither finished the scan nor exhausted itself,
/// which is the state the server reports while it is still streaming.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct RangeScanContinueSummary {
    pub more: bool,
    pub complete: bool,
    pub server_duration: Option<Duration>,
}

impl TraceAttributes for RangeScanContinueSummary {
    fn server_duration(&self) -> Option<Duration> {
        self.server_duration
    }
}

#[derive(Debug, Clone)]
pub struct RangeScanCancelResponse {
    pub server_duration: Option<Duration>,
}

impl TryFromClientResponse for RangeScanCancelResponse {
    fn try_from(resp: ClientResponse) -> Result<Self> {
        let packet = resp.packet();

        match packet.status {
            Status::Success => {}
            Status::KeyNotFound => {
                return Err(server_error(&packet, ServerErrorKind::KeyNotFound));
            }
            _ => return Err(OpsCrud::decode_common_error(&packet)),
        }

        let server_duration = match &packet.framing_extras {
            Some(f) => extframe::decode_res_ext_frames(f)?,
            None => None,
        };

        Ok(RangeScanCancelResponse { server_duration })
    }
}

impl TraceAttributes for RangeScanCancelResponse {
    fn server_duration(&self) -> Option<Duration> {
        self.server_duration
    }
}

fn server_error(packet: &crate::memdx::packet::ResponsePacket, kind: ServerErrorKind) -> Error {
    ServerError::new(kind, packet.op_code, packet.status, packet.opaque).into()
}

// ---------------------------------------------------------------------------
// Items
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct RangeScanFullItem {
    pub key: Bytes,
    pub value: Bytes,
    pub flags: u32,
    pub cas: u64,
    pub expiry: u32,
    pub seqno: u64,
    pub datatype: u8,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct RangeScanKeyOnlyItem {
    pub key: Bytes,
}

/// The items in one continue response packet.
///
/// **Iterators over the packet's own buffer, not a `Vec` of decoded items.**
/// Every key and value is a `Bytes` slice of the received frame, so walking a
/// response allocates nothing however many documents it carries -- which is
/// what keeps the per-document term of a scan's cost at zero. Collecting them
/// is the caller's decision to pay for.
#[derive(Debug)]
pub enum RangeScanItemIter {
    Full(RangeScanFullItemIter),
    KeyOnly(RangeScanKeyOnlyItemIter),
}

#[derive(Debug)]
pub struct RangeScanFullItemIter {
    data: Bytes,
}

impl Iterator for RangeScanFullItemIter {
    type Item = Result<RangeScanFullItem>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.data.is_empty() {
            return None;
        }

        if self.data.len() < ITEM_HEADER_LEN {
            self.data.clear();
            return Some(Err(Error::new_protocol_error(
                "range scan item was shorter than its fixed header",
            )));
        }

        let header = self.data.split_to(ITEM_HEADER_LEN);
        let flags = u32::from_be_bytes(header[0..4].try_into().unwrap());
        let expiry = u32::from_be_bytes(header[4..8].try_into().unwrap());
        let seqno = u64::from_be_bytes(header[8..16].try_into().unwrap());
        let cas = u64::from_be_bytes(header[16..24].try_into().unwrap());
        let datatype = header[24];

        let key = match take_uleb_prefixed(&mut self.data) {
            Ok(k) => k,
            Err(e) => {
                self.data.clear();
                return Some(Err(e));
            }
        };
        let value = match take_uleb_prefixed(&mut self.data) {
            Ok(v) => v,
            Err(e) => {
                self.data.clear();
                return Some(Err(e));
            }
        };

        Some(Ok(RangeScanFullItem {
            key,
            value,
            flags,
            cas,
            expiry,
            seqno,
            datatype,
        }))
    }
}

#[derive(Debug)]
pub struct RangeScanKeyOnlyItemIter {
    data: Bytes,
}

impl Iterator for RangeScanKeyOnlyItemIter {
    type Item = Result<RangeScanKeyOnlyItem>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.data.is_empty() {
            return None;
        }

        match take_uleb_prefixed(&mut self.data) {
            Ok(key) => Some(Ok(RangeScanKeyOnlyItem { key })),
            Err(e) => {
                self.data.clear();
                Some(Err(e))
            }
        }
    }
}

/// Split off a uleb128-length-prefixed byte string, advancing `data` past both.
///
/// The returned `Bytes` shares the frame's buffer; this does not copy.
fn take_uleb_prefixed(data: &mut Bytes) -> Result<Bytes> {
    let mut len: u64 = 0;
    let mut shift = 0u32;
    loop {
        if data.is_empty() {
            return Err(Error::new_protocol_error(
                "range scan item length ran off the end of the packet",
            ));
        }
        let byte = data[0];
        data.advance(1);
        len |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift >= 64 {
            return Err(Error::new_protocol_error(
                "range scan item length was not a valid uleb128",
            ));
        }
    }

    let len = usize::try_from(len)
        .map_err(|_| Error::new_protocol_error("range scan item length overflowed"))?;
    if len > data.len() {
        return Err(Error::new_protocol_error(
            "range scan item claimed more bytes than the packet held",
        ));
    }
    Ok(data.split_to(len))
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct OpsRangeScan {
    pub ext_frames_enabled: bool,
}

impl OpsRangeScan {
    /// Encode the on-behalf-of frame, which is the only framing extra a range
    /// scan uses.
    fn encode_req_ext_frames(
        &self,
        on_behalf_of: Option<&str>,
        buf: &mut [u8],
    ) -> Result<(Magic, usize)> {
        let mut offset = 0;

        if let Some(obo) = on_behalf_of {
            if !self.ext_frames_enabled {
                return Err(invalid_argument(
                    "cannot use framing extras when its not enabled",
                    "on_behalf_of",
                ));
            }
            extframe::append_ext_frame(
                ExtReqFrameCode::OnBehalfOf,
                obo.as_bytes(),
                buf,
                &mut offset,
            )?;
        }

        let magic = if offset > 0 {
            Magic::ReqExt
        } else {
            Magic::Req
        };

        Ok((magic, offset))
    }

    pub async fn range_scan_create<D>(
        &self,
        dispatcher: &D,
        request: RangeScanCreateRequest<'_>,
    ) -> Result<StandardPendingOp<RangeScanCreateResponse>>
    where
        D: Dispatcher,
    {
        let mut ext_frame_buf = [0; 128];
        let (magic, used) = self.encode_req_ext_frames(request.on_behalf_of, &mut ext_frame_buf)?;
        let framing_extras = if used > 0 {
            Some(&ext_frame_buf[..used])
        } else {
            None
        };

        let value = request.config.encode()?;

        let packet = RequestPacket {
            magic,
            op_code: OpCode::RangeScanCreate,
            datatype: u8::from(DataTypeFlag::Json),
            vbucket_id: Some(request.vbucket_id),
            cas: None,
            extras: None,
            key: None,
            value: Some(&value),
            framing_extras,
            opaque: None,
        };

        let pending_op = dispatcher.dispatch(packet, false, None).await?;

        Ok(StandardPendingOp::new(pending_op))
    }

    /// Dispatch a continue, which answers with a **stream** of packets.
    ///
    /// `is_persistent` is `true`: the opaque stays registered after the first
    /// reply, and the caller reads until a response reports `NeedsContinue` or
    /// `Complete`. Dropping the returned op unregisters the opaque, so it must
    /// not be dropped while the server still has packets to send -- see
    /// `KvClientOps::range_scan_continue`.
    pub async fn range_scan_continue<D>(
        &self,
        dispatcher: &D,
        request: RangeScanContinueRequest<'_>,
    ) -> Result<StandardPendingOp<RangeScanContinueResponse>>
    where
        D: Dispatcher,
    {
        let mut ext_frame_buf = [0; 128];
        let (magic, used) = self.encode_req_ext_frames(request.on_behalf_of, &mut ext_frame_buf)?;
        let framing_extras = if used > 0 {
            Some(&ext_frame_buf[..used])
        } else {
            None
        };

        if request.scan_uuid.len() != SCAN_UUID_LEN {
            return Err(invalid_argument("scan uuid must be 16 bytes", "scan_uuid"));
        }

        let timeout_ms = request
            .timeout
            .map(|d| d.as_millis().min(u128::from(u32::MAX)) as u32)
            .unwrap_or(0);

        // **A stack array, not a `Vec`.** The extras block is 28 bytes fixed by
        // the protocol, so heap-allocating it bought nothing and cost one
        // allocation per continue -- which is one per vbucket on every drain
        // round of a fan-out.
        let mut extra_buf = [0u8; CONTINUE_EXTRAS_LEN];
        extra_buf[..SCAN_UUID_LEN].copy_from_slice(request.scan_uuid);
        extra_buf[16..20].copy_from_slice(&request.max_count.to_be_bytes());
        extra_buf[20..24].copy_from_slice(&timeout_ms.to_be_bytes());
        extra_buf[24..28].copy_from_slice(&request.max_bytes.to_be_bytes());

        let packet = RequestPacket {
            magic,
            op_code: OpCode::RangeScanContinue,
            datatype: 0,
            vbucket_id: Some(request.vbucket_id),
            cas: None,
            extras: Some(&extra_buf),
            key: None,
            value: None,
            framing_extras,
            opaque: None,
        };

        let pending_op = dispatcher.dispatch(packet, true, None).await?;

        Ok(StandardPendingOp::new(pending_op))
    }

    pub async fn range_scan_cancel<D>(
        &self,
        dispatcher: &D,
        request: RangeScanCancelRequest<'_>,
    ) -> Result<StandardPendingOp<RangeScanCancelResponse>>
    where
        D: Dispatcher,
    {
        let mut ext_frame_buf = [0; 128];
        let (magic, used) = self.encode_req_ext_frames(request.on_behalf_of, &mut ext_frame_buf)?;
        let framing_extras = if used > 0 {
            Some(&ext_frame_buf[..used])
        } else {
            None
        };

        if request.scan_uuid.len() != SCAN_UUID_LEN {
            return Err(invalid_argument("scan uuid must be 16 bytes", "scan_uuid"));
        }

        // Fixed by the protocol at the scan uuid's width; see the continue.
        let mut extra_buf = [0u8; SCAN_UUID_LEN];
        extra_buf.copy_from_slice(request.scan_uuid);

        let packet = RequestPacket {
            magic,
            op_code: OpCode::RangeScanCancel,
            datatype: 0,
            vbucket_id: Some(request.vbucket_id),
            cas: None,
            extras: Some(&extra_buf),
            key: None,
            value: None,
            framing_extras,
            opaque: None,
        };

        let pending_op = dispatcher.dispatch(packet, false, None).await?;

        Ok(StandardPendingOp::new(pending_op))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The body must not change when how it is built does.**
    ///
    /// These strings were captured from cbcore-rs's implementation that built
    /// the JSON through owned `String`s -- a `to_string` for the vbucket uuid, a
    /// `format!` for the collection id and a `BASE64.encode` per bound -- before
    /// any of that was replaced with the streaming serialisers above. They are a
    /// golden record of what the server was already being sent, not a
    /// restatement of what the current code does, which is the only reason they
    /// are evidence.
    ///
    /// The cases between them cover every branch that alters the output: an
    /// absent collection and a present one, inclusive and exclusive bounds, a
    /// range and a sampling config, a snapshot with and without its optional
    /// fields, an empty bound (base64 of nothing) and bounds carrying `0xff`
    /// and `0x00` (which distinguish base64 from any accidental text handling).
    #[test]
    fn json_body_is_unchanged_by_the_streaming_serialisers() {
        let range_only = RangeScanConfig {
            collection_id: 0,
            keys_only: false,
            range: Some(RangeScanCreateRangeScanConfig {
                start: Some(b""),
                end: Some(&[0xff; 16]),
                exclusive_start: None,
                exclusive_end: None,
            }),
            sampling: None,
            snapshot: None,
        };
        assert_eq!(
            String::from_utf8(range_only.encode().unwrap()).unwrap(),
            r#"{"range":{"start":"","end":"/////////////////////w=="}}"#
        );

        let full = RangeScanConfig {
            collection_id: 0x1a2b,
            keys_only: true,
            range: Some(RangeScanCreateRangeScanConfig {
                start: None,
                end: None,
                exclusive_start: Some(b"abc"),
                exclusive_end: Some(b"xyz\xff\x00"),
            }),
            sampling: None,
            snapshot: Some(RangeScanCreateSnapshotRequirements {
                vb_uuid: 194_620_573_618_909,
                seq_no: 41_233,
                seq_no_exists: true,
                timeout: Some(Duration::from_secs(30)),
            }),
        };
        assert_eq!(
            String::from_utf8(full.encode().unwrap()).unwrap(),
            r#"{"collection":"1a2b","key_only":true,"range":{"excl_start":"YWJj","excl_end":"eHl6/wA="},"snapshot_requirements":{"vb_uuid":"194620573618909","seqno":41233,"seqno_exists":true,"timeout_ms":30000}}"#
        );

        let sampling = RangeScanConfig {
            collection_id: 9,
            keys_only: false,
            range: None,
            sampling: Some(RangeScanCreateRandomSamplingConfig {
                seed: 77,
                samples: 5,
            }),
            snapshot: Some(RangeScanCreateSnapshotRequirements {
                vb_uuid: 1,
                seq_no: 2,
                seq_no_exists: false,
                timeout: None,
            }),
        };
        assert_eq!(
            String::from_utf8(sampling.encode().unwrap()).unwrap(),
            r#"{"collection":"9","sampling":{"seed":77,"samples":5},"snapshot_requirements":{"vb_uuid":"1","seqno":2}}"#
        );
    }

    /// The body is written into a buffer sized up front, not one grown into.
    ///
    /// **This is asserted here rather than in the allocation test** because it
    /// only shows up for a body over 128 bytes, which is `serde_json::to_vec`'s
    /// starting capacity: a scan of short keys with no snapshot requirement fits
    /// inside that and costs the same either way. A representative body -- the
    /// golden `full` case above, which any snapshot-pinned scan produces -- does
    /// not, and `to_vec` reallocs for it every time.
    #[test]
    fn the_create_body_is_presized_rather_than_grown() {
        let full = RangeScanConfig {
            collection_id: 0x1a2b,
            keys_only: true,
            range: Some(RangeScanCreateRangeScanConfig {
                start: None,
                end: None,
                exclusive_start: Some(b"abc"),
                exclusive_end: Some(b"xyz\xff\x00"),
            }),
            sampling: None,
            snapshot: Some(RangeScanCreateSnapshotRequirements {
                vb_uuid: 194_620_573_618_909,
                seq_no: 41_233,
                seq_no_exists: true,
                timeout: Some(Duration::from_secs(30)),
            }),
        };

        let body = full.encode().unwrap();
        assert!(
            body.len() > 128,
            "a representative body is {} bytes, which no longer outgrows \
             `serde_json::to_vec`'s 128 byte start -- so the reserve this test \
             guards is not buying the realloc it was measured to buy",
            body.len()
        );
        assert!(
            body.capacity() >= BODY_RESERVE,
            "the body ended up with a capacity of {}, so it was grown as it was \
             written rather than sized once",
            body.capacity()
        );
    }

    /// The validation `encode` performs is not incidental: a body the server
    /// rejects costs a round trip to find out.
    #[test]
    fn contradictory_configs_are_refused() {
        let neither = RangeScanConfig {
            collection_id: 0,
            keys_only: false,
            range: None,
            sampling: None,
            snapshot: None,
        };
        assert!(neither.encode().is_err());

        let both_starts = RangeScanConfig {
            collection_id: 0,
            keys_only: false,
            range: Some(RangeScanCreateRangeScanConfig {
                start: Some(b"a"),
                end: Some(b"z"),
                exclusive_start: Some(b"a"),
                exclusive_end: None,
            }),
            sampling: None,
            snapshot: None,
        };
        assert!(both_starts.encode().is_err());
    }

    /// The item framing is a fixed header plus two uleb128-prefixed strings,
    /// and every field of both comes out of the packet's own buffer.
    #[test]
    fn full_items_decode_out_of_the_packets_buffer() {
        let mut body = Vec::new();
        for (key, value) in [(&b"aa"[..], &b"{\"v\":1}"[..]), (&b"bbb"[..], &b"[]"[..])] {
            body.extend_from_slice(&7u32.to_be_bytes()); // flags
            body.extend_from_slice(&0u32.to_be_bytes()); // expiry
            body.extend_from_slice(&11u64.to_be_bytes()); // seqno
            body.extend_from_slice(&12u64.to_be_bytes()); // cas
            body.push(1); // datatype
            body.push(key.len() as u8);
            body.extend_from_slice(key);
            body.push(value.len() as u8);
            body.extend_from_slice(value);
        }

        let items: Vec<RangeScanFullItem> = RangeScanFullItemIter {
            data: Bytes::from(body),
        }
        .map(|i| i.unwrap())
        .collect();

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].key, Bytes::from_static(b"aa"));
        assert_eq!(items[0].value, Bytes::from_static(b"{\"v\":1}"));
        assert_eq!(items[0].flags, 7);
        assert_eq!(items[0].seqno, 11);
        assert_eq!(items[0].cas, 12);
        assert_eq!(items[0].datatype, 1);
        assert_eq!(items[1].key, Bytes::from_static(b"bbb"));
        assert_eq!(items[1].value, Bytes::from_static(b"[]"));
    }

    #[test]
    fn key_only_items_decode() {
        let body = vec![2u8, b'a', b'a', 3, b'b', b'b', b'b'];
        let items: Vec<RangeScanKeyOnlyItem> = RangeScanKeyOnlyItemIter {
            data: Bytes::from(body),
        }
        .map(|i| i.unwrap())
        .collect();

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].key, Bytes::from_static(b"aa"));
        assert_eq!(items[1].key, Bytes::from_static(b"bbb"));
    }

    /// A truncated frame must report rather than panic or loop: the length
    /// prefix is server-supplied data.
    #[test]
    fn a_truncated_item_is_an_error_not_a_panic() {
        let mut iter = RangeScanKeyOnlyItemIter {
            data: Bytes::from_static(&[9, b'a', b'a']),
        };
        assert!(iter.next().unwrap().is_err());
        assert!(iter.next().is_none());

        let mut iter = RangeScanFullItemIter {
            data: Bytes::from_static(&[0, 0, 0, 1]),
        };
        assert!(iter.next().unwrap().is_err());
        assert!(iter.next().is_none());
    }

    /// A multi-byte length, which a value over 127 bytes takes.
    #[test]
    fn multi_byte_uleb_lengths_decode() {
        let mut body = vec![0x80, 0x01];
        body.extend_from_slice(&[b'x'; 128]);
        let mut data = Bytes::from(body);
        let out = take_uleb_prefixed(&mut data).unwrap();
        assert_eq!(out.len(), 128);
        assert!(data.is_empty());
    }
}
