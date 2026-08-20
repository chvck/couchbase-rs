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

use std::fmt::{Debug, Display};

use crate::memdx::error::Error;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Magic {
    Req,
    Res,
    ReqExt,
    ResExt,

    ServerReq,
    ServerRes,
}

impl Magic {
    pub fn is_request(&self) -> bool {
        matches!(self, Magic::Req | Magic::ReqExt)
    }

    pub fn is_response(&self) -> bool {
        matches!(self, Magic::Res | Magic::ResExt)
    }

    pub fn is_extended(&self) -> bool {
        matches!(self, Magic::ReqExt | Magic::ResExt)
    }
}

impl From<Magic> for u8 {
    fn from(value: Magic) -> u8 {
        match value {
            Magic::Req => 0x80,
            Magic::Res => 0x81,
            Magic::ReqExt => 0x08,
            Magic::ResExt => 0x18,
            Magic::ServerReq => 0x82,
            Magic::ServerRes => 0x83,
        }
    }
}

impl TryFrom<u8> for Magic {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        let magic = match value {
            0x80 => Magic::Req,
            0x81 => Magic::Res,
            0x08 => Magic::ReqExt,
            0x18 => Magic::ResExt,
            0x82 => Magic::ServerReq,
            0x83 => Magic::ServerRes,
            _ => {
                return Err(Error::new_message_error(format!("unknown magic {value}")));
            }
        };

        Ok(magic)
    }
}

impl Display for Magic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let txt = match self {
            Magic::Req => "Req",
            Magic::Res => "Res",
            Magic::ReqExt => "ReqExt",
            Magic::ResExt => "ResExt",
            Magic::ServerReq => "ServerReq",
            Magic::ServerRes => "ServerRes",
        };
        write!(f, "{txt}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hand-written tables in both directions, so a test walks both. Magic has
    // no `Unknown` variant, so an unlisted byte is an error rather than a
    // carried value.
    const ALL: &[Magic] = &[
        Magic::Req,
        Magic::Res,
        Magic::ReqExt,
        Magic::ResExt,
        Magic::ServerReq,
        Magic::ServerRes,
    ];

    #[test]
    fn all_lists_every_variant() {
        for magic in ALL.iter().copied() {
            // Exhaustive on purpose: no wildcard arm, so a new variant stops
            // this compiling until it is named here, and the list it belongs in
            // is the one directly above.
            match magic {
                Magic::Req
                | Magic::Res
                | Magic::ReqExt
                | Magic::ResExt
                | Magic::ServerReq
                | Magic::ServerRes => {}
            }
        }
    }

    #[test]
    fn every_variant_decodes_back() {
        for magic in ALL.iter().copied() {
            let code = u8::from(magic);
            let decoded = Magic::try_from(code).unwrap();
            assert_eq!(
                decoded, magic,
                "{magic:?} encodes to {code:#04x}, but {code:#04x} decodes to {decoded:?}"
            );
        }
    }

    #[test]
    fn every_code_re_encodes_or_is_rejected() {
        for code in 0..=u8::MAX {
            match Magic::try_from(code) {
                Ok(magic) => {
                    let encoded = u8::from(magic);
                    assert_eq!(
                        encoded, code,
                        "{code:#04x} decodes to {magic:?}, which encodes back to {encoded:#04x}"
                    );
                }
                Err(_) => assert!(
                    !ALL.iter().any(|magic| u8::from(*magic) == code),
                    "{code:#04x} is encodable but not decodable"
                ),
            }
        }
    }
}
