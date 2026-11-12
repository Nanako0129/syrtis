//! Per-model usage breakdown for the popover, backed by tokscale-core's
//! `get_model_report`. Mirrors the design of tokscale's TUI "Models" view
//! (`crates/tokscale-cli/src/tui/ui/models.rs`): one row per model with the
//! token breakdown, message count, cost, and throughput (ms/1K), sorted by
//! cost on the frontend.
//!
//! Like `usage_graph`, this drives the async core on a short-lived
//! current-thread runtime (callers run it inside `spawn_blocking`) and maps the
//! result onto a camelCase JSON shape the frontend consumes directly.

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelEntry {
    client: String,
    model: String,
    provider: String,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    reasoning: i64,
    total: i64,
    message_count: i32,
    cost: f64,
    /// Milliseconds per 1K tokens, when tokscale could time the model. `None`
    /// when no message in the rollup carried a usable duration.
    ms_per_1k_tokens: Option<f64>,
    /// What the local pricing table would charge for this row's tokens, or
    /// `None` when it cannot price them at all.
    ///
    /// Reported rather than judged here on purpose. The frontend folds these
    /// provider-split rows together by client+model, and a ratio computed
    /// before that fold cannot describe the merged cost — one component at
    /// 100x combined with a larger healthy one is a merged 1.1x, not 100x. So
    /// this ships the estimate and the comparison happens after the fold, in
    /// `ModelReportEntry.implausibleCostRatio`.
    pub(crate) cost_estimate: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelReportData {
    pub(crate) entries: Vec<ModelEntry>,
    total_input: i64,
    total_output: i64,
    total_cache_read: i64,
    total_cache_write: i64,
    total_messages: i32,
    total_cost: f64,
    /// Unix-seconds time the LiteLLM pricing dataset was last fetched from
    /// upstream (the on-disk cache write time). `None` before the first fetch.
    /// Surfaced as the "prices updated …" hint in the Models view.
    pricing_updated_at: Option<u64>,
}

/// Report options for the per-model report.
///
/// Providers must stay separate. Under tokscale's default `ClientModel`
/// grouping a model served by two providers folds into ONE row whose
/// `provider` is a comma-joined string (`"nvidia, deepseek"`) and whose tokens
/// are summed — see `aggregate_model_usage_entries` in tokscale-core. That
/// merge is lossy: no downstream consumer can split the row back apart, so any
/// per-provider attribution has to be decided here, at the producer.
fn model_report_options(
    context: &crate::LocalSourceContext,
    year: Option<String>,
) -> tokscale_core::ReportOptions {
    let mut options = context.report_options(year, None);
    options.group_by = tokscale_core::GroupBy::ClientProviderModel;
    options
}

/// Build the per-model report for `year` (empty string = all time).
pub(crate) fn run(context: &crate::LocalSourceContext, year: &str) -> Result<Value, String> {
    let report = compute(context, year)?;

    // Read-only, no network: `get_model_report` has already refreshed and
    // written this cache on its own path, and a cold cache (first ever run,
    // offline) simply yields None, which disables the check rather than
    // reporting on a table that isn't there.
    let pricing = tokscale_core::pricing::PricingService::load_cached_any_age();

    let data = map_report(report, pricing.as_ref());
    serde_json::to_value(data).map_err(|e| format!("serialize model report: {}", e))
}

/// The engine's report, before mapping (split out of [`run`] so
/// `remote_pages` can add remote records to it).
pub(crate) fn compute(
    context: &crate::LocalSourceContext,
    year: &str,
) -> Result<tokscale_core::ModelReport, String> {
    let year = normalize_year(year)?;
    let options = model_report_options(context, year);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("build runtime: {}", e))?;
    runtime.block_on(tokscale_core::get_model_report(options))
}

/// What the local pricing table would charge for this row's tokens, or `None`
/// when it cannot price them.
///
/// This exists because clients that record their own per-message cost
/// (OpenCode, MiMo Code) have it taken verbatim: tokscale-core's
/// `apply_pricing_if_available` returns early on `has_authoritative_cost()`,
/// so no pricing table ever sees those rows. That default is right — the
/// client knows its own billing contract — but it also means a unit error
/// upstream (a per-1K rate applied per-token, a non-USD figure, a mispriced
/// custom provider) reaches the UI with nothing in between. Shipping the
/// estimate alongside the cost gives the frontend something to compare
/// against; what counts as implausible is decided there, after the
/// provider-split rows have been folded together.
///
/// A zero or non-finite estimate is reported as `None`: the table cannot
/// price these tokens at all, which is not evidence about the reported cost
/// and must not become a division by zero downstream.
fn local_cost_estimate(
    pricing: Option<&tokscale_core::pricing::PricingService>,
    entry: &tokscale_core::ModelUsage,
) -> Option<f64> {
    let pricing = pricing?;
    let usage = tokscale_core::TokenBreakdown {
        input: entry.input,
        output: entry.output,
        cache_read: entry.cache_read,
        cache_write: entry.cache_write,
        reasoning: entry.reasoning,
        // `ModelUsage` carries no 1h/5m split, so this has to assume one.
        // Assume ALL of it is 1h, which is the more expensive of the two:
        // this estimate is the denominator of a ratio compared against a
        // threshold, so only an UPPER bound on it is safe. Under-estimating
        // inflates the ratio and invents warnings; over-estimating only
        // deflates it and stays quiet, which is the right way to fail for a
        // guard against costs wrong by three orders of magnitude.
        //
        // The asymmetry is not theoretical. This line used to pass 0 (price
        // everything at the 5m rate) and that produced a real false-positive
        // path, found in review of #309:
        //
        //   `compute_cost` charges a 1h write at 2x base input, deriving it
        //   as `2.0 * tiered_cost(..., input_cost_per_token, ...)` because no
        //   pricing table publishes a 1h key. The 5m rate is the table's
        //   `cache_creation_input_token_cost` — which some entries omit. The
        //   provider hint steers `claude-haiku-4-5` to
        //   `perplexity/anthropic/claude-haiku-4-5`, a resale entry with no
        //   such key (the canonical one has 1.25e-06; upstream tokscale #57).
        //   So a 5m-priced estimate dropped that row's cache write entirely
        //   while tokscale still billed it. A row that builds a large cache,
        //   has a small input/output remainder and takes no later cache reads
        //   — ordinary for a session that ends after priming — then reaches
        //   `cost / estimate` above 50 on a LOCALLY priced row, showing "cost
        //   reported by the client" over a number no client reported.
        //
        // Pricing the whole write at 1h closes that path at the source rather
        // than bounding it: the estimate now includes those tokens even when
        // the matched entry has no 5m rate, because the 1h rate is derived
        // from input rather than looked up. Where the real split was 5m the
        // estimate runs high by at most 2.0/1.25 = 1.6x, which only makes the
        // guard quieter.
        //
        // The rows the guard exists for are unaffected either way: OpenCode's
        // self-reported costs come through deepseek/openrouter with no cache
        // write at all.
        //
        // Two earlier comments here argued this was safe with `0`, first
        // because the gap was "a few percent" (unmeasured; it reaches 60% of
        // the cache-write component) and then because the affected and judged
        // rows were disjoint (they are not — every row is estimated). Both
        // were reasoning about how big the error was. The error's DIRECTION
        // was the part that mattered.
        cache_write_1h: entry.cache_write,
    };
    let estimate = pricing.calculate_cost_with_provider(&entry.model, Some(&entry.provider), &usage);
    (estimate.is_finite() && estimate > 0.0).then_some(estimate)
}

fn normalize_year(year: &str) -> Result<Option<String>, String> {
    let trimmed = year.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.len() == 4 && trimmed.chars().all(|c| c.is_ascii_digit()) {
        Ok(Some(trimmed.to_string()))
    } else {
        Err(format!("invalid year filter: {}", year))
    }
}

pub(crate) fn map_report(
    report: tokscale_core::ModelReport,
    pricing: Option<&tokscale_core::pricing::PricingService>,
) -> ModelReportData {
    ModelReportData {
        entries: report
            .entries
            .into_iter()
            .map(|e| {
                let cost_estimate = local_cost_estimate(pricing, &e);
                // saturating_add so #766's i64::MAX-clamped buckets (corrupt
                // Antigravity DB) can't overflow this FFI-exposed total in
                // debug/release (see agents_report.rs's map_report for the
                // same pattern).
                let total = e
                    .input
                    .saturating_add(e.output)
                    .saturating_add(e.cache_read)
                    .saturating_add(e.cache_write)
                    .saturating_add(e.reasoning);
                ModelEntry {
                    client: e.client,
                    model: e.model,
                    provider: e.provider,
                    input: e.input,
                    output: e.output,
                    cache_read: e.cache_read,
                    cache_write: e.cache_write,
                    reasoning: e.reasoning,
                    total,
                    message_count: e.message_count,
                    cost: e.cost,
                    ms_per_1k_tokens: e.performance.ms_per_1k_tokens,
                    cost_estimate,
                }
            })
            .collect(),
        total_input: report.total_input,
        total_output: report.total_output,
        total_cache_read: report.total_cache_read,
        total_cache_write: report.total_cache_write,
        total_messages: report.total_messages,
        total_cost: report.total_cost,
        pricing_updated_at: tokscale_core::pricing::pricing_cached_at(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// The display-grouping case table is shared with SelfTest's
    /// `ModelGrouping.groupID` check. Asserting it against the engine's own
    /// grouping function ties the Swift helper to the Rust fold on the table's
    /// inputs: a pin advance that changes the result for any of them fails
    /// here. A change on inputs the table does not list is not caught.
    #[test]
    fn model_grouping_cases_match_the_engine() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../Tests/fixtures/model-grouping-cases.json");
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("case table missing at {}: {e}", path.display()));
        let table: serde_json::Value = serde_json::from_str(&text).expect("case table parses");
        let cases = table["cases"].as_array().expect("`cases` is an array");
        // Control: an empty or truncated table must not pass vacuously.
        assert!(cases.len() >= 28, "case table has {} rows, expected >= 28", cases.len());
        for case in cases {
            let input = case[0].as_str().expect("input is a string");
            let expected = case[1].as_str().expect("expected is a string");
            assert_eq!(
                tokscale_core::normalize_model_for_grouping(input),
                expected,
                "engine grouping for {input}"
            );
        }
    }

    struct FixtureHome(PathBuf);

    impl FixtureHome {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "tokenbar-model-report-{}-{}-{label}",
                std::process::id(),
                nanos
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for FixtureHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Debug, serde::Deserialize)]
    struct TestModelPerformance {
        #[serde(rename = "msPer1KTokens")]
        ms_per_1k_tokens: Option<f64>,
    }

    #[derive(Debug, serde::Deserialize)]
    #[serde(rename_all = "snake_case")]
    struct TestModelUsage {
        client: String,
        merged_clients: Option<String>,
        workspace_key: Option<String>,
        workspace_label: Option<String>,
        session_id: Option<String>,
        model: String,
        provider: String,
        input: i64,
        output: i64,
        cache_read: i64,
        cache_write: i64,
        reasoning: i64,
        message_count: i32,
        cost: f64,
        performance: TestModelPerformance,
    }

    struct TestModelReport {
        entries: Vec<TestModelUsage>,
    }

    fn model_report_direct(
        home: &Path,
        client: &str,
        group_by: Option<tokscale_core::GroupBy>,
    ) -> tokscale_core::ModelReport {
        let context = crate::LocalSourceContext::for_home(home.to_path_buf());
        let mut options = model_report_options(&context, None);
        // report_options defaults to environment roots for production. Tests
        // must opt out so only the fixture home and requested lane are read.
        options.use_env_roots = false;
        options.clients = Some(vec![client.to_string()]);
        if let Some(group_by) = group_by {
            options.group_by = group_by;
        }

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(tokscale_core::get_model_report(options))
            .unwrap()
    }

    #[test]
    fn model_report_child() {
        let Some(home) = std::env::var_os("TOKENBAR_MODEL_REPORT_HOME") else {
            return;
        };
        let client = std::env::var("TOKENBAR_MODEL_REPORT_CLIENT").unwrap();
        let group_by = match std::env::var("TOKENBAR_MODEL_REPORT_GROUP_BY")
            .unwrap_or_default()
            .as_str()
        {
            "client,model" => Some(tokscale_core::GroupBy::ClientModel),
            "client,provider,model" => Some(tokscale_core::GroupBy::ClientProviderModel),
            _ => None,
        };
        let report = model_report_direct(Path::new(&home), &client, group_by);
        println!(
            "TOKENBAR_MODEL_REPORT_RESULT={}",
            serde_json::to_string(&report.entries).unwrap()
        );
    }

    fn model_report(
        context: &crate::LocalSourceContext,
        client: &str,
        group_by: Option<tokscale_core::GroupBy>,
    ) -> TestModelReport {
        let fixture_home = context.home_dir.as_ref().unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "model_report::tests::model_report_child",
                "--nocapture",
            ])
            .env("TOKENBAR_MODEL_REPORT_HOME", fixture_home)
            .env("TOKENBAR_MODEL_REPORT_CLIENT", client)
            .env(
                "TOKENBAR_MODEL_REPORT_GROUP_BY",
                group_by.map(|group| group.to_string()).unwrap_or_default(),
            )
            // The pricing configuration is process-local in tokscale; keep it
            // hermetic in the child instead of mutating the test runner.
            .env("TOKSCALE_CONFIG_DIR", fixture_home.join(".tokscale-config"))
            .env("TOKSCALE_PRICING_CACHE_ONLY", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "model report child failed: {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let marker = "TOKENBAR_MODEL_REPORT_RESULT=";
        let stdout = String::from_utf8_lossy(&output.stdout);
        let json = stdout
            .lines()
            .find_map(|line| line.strip_prefix(marker))
            .unwrap_or_else(|| {
                panic!(
                    "model report child emitted no result: {}",
                    String::from_utf8_lossy(&output.stdout)
                )
            });
        TestModelReport {
            entries: serde_json::from_str(json).unwrap(),
        }
    }

    fn write_jcode_fixture(home: &Path, snapshot: &str, journal: Option<&str>) {
        let sessions = home.join(".jcode/sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(sessions.join("session_attr.json"), snapshot).unwrap();
        if let Some(journal) = journal {
            fs::write(sessions.join("session_attr.journal.jsonl"), journal).unwrap();
        }
    }

    fn write_mux_fixture(home: &Path, contents: &str) {
        let session = home.join(".mux/sessions/workspace-empty");
        fs::create_dir_all(&session).unwrap();
        fs::write(session.join("session-usage.json"), contents).unwrap();
    }

    #[test]
    fn production_options_group_by_client_provider_model() {
        // This anchors the producer choice: reverting the assignment to the
        // default ClientModel would make provider attribution lossy again.
        let options = model_report_options(
            &crate::LocalSourceContext::for_home(PathBuf::from("/unused")),
            None,
        );
        assert_eq!(
            options.group_by,
            tokscale_core::GroupBy::ClientProviderModel
        );
    }

    #[test]
    fn model_report_keeps_jcode_providers_split() {
        let home = FixtureHome::new("split");
        // The snapshot and journal deliberately reuse one model id while the
        // journal metadata overrides only its provider, exercising the parser
        // seam that would otherwise be impossible to split downstream.
        write_jcode_fixture(
            home.path(),
            r#"{"id":"session_attr","provider_key":"openai","model":"shared-provider-model","working_dir":"/fixture","messages":[{"id":"snapshot-assistant","role":"assistant","timestamp":"2026-06-16T12:00:01Z","token_usage":{"input_tokens":101,"output_tokens":17,"cache_read_input_tokens":23,"cache_creation_input_tokens":29,"reasoning_output_tokens":31}}]}"#,
            Some(
                r#"{"meta":{"provider_key":"nvidia","model":"shared-provider-model","working_dir":"/fixture"},"append_messages":[{"id":"journal-assistant","role":"assistant","timestamp":"2026-06-16T12:00:02Z","token_usage":{"input_tokens":211,"output_tokens":37,"cache_read_input_tokens":43,"cache_creation_input_tokens":47,"reasoning_output_tokens":53}}]}
"#,
            ),
        );
        let context = crate::LocalSourceContext::for_home(home.path().to_path_buf());

        let split = model_report(&context, "jcode", None).entries;
        assert_eq!(split.len(), 2);
        let openai = split
            .iter()
            .find(|entry| entry.provider == "openai")
            .unwrap();
        let nvidia = split
            .iter()
            .find(|entry| entry.provider == "nvidia")
            .unwrap();
        assert_eq!(openai.client, "jcode");
        assert_eq!(openai.model, "shared-provider-model");
        assert_eq!(openai.input, 101);
        assert_eq!(openai.output, 17);
        assert_eq!(openai.cache_read, 23);
        assert_eq!(openai.cache_write, 29);
        assert_eq!(openai.reasoning, 31);
        assert_eq!(
            openai.input
                + openai.output
                + openai.cache_read
                + openai.cache_write
                + openai.reasoning,
            201
        );
        assert_eq!(nvidia.client, "jcode");
        assert_eq!(nvidia.model, "shared-provider-model");
        assert_eq!(nvidia.input, 211);
        assert_eq!(nvidia.output, 37);
        assert_eq!(nvidia.cache_read, 43);
        assert_eq!(nvidia.cache_write, 47);
        assert_eq!(nvidia.reasoning, 53);
        assert_eq!(
            nvidia.input
                + nvidia.output
                + nvidia.cache_read
                + nvidia.cache_write
                + nvidia.reasoning,
            391
        );

        let merged_report =
            model_report(&context, "jcode", Some(tokscale_core::GroupBy::ClientModel));
        assert_eq!(merged_report.entries.len(), 1);
        let merged = &merged_report.entries[0];
        assert_eq!(merged.client, "jcode");
        assert_eq!(merged.model, "shared-provider-model");
        assert_eq!(merged.message_count, 2);
        let mut merged_providers = merged.provider.split(", ").collect::<Vec<_>>();
        merged_providers.sort_unstable();
        assert_eq!(merged_providers, vec!["nvidia", "openai"]);
        assert_eq!(merged.input, 312);
        assert_eq!(merged.output, 54);
        assert_eq!(merged.cache_read, 66);
        assert_eq!(merged.cache_write, 76);
        assert_eq!(merged.reasoning, 84);
        assert_eq!(openai.input + nvidia.input, merged.input);
        assert_eq!(openai.output + nvidia.output, merged.output);
        assert_eq!(openai.cache_read + nvidia.cache_read, merged.cache_read);
        assert_eq!(openai.cache_write + nvidia.cache_write, merged.cache_write);
        assert_eq!(openai.reasoning + nvidia.reasoning, merged.reasoning);
    }

    #[test]
    fn model_report_keeps_empty_mux_provider_visible() {
        let home = FixtureHome::new("empty-provider");
        write_mux_fixture(
            home.path(),
            r#"{"version":1,"byModel":{"providerless-model":{"input":{"tokens":307},"cached":{"tokens":11},"cacheCreate":{"tokens":13},"output":{"tokens":19},"reasoning":{"tokens":17}},"nvidia:providerless-model":{"input":{"tokens":401},"cached":{"tokens":29},"cacheCreate":{"tokens":31},"output":{"tokens":23},"reasoning":{"tokens":37}}},"lastRequest":{"timestamp":1781601600000}}"#,
        );
        let context = crate::LocalSourceContext::for_home(home.path().to_path_buf());

        let report = model_report(&context, "mux", None);
        assert_eq!(report.entries.len(), 2);
        let entry = report
            .entries
            .iter()
            .find(|entry| entry.provider.is_empty())
            .unwrap();
        // A provider-less mux key must remain a visible empty-provider row;
        // dropping it would hide usage, while merging it would lose its lane.
        assert_eq!(entry.client, "mux");
        assert_eq!(entry.model, "providerless-model");
        assert_eq!(entry.provider, "");
        assert_eq!(entry.input, 307);
        assert_eq!(entry.output, 19);
        assert_eq!(entry.cache_read, 11);
        assert_eq!(entry.cache_write, 13);
        assert_eq!(entry.reasoning, 17);
        let nvidia = report
            .entries
            .iter()
            .find(|entry| entry.provider == "nvidia")
            .unwrap();
        assert_eq!(nvidia.model, "providerless-model");
        assert_eq!(nvidia.input, 401);
        assert_eq!(nvidia.output, 23);
        assert_eq!(nvidia.cache_read, 29);
        assert_eq!(nvidia.cache_write, 31);
        assert_eq!(nvidia.reasoning, 37);
    }

    #[test]
    fn model_report_single_provider_matches_client_model() {
        let home = FixtureHome::new("single-provider");
        write_jcode_fixture(
            home.path(),
            r#"{"id":"session_attr","provider_key":"openai","model":"single-provider-model","working_dir":"/fixture","messages":[{"id":"assistant","role":"assistant","timestamp":"2026-06-16T12:00:01Z","token_usage":{"input_tokens":211,"output_tokens":37,"cache_read_input_tokens":43,"cache_creation_input_tokens":47,"reasoning_output_tokens":53}}]}"#,
            None,
        );
        let context = crate::LocalSourceContext::for_home(home.path().to_path_buf());

        let provider_grouped = model_report(&context, "jcode", None);
        let client_grouped =
            model_report(&context, "jcode", Some(tokscale_core::GroupBy::ClientModel));
        assert_eq!(provider_grouped.entries.len(), 1);
        assert_eq!(client_grouped.entries.len(), 1);
        let provider_entry = &provider_grouped.entries[0];
        let client_entry = &client_grouped.entries[0];
        // One provider should not change the Models card's established row or
        // numbers when the grouping gains the provider dimension.
        assert_eq!(provider_entry.client, client_entry.client);
        assert_eq!(provider_entry.merged_clients, client_entry.merged_clients);
        assert_eq!(provider_entry.model, client_entry.model);
        assert_eq!(provider_entry.provider, client_entry.provider);
        assert_eq!(provider_entry.workspace_key, client_entry.workspace_key);
        assert_eq!(provider_entry.workspace_label, client_entry.workspace_label);
        assert_eq!(provider_entry.session_id, client_entry.session_id);
        assert_eq!(provider_entry.input, client_entry.input);
        assert_eq!(provider_entry.output, client_entry.output);
        assert_eq!(provider_entry.cache_read, client_entry.cache_read);
        assert_eq!(provider_entry.cache_write, client_entry.cache_write);
        assert_eq!(provider_entry.reasoning, client_entry.reasoning);
        assert_eq!(provider_entry.message_count, client_entry.message_count);
        assert_eq!(provider_entry.cost, client_entry.cost);
        assert_eq!(
            provider_entry.performance.ms_per_1k_tokens,
            client_entry.performance.ms_per_1k_tokens
        );
    }

    /// #766 clamps corrupt Antigravity varints to `i64::MAX` per bucket. Two
    /// such buckets in one model entry must saturate the mapped `total`, not
    /// overflow it (a plain `+` panics in debug / wraps in release).
    fn entry(input: i64, output: i64, cache_read: i64, cache_write: i64, reasoning: i64) -> tokscale_core::ModelUsage {
        tokscale_core::ModelUsage {
            client: "antigravity_cli".to_string(),
            merged_clients: None,
            workspace_key: None,
            workspace_label: None,
            session_id: None,
            model: "gemini-3-pro".to_string(),
            provider: "antigravity".to_string(),
            input,
            output,
            cache_read,
            cache_write,
            reasoning,
            message_count: 1,
            cost: 0.0,
            performance: tokscale_core::ModelPerformance::default(),
        }
    }

    fn wrap(entries: Vec<tokscale_core::ModelUsage>) -> tokscale_core::ModelReport {
        tokscale_core::ModelReport {
            entries,
            total_input: 0,
            total_output: 0,
            total_cache_read: 0,
            total_cache_write: 0,
            total_messages: 1,
            total_cost: 0.0,
            processing_time_ms: 0,
        }
    }

    #[test]
    fn total_saturates_on_overlarge_buckets() {
        let report = wrap(vec![entry(i64::MAX, i64::MAX, 0, 0, 0)]);
        let mapped = map_report(report, None);
        assert_eq!(mapped.entries[0].total, i64::MAX);
    }

    /// The two-MAX-field case above only pins `input`/`output` into the fold.
    /// Pin the other three fields too: nonzero `input`/`output`/`reasoning`
    /// plus a clamped `cache_write`, so `cache_read`/`cache_write` inclusion
    /// is independently exercised, not just present-but-untested.
    #[test]
    fn total_saturates_when_cache_write_is_overlarge() {
        let report = wrap(vec![entry(10, 20, i64::MAX, i64::MAX, 5)]);
        let mapped = map_report(report, None);
        assert_eq!(mapped.entries[0].total, i64::MAX);
    }

    /// The saturating cases can't catch a dropped operand (another MAX field
    /// keeps the total at MAX), so pin every field's inclusion with distinct
    /// powers of two: omitting any one operand changes the exact sum.
    #[test]
    fn total_includes_every_token_field() {
        let report = wrap(vec![entry(1, 2, 4, 8, 16)]);
        let mapped = map_report(report, None);
        assert_eq!(mapped.entries[0].total, 31);
    }

    // MARK: - Implausible-cost guard

    /// A hermetic stand-in for the LiteLLM table: one priced model at
    /// $1.00 per 1M input tokens and nothing else, so every estimate below is
    /// a number this test states rather than one the shipping dataset supplies
    /// (which would drift with upstream and make the assertions meaningless).
    fn priced_service(model: &str) -> tokscale_core::pricing::PricingService {
        let mut litellm = std::collections::HashMap::new();
        litellm.insert(
            model.to_string(),
            tokscale_core::pricing::litellm::ModelPricing {
                input_cost_per_token: Some(1e-6),
                ..Default::default()
            },
        );
        tokscale_core::pricing::PricingService::new(litellm, std::collections::HashMap::new())
    }

    /// 1M input tokens, which the table above prices at exactly $1.00 — so
    /// every expected estimate below is a round number this test states.
    fn priced_entry(model: &str, cost: f64) -> tokscale_core::ModelUsage {
        let mut e = entry(1_000_000, 0, 0, 0, 0);
        e.model = model.to_string();
        e.provider = "deepseek".to_string();
        e.cost = cost;
        e
    }

    #[test]
    fn priced_model_reports_the_table_estimate() {
        let service = priced_service("m");
        // Independent of `cost`: the estimate describes the tokens, and the
        // comparison against cost happens downstream, after the fold.
        for cost in [0.0, 1.0, 1000.0, f64::NAN] {
            assert_eq!(
                local_cost_estimate(Some(&service), &priced_entry("m", cost)),
                Some(1.0),
                "cost {cost} must not change the estimate"
            );
        }
    }

    #[test]
    fn estimate_scales_with_the_tokens() {
        // Pins that the tokens actually reach the pricing call: a version that
        // priced a fixed or empty breakdown would return Some(1.0) here too.
        let service = priced_service("m");
        let mut e = priced_entry("m", 1.0);
        e.input = 3_000_000;
        assert_eq!(local_cost_estimate(Some(&service), &e), Some(3.0));
    }

    /// The false-positive path from #309's review: an entry with an input
    /// rate but no 5m cache-write rate (what the provider hint selects for
    /// haiku). Pricing the write at 5m drops it from the estimate while
    /// tokscale still bills it at 2x input, so a cache-heavy row with a small
    /// remainder clears the 50x threshold on a locally priced row.
    #[test]
    fn cache_write_is_estimated_even_without_a_5m_rate() {
        let mut litellm = std::collections::HashMap::new();
        litellm.insert(
            "no5m".to_string(),
            tokscale_core::pricing::litellm::ModelPricing {
                input_cost_per_token: Some(1e-6),
                // cache_creation_input_token_cost deliberately absent.
                ..Default::default()
            },
        );
        let service = tokscale_core::pricing::PricingService::new(
            litellm,
            std::collections::HashMap::new(),
        );

        let mut e = entry(100_000, 0, 0, 10_000_000, 0);
        e.model = "no5m".to_string();
        e.provider = "anthropic".to_string();
        e.cost = 0.0;

        // 100K input at 1e-6 is 0.10; 10M cache write at 2x input is 20.00.
        let estimate = local_cost_estimate(Some(&service), &e).expect("priced");
        assert!(
            estimate > 20.0,
            "the cache write must be in the estimate, got {estimate}"
        );

        // What tokscale would charge if that write were entirely 1h — the
        // estimate must not sit below it, or the ratio inflates.
        let billed = service.calculate_cost_with_provider(
            "no5m",
            Some("anthropic"),
            &tokscale_core::TokenBreakdown {
                input: e.input,
                output: 0,
                cache_read: 0,
                cache_write: e.cache_write,
                reasoning: 0,
                cache_write_1h: e.cache_write,
            },
        );
        assert!(
            estimate >= billed,
            "estimate {estimate} must upper-bound the billed {billed}"
        );
        // Pricing it at 5m instead would have left 0.10 against a billed
        // 20.10 — a ratio of 201x on a locally priced row.
        assert!(
            billed / estimate <= 1.0,
            "ratio must not exceed 1.0 when cost equals the fully-1h price"
        );
    }

    #[test]
    fn unpriceable_rows_report_no_estimate() {
        let service = priced_service("m");
        // A model the table does not carry. Reported as None rather than 0.0
        // so the frontend cannot divide by it.
        assert_eq!(
            local_cost_estimate(Some(&service), &priced_entry("other", 99_999.0)),
            None
        );
        // No cached table at all (first run, offline).
        assert_eq!(local_cost_estimate(None, &priced_entry("m", 1000.0)), None);
        // Zero tokens price to 0.0, which is not a usable denominator either.
        let mut empty = priced_entry("m", 1000.0);
        empty.input = 0;
        assert_eq!(local_cost_estimate(Some(&service), &empty), None);
    }

    #[test]
    fn estimate_reaches_the_serialized_entry() {
        // The cases above test the function directly; this one proves
        // map_report carries it onto the wire shape, under the key Swift reads.
        let service = priced_service("m");
        let report = wrap(vec![priced_entry("m", 1000.0), priced_entry("other", 1.0)]);
        let mapped = map_report(report, Some(&service));
        assert_eq!(mapped.entries[0].cost_estimate, Some(1.0));
        assert_eq!(mapped.entries[1].cost_estimate, None);

        let json = serde_json::to_value(&mapped).unwrap();
        assert_eq!(json["entries"][0]["costEstimate"], serde_json::json!(1.0));
        assert!(json["entries"][1]["costEstimate"].is_null());
    }

    #[test]
    fn normalize_year_accepts_only_four_digits() {
        assert_eq!(normalize_year(""), Ok(None));
        assert_eq!(normalize_year(" 2026 "), Ok(Some("2026".to_string())));
        assert!(normalize_year("26").is_err());
    }
}
