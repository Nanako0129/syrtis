# Contributing to Syrtis

Syrtis welcomes focused fixes, well-supported bug reports, and carefully scoped improvements. This document is the public entry point for contributors; it explains how to prepare a change and routes durable project facts to the repository knowledge base.

> [`docs/knowledge/`](docs/knowledge/README.md) is the canonical source for architecture, verification, workflow, vendor, release, and maintenance decisions. If this guide and an execution source disagree, verify the execution source and update the canonical knowledge rather than copying a second version here.

## Contents

- [Before you start](#before-you-start)
- [Project knowledge](#project-knowledge)
- [Development environment](#development-environment)
- [Repository workflow](#repository-workflow)
- [Change-specific guardrails](#change-specific-guardrails)
- [Verification](#verification)
- [Pull requests](#pull-requests)
- [Review, credit, and integration](#review-credit-and-integration)
- [Public repository safety](#public-repository-safety)

---

## Before you start

Syrtis is in a maintenance phase. User-visible correctness, missing or stale data, cache invalidation, the Rust-to-Swift contract, and release-chain failures take priority over broad client expansion or large UI redesigns. The rolling tokscale inventory in [issue #45](https://github.com/Nanako0129/syrtis/issues/45) is a decision surface, not a promise to implement every listed item.

| Proposed contribution | Expected first step |
|---|---|
| Wrong or missing usage, cost, quota, or hidden-client data | Describe a minimal reproduction, the expected value, and the affected client or report |
| Parser, cache, deduplication, aggregation, or FFI correction | Identify the producing layer and the smallest deterministic old-fail/new-pass fixture |
| Narrow upstream tokscale fix | Compare the current upstream diff with the shared engine and its `UPSTREAM.md` ledger |
| New client, broad UI redesign, or major product expansion | Open a discussion or focused issue before implementation |
| Release, Sparkle, Homebrew, or migration change | Describe the affected delivery surface; publishing remains maintainer-controlled |

See the current priority order in [`docs/knowledge/current-state.md`](docs/knowledge/current-state.md) before treating an inventory item as committed work.

## Project knowledge

Start with the [knowledge index](docs/knowledge/README.md), then follow the task-specific route below. These documents explain the contract; the linked code, build files, and workflows remain the execution sources.

If you use an automated coding client, it must also follow [`AGENTS.md`](AGENTS.md) and the applicable nested adapter before editing. Those files route agents and define automation boundaries; they do not replace the contributor workflow in this guide.

| Change area | Required reading |
|---|---|
| Rust, C ABI, Swift data flow, reports, or filters | [`architecture.md`](docs/knowledge/architecture.md) and [`verification.md`](docs/knowledge/verification.md) |
| Branches, commits, review, or integration | [`workflow.md`](docs/knowledge/workflow.md) and [`communication.md`](docs/knowledge/communication.md) |
| Tests, fixtures, cache invalidation, or UX acceptance | [`verification.md`](docs/knowledge/verification.md) |
| Shared tokscale engine or consumer pin | [`vendor/AGENTS.md`](vendor/AGENTS.md), [`vendor/README.md`](vendor/README.md), and [`vendor-tokscale.md`](docs/knowledge/vendor-tokscale.md) |
| Sparkle, appcast, Homebrew, Pages, or release notes | [`release.md`](docs/knowledge/release.md) and [`workflow.md`](docs/knowledge/workflow.md) |
| Landing site structure, deployment, or product claims | [`landing/AGENTS.md`](landing/AGENTS.md) and [`release.md`](docs/knowledge/release.md) |

Durable architecture, verification, workflow, and release facts belong in `docs/knowledge/`. The engine's exact upstream baseline and local patch records belong in its immutable [`UPSTREAM.md`](https://github.com/Nanako0129/tokscale-core/blob/bb9a2a9ac787344bb4bd3120d645208217b21016/UPSTREAM.md); Syrtis's source and pin belong in `vendor/README.md`. A reviewed Native pin advance normally uses only the exact `tokscale-core` gitlink; when it travels with an app-owned C ABI change, as the `tb_window_usage` account parameter does, that change is named and the Windows port is notified under the `ctb.h` signature rule. Consumer gates re-establish build-decoder compatibility, and the engine's LocalOnly, CostCoverage, embedded-cost, and partial-estimation semantics are not claimed to have reached Swift.

## Development environment

The supported application target is Apple Silicon on macOS 14 or later. Building it needs Swift 6.2 or newer (Xcode 26+), which is a separate thing from that target and stricter than it: the Liquid Glass surfaces are compiled against the macOS 26 SDK, and `if #available(macOS 26.0, *)` guards them at runtime rather than at compile time, so an older toolchain cannot type-check them at all. `Package.swift` declares `swift-tools-version: 6.0`, which is the manifest format and not the floor — `Sources/Syrtis/GlassBackground.swift` states the floor with a `#error` so an older toolchain fails on that line instead of on the symbols it cannot see. CI uses stable Rust. Clone with `--recurse-submodules`, or run `git submodule update --init --recursive` before building. Run build and Swift commands from the repository root because the Rust static-library search path in [`Package.swift`](Package.swift) is relative.

Build the Rust static library first, link the Swift package, and run the UI-free contract checks:

```bash
make build
swift run Syrtis --selftest
swift run Syrtis --smoke
```

Use `make run` to build and launch Syrtis. For ordinary popover or interaction acceptance that does not require `.app` bundle semantics, use:

```bash
swift run Syrtis --open-popover
```

These commands are onboarding checks, not the complete gate for every change. The canonical change-specific gates are in [`docs/knowledge/verification.md`](docs/knowledge/verification.md).

## Repository workflow

Create a topic branch from the current upstream `main` branch, and keep each branch, commit, and pull request focused on one reviewable concern.

| Area | Convention |
|---|---|
| Branch name | `<type>/<kebab-summary>`, where `type` is the Conventional Commit prefix of the branch's primary concern — the type its pull-request title carries (`feat`, `fix`, `docs`, `chore`, `refactor`, `test`, `ci`, `build`, `perf`, or any other prefix this repository's commits already use; the list is not closed) — such as `fix/registry-sleep-cancellation` or `docs/contributing-guide`. Supporting commits on the branch may use other types. The same rule applies in Syrtis-Windows and tokscale-core. Existing `<type>-<summary>` branches keep their names. |
| Commit subject | `type(scope): imperative subject` |
| Commit scope | One reviewable concern; do not mix unrelated cleanup with the functional change |
| Submission | Open a pull request for the completed topic; core, FFI, vendor, contract, release-chain, user-visible, or multi-file changes require full review |
| Durable project facts | Update the appropriate canonical knowledge document in the same change |
| Generated or local artifacts | Keep build outputs, private fixtures, and machine-local instructions out of the patch |

## Change-specific guardrails

> **Cross-language contract:** A boundary change must check the C declarations in `Sources/CTB/include/ctb.h`, the Rust FFI entry point or mapper, the Swift decoder or model, and parity or smoke evidence. Successful heap JSON pointers must retain the documented `tb_free` ownership path.

> **Pre-aggregation:** Do not try to subtract a client after contributions have been combined into a mixed aggregate. Pass non-empty filters to the producer while client identity is still available, and preserve the documented `nil` or empty-list semantics across Swift, C, and Rust.

> **Shared-engine boundary:** Land shared Rust changes and their ledger updates in `tokscale-core`, then advance Syrtis's submodule only to a reviewed engine commit. Syrtis keeps `tb_core_ffi`, the C header, Swift, root lockfile, and app wiring in this repository.

| Change class | Required evidence or treatment |
|---|---|
| Data-dependent correctness | A hermetic old-fail/new-pass fixture plus a non-trigger regression case |
| Parser reads a sibling file or WAL | Fingerprint, active lane, latest-mtime probe, pruning, and report-parity coverage |
| Parsed output, attribution, dedup key, or resume state changes | An explicit local cache-schema decision and a same-fingerprint stale-cache regression |
| Rust-to-Swift payload changes | C header, Rust mapper, Swift decoder or model, and end-to-end parity or smoke evidence |
| UI lifecycle changes | Evidence that closing the popover or settings window stops polling and background rendering |
| Product or repository documentation | Relative links, canonical ownership, public-safe wording, and the documentation gate |
| Landing-only changes | Aligned English and `zh-tw` content plus the package build declared under `landing/` |

## Verification

Select evidence that proves the behavior being changed. A live app run without the triggering data cannot prove a data-dependent correctness fix, and a green CI run does not replace the full local code-change gate.

| Change | Minimum contributor evidence |
|---|---|
| Every patch | Review tracked, staged, and untracked files; before submission, run `git diff --check origin/main...HEAD` or the equivalent current upstream-main comparison, then inspect any remaining working-tree files separately |
| Documentation or routing | Follow the [documentation checks](docs/knowledge/verification.md#documentation-checks) and verify every relative link |
| Rust or cross-language code | Run the [local full code-change gates](docs/knowledge/verification.md#local-full-code-change-gates) |
| Parser, cache, deduplication, or aggregation | Add the relevant [hermetic fixture](docs/knowledge/verification.md#hermetic-fixtures) and cache or sibling matrix |
| FFI contract | Run the Rust and Swift gates, `--selftest`, and `--smoke`; verify ownership and envelope shape |
| Popover, lens, keyboard, scroll, or appearance | Use the [local UX acceptance path](docs/knowledge/verification.md#local-build-and-ux-acceptance) and record the interaction checked |
| Local icon, `Info.plist`, `LSUIElement`, or autostart behavior | Use the bundle-only path in the verification contract and record cleanup of temporary artifacts |
| Homebrew, stable Sparkle update, or formal install path | Validate `/Applications/Syrtis.app` as documented; do not substitute the temporary `dist/Syrtis.app` |

If a smoke or live check is limited by local credentials, session data, or a provider response, report that environmental limitation separately. Do not use it to replace or invalidate hermetic evidence.

## Pull requests

Write GitHub issues and pull requests in English with soft-wrapped paragraphs. Keep repository links relative and external-project references public.

| PR field | Expected content |
|---|---|
| Motivation or reproduction | What is wrong, who or what is affected, and the smallest reproducible trigger |
| Root cause | The producing layer or data flow responsible for the behavior |
| Scope | What changed, what deliberately did not change, and why the boundary is narrow |
| Verification | Exact commands, fixture names, results, and any before/after measurement required by the claim |
| Canonical documentation | Documents updated, or why the change does not alter a durable project fact |
| Known limitations | Environmental constraints, deferred work, and remaining risk |
| Contributor identity | The actual GitHub handle or authorship represented by the commits |

Correctness claims require regression evidence or a source-traced proof with an explicit limitation. Performance claims require before/after measurements with the workload and lifecycle stated. Upstream-fidelity claims require the exact upstream diff and an explanation of retained local adaptations.

## Review, credit, and integration

Contributors may use the normal fork and pull-request flow. Submission is a proposal: passing checks do not guarantee merge, and a merged change does not by itself authorize a release.

| Stage | Project expectation |
|---|---|
| Review | Maintainers read the complete diff and revalidate important paths against the current base |
| Authorship | Rebase-and-merge preserves contributor authorship; no `Co-Authored-By` trailer is required |
| Credit | Public credit follows actual code, reproduction, logs, or root-cause evidence and names contributors by GitHub handle |
| Integration | Maintainers control merge or fast-forward changes into `main` |
| Release | Tags, GitHub Releases, appcast changes, Homebrew updates, and published assets remain maintainer-controlled |

## Public repository safety

Public issues, pull requests, fixtures, and documentation must contain only sanitized, reproducible evidence. If a reproduction depends on private session data, replace it with the smallest hermetic synthetic fixture that preserves the triggering shape.

| Material | Public-safe treatment |
|---|---|
| Credentials, tokens, keys, or signed private material | Never commit, paste, or include them in logs or screenshots |
| Personal session content or account data | Replace with a synthetic fixture and non-identifying values |
| Personal absolute paths, private hosts, or machine-specific tooling | Use repository-relative paths, documented product-relative paths, placeholders, or omit the detail |
| Machine-local instructions | Keep them outside the tracked repository; they are not project facts |
| Unpublished or sensitive work | Describe only the sanitized project conclusion appropriate for a public repository |
<!-- docs-only CI probe; not for merge -->
