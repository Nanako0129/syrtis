---
status: active
id: vendor-readme
kind: reference
scope: repository
read_when: advancing or auditing the vendor/tokscale-core consumer pin
last_verified: 2026-10-06
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
| Reviewed pin | `8fc63cedfaf4aeec73c9a4e65711c280e7add15e` |
| Upstream and local-patch ledger | Immutable [`UPSTREAM.md`](https://github.com/Nanako0129/tokscale-core/blob/8fc63cedfaf4aeec73c9a4e65711c280e7add15e/UPSTREAM.md) |

## Ownership

The shared repository owns parsers, scanning, cache behavior, pricing, and
aggregation. TokenBar owns the gitlink, root `Cargo.lock`,
`crates/tb_core_ffi`, `Sources/CTB/include/ctb.h`, Swift code, and application
build wiring.

Do not edit shared Rust source inside the TokenBar submodule. Land and verify
engine changes in `tokscale-core`, then advance this pin to the reviewed engine
commit and run the TokenBar consumer gates.

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
