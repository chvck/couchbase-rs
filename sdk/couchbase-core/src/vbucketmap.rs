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

use crate::error::Result;
use crate::error::{Error, ErrorKind};

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct VbucketMap {
    entries: Vec<Vec<i16>>,
    num_replicas: usize,
}

impl VbucketMap {
    pub fn new(entries: Vec<Vec<i16>>, num_replicas: usize) -> Result<Self> {
        if entries.is_empty() {
            return Err(Error::new_message_error(
                "vbucket map must have at least a single entry",
            ));
        }

        Ok(Self {
            entries,
            num_replicas,
        })
    }

    pub fn is_valid(&self) -> bool {
        if let Some(entry) = self.entries.first() {
            return !entry.is_empty();
        }

        false
    }

    pub fn num_vbuckets(&self) -> usize {
        self.entries.len()
    }

    pub fn num_replicas(&self) -> usize {
        self.num_replicas
    }

    pub fn vbucket_by_key(&self, key: &[u8]) -> u16 {
        let checksum = crc32fast::hash(key);
        let mid_bits = (checksum >> 16) as u16 & 0x7fff;
        mid_bits % (self.entries.len() as u16)
    }

    pub fn node_by_vbucket(&self, vb_id: u16, vb_server_idx: u32) -> Result<i16> {
        let num_servers = (self.num_replicas as u32) + 1;
        // `>=`, not `>`: with `num_replicas` replicas there are `num_replicas + 1`
        // copies, so the last valid index is `num_servers - 1`. Asking for
        // `num_servers` used to fall through the bound and come back as the
        // "no server" sentinel, which reads as a routable answer.
        if vb_server_idx >= num_servers {
            return Err(ErrorKind::InvalidReplica {
                requested_replica: vb_server_idx,
                num_servers: num_servers as usize,
            }
            .into());
        }

        if let Some(idx) = self.entries.get(vb_id as usize) {
            if let Some(id) = idx.get(vb_server_idx as usize) {
                Ok(*id)
            } else {
                Ok(-1)
            }
        } else {
            Err(ErrorKind::InvalidVbucket {
                requested_vb_id: vb_id,
                num_vbuckets: self.entries.len(),
            }
            .into())
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::error::ErrorKind;
    use crate::vbucketmap::VbucketMap;

    #[test]
    fn vbucketmap_with_1024_vbs() {
        let vb_map = VbucketMap::new(vec![vec![]; 1024], 1).unwrap();

        assert_eq!(0x0202u16, vb_map.vbucket_by_key(vec![0].as_slice()));
        assert_eq!(
            0x00aau16,
            vb_map.vbucket_by_key(vec![0, 1, 2, 3, 4, 5, 6, 7].as_slice())
        );
        assert_eq!(0x0210u16, vb_map.vbucket_by_key(b"hello"));
        assert_eq!(
            0x03d4u16,
            vb_map.vbucket_by_key(b"hello world, I am a super long key lets see if it works")
        );
    }

    #[test]
    fn vbucketmap_with_64_vbs() {
        let vb_map = VbucketMap::new(vec![vec![]; 64], 1).unwrap();

        assert_eq!(0x0002u16, vb_map.vbucket_by_key(vec![0].as_slice()));
        assert_eq!(
            0x002au16,
            vb_map.vbucket_by_key(vec![0, 1, 2, 3, 4, 5, 6, 7].as_slice())
        );
        assert_eq!(0x0010u16, vb_map.vbucket_by_key(b"hello"));
        assert_eq!(
            0x0014u16,
            vb_map.vbucket_by_key(b"hello world, I am a super long key lets see if it works")
        );
    }

    #[test]
    fn vbucketmap_with_48_vbs() {
        let vb_map = VbucketMap::new(vec![vec![]; 48], 1).unwrap();

        assert_eq!(0x0012u16, vb_map.vbucket_by_key(vec![0].as_slice()));
        assert_eq!(
            0x000au16,
            vb_map.vbucket_by_key(vec![0, 1, 2, 3, 4, 5, 6, 7].as_slice())
        );
        assert_eq!(0x0010u16, vb_map.vbucket_by_key(b"hello"));
        assert_eq!(
            0x0004u16,
            vb_map.vbucket_by_key(b"hello world, I am a super long key lets see if it works")
        );
    }

    #[test]
    fn vbucketmap_with_13_vbs() {
        let vb_map = VbucketMap::new(vec![vec![]; 13], 1).unwrap();

        assert_eq!(0x000cu16, vb_map.vbucket_by_key(vec![0].as_slice()));
        assert_eq!(
            0x0008u16,
            vb_map.vbucket_by_key(vec![0, 1, 2, 3, 4, 5, 6, 7].as_slice())
        );
        assert_eq!(0x0008u16, vb_map.vbucket_by_key(b"hello"));
        assert_eq!(
            0x0003u16,
            vb_map.vbucket_by_key(b"hello world, I am a super long key lets see if it works")
        );
    }
    /// The lookup itself, over a map whose rows are full width.
    ///
    /// Ported from cbcore-rs `src/vbucketmap.rs`. `-1` is this type's "no server
    /// assigned" sentinel rather than an error, so it is asserted as a value;
    /// the two out-of-range cases are errors.
    #[test]
    fn nodes_are_read_back_by_vbucket_and_replica() {
        let map = VbucketMap::new(vec![vec![0, 1], vec![1, 0], vec![2, -1]], 1).unwrap();

        assert_eq!(map.num_vbuckets(), 3);
        assert_eq!(map.num_replicas(), 1);
        assert_eq!(map.node_by_vbucket(0, 0).unwrap(), 0);
        assert_eq!(map.node_by_vbucket(0, 1).unwrap(), 1);
        assert_eq!(map.node_by_vbucket(2, 0).unwrap(), 2);

        // An explicit `-1` slot: no server holds this replica.
        assert_eq!(map.node_by_vbucket(2, 1).unwrap(), -1);

        // Past the replica count. One replica means two copies, so index 2 is
        // out of range and must not read back as the sentinel.
        let err = map.node_by_vbucket(0, 2).unwrap_err();
        assert!(
            matches!(err.kind(), ErrorKind::InvalidReplica { requested_replica, num_servers }
                     if *requested_replica == 2 && *num_servers == 2),
            "expected InvalidReplica, got {err:?}"
        );

        // Past the end of the map.
        let err = map.node_by_vbucket(3, 0).unwrap_err();
        assert!(
            matches!(err.kind(), ErrorKind::InvalidVbucket { requested_vb_id, num_vbuckets }
                     if *requested_vb_id == 3 && *num_vbuckets == 3),
            "expected InvalidVbucket, got {err:?}"
        );
    }

    /// A row narrower than the replica count reads as unassigned, not as a
    /// panic and not as someone else's node.
    ///
    /// Ported from cbcore-rs. The router turns this `-1` into
    /// `ErrorKind::NoServerAssigned`, so the sentinel is the contract between
    /// the two and worth pinning here rather than only there.
    #[test]
    fn short_rows_report_no_server_rather_than_reading_the_next_row() {
        let map = VbucketMap::new(vec![vec![7], vec![8]], 1).unwrap();

        assert_eq!(map.node_by_vbucket(0, 0).unwrap(), 7);
        assert_eq!(map.node_by_vbucket(1, 0).unwrap(), 8);
        assert_eq!(map.node_by_vbucket(0, 1).unwrap(), -1);
    }

    /// A map with no rows is refused outright; a map whose rows are all empty
    /// constructs but is not valid.
    ///
    /// Ported from cbcore-rs, which refuses both in the constructor. Here the
    /// second case is `is_valid`'s job, and this pins which check owns which.
    #[test]
    fn an_empty_map_is_refused_and_empty_rows_are_not_valid() {
        assert!(VbucketMap::new(vec![], 1).is_err());

        let no_slots = VbucketMap::new(vec![vec![], vec![]], 1).unwrap();
        assert!(!no_slots.is_valid());
        assert!(VbucketMap::new(vec![vec![0]], 1).unwrap().is_valid());
    }
}
