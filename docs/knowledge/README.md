---
status: active
id: kb-index
kind: index
scope: repository
read_when: before any Syrtis task or handoff
last_verified: 2026-10-06
sources: ["README.md", "CONTRIBUTING.md", "AGENTS.md", "Makefile", "Package.swift", ".github/workflows/ci.yml", ".github/workflows/ci-release.yml", ".github/workflows/pages.yml", ".github/workflows/release.yml", ".github/workflows/update-install-count.yml"]
---

# Syrtis project knowledge

## 文件目的

這份索引是 Syrtis 的 canonical project knowledge entry point。它把架構、工作流程、驗證、shared tokscale engine、發版、溝通、目前狀態與歷史決策分開保存，讓人類維護者與不同 coding client 都能從同一份 project-owned source 接手。

公開 contributor 從 [`CONTRIBUTING.md`](../../CONTRIBUTING.md) 進入；該文件負責 onboarding、穩定 guardrail 與任務路由，技術與流程事實仍由本 knowledge tree 擁有。

> Adapters 只負責 routing、invariants、authorization 與 private boundary。若一條規則需要在多個 client 之間一致，請改 canonical document，不要在 adapter 內複製一份。

## 目錄

- [依任務載入](#依任務載入)
- [Canonical tree](#canonical-tree)
- [Source hierarchy](#source-hierarchy)
- [Authoring contract](#authoring-contract)
- [Private overlay boundary](#private-overlay-boundary)
- [Migration coverage](#migration-coverage)

---

## 依任務載入

| 任務 | 先讀 | 接著讀 |
|---|---|---|
| 架構、FFI、資料流 | [`architecture.md`](architecture.md) | [`verification.md`](verification.md) |
| 分支、PR、merge、授權 | [`workflow.md`](workflow.md) | [`communication.md`](communication.md) |
| 測試、fixture、cache、跨語言契約 | [`verification.md`](verification.md) | [`architecture.md`](architecture.md) |
| 效能／正確性 before-after 量測、對拍 oracle | [`measurement.md`](measurement.md) | [`history/dashboard-load-performance.md`](history/dashboard-load-performance.md) |
| Provider quota pace／historical calculation | [`plans/provider-quota-pace.md`](plans/provider-quota-pace.md) | [`plans/codex-historical-pace-v2.md`](plans/codex-historical-pace-v2.md)、[`architecture.md`](architecture.md)、[`verification.md`](verification.md) |
| tokscale engine change 或 consumer pin | [`vendor-tokscale.md`](vendor-tokscale.md) | [`vendor/README.md`](../../vendor/README.md) |
| Sparkle、appcast、Homebrew、Pages | [`release.md`](release.md) | [`workflow.md`](workflow.md) |
| 維護期優先順序 | [`current-state.md`](current-state.md) | [`history/README.md`](history/README.md) |
| 歷史根因或已 parked 調查 | [`history/README.md`](history/README.md) | 對應的 history 文件 |
| 穩定設計 rationale | [`decisions/0001-canonical-knowledge-base.md`](decisions/0001-canonical-knowledge-base.md)、[`decisions/0002-streaming-and-preaggregation.md`](decisions/0002-streaming-and-preaggregation.md)、[`decisions/0003-selective-upstream-alignment.md`](decisions/0003-selective-upstream-alignment.md) | `decisions/` 中對應文件 |
| 歷史 alignment／engine extraction | [`plans/tokscale-alignment.md`](plans/tokscale-alignment.md)、[`plans/shared-rust-engine-extraction.md`](plans/shared-rust-engine-extraction.md) | [`vendor-tokscale.md`](vendor-tokscale.md) |

## Canonical tree

```text
AGENTS.md
CLAUDE.md
CONTRIBUTING.md
vendor/AGENTS.md
landing/AGENTS.md
docs/knowledge/
├── README.md
├── architecture.md
├── workflow.md
├── verification.md
├── measurement.md
├── vendor-tokscale.md
├── release.md
├── communication.md
├── current-state.md
├── migration-ledger.md
├── history/
│   ├── README.md
│   ├── native-rewrite.md
│   ├── liquid-glass-experiments.md
│   └── release-and-ui-incidents.md
├── decisions/
│   ├── 0001-canonical-knowledge-base.md
│   ├── 0002-streaming-and-preaggregation.md
│   └── 0003-selective-upstream-alignment.md
└── plans/
    ├── README.md
    ├── codex-historical-pace-v2.md
    ├── provider-quota-pace.md
    ├── shared-rust-engine-extraction.md
    └── tokscale-alignment.md
```

## Source hierarchy

| Source | Authority |
|---|---|
| Public [`tokscale-core/UPSTREAM.md`](https://github.com/Nanako0129/tokscale-core/blob/8fc63cedfaf4aeec73c9a4e65711c280e7add15e/UPSTREAM.md) | Exact shared-engine upstream baseline, selected changes, reports, and local-patch ledger |
| `vendor/README.md` | Syrtis's shared-engine repository and reviewed consumer pin |
| `.github/workflows/*.yml` | Runtime CI, Pages, release, and install-count gates |
| `Makefile` | Local build order and stale Rust static-library relink guard |
| `Package.swift` | SwiftPM targets and linker contract |
| `README.md` | Product-facing description and install path |
| `CONTRIBUTING.md` | Public contributor onboarding, stable guardrails, and routes to canonical and execution sources |
| `docs/knowledge/` | Human-readable synthesis of durable project facts and decisions |
| `.agent-local/` | Optional machine-local overlay; never a public source of project facts |

When a synthesized document and an execution source disagree, verify against the execution source and update the synthesis. Do not copy the exact engine ledger or a CI command into a second ledger.

## Authoring contract

Canonical documents carry stable frontmatter with `id`, `kind`, `status`, `scope`, `read_when`, `last_verified`, and `sources`. Human-readable prose comes first; use tables for mappings and state, blockquotes for important constraints, fenced blocks for commands, and Mermaid for architecture or flows. Use relative links for repository files, meaningful image alt text, and a TOC when a document has more than five `##` sections.

中英文相鄰要保留半形空格，中文文件使用台灣繁體與全形標點。GitHub issue、PR 與留言的段落不做固定欄寬 hard-wrap；這份 repository knowledge 則以可讀性與穩定連結為優先。

## Private overlay boundary

Private memories and plans may contain local paths, credential handling details, machine-specific tooling, personal writing preferences, or unpublished work. The public tree records only the project-owned conclusion after sanitization. A private overlay may add local operating instructions, but it must not silently override the canonical architecture, verification, authorization, or release boundaries.

> Never commit credential values or locations, machine-specific tooling, or unpublished material. Keep the sanitized topic and its treatment in [`migration-ledger.md`](migration-ledger.md).

## Migration coverage

[`migration-ledger.md`](migration-ledger.md) is the no-gaps inventory for the 37 memory sources, 19 plan sources, and 2 local sources reviewed for this migration. Every row has an opaque source ID, a privacy boundary, a treatment, a non-empty canonical destination, and a verification statement. Private and other-project sources remain classified rather than copied into canonical project facts.
