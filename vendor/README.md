---
status: active
id: vendor-readme
kind: reference
scope: repository
read_when: advancing or auditing the vendor/tokscale-core consumer pin
last_verified: 2026-11-02
sources: [".gitmodules", "vendor/tokscale-core", "docs/knowledge/vendor-tokscale.md"]
---

# Shared Rust core pin

TokenBar consumes the public
[`Nanako0129/tokscale-core`](https://github.com/Nanako0129/tokscale-core)
repository as a Git submodule. Consumer integration rules are documented in
[`docs/knowledge/vendor-tokscale.md`](../docs/knowledge/vendor-tokscale.md).

| Field | Value |
|---|---|
| Path | `vendor/tokscale-core` |
| Repository | `https://github.com/Nanako0129/tokscale-core.git` |
| Reviewed pin | `8535394b092c64dbd98e12d81f47d6eca8142752` |
| Upstream and local-patch ledger | Immutable [`UPSTREAM.md`](https://github.com/Nanako0129/tokscale-core/blob/8535394b092c64dbd98e12d81f47d6eca8142752/UPSTREAM.md) |

## Ownership

The shared repository owns parsers, scanning, cache behavior, pricing, and
aggregation. TokenBar owns the gitlink, root `Cargo.lock`,
`crates/tb_core_ffi`, `Sources/CTB/include/ctb.h`, Swift code, and application
build wiring.

Do not edit shared Rust source inside the TokenBar submodule. Land and verify
engine changes in `tokscale-core`, then advance this pin to the reviewed engine
commit and run the TokenBar consumer gates.

## Cache consequences of the current pin

Claude `parser_version` is 5 and a Claude shard is written in its own cache
envelope (format 100). The first scan after an upgrade re-parses every Claude
transcript once without losing cached history. A downgrade is lossy: an older
engine reads these shards as stale and rewrites them from nothing, dropping
turns that exist only in the cache. Before a first scan of the real cache on this
pin, quit every Syrtis and tokscale process and copy
`~/.config/tokscale/cache/source-message-cache-v2/claude/`. To roll back, in
this order: quit every process that runs the newer engine, restore that
directory, launch the older version, then check that the Claude shards still
begin with format 4 and parser version 4. Details and measurements are in
[`docs/knowledge/vendor-tokscale.md`](../docs/knowledge/vendor-tokscale.md).

## Checkout

Clone recursively:

```bash
git clone --recurse-submodules https://github.com/Nanako0129/TokenBar.git
```

For an existing checkout:

```bash
git submodule update --init --recursive
```

The submodule must be clean and its checked-out `HEAD` must equal the gitlink
before building or releasing.
