//! Per-agent (sub-agent) usage breakdown for the popover, backed by
//! tokscale-core's `get_agents_report`. Mirrors tokscale's TUI "Agents" view,
//! where named sub-agents are ranked by cost; messages with no agent
//! attribution fold into a single "Main" bucket so every message is accounted
//! for.
//!
//! `get_agents_report` folds the SAME deduped, per-client-gated, priced stream
//! as the model/graph/hourly reports (`scan_messages_streaming`), so the agents
//! report agrees with them on copilot/codebuff/kimi/cursor/warp totals
//! (issue #6). Like the other reports, it drives the async core on a
//! short-lived current-thread runtime (callers run it inside `spawn_blocking`)
//! and maps the result onto a camelCase JSON shape the frontend consumes.

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentEntry {
    agent: String,
    clients: Vec<String>,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    reasoning: i64,
    total: i64,
    cost: f64,
    messages: i32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentsReportData {
    entries: Vec<AgentEntry>,
    total_cost: f64,
    total_messages: i32,
}

/// Build the per-agent report for `year` (empty string = all time), restricted
/// to `clients` (None = every client). The client filter is applied in the
/// streaming scan, so an agent bucket shared across clients carries only the
/// selected clients' tokens/cost — a membership filter downstream cannot do
/// this because each `AgentAccumulator` folds all clients into one mixed total.
pub(crate) fn run(
    context: &crate::LocalSourceContext,
    year: &str,
    clients: Option<Vec<String>>,
) -> Result<Value, String> {
    let data = map_report(compute(context, year, clients)?);
    serde_json::to_value(data).map_err(|e| format!("serialize agents report: {}", e))
}

/// The engine's report, before mapping (split out of [`run`] so
/// `remote_pages` can add remote records to it).
pub(crate) fn compute(
    context: &crate::LocalSourceContext,
    year: &str,
    clients: Option<Vec<String>>,
) -> Result<tokscale_core::AgentReport, String> {
    let year = normalize_year(year)?;
    let options = context.report_options(year, clients);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("build runtime: {}", e))?;
    runtime.block_on(tokscale_core::get_agents_report(options))
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

pub(crate) fn map_report(report: tokscale_core::AgentReport) -> AgentsReportData {
    AgentsReportData {
        entries: report
            .entries
            .into_iter()
            .map(|e| {
                // Single source of the `total` formula; it MUST stay identical
                // to the token-total used for the cost-then-total sort in
                // tokscale-core's get_agents_report. The core preserves order,
                // so this mapper does NOT re-sort. saturating_add so #766's
                // i64::MAX-clamped buckets (corrupt Antigravity DB) can't
                // overflow this FFI-exposed total in debug/release.
                let total = e
                    .input
                    .saturating_add(e.output)
                    .saturating_add(e.cache_read)
                    .saturating_add(e.cache_write)
                    .saturating_add(e.reasoning);
                AgentEntry {
                    agent: e.agent,
                    clients: e.clients,
                    input: e.input,
                    output: e.output,
                    cache_read: e.cache_read,
                    cache_write: e.cache_write,
                    reasoning: e.reasoning,
                    total,
                    cost: e.cost,
                    messages: e.messages,
                }
            })
            .collect(),
        total_cost: report.total_cost,
        total_messages: report.total_messages,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #766 clamps corrupt Antigravity varints to `i64::MAX` per bucket. Two
    /// such buckets in one agent entry must saturate the mapped `total`, not
    /// overflow it (a plain `+` panics in debug / wraps in release).
    #[test]
    fn total_saturates_on_overlarge_buckets() {
        let report = tokscale_core::AgentReport {
            entries: vec![tokscale_core::AgentReportEntry {
                agent: "Main".to_string(),
                clients: vec!["antigravity_cli".to_string()],
                input: i64::MAX,
                output: i64::MAX,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
                cost: 0.0,
                messages: 1,
            }],
            total_cost: 0.0,
            total_messages: 1,
            processing_time_ms: 0,
        };

        let mapped = map_report(report);
        assert_eq!(mapped.entries[0].total, i64::MAX);
    }

    #[test]
    fn run_rejects_an_invalid_year_filter_before_scanning() {
        let context =
            crate::LocalSourceContext::for_home(std::path::PathBuf::from("/nonexistent-020-home"));
        assert!(run(&context, "26", None).is_err());
        assert!(run(&context, "abcd", None).is_err());
    }

    #[test]
    fn normalize_year_accepts_only_four_digits() {
        assert_eq!(normalize_year(""), Ok(None));
        assert_eq!(normalize_year(" 2026 "), Ok(Some("2026".to_string())));
        assert!(normalize_year("26").is_err());
        assert!(normalize_year("20x6").is_err());
    }
}
