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

use std::fmt::{Display, Formatter};

use crate::memdx::error::Error;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OpCode {
    Get,
    Set,
    Add,
    Replace,
    Delete,
    Increment,
    Decrement,
    Noop,
    Stat,
    GetAllVBSeqnos,
    Touch,
    GAT,
    Append,
    Prepend,
    Hello,
    GetClusterConfig,
    GetCollectionId,
    SubDocGet,
    SubDocExists,
    SubDocDictAdd,
    SubDocDictSet,
    SubDocDelete,
    SubDocReplace,
    SubDocArrayPushLast,
    SubDocArrayPushFirst,
    SubDocArrayInsert,
    SubDocArrayAddUnique,
    SubDocCounter,
    SubDocMultiLookup,
    SubDocMultiMutation,
    SubDocGetCount,
    SubDocReplaceBodyWithXattr,
    RangeScanCreate,
    RangeScanContinue,
    RangeScanCancel,
    GetErrorMap,
    SelectBucket,
    GetLocked,
    UnlockKey,
    GetMeta,
    SASLAuth,
    SASLListMechs,
    SASLStep,
    Unknown(u8),
}

impl From<OpCode> for u8 {
    fn from(value: OpCode) -> Self {
        match value {
            OpCode::Get => 0x00,
            OpCode::Set => 0x01,
            OpCode::Add => 0x02,
            OpCode::Replace => 0x03,
            OpCode::Delete => 0x04,
            OpCode::Increment => 0x05,
            OpCode::Decrement => 0x06,
            OpCode::Noop => 0x0a,
            OpCode::Stat => 0x10,
            OpCode::GetAllVBSeqnos => 0x48,
            OpCode::Append => 0x0e,
            OpCode::Prepend => 0x0f,
            OpCode::Touch => 0x1c,
            OpCode::GAT => 0x1d,
            OpCode::Hello => 0x1f,
            OpCode::SASLListMechs => 0x20,
            OpCode::SASLAuth => 0x21,
            OpCode::SASLStep => 0x22,
            OpCode::SelectBucket => 0x89,
            OpCode::GetLocked => 0x94,
            OpCode::UnlockKey => 0x95,
            OpCode::GetMeta => 0xa0,
            OpCode::GetClusterConfig => 0xb5,
            OpCode::GetCollectionId => 0xbb,
            OpCode::SubDocGet => 0xc5,
            OpCode::SubDocExists => 0xc6,
            OpCode::SubDocDictAdd => 0xc7,
            OpCode::SubDocDictSet => 0xc8,
            OpCode::SubDocDelete => 0xc9,
            OpCode::SubDocReplace => 0xca,
            OpCode::SubDocArrayPushLast => 0xcb,
            OpCode::SubDocArrayPushFirst => 0xcc,
            OpCode::SubDocArrayInsert => 0xcd,
            OpCode::SubDocArrayAddUnique => 0xce,
            OpCode::SubDocCounter => 0xcf,
            OpCode::SubDocMultiLookup => 0xd0,
            OpCode::SubDocMultiMutation => 0xd1,
            OpCode::SubDocGetCount => 0xd2,
            OpCode::SubDocReplaceBodyWithXattr => 0xd3,
            OpCode::RangeScanCreate => 0xda,
            OpCode::RangeScanContinue => 0xdb,
            OpCode::RangeScanCancel => 0xdc,
            OpCode::GetErrorMap => 0xfe,
            OpCode::Unknown(code) => code,
        }
    }
}

impl TryFrom<u8> for OpCode {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        let code = match value {
            0x00 => OpCode::Get,
            0x01 => OpCode::Set,
            0x02 => OpCode::Add,
            0x03 => OpCode::Replace,
            0x04 => OpCode::Delete,
            0x05 => OpCode::Increment,
            0x06 => OpCode::Decrement,
            0x0a => OpCode::Noop,
            0x10 => OpCode::Stat,
            0x48 => OpCode::GetAllVBSeqnos,
            0x0e => OpCode::Append,
            0x0f => OpCode::Prepend,
            0x1c => OpCode::Touch,
            0x1d => OpCode::GAT,
            0x1f => OpCode::Hello,
            0x20 => OpCode::SASLListMechs,
            0x21 => OpCode::SASLAuth,
            0x22 => OpCode::SASLStep,
            0x89 => OpCode::SelectBucket,
            0x94 => OpCode::GetLocked,
            0x95 => OpCode::UnlockKey,
            0xa0 => OpCode::GetMeta,
            0xb5 => OpCode::GetClusterConfig,
            0xbb => OpCode::GetCollectionId,
            0xc5 => OpCode::SubDocGet,
            0xc6 => OpCode::SubDocExists,
            0xc7 => OpCode::SubDocDictAdd,
            0xc8 => OpCode::SubDocDictSet,
            0xc9 => OpCode::SubDocDelete,
            0xca => OpCode::SubDocReplace,
            0xcb => OpCode::SubDocArrayPushLast,
            0xcc => OpCode::SubDocArrayPushFirst,
            0xcd => OpCode::SubDocArrayInsert,
            0xce => OpCode::SubDocArrayAddUnique,
            0xcf => OpCode::SubDocCounter,
            0xd0 => OpCode::SubDocMultiLookup,
            0xd1 => OpCode::SubDocMultiMutation,
            0xd2 => OpCode::SubDocGetCount,
            0xd3 => OpCode::SubDocReplaceBodyWithXattr,
            0xda => OpCode::RangeScanCreate,
            0xdb => OpCode::RangeScanContinue,
            0xdc => OpCode::RangeScanCancel,
            0xfe => OpCode::GetErrorMap,
            _ => OpCode::Unknown(value),
        };

        Ok(code)
    }
}

impl Display for OpCode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let txt = match self {
            OpCode::Get => "Get",
            OpCode::Set => "Set",
            OpCode::Add => "Add",
            OpCode::Replace => "Replace",
            OpCode::Delete => "Delete",
            OpCode::Increment => "Increment",
            OpCode::Decrement => "Decrement",
            OpCode::Noop => "Noop",
            OpCode::Stat => "Stat",
            OpCode::GetAllVBSeqnos => "Get all VB seqnos",
            OpCode::Append => "Append",
            OpCode::Prepend => "Prepend",
            OpCode::Touch => "Touch",
            OpCode::GAT => "GAT",
            OpCode::GetMeta => "Get meta",
            OpCode::Hello => "Hello",
            OpCode::GetClusterConfig => "Get cluster config",
            OpCode::GetCollectionId => "Get collection id",
            OpCode::GetErrorMap => "Get error map",
            OpCode::SelectBucket => "Select bucket",
            OpCode::GetLocked => "Get locked",
            OpCode::UnlockKey => "Unlock key",
            OpCode::SASLAuth => "SASL auth",
            OpCode::SASLListMechs => "SASL list mechanisms",
            OpCode::SASLStep => "SASL step",
            OpCode::SubDocGet => "SubDoc get",
            OpCode::SubDocExists => "SubDoc exists",
            OpCode::SubDocDictAdd => "SubDoc dict add",
            OpCode::SubDocDictSet => "SubDoc dict set",
            OpCode::SubDocDelete => "SubDoc delete",
            OpCode::SubDocReplace => "SubDoc replace",
            OpCode::SubDocArrayPushLast => "SubDoc array push last",
            OpCode::SubDocArrayPushFirst => "SubDoc array push first",
            OpCode::SubDocArrayInsert => "SubDoc array insert",
            OpCode::SubDocArrayAddUnique => "SubDoc array add unique",
            OpCode::SubDocCounter => "SubDoc counter",
            OpCode::SubDocMultiLookup => "SubDoc multi lookup",
            OpCode::SubDocMultiMutation => "SubDoc multi mutation",
            OpCode::SubDocGetCount => "SubDoc get count",
            OpCode::SubDocReplaceBodyWithXattr => "SubDoc replace body with Xattr",
            OpCode::RangeScanCreate => "Range scan create",
            OpCode::RangeScanContinue => "Range scan continue",
            OpCode::RangeScanCancel => "Range scan cancel",
            OpCode::Unknown(code) => {
                return write!(f, "x{code:02x}");
            }
        };
        write!(f, "{txt}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The two tables above are written out by hand in opposite directions, so
    // nothing but a test keeps them agreeing: `GetMeta` was encodable as 0xa0
    // for a long time while 0xa0 decoded to `Unknown(0xa0)`.
    const ALL: &[OpCode] = &[
        OpCode::Get,
        OpCode::Set,
        OpCode::Add,
        OpCode::Replace,
        OpCode::Delete,
        OpCode::Increment,
        OpCode::Decrement,
        OpCode::Noop,
        OpCode::Stat,
        OpCode::GetAllVBSeqnos,
        OpCode::Touch,
        OpCode::GAT,
        OpCode::Append,
        OpCode::Prepend,
        OpCode::Hello,
        OpCode::GetClusterConfig,
        OpCode::GetCollectionId,
        OpCode::SubDocGet,
        OpCode::SubDocExists,
        OpCode::SubDocDictAdd,
        OpCode::SubDocDictSet,
        OpCode::SubDocDelete,
        OpCode::SubDocReplace,
        OpCode::SubDocArrayPushLast,
        OpCode::SubDocArrayPushFirst,
        OpCode::SubDocArrayInsert,
        OpCode::SubDocArrayAddUnique,
        OpCode::SubDocCounter,
        OpCode::SubDocMultiLookup,
        OpCode::SubDocMultiMutation,
        OpCode::SubDocGetCount,
        OpCode::SubDocReplaceBodyWithXattr,
        OpCode::RangeScanCreate,
        OpCode::RangeScanContinue,
        OpCode::RangeScanCancel,
        OpCode::GetErrorMap,
        OpCode::SelectBucket,
        OpCode::GetLocked,
        OpCode::UnlockKey,
        OpCode::GetMeta,
        OpCode::SASLAuth,
        OpCode::SASLListMechs,
        OpCode::SASLStep,
    ];

    #[test]
    fn all_lists_every_named_variant() {
        for op in ALL.iter().copied() {
            // Exhaustive on purpose: no wildcard arm, so a new variant stops
            // this compiling until it is named here, and the list it belongs in
            // is the one directly above.
            match op {
                OpCode::Get
                | OpCode::Set
                | OpCode::Add
                | OpCode::Replace
                | OpCode::Delete
                | OpCode::Increment
                | OpCode::Decrement
                | OpCode::Noop
                | OpCode::Stat
                | OpCode::GetAllVBSeqnos
                | OpCode::Touch
                | OpCode::GAT
                | OpCode::Append
                | OpCode::Prepend
                | OpCode::Hello
                | OpCode::GetClusterConfig
                | OpCode::GetCollectionId
                | OpCode::SubDocGet
                | OpCode::SubDocExists
                | OpCode::SubDocDictAdd
                | OpCode::SubDocDictSet
                | OpCode::SubDocDelete
                | OpCode::SubDocReplace
                | OpCode::SubDocArrayPushLast
                | OpCode::SubDocArrayPushFirst
                | OpCode::SubDocArrayInsert
                | OpCode::SubDocArrayAddUnique
                | OpCode::SubDocCounter
                | OpCode::SubDocMultiLookup
                | OpCode::SubDocMultiMutation
                | OpCode::SubDocGetCount
                | OpCode::SubDocReplaceBodyWithXattr
                | OpCode::RangeScanCreate
                | OpCode::RangeScanContinue
                | OpCode::RangeScanCancel
                | OpCode::GetErrorMap
                | OpCode::SelectBucket
                | OpCode::GetLocked
                | OpCode::UnlockKey
                | OpCode::GetMeta
                | OpCode::SASLAuth
                | OpCode::SASLListMechs
                | OpCode::SASLStep => {}
                OpCode::Unknown(code) => {
                    panic!("ALL holds named variants only, found Unknown({code:#04x})")
                }
            }
        }
    }

    #[test]
    fn every_named_variant_decodes_back() {
        for op in ALL.iter().copied() {
            let code = u8::from(op);
            let decoded = OpCode::try_from(code).unwrap();
            assert_eq!(
                decoded, op,
                "{op:?} encodes to {code:#04x}, but {code:#04x} decodes to {decoded:?}"
            );
        }
    }

    #[test]
    fn every_code_re_encodes_to_itself() {
        for code in 0..=u8::MAX {
            let op = OpCode::try_from(code).unwrap();
            let encoded = u8::from(op);
            assert_eq!(
                encoded, code,
                "{code:#04x} decodes to {op:?}, which encodes back to {encoded:#04x}"
            );
        }
    }
}
