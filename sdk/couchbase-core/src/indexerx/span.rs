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

//! Bounds on an index scan.
//!
//! **The values are JSON, not collatejson.** Everyone expects otherwise,
//! because the index itself is collatejson-encoded — but the wire carries
//! `json.Marshal` output and the *indexer* encodes it on arrival
//! (`scan_request.go`'s `newLowKey`/`newHighKey`). That is the single fact that
//! makes this client tractable, so it is stated here rather than left to be
//! rediscovered.
//!
//! An unbounded end is **omitted** rather than encoded as a sentinel, which is
//! why [`Filter`]'s ends are `Option`.
//!
//! ### What a position means
//!
//! A [`Filter`] bounds one *index key position*, and the positions are the
//! index's own key expressions in declaration order. A caller that thinks in
//! terms of some higher-level column has to do that mapping itself — this
//! layer knows the index has N ordered key positions and nothing about what
//! they hold.
//!
//! A [`Scan`] is a contiguous run: its filters bound a *prefix* of the key, and
//! bounding position 2 while leaving position 1 free does not describe a
//! contiguous run of entries. Several `Scan`s in one request are a disjunction,
//! which is how an `$in`-shaped predicate is expressed in one round trip.
//!
//! ### What the indexer does with them
//!
//! Two things worth knowing, both verified against the indexer rather than
//! assumed, because both would otherwise be duplicated here wrongly:
//!
//! - **Descending key columns are handled server-side.** Send logical low and
//!   high in key order; the indexer swaps them per column against the index
//!   definition's `Desc` flags and reverse-collates
//!   (`scan_request.go`'s "Reverse Collation fix"). A caller never encodes for
//!   direction. Entries then *arrive* descending in that column, which is the
//!   caller's problem and not this type's.
//! - **Inclusivity is exact even on a composite span.** The indexer seeks with
//!   a joined key range and `Both` inclusion, then re-checks every column
//!   against its own inclusion (`scan_pipeline.go`'s `applyFilter`). An
//!   exclusive bound on a middle column means what it says.

/// Which ends of a range are part of it.
///
/// The discriminants are the wire values and are not arbitrary — they are
/// `queryport/client`'s `Neither`/`Low`/`High`/`Both` in declaration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Inclusion {
    /// `low < x < high`
    Neither = 0,
    /// `low <= x < high`
    Low = 1,
    /// `low < x <= high`
    High = 2,
    /// `low <= x <= high`
    #[default]
    Both = 3,
}

impl Inclusion {
    pub(crate) fn as_wire(self) -> u32 {
        self as u32
    }
}

/// A bound on one index key position.
///
/// `low`/`high` are JSON documents — `b"5"`, `b"\"abc\""`, `b"{\"r\":3}"`.
/// `None` is unbounded at that end.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Filter {
    pub low: Option<Vec<u8>>,
    pub high: Option<Vec<u8>>,
    pub inclusion: Inclusion,
}

impl Filter {
    /// Bounded at both ends, inclusive.
    pub fn range(low: impl Into<Vec<u8>>, high: impl Into<Vec<u8>>) -> Filter {
        Filter {
            low: Some(low.into()),
            high: Some(high.into()),
            inclusion: Inclusion::Both,
        }
    }

    /// A single value, expressed as an inclusive range over it.
    ///
    /// Distinct from [`Scan::equals`], which pins the *whole* key. This pins
    /// one position and leaves the rest free.
    pub fn eq(value: impl Into<Vec<u8>> + Clone) -> Filter {
        Filter::range(value.clone(), value)
    }

    /// Unbounded at both ends: every entry, at this position.
    pub fn any() -> Filter {
        Filter::default()
    }

    pub fn with_inclusion(mut self, inclusion: Inclusion) -> Filter {
        self.inclusion = inclusion;
        self
    }
}

/// One contiguous run of index entries.
///
/// Either a prefix of `filters`, or `equals` pinning the whole key. The Go
/// client sends one or the other and never both — `equals` wins if present —
/// so the constructors keep them apart rather than leaving a struct that can
/// express the ambiguity.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Scan {
    pub filters: Vec<Filter>,
    pub equals: Vec<Vec<u8>>,
}

impl Scan {
    /// A run bounded by a prefix of the key positions.
    pub fn filtered(filters: Vec<Filter>) -> Scan {
        Scan {
            filters,
            equals: Vec::new(),
        }
    }

    /// An exact match on the whole key — one JSON value per key position.
    pub fn equals(values: Vec<Vec<u8>>) -> Scan {
        Scan {
            filters: Vec::new(),
            equals: values,
        }
    }

    /// Every entry in the index.
    ///
    /// An empty `Scan` is how the indexer spells a full scan: `fillScans` reads
    /// "no filters" as `getScanAll()`. Sending no `Scan`s at all does the same
    /// thing, so this exists to make the intent legible at a call site rather
    /// than because the wire needs it.
    pub fn all() -> Scan {
        Scan::default()
    }

    pub fn is_all(&self) -> bool {
        self.filters.is_empty() && self.equals.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inclusion_discriminants_are_the_wire_values() {
        // queryport/client/client.go: Neither = iota, then Low, High, Both.
        assert_eq!(Inclusion::Neither.as_wire(), 0);
        assert_eq!(Inclusion::Low.as_wire(), 1);
        assert_eq!(Inclusion::High.as_wire(), 2);
        assert_eq!(Inclusion::Both.as_wire(), 3);
    }

    #[test]
    fn an_unbounded_end_is_absent_rather_than_a_sentinel() {
        let f = Filter {
            low: Some(b"5".to_vec()),
            high: None,
            inclusion: Inclusion::Low,
        };
        assert_eq!(f.high, None);
    }

    #[test]
    fn eq_is_an_inclusive_range_over_one_value() {
        let f = Filter::eq(b"42".to_vec());
        assert_eq!(f.low.as_deref(), Some(&b"42"[..]));
        assert_eq!(f.high.as_deref(), Some(&b"42"[..]));
        assert_eq!(f.inclusion, Inclusion::Both);
    }

    #[test]
    fn a_scan_is_either_filtered_or_equal_but_not_both() {
        assert!(Scan::filtered(vec![Filter::any()]).equals.is_empty());
        assert!(Scan::equals(vec![b"1".to_vec()]).filters.is_empty());
    }

    #[test]
    fn an_empty_scan_is_the_whole_index() {
        assert!(Scan::all().is_all());
        assert!(!Scan::filtered(vec![Filter::eq(b"1".to_vec())]).is_all());
    }
}
