//! Contribution-graph payload for the popover.
//!
//! Session parsing, dedup, and pricing are delegated to the vendored
//! `tokscale-core` crate (see `vendor/tokscale-core`), which covers every
//! supported agent and ships mature LiteLLM/OpenRouter pricing. This module
//! is now a thin adapter: it drives `tokscale-core`'s local graph report and
//! maps the resulting `GraphResult` back onto the camelCase JSON shape the
//! frontend already consumes (`src/lib/types.ts` `UsagePayload`).

use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

const VERSION: &str = concat!("syrtis-core/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenBreakdown {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub reasoning: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ClientContribution {
    client: String,
    model_id: String,
    provider_id: String,
    tokens: TokenBreakdown,
    cost: f64,
    messages: i32,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct DailyTotals {
    tokens: i64,
    cost: f64,
    messages: i32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DailyContribution {
    date: String,
    totals: DailyTotals,
    intensity: u8,
    token_breakdown: TokenBreakdown,
    clients: Vec<ClientContribution>,
    /// Per-day turns by exact client id. Daily/Monthly read these instead of
    /// requesting the hourly report, which cost a second full scan of the
    /// clients that dominate turn data.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    turns_by_client: BTreeMap<String, i64>,
}

#[derive(Debug, Clone, Serialize)]
struct DateRange {
    start: String,
    end: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct YearSummary {
    year: String,
    total_tokens: i64,
    total_cost: f64,
    range: DateRange,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DataSummary {
    total_tokens: i64,
    total_cost: f64,
    total_days: i32,
    active_days: i32,
    average_per_day: f64,
    max_cost_in_single_day: f64,
    clients: Vec<String>,
    models: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportMeta {
    generated_at: String,
    version: String,
    date_range: DateRange,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TokenContributionData {
    meta: ExportMeta,
    summary: DataSummary,
    years: Vec<YearSummary>,
    contributions: Vec<DailyContribution>,
}

/// Build the contribution-graph payload for `year` (empty string = all time).
///
/// Invoked from `lib.rs` inside `spawn_blocking`, so the calling thread has no
/// Tokio reactor — we spin up a short-lived current-thread runtime to drive
/// the async `generate_local_graph_report`. That entry point uses cached
/// pricing with a graceful offline fallback, and `PricingService` is a process
/// -wide `OnceCell`, so the network fetch happens at most once per launch.
pub(crate) fn run(context: &crate::LocalSourceContext, year: &str) -> Result<Value, String> {
    to_payload(compute(context, year)?)
}

/// The engine's graph for `year`, before mapping (split out of [`run`] so
/// `remote_pages` can add remote records to it).
pub(crate) fn compute(
    context: &crate::LocalSourceContext,
    year: &str,
) -> Result<tokscale_core::GraphResult, String> {
    let year = normalize_year(year)?;
    let options = context.report_options(year, None);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("build runtime: {}", e))?;
    runtime.block_on(tokscale_core::generate_local_graph_report(options))
}

/// The payload of an engine graph: [`map_graph`], serialized.
pub(crate) fn to_payload(graph: tokscale_core::GraphResult) -> Result<Value, String> {
    let payload = map_graph(graph);
    serde_json::to_value(payload).map_err(|e| format!("serialize usage graph: {}", e))
}

pub(crate) fn normalize_year(year: &str) -> Result<Option<String>, String> {
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

/// Map `tokscale-core`'s `GraphResult` onto the frontend-facing payload.
/// Field renames: tokscale uses flat `date_range_start/end` and
/// `range_start/end`, the frontend expects nested `{ start, end }`. Extra
/// tokscale fields (`active_time_ms`, `time_metrics`, `processing_time_ms`)
/// are intentionally dropped. The reported `version` is branded as tokenbar.
fn map_graph(graph: tokscale_core::GraphResult) -> TokenContributionData {
    TokenContributionData {
        meta: ExportMeta {
            generated_at: graph.meta.generated_at,
            version: VERSION.to_string(),
            date_range: DateRange {
                start: graph.meta.date_range_start,
                end: graph.meta.date_range_end,
            },
        },
        summary: DataSummary {
            total_tokens: graph.summary.total_tokens,
            total_cost: graph.summary.total_cost,
            total_days: graph.summary.total_days,
            active_days: graph.summary.active_days,
            average_per_day: graph.summary.average_per_day,
            max_cost_in_single_day: graph.summary.max_cost_in_single_day,
            clients: graph.summary.clients,
            models: graph.summary.models,
        },
        years: graph
            .years
            .into_iter()
            .map(|y| YearSummary {
                year: y.year,
                total_tokens: y.total_tokens,
                total_cost: y.total_cost,
                range: DateRange {
                    start: y.range_start,
                    end: y.range_end,
                },
            })
            .collect(),
        contributions: graph
            .contributions
            .into_iter()
            .map(map_contribution)
            .collect(),
    }
}

fn map_contribution(day: tokscale_core::DailyContribution) -> DailyContribution {
    DailyContribution {
        date: day.date,
        totals: DailyTotals {
            tokens: day.totals.tokens,
            cost: day.totals.cost,
            messages: day.totals.messages,
        },
        intensity: day.intensity,
        token_breakdown: map_breakdown(&day.token_breakdown),
        clients: day
            .clients
            .into_iter()
            .map(|c| ClientContribution {
                client: c.client,
                model_id: c.model_id,
                provider_id: c.provider_id,
                tokens: map_breakdown(&c.tokens),
                cost: c.cost,
                messages: c.messages,
            })
            .collect(),
        turns_by_client: day.turns_by_client,
    }
}

fn map_breakdown(breakdown: &tokscale_core::TokenBreakdown) -> TokenBreakdown {
    TokenBreakdown {
        input: breakdown.input,
        output: breakdown.output,
        cache_read: breakdown.cache_read,
        cache_write: breakdown.cache_write,
        reasoning: breakdown.reasoning,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_rejects_an_invalid_year_filter_before_scanning() {
        let context =
            crate::LocalSourceContext::for_home(std::path::PathBuf::from("/nonexistent-020-home"));
        assert!(run(&context, "26").is_err());
        assert!(run(&context, "abcd").is_err());
    }

    #[test]
    fn normalize_year_accepts_only_four_digits() {
        assert_eq!(normalize_year(""), Ok(None));
        assert_eq!(normalize_year(" 2026 "), Ok(Some("2026".to_string())));
        assert!(normalize_year("26").is_err());
        assert!(normalize_year("20x6").is_err());
    }
}
