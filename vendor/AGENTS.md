# Shared-engine task routing

Read [`vendor/README.md`](README.md) before changing the
`vendor/tokscale-core` pin. Then read
[`docs/knowledge/vendor-tokscale.md`](../docs/knowledge/vendor-tokscale.md) for
the shared-engine boundary and
[`docs/knowledge/verification.md`](../docs/knowledge/verification.md) for
required consumer evidence. Engine implementation work follows the public
engine's immutable
[`AGENTS.md`](https://github.com/Nanako0129/tokscale-core/blob/a024eb7f783a97edf7f8bb5b1508481d0cb5dad2/AGENTS.md).

## Invariants

| Boundary | Rule |
|---|---|
| Pin | `vendor/tokscale-core` must be a clean gitlink at the reviewed engine commit recorded in `vendor/README.md`. |
| Engine ownership | Do not edit shared Rust source inside the TokenBar submodule. Land and verify the engine change in `tokscale-core`, then update this consumer pin. |
| Consumer ownership | TokenBar continues to own `crates/tb_core_ffi`, `Sources/CTB/include/ctb.h`, Swift code, build wiring, and the root `Cargo.lock`. |
| Parity | A pin update must verify materialized and streaming behavior, cache/schema consequences, FFI mappings, and the Rust-to-Swift app gates that the change can affect. |
| Ledger | The engine's `UPSTREAM.md` owns the exact upstream and local-patch ledger. `vendor/README.md` records only TokenBar's source and pin. |

No shared-engine task may push or merge by implication from a plan. Return the
diff and verification evidence to the user for authorization.
