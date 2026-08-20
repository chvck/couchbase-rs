# Couchbase Rust SDK

[![license](https://img.shields.io/github/license/couchbase/couchbase-jvm-clients?color=brightgreen)](https://opensource.org/licenses/Apache-2.0)

This repository contains the Couchbase Rust SDK.

## Project Structure

This repository contains multiple crates that work together to provide the complete Couchbase Rust SDK:

- **`couchbase`** - Main SDK crate providing the, public, high-level API.
- **`couchbase-core`** - Core networking and protocol implementation, not intended for direct use.
- **`couchbase-connstr`** - Connection string parsing and DNS resolution, not intended for direct use.
- **`protostellar`** - Couchbase2 protocol support, not intended for direct use.

## Quick Start

Add the Couchbase SDK to your `Cargo.toml`:

```
cargo add couchbase
```

## Building from Source

```
git clone https://github.com/couchbaselabs/couchbase-rs
cd sdk/couchbase
cargo build 
```

## Testing

Tests use the standard Rust testing framework. To run tests:

```
cd sdk/couchbase
export RCBCONNSTR="couchbases://127.0.0.1
export RCBCUSERNAME="username"
export RCBCPASSWORD="password"
cargo test
```

For a full list of available environment variables, see the [Testing Environment Variables](https://github.com/couchbaselabs/couchbase-rs/blob/main/sdk/couchbase/tests/common/test_config.rs).

### What the cluster has to look like

CI provisions its cluster with `cbdinocluster` and creates the `default` bucket at a **100 MB** RAM
quota (`.github/workflows/tests.yml`). A cluster set up by hand often differs in two ways that each
fail a group of tests for reasons that look nothing like their cause:

- **The bucket management tests create buckets, so the cluster needs quota headroom.** If `default`
  has been given a large quota, it can consume the cluster's entire KV allocation, and every test
  that creates a bucket fails with `RAM quota specified is too large to be provisioned into this
  cluster` — 17 tests across `couchbase-core`'s `bucket_management` and `mgmt` and `couchbase`'s
  `search`. Either keep `default` small, as CI does, or raise the cluster's KV quota:

  ```
  curl -u USER:PASS -X POST http://HOST:8091/pools/default -d 'memoryQuota=6144'
  ```

- **A magma bucket defaults new collections to history retention on.** The collection tests assert
  that a freshly created collection has history *off*, so on a magma `default` with
  `historyRetentionCollectionDefault` true, three tests fail on that assertion:

  ```
  curl -u USER:PASS -X POST http://HOST:8091/pools/default/buckets/default \
       -d 'historyRetentionCollectionDefault=false'
  ```

Leftover state from other suites also matters: a user created without a display name, or an index
sharing a name with one under test in another scope, have both caused failures here. Prefer a
cluster you can throw away.

### Test coverage

Whilst some integration tests are included in the main SDK crate, more extensive tests are run via the Couchbase FIT tool.

### Benchmarks

```bash
# Run benchmarks
cargo bench

# Run specific benchmark
cargo bench collection_crud
```

Benchmarks share the same environment variables as tests.

## Branching Strategy

The rust SDK follows the [semantic versioning](https://semver.org/) strategy.
Dotpatch releases contain only bug fixes.
Dotminor releases may contain new features, but are backwards compatible.
Dotmajor releases are very, very infrequent.

The `main` branch is where development for the next minor release happens.
For each new change a new branch is created and then the change is merged back into `main` via a pull request.

Maintenance branches are named `x.y` where `x` is the major version and `y` is the minor version.
Instead of committing directly to a maintenance branch, first commit to `main` and then cherry-pick to the maintenance branch if possible.

## Documentation

[Couchbase documentation site](https://docs.couchbase.com/rust-sdk/current/hello-world/overview.html)
[API reference documentation](https://docs.rs/couchbase/latest/couchbase/)

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for details.

## Releases

Crates are published automatically via the publish workflow when a tag is pushed to the repository.
At present all crates maintain the same version number, but in the future this may change.
This means that crates are version bumped even if they have had no changes, this is a trade-off to avoid the complexity of managing separate versions for each crate.
