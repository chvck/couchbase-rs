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

//! The queryport frame.
//!
//! ```text
//! +--------+--------+--------+--------+--------+--------+============+
//! |         uint32 length (BE)        |  uint16 flags   |  payload   |
//! +--------+--------+--------+--------+--------+--------+============+
//! ```
//!
//! `length` counts the payload only. `flags` packs three things:
//!
//! ```text
//! bits 0-3    compression   0 = none, the only value the server implements
//! bits 4-7    encoding      0x10 = protobuf, the only valid value
//! bits 8-14   checksum
//! bit  15     reserved
//! ```
//!
//! A frame with `length == 0` and `flags == 0` is the **end-of-response
//! marker**. It carries no payload and is not a protobuf message; decoding it
//! as one is the mistake this codec exists to make impossible, so it decodes to
//! its own variant.
//!
//! Everything this layer can decide, it decides here — an unknown encoding, a
//! checksum that does not match, a length past the configured maximum. Nothing
//! downstream re-checks a frame, because by the time a frame is downstream it
//! is known good.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

/// Payload encoding, from bits 4-7 of the flags.
///
/// Protobuf is the only value the server accepts and the only one we send. It
/// is an enum rather than a constant so that an unknown encoding is a value we
/// can name in an error rather than a bare number.
const ENCODING_PROTOBUF: u8 = 0x10;
const ENCODING_MASK: u16 = 0x00F0;
const COMPRESSION_MASK: u16 = 0x000F;
const CHECKSUM_MASK: u16 = 0x7F00;
const CHECKSUM_SHIFT: u16 = 8;

const HEADER_LEN: usize = 6;

/// The default ceiling on a single payload, matching the Go client's
/// `maxPayload`. It is a codec parameter rather than a constant because a
/// scan's response batch size is configurable on the server and a client that
/// cannot be told about it would fail on a legitimately large frame.
pub const DEFAULT_MAX_PAYLOAD: usize = 1000 * 1024;

/// A frame as it appears on the wire, before anyone has decided what its
/// payload means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// A protobuf `QueryPayload`, still encoded.
    Payload(Bytes),
    /// `length == 0 && flags == 0` — the server has finished responding.
    EndOfResponse,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    /// The encoding nibble named something other than protobuf.
    UnknownEncoding(u8),
    /// The compression nibble named a scheme the server never sends and we
    /// have therefore never implemented.
    UnsupportedCompression(u8),
    /// The header's checksum does not describe the header's length.
    ChecksumMismatch { expected: u8, actual: u8 },
    /// The frame claims a payload larger than this codec was configured for.
    PayloadTooLong { len: usize, max: usize },
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtocolError::UnknownEncoding(e) => {
                write!(f, "unknown payload encoding 0x{e:02x}")
            }
            ProtocolError::UnsupportedCompression(c) => {
                write!(f, "unsupported compression scheme {c}")
            }
            ProtocolError::ChecksumMismatch { expected, actual } => {
                write!(
                    f,
                    "header checksum mismatch: expected {expected}, computed {actual}"
                )
            }
            ProtocolError::PayloadTooLong { len, max } => {
                write!(f, "payload of {len} bytes exceeds the maximum of {max}")
            }
        }
    }
}

impl std::error::Error for ProtocolError {}

#[derive(Debug)]
pub enum CodecError {
    Io(std::io::Error),
    Protocol(ProtocolError),
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodecError::Io(e) => write!(f, "indexerx codec IO error: {e}"),
            CodecError::Protocol(e) => write!(f, "indexerx codec protocol error: {e}"),
        }
    }
}

impl std::error::Error for CodecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CodecError::Io(e) => Some(e),
            CodecError::Protocol(e) => Some(e),
        }
    }
}

impl From<std::io::Error> for CodecError {
    fn from(e: std::io::Error) -> Self {
        CodecError::Io(e)
    }
}

impl From<ProtocolError> for CodecError {
    fn from(e: ProtocolError) -> Self {
        CodecError::Protocol(e)
    }
}

/// The checksum the header carries, computed over the *length field* rather
/// than the payload.
///
/// It is seven bits drawn from the low nibbles of the two least significant
/// length bytes, which makes it a weak check — it catches a truncated or
/// shifted length and nothing else. That is what it is for: a frame whose
/// length is wrong desynchronises the stream permanently, and every subsequent
/// read is garbage, so it is worth four instructions to fail on the first one.
fn checksum(len_be: [u8; 4]) -> u8 {
    let mut ck = len_be[3] & 0x0F;
    ck <<= 4;
    ck |= len_be[2] & 0x0F;
    ck & 0x7F
}

pub struct PacketCodec {
    max_payload: usize,
}

impl PacketCodec {
    pub fn new(max_payload: usize) -> Self {
        PacketCodec { max_payload }
    }
}

impl Default for PacketCodec {
    fn default() -> Self {
        PacketCodec::new(DEFAULT_MAX_PAYLOAD)
    }
}

impl Decoder for PacketCodec {
    type Item = Frame;
    type Error = CodecError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Frame>, CodecError> {
        if src.len() < HEADER_LEN {
            src.reserve(HEADER_LEN - src.len());
            return Ok(None);
        }

        let len_be: [u8; 4] = src[0..4].try_into().expect("four bytes");
        let payload_len = u32::from_be_bytes(len_be) as usize;
        let flags = u16::from_be_bytes(src[4..6].try_into().expect("two bytes"));

        // The end-of-response marker, and the only frame for which zero flags
        // are legal. Checked before the encoding nibble, because a marker has
        // no encoding to name.
        if payload_len == 0 && flags == 0 {
            src.advance(HEADER_LEN);
            return Ok(Some(Frame::EndOfResponse));
        }

        if payload_len > self.max_payload {
            return Err(ProtocolError::PayloadTooLong {
                len: payload_len,
                max: self.max_payload,
            }
            .into());
        }

        let encoding = (flags & ENCODING_MASK) as u8;
        if encoding != ENCODING_PROTOBUF {
            return Err(ProtocolError::UnknownEncoding(encoding).into());
        }

        let compression = (flags & COMPRESSION_MASK) as u8;
        if compression != 0 {
            return Err(ProtocolError::UnsupportedCompression(compression).into());
        }

        // Zero means the sender declined to checksum, which the protocol
        // permits; anything else must be right.
        let carried = ((flags & CHECKSUM_MASK) >> CHECKSUM_SHIFT) as u8;
        if carried != 0 {
            let computed = checksum(len_be);
            if carried != computed {
                return Err(ProtocolError::ChecksumMismatch {
                    expected: carried,
                    actual: computed,
                }
                .into());
            }
        }

        if src.len() < HEADER_LEN + payload_len {
            src.reserve(HEADER_LEN + payload_len - src.len());
            return Ok(None);
        }

        src.advance(HEADER_LEN);
        Ok(Some(Frame::Payload(src.split_to(payload_len).freeze())))
    }
}

impl Encoder<Frame> for PacketCodec {
    type Error = CodecError;

    fn encode(&mut self, item: Frame, dst: &mut BytesMut) -> Result<(), CodecError> {
        match item {
            Frame::EndOfResponse => {
                // A client never sends this — it is the server's terminator —
                // but encoding it correctly costs nothing and makes the codec
                // round-trippable, which is what its tests assert.
                dst.reserve(HEADER_LEN);
                dst.put_u32(0);
                dst.put_u16(0);
            }
            Frame::Payload(payload) => {
                if payload.len() > self.max_payload {
                    return Err(ProtocolError::PayloadTooLong {
                        len: payload.len(),
                        max: self.max_payload,
                    }
                    .into());
                }
                let len_be = (payload.len() as u32).to_be_bytes();
                let flags =
                    ENCODING_PROTOBUF as u16 | ((checksum(len_be) as u16) << CHECKSUM_SHIFT);

                dst.reserve(HEADER_LEN + payload.len());
                dst.put_slice(&len_be);
                dst.put_u16(flags);
                dst.put_slice(&payload);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(frame: Frame) -> Frame {
        let mut codec = PacketCodec::default();
        let mut buf = BytesMut::new();
        codec.encode(frame, &mut buf).expect("encode");
        codec
            .decode(&mut buf)
            .expect("decode")
            .expect("a whole frame")
    }

    #[test]
    fn payload_survives_a_round_trip() {
        let frame = Frame::Payload(Bytes::from_static(b"\x08\x01hello"));
        assert_eq!(roundtrip(frame.clone()), frame);
    }

    #[test]
    fn the_end_marker_is_not_an_empty_payload() {
        // The distinction the whole codec exists for: a zero-length frame with
        // zero flags terminates the response and must never reach the protobuf
        // decoder as an empty message.
        assert_eq!(roundtrip(Frame::EndOfResponse), Frame::EndOfResponse);
    }

    #[test]
    fn a_partial_header_yields_no_frame() {
        let mut codec = PacketCodec::default();
        let mut buf = BytesMut::from(&b"\x00\x00"[..]);
        assert_eq!(codec.decode(&mut buf).expect("decode"), None);
        assert_eq!(buf.len(), 2, "the partial header is left for the next read");
    }

    #[test]
    fn a_partial_payload_yields_no_frame() {
        let mut codec = PacketCodec::default();
        let mut buf = BytesMut::new();
        codec
            .encode(Frame::Payload(Bytes::from_static(b"0123456789")), &mut buf)
            .expect("encode");
        buf.truncate(HEADER_LEN + 4);

        assert_eq!(codec.decode(&mut buf).expect("decode"), None);
        assert_eq!(buf.len(), HEADER_LEN + 4, "nothing was consumed");
    }

    #[test]
    fn two_frames_in_one_buffer_decode_separately() {
        let mut codec = PacketCodec::default();
        let mut buf = BytesMut::new();
        codec
            .encode(Frame::Payload(Bytes::from_static(b"one")), &mut buf)
            .expect("encode");
        codec
            .encode(Frame::Payload(Bytes::from_static(b"two")), &mut buf)
            .expect("encode");
        codec
            .encode(Frame::EndOfResponse, &mut buf)
            .expect("encode");

        assert_eq!(
            codec.decode(&mut buf).unwrap(),
            Some(Frame::Payload(Bytes::from_static(b"one")))
        );
        assert_eq!(
            codec.decode(&mut buf).unwrap(),
            Some(Frame::Payload(Bytes::from_static(b"two")))
        );
        assert_eq!(codec.decode(&mut buf).unwrap(), Some(Frame::EndOfResponse));
        assert_eq!(codec.decode(&mut buf).unwrap(), None);
    }

    #[test]
    fn a_corrupt_checksum_fails_the_frame() {
        let mut codec = PacketCodec::default();
        let mut buf = BytesMut::new();
        codec
            .encode(Frame::Payload(Bytes::from_static(b"0123456789")), &mut buf)
            .expect("encode");

        // Corrupt the length's low byte, which is what the checksum covers.
        // The payload is still present and still the right size, so only the
        // checksum can catch this.
        buf[3] ^= 0x01;

        match codec.decode(&mut buf) {
            Err(CodecError::Protocol(ProtocolError::ChecksumMismatch { .. })) => {}
            other => panic!("expected a checksum mismatch, got {other:?}"),
        }
    }

    #[test]
    fn a_zero_checksum_is_accepted() {
        // The protocol lets a sender decline to checksum, and the Go server
        // does so on some paths. Rejecting those frames would be a client that
        // works against itself and not against Couchbase.
        let mut codec = PacketCodec::default();
        let mut buf = BytesMut::new();
        buf.put_u32(3);
        buf.put_u16(ENCODING_PROTOBUF as u16);
        buf.put_slice(b"abc");

        assert_eq!(
            codec.decode(&mut buf).unwrap(),
            Some(Frame::Payload(Bytes::from_static(b"abc")))
        );
    }

    #[test]
    fn an_unknown_encoding_fails_before_the_payload_is_read() {
        let mut codec = PacketCodec::default();
        let mut buf = BytesMut::new();
        buf.put_u32(3);
        buf.put_u16(0x0020); // not protobuf

        // Deliberately no payload: the encoding check must fire on the header
        // alone, or a desynchronised stream reads garbage as a message.
        match codec.decode(&mut buf) {
            Err(CodecError::Protocol(ProtocolError::UnknownEncoding(0x20))) => {}
            other => panic!("expected an unknown-encoding error, got {other:?}"),
        }
    }

    #[test]
    fn compression_is_refused_rather_than_ignored() {
        let mut codec = PacketCodec::default();
        let mut buf = BytesMut::new();
        buf.put_u32(3);
        buf.put_u16(ENCODING_PROTOBUF as u16 | 0x0001); // snappy
        buf.put_slice(b"abc");

        match codec.decode(&mut buf) {
            Err(CodecError::Protocol(ProtocolError::UnsupportedCompression(1))) => {}
            other => panic!("expected an unsupported-compression error, got {other:?}"),
        }
    }

    #[test]
    fn an_oversize_length_fails_before_allocating_for_it() {
        let mut codec = PacketCodec::new(64);
        let mut buf = BytesMut::new();
        let len_be = 1_000_000u32.to_be_bytes();
        buf.put_slice(&len_be);
        buf.put_u16(ENCODING_PROTOBUF as u16 | ((checksum(len_be) as u16) << CHECKSUM_SHIFT));

        match codec.decode(&mut buf) {
            Err(CodecError::Protocol(ProtocolError::PayloadTooLong {
                len: 1_000_000,
                max: 64,
            })) => {}
            other => panic!("expected a payload-too-long error, got {other:?}"),
        }
    }

    #[test]
    fn checksum_matches_the_go_implementation() {
        // Transcribed from transport/util.go computeChecksum, which takes the
        // low nibbles of the two least significant length bytes. These are the
        // values a Couchbase indexer will actually send.
        assert_eq!(checksum(0u32.to_be_bytes()), 0x00);
        assert_eq!(checksum(1u32.to_be_bytes()), 0x10);
        assert_eq!(checksum(0x1234u32.to_be_bytes()), 0x42);
        // 0xF0 before the mask, so the top bit is dropped and this is 0x70
        // rather than 0xF0 — the one place the seven-bit width is observable.
        assert_eq!(checksum(0xFFu32.to_be_bytes()), 0x70);
    }
}
