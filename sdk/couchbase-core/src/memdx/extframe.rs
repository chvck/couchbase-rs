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

use crate::memdx::durability_level::{DurabilityLevel, DurabilityLevelSettings};
use crate::memdx::error;
use crate::memdx::error::Error;
use crate::memdx::ext_frame_code::{ExtReqFrameCode, ExtResFrameCode};
use bytes::BufMut;
use std::time::Duration;

pub(crate) fn decode_res_ext_frames(buf: &[u8]) -> error::Result<Option<Duration>> {
    let mut server_duration_data = None;

    iter_ext_frames(buf, |code, data| {
        if code == ExtResFrameCode::ServerDuration {
            server_duration_data = Some(decode_server_duration_ext_frame(data));
        }
    })?;

    if let Some(data) = server_duration_data {
        return Ok(Some(data?));
    }

    Ok(None)
}

pub fn decode_ext_frame(buf: &[u8]) -> error::Result<(ExtResFrameCode, &[u8], usize)> {
    if buf.is_empty() {
        return Err(Error::new_protocol_error(
            "empty value buffer when decoding ext frame",
        ));
    }

    let mut buf_pos = 0;

    let frame_header = buf[buf_pos];
    // Widened out of `u8` deliberately. Both of these are an escape value *added*
    // to 15, so the largest either expresses is 15 + 255 = 270 -- which does not
    // fit the byte it was read from. As `u8` the additions below overflowed: a
    // panic in a debug build and a silent wrap in a release one, on a length this
    // side reads straight off the wire.
    let mut u_frame_code: u16 = ((frame_header & 0xF0) >> 4) as u16;
    let mut frame_code = ExtResFrameCode::from(u_frame_code);
    let mut frame_len: usize = (frame_header & 0x0F) as usize;
    buf_pos += 1;

    if u_frame_code == 15 {
        if buf.len() < buf_pos + 1 {
            return Err(Error::new_protocol_error(
                "unexpected eof decoding ext frame",
            ));
        }

        let frame_code_ext = buf[buf_pos] as u16;
        u_frame_code = 15 + frame_code_ext;
        frame_code = ExtResFrameCode::from(u_frame_code);
        buf_pos += 1;
    }

    if frame_len == 15 {
        if buf.len() < buf_pos + 1 {
            return Err(Error::new_protocol_error(
                "unexpected eof decoding ext frame",
            ));
        }

        let frame_len_ext = buf[buf_pos] as usize;
        frame_len = 15 + frame_len_ext;
        buf_pos += 1;
    }

    let u_frame_len = frame_len;
    if buf.len() < buf_pos + u_frame_len {
        return Err(Error::new_protocol_error(
            "unexpected eof decoding ext frame",
        ));
    }

    let frame_body = &buf[buf_pos..buf_pos + u_frame_len];
    buf_pos += u_frame_len;

    Ok((frame_code, frame_body, buf_pos))
}

/// Calls `cb` once for every extras frame in `buf`.
///
/// A response may carry several: ReadUnits, WriteUnits and ThrottleDuration all
/// travel in the same block as ServerDuration, and nothing orders them. Stopping
/// after the first meant a response that led with any of the others reported no
/// server duration at all -- which feeds every KV response and the orphan
/// reporter.
fn iter_ext_frames(buf: &[u8], mut cb: impl FnMut(ExtResFrameCode, &[u8])) -> error::Result<()> {
    let mut rest = buf;

    while !rest.is_empty() {
        let (frame_code, frame_body, buf_pos) = decode_ext_frame(rest)?;

        cb(frame_code, frame_body);

        // decode_ext_frame rejects an empty buffer and every frame carries a
        // header byte, so buf_pos is always positive and this terminates.
        rest = &rest[buf_pos..];
    }

    Ok(())
}

pub fn append_ext_frame(
    frame_code: ExtReqFrameCode,
    frame_body: &[u8],
    buf: &mut [u8],
    offset: &mut usize,
) -> error::Result<()> {
    let frame_len = frame_body.len();

    if *offset >= buf.len() {
        return Err(Error::new_invalid_argument_error(
            "buffer overflow",
            "ext frame".to_string(),
        ));
    }

    buf[*offset] = 0;
    let hdr_byte_ptr = *offset;
    *offset += 1;
    let u_frame_code: u16 = frame_code.into();

    if u_frame_code < 15 {
        buf[hdr_byte_ptr] |= ((u_frame_code & 0x0f) << 4) as u8;
    } else {
        if u_frame_code - 15 >= 15 {
            return Err(Error::new_invalid_argument_error(
                "ext frame code too large to encode",
                "ext frame".to_string(),
            ));
        }
        buf[hdr_byte_ptr] |= 0xF0;

        if *offset + 1 > buf.len() {
            return Err(Error::new_invalid_argument_error(
                "buffer overflow",
                "ext frame".to_string(),
            ));
        }
        // One byte, matching `decode_ext_frame` and the protocol: the escape
        // nibble is followed by a single byte that is added to it.
        buf[*offset] = (u_frame_code - 15) as u8;
        *offset += 1;
    }

    if frame_len < 15 {
        buf[hdr_byte_ptr] |= (frame_len as u8) & 0xF;
    } else {
        // The escape byte holds 0..=255 and is *added* to 15, so 270 is the
        // longest body the wire form expresses -- and `decode_ext_frame` in this
        // file already reads exactly that. Capping the encoder at 15 here made it
        // refuse what its own decoder accepts, from 30 bytes up: an on-behalf-of
        // username of 30 characters failed to encode at all, and this cluster
        // holds names of 53. Anything that fits the wire but not the caller's
        // buffer is caught by the bounds checks below.
        if frame_len - 15 > u8::MAX as usize {
            return Err(Error::new_invalid_argument_error(
                "ext frame len too large to encode",
                "ext frame".to_string(),
            ));
        }
        buf[hdr_byte_ptr] |= 0x0F;
        if *offset + 1 > buf.len() {
            return Err(Error::new_invalid_argument_error(
                "buffer overflow",
                "ext frame".to_string(),
            ));
        }
        // One byte, as above. Reachable: an on-behalf-of username of fifteen
        // bytes or more takes this branch, and two bytes here put the first
        // character of the username where the server reads a length.
        buf[*offset] = (frame_len - 15) as u8;
        *offset += 1;
    }

    if frame_len > 0 {
        if *offset + frame_len > buf.len() {
            return Err(Error::new_invalid_argument_error(
                "buffer overflow",
                "ext frame".to_string(),
            ));
        }
        buf[*offset..*offset + frame_len].copy_from_slice(frame_body);
        *offset += frame_len;
    }

    Ok(())
}

pub fn make_uleb128_32(collection_id: u32, buf: &mut [u8]) -> usize {
    let mut cid = collection_id;
    let mut count = 0;
    loop {
        let mut c: u8 = (cid & 0x7f) as u8;
        cid >>= 7;
        if cid != 0 {
            c |= 0x80;
        }

        buf[count] = c;
        count += 1;
        if c & 0x80 == 0 {
            break;
        }
    }

    count
}

/// The durability frame body: one byte for the level, plus two for a timeout.
///
/// Returned in a stack array rather than a `Vec` because it is at most three
/// bytes and every durable mutation encodes one. The `Vec` form allocated with
/// capacity exactly 1 and then pushed twice, so a three-byte frame cost an
/// allocation and up to two reallocations.
pub fn encode_durability_ext_frame(
    level: DurabilityLevel,
    timeout: Option<Duration>,
) -> error::Result<([u8; 3], usize)> {
    let mut buf = [0u8; 3];
    buf[0] = level.into();

    if timeout.is_none() {
        return Ok((buf, 1));
    }

    let timeout = timeout.unwrap();

    let mut timeout_millis = timeout.as_millis();
    if timeout_millis > 65535 {
        return Err(Error::new_invalid_argument_error(
            "cannot encode durability timeout greater than 65535 milliseconds",
            "durability_level_timeout".to_string(),
        ));
    }

    if timeout_millis == 0 {
        timeout_millis = 1;
    }

    buf[1] = (timeout_millis >> 8) as u8;
    buf[2] = timeout_millis as u8;

    Ok((buf, 3))
}

pub(crate) fn decode_server_duration_ext_frame(mut data: &[u8]) -> error::Result<Duration> {
    if data.len() != 2 {
        return Err(Error::new_protocol_error(
            "invalid server duration ext frame length",
        ));
    }

    let dura_enc = ((data[0] as u32) << 8) | (data[1] as u32);
    let dura_micros = ((dura_enc as f32).powf(1.74) / 2.0).round();

    Ok(Duration::from_micros(dura_micros as u64))
}

/// Reads a durability frame body back.
///
/// Takes a slice: it used to take `&mut Vec<u8>` and consume the body with
/// `remove(0)`, which forced the caller to own and hand over a heap buffer to
/// read three bytes out of.
pub(crate) fn decode_durability_level_ext_frame(
    data: &[u8],
) -> error::Result<DurabilityLevelSettings> {
    if data.len() == 1 {
        let durability = DurabilityLevel::from(data[0]);

        return Ok(DurabilityLevelSettings::new(durability));
    } else if data.len() == 3 {
        let durability = DurabilityLevel::from(data[0]);
        let timeout_millis = ((data[1] as u32) << 8) | (data[2] as u32);

        return Ok(DurabilityLevelSettings::new_with_timeout(
            durability,
            Duration::from_millis(timeout_millis as u64),
        ));
    }

    Err(Error::new_message_error(
        "invalid durability ext frame length",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memdx::durability_level::DurabilityLevel;
    use std::time::Duration;

    fn test_one_durability(
        l: DurabilityLevel,
        d: impl Into<Option<Duration>>,
        expected_bytes: &[u8],
    ) {
        let d = d.into();
        let (data, len) = encode_durability_ext_frame(l, d).expect("encode failed");
        assert_eq!(&data[..len], expected_bytes);

        let settings = decode_durability_level_ext_frame(&data[..len]).expect("decode failed");
        assert_eq!(settings.durability_level, l);

        let decoded_timeout = settings.timeout.unwrap_or(Duration::from_millis(0));
        if let Some(d) = d {
            let diff = (decoded_timeout.as_millis() as i64 - d.as_millis() as i64).abs();
            assert!(
                diff <= 1,
                "Expected relative difference less than 1ms, got {}",
                diff
            );
        } else {
            assert_eq!(0, decoded_timeout.as_millis() as i64);
        }
    }

    #[test]
    fn test_durability_ext_frame_majority_no_duration() {
        test_one_durability(DurabilityLevel::MAJORITY, None, &[0x01]);
    }

    #[test]
    fn test_durability_ext_frame_majority_persist_active_no_duration() {
        test_one_durability(DurabilityLevel::MAJORITY_AND_PERSIST_ACTIVE, None, &[0x02]);
    }

    #[test]
    fn test_durability_ext_frame_majority_duration_0() {
        test_one_durability(
            DurabilityLevel::MAJORITY,
            Duration::from_millis(0),
            &[0x01, 0x00, 0x01],
        );
    }

    #[test]
    fn test_durability_ext_frame_majority_duration_1() {
        test_one_durability(
            DurabilityLevel::MAJORITY,
            Duration::from_millis(1),
            &[0x01, 0x00, 0x01],
        );
    }

    #[test]
    fn test_durability_ext_frame_majority_duration_12201() {
        test_one_durability(
            DurabilityLevel::MAJORITY,
            Duration::from_millis(12201),
            &[0x01, 0x2f, 0xa9],
        );
    }

    #[test]
    fn test_durability_ext_frame_majority_duration_max() {
        test_one_durability(
            DurabilityLevel::MAJORITY,
            Duration::from_millis(65535),
            &[0x01, 0xff, 0xff],
        );
    }

    #[test]
    fn test_append_preserve_expiry() {
        let mut buf = [0; 128];
        let mut offset = 0;
        append_ext_frame(ExtReqFrameCode::PreserveTTL, &[], &mut buf, &mut offset).unwrap();

        assert_eq!(&buf[..offset], &[80]);
    }

    #[test]
    fn test_append_durability_level_no_timeout() {
        let mut buf = [0; 128];
        let mut offset = 0;
        append_ext_frame(ExtReqFrameCode::Durability, &[0x01], &mut buf, &mut offset).unwrap();

        assert_eq!(&buf[..offset], &[17, 1]);
    }

    #[test]
    fn test_append_durability_level_timeout() {
        let mut buf = [0u8; 128];
        let mut offset = 0;
        append_ext_frame(
            ExtReqFrameCode::Durability,
            &[0x01, 0x00, 0x01],
            &mut buf,
            &mut offset,
        )
        .unwrap();

        assert_eq!(&buf[..offset], &[19, 1, 0, 1]);
    }

    /// A response extras block holding a ReadUnits frame and then a
    /// ServerDuration frame -- an ordering the server is free to use, and which
    /// used to hide the duration entirely because only the first frame was read.
    /// Hand-built rather than encoded, to stay honest about what is on the wire:
    /// the header byte is a nibble of frame code and a nibble of body length.
    fn read_units_then_server_duration() -> Vec<u8> {
        // ReadUnits is 0x01 with a 2-byte body, ServerDuration 0x00 with a
        // 2-byte body.
        vec![0x12, 0x00, 0x05, 0x02, 0x00, 0x64]
    }

    #[test]
    fn a_server_duration_on_its_own_is_found() {
        assert!(decode_res_ext_frames(&[0x02, 0x00, 0x64])
            .unwrap()
            .is_some());
    }

    #[test]
    fn frames_without_a_duration_report_none_rather_than_failing() {
        assert!(decode_res_ext_frames(&[0x12, 0x00, 0x05])
            .unwrap()
            .is_none());
    }

    /// Ported from cbcore-rs `src/memdx/serverduration.rs::basic`, decode half.
    ///
    /// The pairs are cbcore-rs's, which reads the same wire field; the unit
    /// here is microseconds rather than nanoseconds, which is what the server
    /// actually sends and what gocbcore reads. Only the decode direction ports:
    /// this crate has no encoder for the field.
    ///
    /// The tolerance is one part per million because the arithmetic is `f32`
    /// where cbcore-rs's is `f64`; the largest pair differs by 13 µs out of
    /// 120 s, which is mantissa precision, not a formula difference.
    #[test]
    fn server_durations_decode_from_their_encoded_form() {
        for (encoded, expected_micros) in [
            (0x0000u16, 0u64),
            (0x0001, 1),
            (0x0127, 9_919),
            (0xd8da, 89_997_489),
            (0xe664, 99_999_149),
            (0xf35d, 109_999_659),
            (0xffff, 120_125_043),
        ] {
            let got = decode_server_duration_ext_frame(&encoded.to_be_bytes())
                .expect("decode failed")
                .as_micros() as u64;

            let tolerance = std::cmp::max(1, expected_micros / 1_000_000);
            assert!(
                got.abs_diff(expected_micros) <= tolerance,
                "encoded {encoded:#06x}: got {got} µs, expected {expected_micros} µs"
            );
        }
    }

    /// A wrong-length body is refused rather than read past.
    #[test]
    fn a_server_duration_of_the_wrong_length_is_refused() {
        assert!(decode_server_duration_ext_frame(&[]).is_err());
        assert!(decode_server_duration_ext_frame(&[0x01]).is_err());
        assert!(decode_server_duration_ext_frame(&[0x01, 0x27, 0x00]).is_err());
    }

    /// **The server may order its response frames as it likes.** A server
    /// duration behind a read-units frame used to be dropped, because the
    /// walk decoded exactly one frame and the remainder was thrown away.
    #[test]
    fn a_server_duration_behind_another_frame_is_still_found() {
        // ReadUnits (code 0x01, 2 bytes), then ServerDuration (code 0x00, 2 bytes).
        let buf = [0x12u8, 0x00, 0x0a, 0x02, 0x01, 0x27];

        let duration = decode_res_ext_frames(&buf)
            .expect("decode failed")
            .expect("no server duration found");
        assert_eq!(duration.as_micros(), 9_919);

        // ...and in the order the single-frame walk happened to handle.
        let buf = [0x02u8, 0x01, 0x27, 0x12, 0x00, 0x0a];
        assert_eq!(
            decode_res_ext_frames(&buf).unwrap().unwrap().as_micros(),
            9_919
        );
    }

    /// A response carrying no server duration reads as `None`, not as zero.
    #[test]
    fn frames_without_a_server_duration_yield_none() {
        assert!(decode_res_ext_frames(&[]).unwrap().is_none());
        assert!(decode_res_ext_frames(&[0x12, 0x00, 0x0a])
            .unwrap()
            .is_none());
    }

    #[test]
    fn no_frames_means_no_duration() {
        assert!(decode_res_ext_frames(&[]).unwrap().is_none());
    }

    /// A username of thirty bytes or more used to be refused outright: the
    /// encoder capped the escape byte at 15 where the wire form and this file's
    /// own decoder both allow 255 added to 15. The cluster this was found on
    /// holds usernames of 53 characters.
    #[test]
    fn a_long_frame_body_round_trips_through_the_escape_byte() {
        for len in [15usize, 29, 30, 53, 200, 270] {
            let body = vec![b'u'; len];
            let mut buf = [0u8; 512];
            let mut offset = 0;

            append_ext_frame(ExtReqFrameCode::OnBehalfOf, &body, &mut buf, &mut offset)
                .unwrap_or_else(|e| panic!("a {len}-byte body should encode: {e}"));

            let (code, decoded, _) = decode_ext_frame(&buf[..offset])
                .unwrap_or_else(|e| panic!("a {len}-byte body should decode: {e}"));

            assert_eq!(u16::from(code), u16::from(ExtReqFrameCode::OnBehalfOf));
            assert_eq!(decoded, body.as_slice(), "body of {len} did not survive");
        }
    }

    /// Past what the escape byte can express, it is still an error.
    #[test]
    fn a_body_longer_than_the_wire_form_allows_is_refused() {
        let body = vec![b'u'; 271];
        let mut buf = [0u8; 512];
        let mut offset = 0;

        assert!(
            append_ext_frame(ExtReqFrameCode::OnBehalfOf, &body, &mut buf, &mut offset).is_err()
        );
    }

    #[test]
    fn every_frame_is_visited_not_just_the_first() {
        let mut seen = Vec::new();

        iter_ext_frames(&read_units_then_server_duration(), |code, _| {
            seen.push(code)
        })
        .unwrap();

        assert_eq!(
            seen,
            vec![ExtResFrameCode::ReadUnits, ExtResFrameCode::ServerDuration]
        );
    }
    /// Ported from cbcore-rs `src/memdx/frame_extras.rs::basic`, encode half:
    /// four request frames written into one buffer, read back in order.
    ///
    /// cbcore-rs round-trips these through its own request-frame reader. There
    /// is no request-frame decoder here — `decode_ext_frame` yields
    /// `ExtResFrameCode`, a different namespace — so the read-back walks the
    /// bytes and checks the code numbers and bodies rather than typed frames.
    #[test]
    fn several_request_frames_append_in_order() {
        let mut buf = [0u8; 128];
        let mut offset = 0;

        append_ext_frame(
            ExtReqFrameCode::OnBehalfOf,
            b"user-1",
            &mut buf,
            &mut offset,
        )
        .unwrap();
        append_ext_frame(ExtReqFrameCode::PreserveTTL, &[], &mut buf, &mut offset).unwrap();
        append_ext_frame(
            ExtReqFrameCode::OnBehalfOf,
            b"user-2",
            &mut buf,
            &mut offset,
        )
        .unwrap();
        append_ext_frame(ExtReqFrameCode::PreserveTTL, &[], &mut buf, &mut offset).unwrap();

        assert_eq!(
            &buf[..offset],
            &[
                0x46, b'u', b's', b'e', b'r', b'-', b'1', // on-behalf-of, 6 bytes
                0x50, // preserve-ttl, no body
                0x46, b'u', b's', b'e', b'r', b'-', b'2', //
                0x50,
            ]
        );

        let mut rest = &buf[..offset];
        let mut seen: Vec<(u16, Vec<u8>)> = vec![];
        while !rest.is_empty() {
            let (code, body, used) = decode_ext_frame(rest).unwrap();
            seen.push((code.into(), body.to_vec()));
            rest = &rest[used..];
        }

        assert_eq!(
            seen,
            vec![
                (0x04, b"user-1".to_vec()),
                (0x05, vec![]),
                (0x04, b"user-2".to_vec()),
                (0x05, vec![]),
            ]
        );
    }

    /// A body of fifteen bytes or more escapes its length into **one** extra
    /// byte, which is what `decode_ext_frame` and the server both read.
    ///
    /// Reachable in production: an on-behalf-of username this long is ordinary.
    /// Two bytes here put the username's first character where the length's
    /// continuation is read, so the frame — and every frame after it — is
    /// misparsed.
    #[test]
    fn a_long_frame_body_escapes_its_length_into_one_byte() {
        let user = b"a-user-with-a-long-name";
        assert!(user.len() >= 15);

        let mut buf = [0u8; 128];
        let mut offset = 0;
        append_ext_frame(ExtReqFrameCode::OnBehalfOf, user, &mut buf, &mut offset).unwrap();

        // Header, one length-escape byte, then the body.
        assert_eq!(offset, 2 + user.len());
        assert_eq!(buf[0], 0x4F);
        assert_eq!(buf[1], (user.len() - 15) as u8);

        let (code, body, used) = decode_ext_frame(&buf[..offset]).unwrap();
        assert_eq!(u16::from(code), 0x04);
        assert_eq!(body, user);
        assert_eq!(used, offset);
    }
}
