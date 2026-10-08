---
status: active
id: kb-verification
kind: canonical
scope: repository
read_when: changing runtime code, running a local build or UX acceptance, parser output, cache behavior, FFI contracts, or this knowledge tree
last_verified: 2026-10-08
sources: [".github/workflows/ci.yml", ".github/workflows/ci-release.yml", "Makefile", "Package.swift", "scripts/bundle.sh", "Sources/Syrtis/ClientTray.swift", "Sources/Syrtis/StatusItemController.swift", "Sources/Syrtis/MenuBarTextColor.swift", "Sources/Syrtis/Views/AgentIconView.swift", "Sources/Syrtis/Views/SettingsPanel.swift", "Sources/Syrtis/SelfTest.swift", "Sources/CrossCheckHarness/main.swift", "Sources/TokenBarCore/AgentUsage.swift", "Sources/Syrtis/ClaudeExtraRoots.swift", "crates/tb_core_ffi/src/agent_account_scope.rs", "crates/tb_core_ffi/src/agent_quota_history.rs", "crates/tb_core_ffi/src/agent_kiro.rs", "crates/tb_core_ffi/src/kiro_integrations.rs", "crates/tb_core_ffi/src/extra_scan_paths.rs", "docs/knowledge/plans/provider-quota-pace.md", "docs/knowledge/plans/codex-historical-pace-v2.md", "public TokenBar-Windows PR #7", "public Syrtis PR #114", "public TokenBar-Windows PR #12", "AGENTS.md", "memory-derived hermetic verification practice", "memory-derived local build indexing incident"]
---

# Verification contract

## 文件目的

這份文件把 Syrtis 的驗證分成 deterministic fixture、跨語言契約、runtime smoke、cache invalidation 與 repository hygiene。目標不是堆命令，而是讓每個修正都證明「舊行為會失敗、新行為正確、常見資料不回歸」。

## 目錄

- [Evidence model](#evidence-model)
- [Hermetic fixtures](#hermetic-fixtures)
- [Runtime and FFI gates](#runtime-and-ffi-gates)
- [Local build and UX acceptance](#local-build-and-ux-acceptance)
- [Cache and sibling checks](#cache-and-sibling-checks)
- [Cross-language invariants](#cross-language-invariants)
- [Documentation checks](#documentation-checks)
- [Failure interpretation](#failure-interpretation)

---

## Evidence model

| Evidence layer | Answers | Cannot prove alone |
|---|---|---|
| Hermetic fixture | 觸發條件下 old/new 是否分歧、修正是否收斂 | 真實 GUI lifecycle 或 provider 網路狀態 |
| Unit or core test | 純函式、parser、fold、schema contract 是否穩定 | Swift/AppKit integration |
| FFI smoke | Rust -> C ABI -> Swift decoder 是否能端到端運作 | 所有特殊資料條件的正確數字 |
| Live app check | 真實 session、視窗 lifecycle、外觀與更新流程是否不崩 | 沒有觸發資料時的 correctness fix |
| CI | 可重複的 build/selftest/smoke gate | 本機 private data 與人工 UX 判斷 |

## Hermetic fixtures

當修正效果取決於本機可能沒有的 session、duplicate key、cursor、WAL、sibling metadata 或 provider cost 時，優先建立合成 fixture。測試應同時保留 old-fail/new-pass 證據，並另加無觸發條件的保值 case。

> **Hermetic 原則：** Live app 在沒有觸發條件時顯示「沒有變化」，只證明常見資料不崩，不能證明修正有效。權威證據是可重跑、與本機資料無關的 fixture。

PT0 的 hermetic authorities are Rust last-good and binding decisions, refresh status-before-body ordering, Grok monthly additive behavior, Copilot loader precedence, Antigravity precedence, and the Kiro multi-source token loader (kiro-cli SQLite and Kiro IDE token file) with its last-good caching; the FFI A/B publication-generation ordering and Swift diagnostic-candidate plus isolated UserDefaults scalar and local publication-state tests are required at the cross-language seam. The Rust fixture must pause A after its gate helper returns, let B obtain generation 2 and record its return first, then release A; it checks both return order and payload generations/content. A separate exhaustion fixture starts at `u64::MAX - 1` and proves the next call fails closed without invoking the publication body or repeating a generation. Swift fixtures distinguish bridge failure from malformed/missing successful data, prove Settings identities change across generations, legacy resolved values, selections, and exclusions, drive the tray apply seam with generation 2 terminal followed by generation 1 success to prove the late result resolves to generation 2 and cannot revive its scalar, and prove a generation 3 Dashboard publication replaces both an older tray payload and scalar before the tray's own poll returns while changing the gauge signature that gates immediate rendering. Live smoke requires authorization for that run and cannot replace these fixtures.

| Fixture property | Required assertion |
|---|---|
| Duplicate or replay | 舊路徑的 total 與對照路徑分歧；新路徑與對照收斂 |
| Sibling-only write | 預設 fingerprint 不失效；完整 fingerprint、mtime probe、prune 都失效 |
| Provider cost | 缺失成本可估算；明確 provider-reported 成本不可被 stale pricing 覆蓋 |
| Hidden client | non-empty partial selection 在 Rust fold 前排除未選 client；`nil`／empty clients 依 C ABI contract 代表 all clients；all-hidden 由 Swift lens strict membership 阻擋；對外發布面另以 hermetic payload-builder fixture 證明隱藏 client 的 token／cost 與 top-client 標籤都不出現在送出的 payload，且未註冊 id 在 producer 端就被正向 allowlist 濾掉——既不進數字也不進標籤，因此中性常數自 `payload()` 已不可達，改以「整張圖只有未註冊 id 時發布 nothing」證明其為死路 |
| Quota account scope | 以 temporary application-data root 驗證 exact 32-byte key、每次 reload、single winner、random／length rejection、key-loss orphan recovery、one-poll deferral、atomic pre／post-commit outcome、authenticated metadata MAC、valid payload＋valid MAC 的 wrong schema、valid-MAC semantic conflict、quarantine collision／identity-bound rollback與 raw-value privacy；macOS／Unix另驗證 directory `0700`／file `0600`、final symlink／non-regular／mode／inode swap fail-closed，以及 final directory 拒絕但 trusted root 下 symlinked ancestor 允許。Windows 端的 account-scope fixture 與低階 secure-storage mechanics 自 #276 起只存在於 Syrtis-Windows 自己的 `crates/tb_core_ffi`（[Syrtis-Windows#147](https://github.com/Nanako0129/Syrtis-Windows/issues/147)），本 repo 不再保留。不得呼叫真實 Keychain 或 provider credential |
| Quota history | Reset jitter、floating zero、duration lifecycle、partial／future-reset cycles、early／irregular reset 的 observed-duration close、supersede 門檻（28 小時以下的窗須逐 gap 證明與量子時代**完全相同**，不得只釘邊界；週窗須證明 20 分鐘漂移不關窗而 3 小時關窗；`successor_duration_from_series` 的測試兩個候選 group 必須帶**不同** duration，且低鍵那組帶**長** duration，否則放寬判準的變異無法被觀察）、切短 cycle 不進擬合（須同時斷言 `complete_cycles` 差 1 與 pace 相同，斷言在回傳值上；`advertised_nominal_duration` 的 `None` 分支須以 duration 異質的全 Observed series 覆蓋）、upgrade repair（他系列交易仍 restamp 殘留 group；舊點已 prune 則不發明 cycle）、active-series capacity、account isolation、corrupt recovery與current-actual shift都以temporary v3-family store（schema 4）驗證；Codex schema-2只驗current-account-only、byte-exact read-only migration，live provider refresh只作smoke |
| Quota history identity | 三次連續的 credential marker 變更必須產生**同一個** `SeriesKey`，且同一組 marker 仍必須產生**三個相異**的 `ProviderCacheBinding` 與三個相異的 plan-cache key——兩者要一起斷言，否則證明不了「拆身分」而只證明了「換了個鍵」。另需：`scope-history-v1` 的 known vector（期望值必須在 crate 外獨立算出，不得取自實作）與對 `scope-id-v1`／`scope-lineage-v1` 的 domain separation；常數路徑不得建立 metadata 檔；`accountScope` 為 `Err` 但 `historyScope` 為 `Ok` 的快照必須寫入**零**筆 history；每一條 authoritative 路線各有 byte-equality fixture。禁止在 real store 上跑 before/after——那會改寫使用者的生產歷史庫 |
| Overflow input | old arithmetic fails or wraps in the targeted site；new saturating path remains bounded |
| Individual client tray | UI-free SelfTest鎖定兩個defaults的parse前byte cap、entry／ID cap、deterministic serialization與超限no-writeback；client-scoped Auto／explicit／missing、error-only quota provider仍可配置、`antigravity-cli`只在quota lookup映射到`antigravity`且identity保持獨立、main完整route與per-client lens記憶互不污染、Settings row／picker狀態、official icon 1x＋2x reps、newer accepted publication與visible／AX／tooltip privacy都由synthetic graph／quota payload驗證，不建立真實system status items |
| Usage attribution | 政策以結構性斷言驗證而非逐條列舉：每個 bound provider 必須指名 `providerOwnClient` 保護對象、每個 first-party 廠商必須能從其 opencode 標籤解析回自己的 client、表內每個 client 必須在註冊表中。另驗證：來源 client 可為註冊表外的動態 id 而目標不可、自家訂閱優先且不需要 quota snapshot、未調查的來源回傳 nil 而非斷言 API 支出、已宣告的 router 計入它簽入的訂閱 |
| Quota curve snapshot | Binding admission 以真實 snapshots 驗證（trusted scope 不足以綁定：帶 `error` 的 last-good 與帶 `transport_diagnostic` 的 degraded 都必須排除），window key 由 production mapper 產生而非寫進 fixture，因此 mapper 端改身分會失敗而不是靜默 unbind；lifecycle 驗證 serialization 失敗保留前一個 tuple、generation 過期為錯誤、process restart 後不供應；binding lock 必須在 history I/O 前釋放，且 read 之後重新解析 binding——tuple 在 I/O 期間被替換（含同 generation 換帳號）必須 fail closed，settled binding 則仍正常供應 |
| Cache schema | 舊版本 cache 不被當成新 layout 靜默接受；新 layout 可重建並 reload |
| Provider transport fallback | last-good binding、refresh status-before-body、terminal/absent/4xx/schema/required-meter clearing、Grok additive monthly、Copilot loader、OpenCode Go loader／`usable_success("opencode")` last-good／sibling-isolated window decode、以及 diagnostic allowlist 都以 hermetic responses 驗證。Provider 註冊一致性（#324）：Rust `the_provider_table_is_todays_card_order` 釘住表格順序（一致性檢查都走訪表格，所以「從表中移除一個 provider」只有它能對每個 provider 都抓到；codex、kiro、opencode 另有既有 selftest 直接點名）、`every_provider_in_the_table_has_a_usable_success_rule`（含表外 id 的 control）、`fetch_in_order` 三個 stub 測試（完成順序≠表格順序、空結果與多帳號、互等 stub 證明並行）；Swift selftest 以 `tb_quota_provider_ids` 的回傳逐一檢查 ClientRegistry、官方 icon、demo 卡片與 transport 歸屬，並有「清單非空且含 codex」與「表外 id 記為 unknown」兩個 control。Rust 部分由 CI 的 `cargo test -p tb_core_ffi --locked` 執行（動到 Rust 的 PR 與每次 main push），Swift 部分由 selftest 執行。`ProviderId` 是 enum。以下都在非測試建置實測為編譯失敗：在 fetch 裡寫 `ProviderId::Test(..)`（E0599；測試建置下會通過，因為 `Test` 只存在於 `cfg(test)`）、`usable_success` 漏一個分支（E0004）、把 id 改成字串（E0308）。`the_caller_id_is_stamped_on_a_successful_snapshot` 以另一個 id 建立 fixture（呼叫前先斷言它帶著那個 id），經 apply 後發出的是呼叫端的 id、last-good 存在該 id 的 slot；拿掉蓋章那行即紅（先失敗的是發出 id 的斷言；把它拿掉，slot 斷言也會獨立失敗，因為 `usable_success` 對 fixture id 回 false、快取被清除）。「卡片以其資料來源 provider 的 id 發出」只有部分受測：`each_fetch_is_handed_its_own_entry_id` 釘住 `fetch_table` 把每列自己的 id 交給該列的 fetch（改成交別的 id 即紅），蓋章測試釘住 Success 分支。表格配對、fetch 是否把 id 傳下去，以及錯誤卡片、last-good fallback、`enrich` 等其他可能改動 id 的地方都沒有專門檢查，靠審查 |
| Grok Bot adapter | Synthetic desktop／SQLite stores 驗證 active-account precedence、解碼失敗不得跨帳號回退、signed-out 不復活 legacy login、team header 與 endpoint 配對；quota mapper 驗證 stable weekly key、provider duration、無效 meter、pooled allowance，以及 camelCase／snake_case 的明確無配額狀態；保留有效零用量與省略 optional flag 的對照案例。使用 temporary scope backend 驗證 credential rotation／帳號／team 的 cache 與 history 分離，並由 production outcome adapter 驗證同 request transient 才可沿用 last-good，terminal／absent／schema error 清除。Swift selftest 驗證分組成員、獨立額度開關、實際卡片 visibility filter 與 Auto selection 一致；另由正式 quota poll 和 curve reader 驅動 Bot-only fixture，沒有 graph／本機用量仍須載入 sparkline、summary 與 heatmap，並驗證 Build／Bot 分離、隱藏後清除、publication generation 切換與無配額不得沿用歷史；不得在 unit tests 使用真實 Keychain 或 provider credential。**Keychain 事前同意**另以五個 adapter case 驗證：加密 fixture 在同意缺席時必須回傳 marker error 且 decrypt closure 從未被呼叫（closure 本身是 `unreachable!()`），配一個計數 closure 的 control 證明同一份 fixture 在同意存在時確實走到 decrypt——沒有這個 control，只要 fixture 因為 key 名稱或 active account 寫錯而根本到不了 decrypt，否定式斷言就會安靜通過；同意缺席且存在 desktop 登入時不得回退到 Cursor 憑證（同樣配一個「沒有 desktop 登入時 Cursor fixture 確實載入」的 control）；`plaintext:v1:` secret 在同意缺席時仍須載入，證明 gate 釘在 decrypt 而不是「有登入」；純 Cursor 使用者完全不受影響。`agent_usage` 驗證 marker 映成 `source == "keychain-consent"`、其餘 terminal 與成功都仍是 `"oauth"`（否則測試可由「全部映成新 marker」通過），且該 source 會進入實際發布的 snapshot。Swift selftest 驗證 placeholder source 不進 `configuredClientIds`（配一個「一般 error 卡片仍可導覽」的 control）、consent 列仍留在限額卡片、儲存的答案為三值（`nil`／`false`／`true`，不可塌成 `bool(forKey:)`），以及授權會送出精確的 `{"grok-bot":true}` 並喚醒輪詢迴圈、拒絕則兩者皆無。變異證據：刪掉 decrypt 前的 guard 使兩個 adapter case 轉紅；把 control 的同意翻成 false 使 control 轉紅 |
| FFI publication | Provider run、JSON serialize、envelope、raw C-pointer publication 的 single-flight、gate-assigned checked `publicationGeneration`、exhaustion fail-closed、可反轉的 C return order，以及單次 run 內 provider 並行分別驗證 |
| Source-aware filter parity | `tb_filter_parity_probe` uses one context, fresh graph, `token0…token5`, exact integer comparison, diagnostic-only cost deltas because pricing refreshes independently from source generations, and short-circuits invalid later scans. Its synchronous callback seam tests stable match/mismatch, price-only refresh, tokenUnavailable versus graph failure, every source-change boundary, Agents-only late changes, call ordering, and a size-changing append token. The dedicated vendor fixture covers canonical, cc-mirror exact gating, synthetic gateway, duplicate canonical paths, inherited scanner-root isolation, unattributed `Main`, and cold/warm cache parity. |
| Extra scan roots | `crates/tb_core_ffi/src/extra_scan_paths.rs` builds a real `LocalSourceContext` pointed at a temp `home_dir` and writes real fixture `.jsonl` files under a second temp root, then calls `set_from_json` and re-derives `report_options`/`parse_options` — no mock scanner. Because Claude's dedup key (`{message.id}:{requestId}`) does not include the source root, every fixture states which content relationship it exercises: id-disjoint (extra root sums with the primary), duplicate dedup key across two roots (total unchanged), and the same path registered twice (canonical dedup, unchanged). Coverage: adding an id-disjoint extra root increases the total and removing/never-calling the setter reproduces pre-feature behavior exactly (`scanner_settings.extra_scan_paths` equals `ScannerSettings::default()`'s empty map); mutating the extra fixture's token count is demonstrated red-then-green by reverting the `report_options`/`parse_options` wiring and rerunning the suite, which turns 7 of the 12 cases red while exactly the five that do not depend on the wiring stay green (duplicate dedup key, malformed JSON, file-as-root, never-call, and unsupported client id) — that boundary is the expected one; a path is registered whenever it can still become a readable directory, so an absent root stays in the registry and a later scan picks it up with no second setter call (the regression creates the directory and its fixture after registering and asserts the total moves 0 → 100), while a path that can never be a scan root — empty, relative, or an existing non-directory — is rejected and kept out, with the file-as-root case using a real parseable transcript so that a regression would show up as its contents entering the totals rather than as a shape mismatch; an unsupported client id is rejected before any path is counted; none of these panic or block the rest of the registry; malformed JSON returns a structured error and leaves the registry untouched; a warm `tb_graph` cache cannot hide a newly registered root, by two separate mechanisms that are tested separately — `local_source_change_token` moves once the root's files enter the scan set, which covers a cache entry that has aged past its window, and `lib.rs`'s `setting_extra_scan_paths_drops_the_caches_that_could_answer_from_the_old_roots` covers the window itself by seeding `GRAPH_CACHE` and the tail stamp, driving the real `extern "C"` setter, and asserting both are gone; that test drives the FFI entry point rather than `set_from_json` because the invalidation hangs off the wrapper, and deleting the `invalidate_scan_caches()` call turns it red; `a_scan_that_started_before_a_root_change_does_not_publish_its_result` covers the third window, an in-flight scan publishing after the clear, by taking the generation as a scan would, moving it through the real setter rather than by bumping the counter directly, and asserting both `publish_graph` and `stamp_tick_if_current` refuse — with a same-generation control so a guard that refused everything could not pass; its three mutation targets (each guard's generation check, and the bump itself) were each removed and each turned it red; and a config dir's `projects` and `transcripts` sub-roots both contribute (id-disjoint), proving neither sub-path is silently dropped. `Sources/Syrtis/SelfTest.swift`'s Claude-extra-scan-roots section is Swift-only and does not call the FFI (the live process re-reads `HOME` from the environment, so Swift cannot point it at a temp root); it covers `ClaudeExtraRoots.expand`'s two-sub-root output, `payloadJSON`'s exact wire shape (one dir, multiple dirs, an explicit empty array on `[]` rather than an absent key, and dedup of a repeated dir), and the `isRejectedRoot`/`isMissing` path-sanity predicates. |

> **Quota account-scope evidence boundary：** Legacy development Keychain path 仍被架構禁止。本次 test refactor 的 production-prefix byte freeze 只證明這次沒有新增或修改 production bytes；compiler／build 只能證明 module 被編譯並符合型別與連結契約，不能自動證明未來不會重新引入 Keychain query。Source／Cargo spelling scan 不是 behavioral guard，因此不列入此 fixture contract。

> Quota-history schema 4 migration：v3 → v4 是就地、惰性的升級，驗收全部釘在「能失敗」上。`a_v3_store_upgrades_in_memory_keeps_every_sample_and_converts_on_disk_only_when_written` 把三條宣稱綁在同一個函式裡，因為順序是隱形的前提：v3 store 若被 quarantine 而非升級，store 會是空的，「每筆樣本的 plan 都是 None」在空集合上恆真。它先斷言樣本數（5 筆、2 條 series，且刻意跨 phase bucket——同桶樣本會互相取代，否則 fixture 會比讀起來小得多），再斷言載入後未 quarantine、schema 為 4、且與升級前的 store 完全相等；接著重讀磁碟，斷言檔案仍是 `schemaVersion: 3` 且樣本仍無 `plan` 鍵（惰性語意本身，若只斷言記憶體中的版本，惰性與積極兩種實作都會綠）；最後跑一次真的會寫檔的交易，才斷言磁碟轉為 4、樣本數 N+1、且全部 `plan` 為 `None`。fixture 由真實寫入路徑產生後再降級成 v3 形狀，並斷言 `plan` 鍵確實存在才移除——否則某天停止序列化該欄位會讓這個測試悄悄退化成 v4 讀 v4。V5（`#[ignore]`、由 `TOKENBAR_QUOTA_STORE_COPY_SOURCE` 指向操作者自行複製的真實 store 副本）連同另外三個同型的 operator-copy 測試已於 2026-09-07 移除（#274）：它們從不在 CI 執行，hermetic 的 `a_v3_store_upgrades_in_memory_keeps_every_sample_and_converts_on_disk_only_when_written` 才是 gate。結尾的位元相等只能證明「本測試沒有改動來源」，對複製與斷言之間的 panic 無效、對行程被砍完全無效——不開啟實體檔案才是唯一能挺過那些的保證。本機以副本實測 31 series／2835 樣本全數保留。`a_v3_reader_cannot_parse_a_v4_store` 用一份 v3 形狀的鏡射結構把降級的不可逆性變成可證偽的斷言，而非散文。變異驗證逐一紅轉綠：拿掉升級分支（hermetic 與真實資料各紅一次，後者的失敗訊息正是使用者會遇到的 quarantine）、把升級改成載入即寫檔、讓寫入路徑塞入非 `None` 的 plan、拿掉鏡射結構的 `deny_unknown_fields`、給新欄位加上 `skip_serializing_if`。
>
> Historical runtime retirement checkpoint（2026-07-31）：`agent_history.rs` 的 v2 writer／evaluator 與專用 evaluator tests 已刪除；現行 gate 是 `agent_quota_history.rs` 的 **schema-4** evaluator 加上 current-account-only schema-2 importer regressions（檔名中的 `v3` 是 store 家族代號,與 schema 版本無關——見上一段的遷移）。舊 Historical pace v2 cross-port checkpoint 仍保留於下方，僅作歷史 parity 證據，不是 current gate。

> Load-time repair persistence（#207）：`a_load_time_clock_repair_reaches_disk_once` 與 `a_load_time_sample_drop_reaches_disk_once` 各跑兩次 body 什麼都不改的交易，斷言第一次 `save_call_count` 為 1、寫出的檔案不再需要修復，第二次仍為 1（修好的檔案不重寫）；兩者都先斷言 fixture 在磁碟上確實需要修復，否則「寫了一次」可能只是別的原因。`a_v3_stamp_alone_is_not_written_by_a_no_op_transaction` 釘住惰性升級不受影響：只有 v3 版本號的檔案，同樣的交易 0 次寫檔、位元組不變。存檔失敗時，若 body 本身沒有變更，`a_failed_repair_write_back_does_not_fail_the_transaction` 斷言交易仍回 `Ok`（寫回修復只是 housekeeping），並以 body 有變更時仍回 `AtomicSave` 當 control。v3 檔若同時需要修復，會隨這次寫回一併轉為 v4，與任何其他有寫入理由的交易相同。`a_future_sample_drop_reaches_disk_once_and_keeps_the_series` 釘住時鐘修復只丟樣本：一條 series 同時有上限以前與超過上限的讀數，no-op 交易寫檔一次，磁碟上 series 仍在、只剩上限以前那筆，第二次不寫；`future_sample_evidence_drops_only_that_sample` 在載入端斷言同一件事並確認兄弟 series 不受影響。變異驗證（2026-09-27 實測）：把樣本過濾改回整條 series 刪除、或完全不過濾（兩個 future-sample 測試紅）；交易忽略 `repaired`（時鐘、樣本丟棄、future-sample 寫回三者紅）；樣本丟棄不設旗標（樣本丟棄那個紅）；時鐘修復不設旗標（時鐘與 future-sample 寫回兩者紅）；旗標恆為真（時鐘、樣本丟棄、future-sample 寫回、v3 四者紅）；存檔失敗一律回錯（存檔失敗那個紅）；過濾邊界改成 `< upper_bound` 或 `<= observation_now`（future-sample 寫回紅，它有一筆剛好在上限、一筆落在 `(observation_now, upper_bound]`，並以 control 斷言四筆讀數各在不同 phase bucket）。`repair_is_a_fixed_point` 的 fixture 也含一筆超前樣本，確保冪等性是在新分支上量的。

## Runtime and FFI gates

The current CI runtime sources are [`.github/workflows/ci.yml`](../../.github/workflows/ci.yml) (every PR and every main push) and [`.github/workflows/ci-release.yml`](../../.github/workflows/ci-release.yml) (main pushes only: the release-configuration build and the bundled selftest, beside `ci.yml`). CI builds the Rust static library (on a pull request whose Rust inputs match an entry main saved, it links that library instead of rebuilding — the key and its inputs are in the workflow; pushes to main always build it), builds Swift, runs the core selftest, and runs the FFI smoke binary. Both `ci.yml` and `release.yml` run on GitHub's `xcode-27` image, and a local build needs Xcode 27 too: the macOS 27+ glass panel is built on an API only the macOS 27 SDK has (see [`history/liquid-glass-experiments.md`](history/liquid-glass-experiments.md#macos-27-glass-panel)). Those are CI build and smoke checks, not the complete local code-change gate. The local build order comes from [`Makefile`](../../Makefile) and the linker contract comes from [`Package.swift`](../../Package.swift).

```bash
cargo build --release
swift build
make selftest          # = swift run Syrtis --selftest -AppleLanguages "(en)"
swift run Syrtis --smoke
```

每一項都跑 debug configuration 的 bare executable，`Bundle.main.bundleIdentifier` 因此是 nil。凡是以那個差異為條件的值，在 suite 看得到的地方是一種樣子、在出貨的地方是另一種樣子，而任何 source scan 都關不掉這個缺口（#146 寫過三道、三道都被繞過，缺口不在原始碼文字裡）。CI 因此在 push 到 main 時多跑一次 `make selftest-bundled`：release build、裝進 `.app`、從 bundled binary 執行同一套 suite。

bundle identity 是這個 target 唯一的旋鈕，而它在兩個危害之間取捨。bundled run 會把 `UserDefaults.standard` 解析到 identifier 指名的 domain，而 suite 確實會寫進去（`PopoverChrome.heightKey` 與 dashboard year key 都是先寫再還原，因為生產型別直接讀 `.standard`）；用出貨 identifier 就會寫進安裝版自己的偏好設定。

所以本機預設是拋棄式的 `com.nyanako.tokenbar.selftest`，而**那是比較弱的 gate**：它抓得到以「identifier 為 nil」為條件的值，抓不到以出貨字串本身為條件的值——後者在本機走安全分支，只有裝起來之後才走另一條。CI 用 `make selftest-bundled SELFTEST_BUNDLE_ID=` 補上，空值代表「`scripts/bundle.sh` 的預設」也就是出貨 identifier；runner 是拋棄式的、沒有安裝版可污染，開發者的 Mac 有。空值而非再寫一次字面值，是為了讓未來改名只有一處要動、不會把這道 gate 無聲地弱化成對不上的字串。

app 名稱**不是**第二個旋鈕。它一度是，而那等於把同一個逃脫換一個屬性重演一次：以 `CFBundleName == "Syrtis"` 或 bundle URL 結尾為條件的值，在叫別的名字的 gate 裡會走安全分支。所以它組出來的是貨真價實的 `Syrtis.app`，改用 `OUT_DIR=dist/selftest` 讓路，順便仍然不會覆蓋手動建的 `dist/Syrtis.app`。

與正式發版 bundle 仍然不同、且**不打算**逐輪 review 才發現的部分：安裝路徑（`dist/selftest/` 而非 `/Applications/`）、version 與 build number（用 `bundle.sh` 預設，正式值由 `release.yml` 傳入）、簽章（ad-hoc 而非 Developer ID）。以這三者為條件的值超出這道 gate 能觀察的範圍，任何本機組裝的 bundle 都關不掉，只有安裝 notarized build 才行。這裡**觀察得到**的是 identifier、名稱，以及 release configuration 本身。

**它不是 `make selftest` 的超集**：`#if DEBUG` 後面的斷言在 release configuration 不存在，所以 bundled run 的斷言數比較少。兩者都是 gate，互不取代。

### Local full code-change gates

For Rust or cross-language code changes, the local full gate adds formatting, the Rust test suite, the all-targets Clippy pass, and the repository build:

```bash
cargo fmt --all -- --check
cargo test
cargo clippy --workspace --all-targets
make build
make selftest          # = swift run Syrtis --selftest -AppleLanguages "(en)"
swift run Syrtis --smoke
```

`cargo test` and `cargo clippy --workspace --all-targets` are local full code-change gates. CI runs a subset on every run that builds Rust (a PR whose Rust inputs or the fixtures its tests read under `Fixtures/` and `Tests/fixtures/` changed, and every push to `main`; a PR that links main's cached library skips them with the build): `cargo test -p tb_core_ffi --locked` — the FFI crate's tests, not the whole workspace's — and `cargo clippy -p tb_core_ffi --all-targets --locked --no-deps`, which fails on error-level lints only (`tb_core_ffi` currently has warnings). CI does not lint `vendor/tokscale-core`: its `#![deny(clippy::all)]` under the floating stable toolchain would let a new Rust release turn every run red, so the workspace-wide Clippy remains a local gate. `--no-deps` is what excludes it; `-p` alone still lints the engine, because a path dependency that is a workspace member gets no lint cap. In the local gate, `--all-targets` is required because `vendor/tokscale-core/src/lib.rs` declares `#![deny(clippy::all)]`, so a test-only lint can fail the gate even when the library target itself is clean. In CI's command, where `--no-deps` leaves the engine out, `--all-targets` makes Clippy check `tb_core_ffi`'s test targets as well as its library.

Live account-scope smoke必須在hermetic security suite通過後才執行，且每次重新執行都需要當次明確授權。若出現任何Keychain或credential授權視窗，立即停止process並把smoke判為失敗；不得要求輸入登入密碼、讀取secret或用真實credential診斷。

| Gate | Expected evidence |
|---|---|
| Rust | Release static library builds from the current source |
| Swift | SwiftPM links against the freshly built library from repository root |
| Selftest | UI-free TokenBarCore assertions pass。部分斷言逐字比對英文 UI 文案，因此語系必須鎖定 `en`（用 `make selftest`，或自行帶 `-AppleLanguages "(en)"`）；在中文系統上直接跑 `swift run Syrtis --selftest` 會因 `Format` 輸出中文而假性失敗，入口會先印出提示 |
| Bundled selftest | 同一套 suite 從 `dist/selftest/Syrtis.app` 的 release binary 通過（`make selftest-bundled`），證明斷言看到的是出貨 configuration。CI 只在 push 到 main 時跑（`.github/workflows/ci-release.yml`，與 `ci.yml` 平行；打 tag 前兩個 run 都要綠），並以 `SELFTEST_BUNDLE_ID=` 帶出貨 identifier；本機預設拋棄式 identifier＝較弱版本。斷言數少於 debug run（`#if DEBUG` 的部分不存在），不是超集 |
| Smoke | Every C ABI entry point decodes or reports an intentional error envelope；account-scope path不得存取Keychain或顯示credential authorization UI |
| Account-scope storage | Hermetic security tests先證明permission、path、locking、atomicity與recovery；live smoke只驗證shipping data flow不彈授權UI，不取代fixture correctness |
| Windows secure storage | M19-B0證明Native candidate與核准Windows security/storage semantic source等價；M19-B1又在hosted Windows x64 runtime執行CNG、owner／exact protected DACL、final-component reparse、file identity、exclusive no-delete-share lock、replace、quarantine、legacy upgrade與error-privacy tests。合併後的exact Windows source另在real ARM64 Windows通過351項Rust tests、12-case provider-v3 CrossCheck、ARM64 PE checks與synthetic WinUI startup；macOS tests與GitHub ARM64 cross-package都不取代這些runtime assertions |
| Relink safety | If Rust changed without Swift source changes, the stale executable is removed before linking |
| Rust format | For Rust changes, run `cargo fmt --all -- --check` on the touched scope; vendor formatting policy may be intentionally separate |
| Local Rust tests | `cargo test` passes across workspace crates and test targets |
| Local Clippy | `cargo clippy --workspace --all-targets` passes, including test-only targets |

## Local build and UX acceptance

`make selftest-chart-input` 是 macOS 27+ 桌面上的獨立 GUI gate，不併入無介面的 `make selftest`。它以 `--demo` 啟動真實 `StatusItemController`／`PopoverView`，先確認另一個應用程式位於前景，再驗證顯示面板會讓受測應用程式成為系統前景並取得 key window；將原生滾輪夾具送入已掛載圖表的 `scrollWheel` 後，真實 `NSScrollView` 必須移動，且不能改變圖表縮放。呈現後的捏合 handler 另驗證縮放與頁面位置。測試會短暫佔用前景，外部點擊或切換視窗會使它報錯；結束時移除測試視窗與狀態列項目、還原偏好及前景應用程式。這個 gate 驗證輸入處理與真實 responder 鏈，不合成系統的滑鼠／觸控板事件投遞，仍須搭配下述實機驗收。

> **⚠️ `make run` 與 bundle 讀的不是同一個 `UserDefaults` 網域。** `swift run Syrtis`（`make run` 就是它）產出裸執行檔、沒有 `CFBundleIdentifier`，`UserDefaults.standard` 因此落在行程名網域 **`Syrtis`**；bundle 用 **`com.nyanako.tokenbar`**（背景見本文件上方 bundle identity 段落）。兩份偏好互不可見，且裸執行檔那一份會隨開發過程被寫入，內容與使用者實際設定無關。
>
> 凡是驗收由偏好驅動的畫面——**usage attribution 宣告、Settings 持久化、狀態列項目狀態**——一律用 `make bundle` 產生的 `dist/Syrtis.app`，並在**出貨 identifier 下**跑，前後備份還原使用者的偏好：
>
> ```bash
> # 驗收前：先退出 /Applications/Syrtis.app，兩者同網域不可並行
> BACKUP=$(mktemp ~/tokenbar-prefs-XXXXXX)
> defaults export com.nyanako.tokenbar "$BACKUP" \
>   && plutil -lint "$BACKUP" \
>   && echo "備份完成：$BACKUP"   # 沒印出來就不要往下做
>
> # ……驗收……
>
> # 驗收後：先結束受測 app 並等它真的退出
> osascript -e 'quit app "Syrtis"' 2>/dev/null || true
> while pgrep -f 'dist/Syrtis.app' >/dev/null; do sleep 1; done
> # 備份不可用就停手：寧可留著髒偏好，也不要刪掉沒有備份的網域
> plutil -lint "$BACKUP" \
>   && defaults delete com.nyanako.tokenbar \
>   && defaults import com.nyanako.tokenbar "$BACKUP"
> ```
>
> **`mktemp` 與兩次 `plutil -lint` 都不是裝飾。** `defaults delete` 是破壞性的；若備份失敗而刪除仍照跑，結果是使用者的正式偏好被清掉且無從還原，或被上一次跑剩的舊檔蓋回去。三個環節各擋一種失敗，沒有一個能取代另一個：`mktemp` 保證檔名不重複（時間戳做不到，兩次執行落在同一秒就會撞）；`&&` 串住 export 的結束狀態；`plutil -lint` 擋掉「寫到一半失敗但檔案非空」——實測磁碟寫入截斷的 plist 會讓 `test -s` 通過而 `plutil -lint` 回非零。還原前再 lint 一次，因為備份是在驗收之前做的，中間隔著人的操作。
>
> 路徑刻意放家目錄而不是 `mktemp -t` 的 `/var/folders`：那裡重開機會整棵清掉，而這份備份是使用者偏好的唯一副本。`mktemp` 的模板 `XXXXXX` 必須在字串結尾，接副檔名不會被替換（實測會生出字面叫 `XXXXXX` 的檔）；`defaults export`／`import` 不需要 `.plist` 副檔名。跨終端機做驗收時把印出來的那個路徑帶著。
>
> **結束 app 那兩行不可省。** 還原一個「還有行程在寫」的網域，本質上就是無效的：受測 app 在前景時持續輪詢額度並寫回自己的偏好（例如 `TrayAnimator.swift:221` 的 `tokenbar.quota.lastRemaining`），所以它可以在 `defaults import` 跑完之後再蓋一次。選單列 app 沒有視窗，最容易忘記它還在。
>
> **`delete` 那一步也不可省。** 實測（2026-09-13，於無關網域）：`defaults import` 是**合併**不是取代——驗收期間新增的鍵會存活下來，只有備份時就存在的鍵會被還原成舊值。先 `delete` 整個網域再 `import` 才會完全還原。
>
> **不要改用一次性的 `BUNDLE_ID` 來迴避備份**，即使那看起來更乾淨。本文件上方已經對同一個旋鈕做過完整推理並得出相反結論：拋棄式 identifier 是**比較弱的 gate**，抓不到以出貨字串本身為條件的值。當場就有實例——`SnapshotStore.swift:364` 要求 bundle id **字面等於** `com.nyanako.tokenbar`，換了 identifier 就整個關掉 restart snapshot，那個面因此無法驗收。
>
> **identifier 只隔離偏好，隔離不了資料。** `crates/tb_core_ffi/src/agent_quota_history.rs:454`、`:518` 與 `agent_account_scope.rs:220` 都是 `dirs::data_dir()` 接上**寫死的** `com.nyanako.tokenbar`，不由 bundle id 推導。所以本機 UX 驗收無論用哪個 identifier，都會讀寫使用者正式的 quota-pace history 與 account-scope storage。`~/Library/Caches/SyrtisDashboardSnapshot` 同樣是固定路徑。**而且這不限於刻意驗額度的場合。** 非 demo 的 bundle 一啟動，`TrayAnimator.start()`（`TrayAnimator.swift:180`）就無條件進入額度輪詢，走生產路徑的 `enrich_snapshot`（`agent_usage.rs:1504`）把觀測寫進那份固定路徑的 store。所以**任何**非 demo 的本機 bundle 驗收都在寫使用者的正式歷史庫，連只看 Settings 或狀態列的也是。要嘛連那份 store 一起備份還原，要嘛改用 `--demo`。

> 這條規則來自一次實際的假回歸。Codex 時間窗歷史卡的每一列都顯示零 token 與零金額，並印出「額度變動了 N%，但這台機器上沒有記錄到」，而同一台機器上安裝的 bundle 顯示正常；當時 engine pin 剛推進過，於是看起來像那次推進造成的回歸。實際鏈條與 engine 無關：`tokenbar.usage.attribution.confirmed` 在兩個網域的內容不同，受測 client 在 bundle 網域有宣告、在行程名網域沒有，於是 `UsageAttribution.resolve` 回 `.unassigned`，`QuotaHistory.swift` 的 `spanTotals` 歸屬閘門把該 span 的每一則訊息都跳過，`spanTokens` 與 `spanCost` 皆為 0，`WindowEquivalence.aggregate` 因此回 `.unaccounted`。週期本身讀固定路徑的 `quota-pace-history-v3.json`，不受網域影響，所以列仍在——這正是它看起來像資料缺失而非組態差異的原因。

> 同時排除掉的方向，記下來避免重查：推進前後兩個 engine pin 對同一批 span 回傳位元相同的訊息數與 token 總量，隔離快取也不改變結果——該鏈條不經過 `UsageAttribution`。

> **不要**讓裸執行檔改讀 `UserDefaults(suiteName: "com.nyanako.tokenbar")` 來迴避這件事：那會讓開發執行檔寫進使用者正式的偏好網域，摧毀 `SELFTEST_BUNDLE_ID` 建立的隔離。

不需要 `.app` bundle 語意、且不依賴 `UserDefaults` 的人工 UI 檢查，優先從 repository root 執行 `swift run Syrtis --open-popover`。需要 `make bundle` 產生的 `dist/Syrtis.app` 的有兩類：一是 icon、`Info.plist`、`LSUIElement`、Sparkle、autostart 或安裝路徑等 bundle-only 行為；二是**任何由偏好驅動的畫面**（usage attribution、Settings 持久化、狀態列項目狀態），且必須照上方網域警告的備份還原程序走。以 Argument Domain 注入初始偏好的 deterministic 檢查**不需要備份還原**，因為它們跑在裸執行檔上、只碰 `Syrtis` 網域，動不到使用者正式的偏好。但**不要以為它們因此就與既有網域無關**：Argument Domain 只覆寫命令列傳進去的鍵，其餘一律往下落到 application domain。例如 `SettingsWindowView.swift:56` 讀 `tokenbar.tabs.hidden`，而下方那組指令沒有注入它，於是開發過程累積在 `Syrtis` 網域的舊值會改變你看到的列。這類檢查的 deterministic 只在「該畫面的每一個輸入都被注入」時成立；做不到就先 `defaults delete Syrtis` 清掉再跑。

Provider quota pace 以 `swift run Syrtis --demo --open-popover` 提供 deterministic 人工驗收面；snapshot badge 明示 `FIXTURE`，且 `DemoUsageDataSource` 不呼叫 live FFI、不讀寫 quota cache。Historical／Linear／Off 都要實際呈現；驗收時必須區分低 remaining 觸發的 quota 長條黃／紅健康色，與 deficit stage 觸發的 pace marker／footer 橘色。橘色只看 actual 有沒有越過 expected 線，不看是哪個 estimator 畫出那條線——Historical 與 Linear 的 deficit 同色，狀態文案仍必須分辨兩者。舊規則（只有 `available` 可上色）已廢止：`available` 由每次 refresh 重跑的 out-of-sample fit gate 決定，同一張卡會在 Historical 與 `learningHistory` 之間來回，把顏色綁在 basis 上會讓使用者看到預測「一下子就不見了」，而底層 deficit 其實一直存在。

Menu bar 的「Font color」驗收涵蓋 Automatic／Custom 切換、正常／偏低／不足三個獨立色塊下方的 16 色面板與 HEX 輸入、深淺預覽、主項目與獨立用戶端數值未改變時立即換色，以及切回 Automatic 後恢復額度／系統字色。以獨立 UserDefaults suite 驗證舊單色設定保留為正常色、另外兩色的預設、三色保存後重讀、25%／10% 邊界與邊界上方的色階選擇、非額度文字／無額度使用正常色、無效儲存值只回退該檔位，以及恢復自動時保留自訂值；同時鎖定自動 gauge 的原始色值。這些 selftest 不取代實際互動。GUI 需確認面板位於自訂顏色列下方、沒有系統調色盤，連續切換三個色塊會更新同一面板的編輯檔位，分別換色不改動其他兩色，並驗證輸入 `#000000`、小寫與省略 `#` 的正規化，以及未完成／無效輸入不改動上一個有效顏色，並在輸入框失去焦點時清空。Argument Domain 注入可驗初始配色，不代表互動寫入或跨 process 持久化已驗證。

Individual client items的deterministic Settings檢查以Argument Domain注入初始偏好；這只驗visual state與initial routing，不在同一process宣稱toggle persistence：

```bash
swift run Syrtis --demo --settings \
  -tokenbar.tray.clients.enabled claude,codex \
  -tokenbar.tray.clients.quotaSelections '{"claude":"auto","codex":"weekly.v1"}'

swift run Syrtis --demo --settings \
  -tokenbar.tray.clients.enabled claude,codex \
  -tokenbar.tray.clients.quotaSelections '{"claude":"auto","codex":"missing.v1"}'

swift run Syrtis --demo --settings \
  -tokenbar.tray.clients.enabled claude,codex \
  -tokenbar.tray.clients.quotaSelections '{"claude":"auto","codex":"weekly.v1"}' \
  -tokenbar.tabs.hidden codex
```

> **本機 bundle 邊界：** `dist/Syrtis.app` 是暫時的驗收產物，不是第二份安裝。日常使用與正式更新的 source of truth 仍是 `/Applications/Syrtis.app`。
>
> `make selftest-bundled` 另外會在 `dist/selftest/Syrtis.app` 產生同名 bundle。它刻意與出貨同名同 identifier（見上方 gate 段落），所以**兩者不可混淆**：UX 驗收與下方清理程序談的一律是 `dist/Syrtis.app`。selftest 產物只被直接執行、在 app lifecycle 之前就結束，不會被 LaunchServices 註冊；`bundle.sh` 也會在 `dist/selftest/` 放一份 `.metadata_never_index`。不需要時整個目錄刪掉即可。

[`scripts/bundle.sh`](../../scripts/bundle.sh) 會在組裝 app 前建立 `dist/.metadata_never_index`，避免 Spotlight 主動索引本機 bundle。但這個 marker 不會回溯刪除既有 Spotlight metadata；實際啟動 `dist/Syrtis.app` 也可能讓 LaunchServices 註冊它。因此本機 UX 驗收完成、且不再需要該 bundle 作為 release artifact 時，應撤銷這個特定 app 的註冊並刪除生成物，不要以重設整個 Launchpad database 作為第一步。

| UX surface | Preferred path | Completion evidence |
|---|---|---|
| Popover、lens、keyboard、scroll、appearance | `swift run Syrtis --open-popover`；但 lens 記憶與 appearance 的互動寫入是偏好驅動的，那部分依上方網域警告改用 bundle 加備份還原 | 實際操作與必要截圖；結束測試 process |
| Individual client status items | `--demo --settings`配合兩個M2 Argument Domain keys驗initial visual state；本機資料、placement／right-click／跨螢幕則用同一`dist/Syrtis.app`（placement 記憶存在 app 自己的偏好網域，依上方網域警告備份還原，不要換 identifier——換了等於記憶歸零） | 預設只有主item；switch點擊後立即以`.mini`原生狀態更新，client shell可稍後於同一defaults reconciliation建立但不得阻塞control setter；Settings body重建不得同步呼叫`SMAppService.status`（本機量測單次約0.5～0.9秒），關閉／重開與連續toggle都須維持可互動；enable／disable／hide／restore保留selection與`tokenbar-client-<id>` placement；`antigravity-cli`在本機error-only provider狀態仍可配置；dashboard選定單一年份時，Settings仍以all-time graph保留所有live client item rows並可停用；client A→B沿用同一popover並各自恢復本次app session停留的lens；主item恢復自己的client-plus-lens route，不被individual item覆寫；主item right-click仍只改global source；1x／2x雙向移動圖示清晰；VoiceOver label不含raw card／error，explicit error fallback固定讀作last-known而非current quota；0與8 items的idle profile沒有per-client loop |
| Icon、bundle identity、Sparkle、autostart | `make bundle` 後啟動 `dist/Syrtis.app` | 記錄 bundle-only 行為；完成後 unregister 並移除本機 bundle |
| Homebrew、Sparkle stable update、正式安裝路徑 | `/Applications/Syrtis.app` | 不以 `dist/Syrtis.app` 代替 installed-app 驗收 |

從 repository root 清理已完成驗收的本機 bundle：

```bash
ROOT="$(git rev-parse --show-toplevel)"
LOCAL_APP="$ROOT/dist/Syrtis.app"
LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"

test -e "$ROOT/dist/.metadata_never_index"
"$LSREGISTER" -u "$LOCAL_APP" 2>/dev/null || true
rm -rf -- "$LOCAL_APP"

```

清理後，Spotlight 與 LaunchServices 查詢都不應再列出 repository 的 `dist/Syrtis.app`；正常情況只保留 `/Applications/Syrtis.app`：

```bash
mdfind "kMDItemContentType == 'com.apple.application-bundle' && kMDItemFSName == 'Syrtis.app'"
"$LSREGISTER" -dump | grep -F 'Syrtis.app'
```

若仍有 stale 結果，先等待 metadata service 收斂並重查；不要直接清空整台機器的 Spotlight index 或重設 Launchpad，因為那會波及其他 app 與使用者排列。

## Cache and sibling checks

A source reader that consumes secondary files must be verified as one unit. The regression matrix is deliberately broader than the parser function itself.

| Seam | Check |
|---|---|
| Fingerprint | Primary-only change and sibling-only change produce different fingerprints |
| Active lane | The source is reachable by the streaming and materialized consumers that claim support |
| Latest mtime | Live-tail change token observes every relevant sibling and WAL |
| Pruning | `modified_after` keeps a session when a relevant sibling is fresher than the primary |
| Cache rebuild | Same-fingerprint stale serialized data is rejected when parser output or attribution changes |
| Report parity | Materialized and streaming reports agree on the fixture's selected fields |

## Cross-language invariants

| Contract | Verification |
|---|---|
| Heap JSON ownership | Every successful FFI pointer is decoded and released through `tb_free`; errors do not leak a second ownership path |
| Envelope shape | `ok` and `data`/`err` fields match `ctb.h` and Swift decoders; agent usage classifies `ok:false` as fixed bridge failure and malformed or `ok:true` missing data as fixed decode failure without public associated text |
| Publication order | Rust `publicationGeneration` is additive, gate-owned, and checked against exhaustion; one shared Swift `@MainActor` coordinator rejects lower generated payloads across DashboardModel, Settings, tray polling, and snapshot restore before payload or scalar apply, while missing generations pass through without changing state |
| Windows compatibility | M19-B1's production C# decoder ignored additive `publicationGeneration` while preserving existing fields; no DTO, C signature, or ownership change was required |
| Windows storage ownership | Owned by Syrtis-Windows since #276 ([Syrtis-Windows#147](https://github.com/Nanako0129/Syrtis-Windows/issues/147)); its copy of `crates/tb_core_ffi` is the source of truth for the CNG／DACL／secure-open／identity／lock／replace／quarantine primitives and their tests. This repository carries none of that code |
| Client filter | Non-empty selected IDs reach Rust before mixed buckets are folded; `nil`／empty client lists mean all clients per `ctb.h`; the Swift lens strict-membership check blocks all-hidden views |
| Arithmetic | Rust report totals, FFI mappers, Swift models, and live-rate consumers use bounded arithmetic where required |
| Stale-data policy | A failed refresh retains the last good value instead of blanking a working card；fallback preserves display-ready fields only for a structural same-binding request transient, marks account scope untrusted, and skips enrichment／history／re-cache. Claude and Grok validate the response, preflight the exact durable target before lineage transfer, then typed-save through a second exact re-read／compare；`TargetMissing`／`TargetChanged`／`TargetMalformed`／`TargetUnverified` are terminal with zero required usage continuation, while only a persistence-stage failure after the second compare succeeds may return the fresh credential and transferred scope with no cache binding. `CredentialsPersisted` is the after-attempt checkpoint even when typed save returns an error. Codex and Antigravity retain their existing ordering and failure policy；Codex save failure leaves auth and metadata bytes unchanged, credentials persist before lineage transfer, transfer failure conditionally rolls back only while the exact just-written root still matches, and its post-binding reuses the verified scopes. External writers can still race compare-to-write／rollback, Keychain `security -U` is not CAS, and credentials plus metadata are not a cross-resource atomic transaction |
| Diagnostic wire | Rust emits only `{category,status?,osCode?}` from the fixed category allowlist；`rateLimited`只允許status 429且不帶osCode，`serverError`只允許500...599且不帶osCode，非HTTP categories不得帶status，unknown category不得帶associated numerics；Swift lossy decoding逐欄丟棄malformed optional integers，非object或缺失／非字串category不建立candidate、unknown values正規化，且public logging維持bounded |
| Swift scalar authority | Successful finite outer payloads replace memory and persistent scalar; Dashboard polling reconciles accepted publications immediately and TrayAnimator reads the coordinator's latest generated payload before its own poll completes; selection／exclusion changes re-reconcile before render; Settings task identity uses `publicationGeneration` or a legacy timestamp-plus-resolved-scalar fingerprint; terminal／Absent／unresolved／hidden-all clear the scalar; only outer FFI failure or missing payload retains it. Isolated UserDefaults tests cover restart non-revival, healthy replacement, explicit fallback windows and their snapshot-time 30-minute stale boundary (#8), Auto exclusion, reconciliation identity collisions, a newer terminal result followed by a late older tray success, and a newer Dashboard success replacing an older tray payload plus scalar while invalidating the gauge-render signature |
| Quota curve identity | Series identity 不跨 ABI：Rust 擁有 publication-owned binding table，Swift 只傳 `clientId`／`windowKey`／`publicationGeneration`。Binding 只在 serialization 成功後替換，generation 不符為錯誤而非 fallback，binding 在 history I/O 後重新解析以拒絕已移動的 identity；absent history 的 `ok:true` + `null` 由專屬 optional-envelope decoder 處理，不得與 decode failure 混同 |
| Historical pace | Rust 的 typed `paceStatus` 擁有 lifecycle／duration，optional nested result 同時擁有 expected、ETA、will-last 與 risk；Swift 只能導出 mode policy、stage 與文字。`available + completeCycles == 0` 是合法的 validated current-window projection：Historical mode 必須使用 backend expected／ETA／will-last，partial risk 缺失時維持 nil、不顯示 risk copy，也不得退回 Linear。只有 `learningHistory` 可明示使用 exact-duration Linear estimate，`learningDuration`／`unavailable`／legacy 不得 silent fallback |
| Lifecycle | Closing a popover or settings window cancels its tasks and stops background rendering |
| Restart snapshot | Disk bytes carry only the aggregated graph payload plus schema/build identity, proven by a recursive exact-key assertion with a canary in every excluded field, not a keyword scan; any shape change bumps `snapshotSchemaVersion`. Restore is rejected for schema、bundle、version or exact-build mismatch, a requested year the snapshot does not match, malformed `knownYears`, a `savedAt` beyond the clock-skew or retention bounds, corrupt bytes, the byte cap and cap+1, a symlinked file or directory, a FIFO, a directory in the file's place, and wrong owner or mode. `--selftest`、`--smoke`、`--demo` and `--icon-gallery` must yield no shipping identity, asserted against an injected production bundle triple with a control proving that triple alone does yield one；a spy fails if the production directory is even resolved. Ordering is covered in both task orders, a superseded fetch must not clear a newer request, and mutation targets include the build comparison、the requested-year gate、the regular-file check、`O_NOFOLLOW`、the content digest、the capture sequence and the indicator's ownership check |

### Credential refresh target matrix

Claude File、Claude pure Keychain decision and Grok exact-entry owners exercise the shipping continuation seams rather than returning a prebuilt failure or credential tuple. Each row first uses `TestRefreshScope` to establish A's lineage and only then captures the metadata baseline. Claude routes a real hermetic `refresh_claude_credentials_with` result through `fetch_claude_login_usage_with`; the synthetic Keychain row supplies account／item reads to the pure decision helper and never invokes `/usr/bin/security`. Grok routes an adapter that actually calls `refresh_credentials_with` through `retry_grok_weekly_after_auth_with`; its counter covers only the required weekly retry, never the additive monthly request.

| Phase／class | Required observation |
|---|---|
| Network-wait `TargetChanged`／`TargetMissing`／`TargetMalformed`／`TargetUnverified` | Terminal；request and preflight exactly once；no transfer、typed save、writer or usage／weekly retry；metadata byte-identical to the established-A baseline；external durable state preserved. Claude also owns explicit logout and mcpOAuth-only as `TargetMissing` without fallback |
| Post-preflight four target classes | Terminal；request、preflight、transfer and typed save exactly once；writer and usage／weekly retry zero；checkpoint sequence includes `MetadataHandled` and the after-attempt `CredentialsPersisted`；external durable state preserved；the transferred new marker may remain as inert A-lineage metadata |
| Genuine persistence failure | Second exact compare succeeds before the injected writer failure；new access credential and transferred A scope reach exactly one required continuation；cache binding is `None`；old durable bytes remain；old／new markers share lineage |
| Success with concurrent sibling write | Exactly one required continuation；reusable binding resolves to A；Claude top-level sibling or Grok foreign／root／entry siblings survive the merge；Keychain output remains pinned to captured account A |

Common assertions use only fixed secret-free labels: `REFRESH-TARGET-PREFLIGHT-TERMINAL`、`REFRESH-TARGET-POST-PREFLIGHT-TERMINAL`、`REFRESH-TARGET-USAGE-BLOCKED`、`REFRESH-TARGET-PERSISTENCE-FAILSOFT`、`REFRESH-TARGET-KEYCHAIN-PIN`、`REFRESH-TARGET-SIBLING-PRESERVATION` and `REFRESH-TARGET-CHECKPOINT-SEQUENCE`. Failure output must not include a token、account、path、JSON body or raw metadata. A preflight owner must not probe `resolve_current(new_marker)` to prove absence because that helper can create the mapping；metadata bytes plus transfer count own that assertion.

Mutation qualification is thirteen compile-preserving changes: four independent typed-save target-class-to-`Persistence` mutations, plus preflight ordering、second re-read／compare、Grok exact-entry selection、Keychain account pinning、persistence cacheability、persistence terminality、Claude usage continuation、Grok weekly continuation and `CredentialsPersisted` placement. Each mutation must fail the intended fixed-label owner with non-zero test exit, preserve unrelated owners, restore exact source bytes／hash／diff fingerprint, and leave the same owner green after restore. Compile failure、zero selected tests、an unrelated panic or a failure scan that ignores process exit is not a kill.

> **Windows secure-storage ownership：** 自 #276 起由 Syrtis-Windows 擁有（[Syrtis-Windows#147](https://github.com/Nanako0129/Syrtis-Windows/issues/147)）。本 repo 的 `crates/tb_core_ffi` 從未在任何 CI 或發版路徑被編譯為 Windows 目標，因此刪除了全部 Windows-only 程式碼；低階 primitive、它們的 threat boundary（trusted per-user anchor、same-SID 不在防禦範圍）與測試，都以 Syrtis-Windows 那份為準。

## Cross-port fixture cross-check

Windows port（[Nanako0129/TokenBar-Windows](https://github.com/Nanako0129/TokenBar-Windows)）的 C# `Syrtis.Core` 是 `Sources/TokenBarCore` 的逐檔移植。Reviewed pin、每次 pin 推進對這個消費者的影響（`crates/tb_core_ffi`、`ctb.h`、parser 與快取）以及歷次推進的內容，都寫在 [`architecture.md` 的 Windows downstream consumer](architecture.md#windows-downstream-consumer)，這裡不重述。

Windows 的 crosscheck oracle 釘在一個固定的 macOS commit（不是 `main`；值由 Windows repo 的 CI 設定擁有，Native 不重述），所以 Native 的 pin 推進不影響該 job；但 oracle 自身的落後因此加大，未來推進 Windows pin 時會一次掀出期間累積的漂移。same-pin 只證明 shared source 相同，不取代下述 cross-check 這道跨語言 gate；app-owned FFI、Swift 與 C# surfaces 仍可獨立漂移。單元測試的期望值由移植者撰寫，因此對「一致地誤讀 Swift 語意」的移植錯誤沒有偵測力；對拍（cross-check）以同一份 fixture JSON 餵 Swift 與 C# 兩邊、逐欄位 diff 輸出，才是移植忠實度的判準。

| 項目 | 內容 |
|---|---|
| Swift harness | [`Sources/CrossCheckHarness/main.swift`](../../Sources/CrossCheckHarness/main.swift)，`TZ=Asia/Taipei swift run crosscheck-harness <fixtures> <out> [usage-pace|format|provider-quota-pace-v3] -AppleLanguages "(en)"`；selector 省略時維持 legacy complete run，所有路徑使用 shipping 程式碼 |
| 語系鎖定 | `Format` 的日期／相對時間與 `UsagePace.durationText` 自 i18n 後具語系相依（查表工具在 `Sources/TokenBarCore/Localization.swift`，harness 透過連結 TokenBarCore 取得）。`make build` 會把 `Sources/Syrtis/Resources/Localizations/*.lproj` 複製到 `.build/debug/`；裸 `swift run Syrtis` 會在入口從 SwiftPM resource bundle 暫存同樣的目錄，harness 本身沒有該 target resource，沒有 `.lproj` 時解析為 `en`。對拍因此**必須**帶 `-AppleLanguages "(en)"`。harness 對此 **fail closed**：解析時只認 `-AppleLanguages <value>`（其餘 dash 開頭的 token 一律拒絕，避免吃掉 fixture 路徑後對錯誤目錄執行），並在 TZ guard 之後檢查 `preferredLocalizations` 必須為 `en`，否則 exit——寧可拒跑，也不要靜默產生對拍永遠對不上的在地化輸出。 |
| 契約與 fixture | Windows repo 的 legacy `crosscheck/` 保留既有 116-case reference；provider v3 handoff 由 Mac-owned [`provider-quota-pace-v3.json`](../../Fixtures/CrossCheck/provider-quota-pace-v3.json) 提供，Rust production serializer 鎖定 payload，Swift／Windows 都必須用 production decoder，無自製 wire mapping |
| 比對 | Windows repo 的 `crosscheck/diff.py`：字串逐 byte、數字 epsilon 1e-9、缺鍵視同 null |
| 執行時機 | `Sources/TokenBarCore` 邏輯或 `Format` 語意變更後；Windows app-owned delta 或 reviewed engine pin advance 後 |
| 不在對拍內：重複 label 的 qualification | 刻意由兩平台各自涵蓋，不進 Swift↔C# 對拍（[#302](https://github.com/Nanako0129/syrtis/issues/302)）。見下方說明 |

**重複 label 的 window qualification 不在對拍範圍內，這是決定，不是缺口。** 同一張卡裡重複的 window label（例如兩列 `Codex Spark`）由 `AgentUsageSnapshot.uniqueCardWindows` 在卡片呈現層加上期間、reset 或序號區分（[`Sources/TokenBarCore/AgentUsage.swift`](../../Sources/TokenBarCore/AgentUsage.swift):639，分層邏輯在 `qualifyingRepeatedLabels`，:683）。harness 的 `lifecycleRows`（[`Sources/CrossCheckHarness/main.swift`](../../Sources/CrossCheckHarness/main.swift):292）只序列化原始 `agent.windows`，所以不論 Windows 是否做了同樣的分層，對拍都是綠的。共用契約是引擎 payload，qualification 疊在它上面、屬於各平台的卡片呈現，因此兩平台各自涵蓋同一個使用者看得到的性質：macOS 由 [`Sources/Syrtis/SelfTest.swift`](../../Sources/Syrtis/SelfTest.swift) 的斷言涵蓋——:4450 起「a repeated whitelisted label is qualified in the card view」、:5211 起「repeated window label is qualified by duration, a unique one is left alone」與同組的 identity／legacy-label migration 斷言、:5306「windows whose lengths render identically still get unique names」；Windows 由它自己 repo 的 `AgentUsageQualifierTests` 涵蓋。要把 qualification 改成共用契約，就得同時移植分層、把 qualified label 加進 harness 輸出並推進 Windows pin，缺一邊都會單邊變紅。

> 首輪實績（2026-07-16）：首跑 115 案例抓到 4 條 printf 捨入 seam 的真實漂移——C# 側以 `Math.Round` 預捨入模擬 `%.nf` 會把非 midpoint 的近半值重新量化；printf 對二進位真值做正確捨入。教訓：**模擬 printf 的中介捨入層一律可疑**。修正與後續 comparator 強化（整數精確比對、bool 嚴格比對、Int64 邊界案例——fixture 現為 116 案）都記錄在 Windows repo。

> Historical pace v2 checkpoint（2026-07-16）：116-case legacy baseline 已重跑，非 historical cases 全數一致。27 個 field differences 只分布在 9 個使用舊 top-level historical scalars 的 cases：`historical-expected-clamped`、`historical-runout-exact-half`、`historical-runout-high-keeps-eta`、`historical-runout-low-forces-lasts`、`historical-with-expected`、`runout-risk-certain`、`runout-risk-clamped-above-one`、`runout-risk-half-percent-rounds-up`、`runout-risk-thirty`。這些是 nested contract 取代 scalar contract 的 intended mismatch；Windows 新增 nested fixture／DTO 並完成 semantic port 前，不得宣稱 historical parity。
>
> Provider-wide v3 checkpoint（2026-07-27）：no-selector Mac harness 以 production decoder 完整產生 42 pace＋74 format cases，不再因單一 malformed legacy row 中止。原本 28／42 pace cases 的 intended mismatch 來自 strict v3 decoder、typed lifecycle 與 no-silent-fallback contract。M19-B1 的 Windows port在 [Windows PR #7](https://github.com/Nanako0129/TokenBar-Windows/pull/7) 完成 production DTO／state machine／selection／presentation；完整 Swift／C# cross-check 119 cases零material difference。Mac-owned fixture 的7張 lifecycle windows與12個 projection／selection／legacy／malformed cases仍由Rust production serializer鎖定，real ARM64 CrossCheck也產生exact 12 cases。Provider-v3 Windows status因此為 **port/parity complete**。

> Shared-engine extraction checkpoint（2026-07-28）：Native [PR #114](https://github.com/Nanako0129/syrtis/pull/114) 與 Windows [PR #12](https://github.com/Nanako0129/TokenBar-Windows/pull/12) 都 pin `b31e39425859393504a2d56cb5af7c93e6461c7d`。Windows current-head gates produced 119 cross-check cases with zero material difference；hosted x64／ARM64 jobs and a separate native ARM64 packaged-FFI run passed. Future same-pin assertions prove shared source equality but do not replace this cross-language gate.

## Documentation checks

The knowledge tree is validated by `scripts/check_knowledge.py`, the `make check-docs` target, and the CI knowledge-validation step. The final documentation gate is:

```bash
python3 scripts/check_knowledge.py --self-test
python3 -m py_compile scripts/check_knowledge.py
python3 scripts/check_knowledge.py
make check-docs
git diff --check origin/main...HEAD
```

These checks cover frontmatter, relative links, canonical reachability, migration-ledger counts and enums, privacy scans, and repository whitespace. Do not claim runtime PASS for a docs-only change.

Three checks close out #291. The last two read this repository's own history and, like the engine checks, answer "cannot ask" rather than fail when that history is missing; `ci.yml` fetches it before the knowledge step and asserts the checkout is no longer shallow, and the Pages job does not fetch it, so there they are skipped:

| Check | Rule |
|---|---|
| Pin owner contract | `vendor/README.md` carries frontmatter and is held to the same frontmatter, link and privacy checks as the knowledge documents |
| Current-advance claims | When the current engine pin advance changed `crates/tb_core_ffi` or `Sources/CTB/include/ctb.h`, a clause that says it describes the current advance (這次、本次、this advance、this pin advance、current pin) and claims that path is unchanged fails; quoted text is ignored. The advance is this branch when the gitlink moved since the merge base with `origin/main` (merge base to working tree), otherwise the newest first-parent commit that moved the gitlink. Claims about earlier advances are not checked, and a current-advance claim worded without a marker escapes |
| `last_verified` moves | A document whose body or `sources` changed since the merge base with `origin/main` must change `last_verified`, unless the old date is already on or after the merge base's commit date, less one day for timezones |

SHA existence is checked only where the repository is known: `check_engine_pin` resolves every 40-hex SHA inside a `tokscale-core/{blob,commit,tree}` URL against the submodule. It needs a complete engine checkout and skips the question ("cannot ask", not a failure) when the submodule is absent or shallow. `ci.yml` unshallows the submodule before its knowledge step, so the check runs there; the Pages job checks out without submodules, so there it is skipped and `ci.yml` is the enforcing gate. A bare 40-hex string in prose is not resolved, because the tree cites commits and tree objects from several repositories (Syrtis, `tokscale-core`, Syrtis-Windows, upstream `tokscale`, CodexBar) and the text alone does not say which. [#295](https://github.com/Nanako0129/syrtis/issues/295) found nine such strings that looked unresolvable and all turned out to be real, just unlabelled. So write a SHA that does not belong to this repository as a link to its repository, or name the repository and, for a tree object, say so next to it.

## Failure interpretation

A failed smoke run caused by missing local credentials, an empty private session tree, or a provider network response is not evidence that the parser or docs are wrong. Record the environmental limitation separately, then rely on hermetic tests and the relevant source-level gate. Conversely, a green live smoke run without a fixture does not close a data-dependent correctness issue.
