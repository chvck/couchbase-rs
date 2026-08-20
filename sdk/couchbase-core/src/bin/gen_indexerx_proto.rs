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

//! Regenerate `src/indexerx/proto/generated.rs` from `query.proto`.
//!
//! ```sh
//! cargo run -p couchbase-core --features proto-codegen --bin gen-indexerx-proto
//! ```
//!
//! This is a binary rather than a `build.rs` on purpose. The generated file is
//! checked in, so an ordinary build of this crate needs neither `prost-build`
//! nor a `protoc` on `PATH` — only whoever changes the schema does. The schema
//! belongs to the indexing service and changes about once a release, so paying
//! a build-time toolchain dependency on every build to track it would be a poor
//! trade.
//!
//! Requires `protoc` (any 3.x). Run it after editing `query.proto` and commit
//! the result alongside.

use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/indexerx/proto");
    let schema = root.join("query.proto");

    let mut config = prost_build::Config::new();
    config.out_dir(&root);

    // Rows arrive by the thousand and their two byte fields are the whole
    // payload, so `IndexEntry` decodes into `Bytes` slices of the frame rather
    // than into a fresh `Vec` per field. `indexerx::proto::decode_response`
    // takes a `Bytes` for the same reason: prost can only hand out a slice if
    // the buffer it decodes from is refcounted.
    //
    // Only `IndexEntry`. The request side's byte fields — a scan's bounds — are
    // built once from owned `Vec`s that the caller hands over, so `Bytes` there
    // would buy a move it already had and change `span`'s public API for it.
    config.bytes([".protoQuery.IndexEntry"]);

    // One file in, one file out, with a name we choose rather than one derived
    // from the proto package — prost snake-cases `protoQuery` to
    // `proto_query.rs`, which does not look like anything else in this tree.
    config.compile_protos(&[&schema], &[&root])?;

    let generated = root.join("proto_query.rs");
    let target = root.join("generated.rs");

    // Every file in this crate carries the licence header, generated ones
    // included. It is lifted from the schema next door rather than restated
    // here, so a regeneration cannot silently drop it or fix it to a stale
    // wording.
    let schema_source = std::fs::read_to_string(&schema)?;
    let header =
        licence_header(&schema_source).ok_or("query.proto has no licence header to copy")?;
    let body = std::fs::read_to_string(&generated)?;
    std::fs::write(&target, format!("{header}\n{body}"))?;
    std::fs::remove_file(&generated)?;

    println!("wrote {}", target.display());
    Ok(())
}

/// The leading `/* … */` block of a source file, or `None` if it does not open
/// with one.
fn licence_header(source: &str) -> Option<&str> {
    if !source.starts_with("/*") {
        return None;
    }
    let end = source.find("*/")?;
    Some(&source[..end + 2])
}
