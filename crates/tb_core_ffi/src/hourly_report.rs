//! Per-hour usage breakdown for the popover, backed by tokscale-core's
//! `get_hourly_report`. Mirrors tokscale's TUI "Hourly" view: one entry per
//! "YYYY-MM-DD HH:00" slot with the token breakdown, message/turn counts, and
//! cost. The frontend folds these slots into a 24-hour-of-day distribution.
//!
//! Like `model_report`, this drives the async core on a short-lived
//! current-thread runtime (callers run it inside `spawn_blocking`) and maps the
//! result onto a camelCase JSON shape the frontend consumes directly.

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct HourlyEntry {
    /// "YYYY-MM-DD HH:00" local-time slot.
    hour: String,
    clients: Vec<String>,
    models: Vec<String>,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    reasoning: i64,
    total: i64,
    message_count: i32,
    turn_count: i32,
    cost: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HourlyReportData {
    entries: Vec<HourlyEntry>,
    total_cost: f64,
}

/// Build the per-hour report for `year` (empty string = all time), restricted
/// to `clients` (None = every client). The client filter is applied in the
/// streaming scan, so shared-hour buckets carry only the selected clients'
/// tokens/cost — a membership filter downstream cannot do this because each
/// `HourAggregator` folds all clients into one mixed total.
pub(crate) fn run(
    context: &crate::LocalSourceContext,
    year: &str,
    clients: Option<Vec<String>>,
) -> Result<Value, String> {
    let data = map_report(compute(context, year, clients)?);
    serde_json::to_value(data).map_err(|e| format!("serialize hourly report: {}", e))
}

/// The engine's report, before mapping (split out of [`run`] so
/// `remote_pages` can add remote records to it).
pub(crate) fn compute(
    context: &crate::LocalSourceContext,
    year: &str,
    clients: Option<Vec<String>>,
) -> Result<tokscale_core::HourlyReport, String> {
    let year = normalize_year(year)?;
    let options = context.report_options(year, clients);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("build runtime: {}", e))?;
    runtime.block_on(tokscale_core::get_hourly_report(options))
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

pub(crate) fn map_report(report: tokscale_core::HourlyReport) -> HourlyReportData {
    HourlyReportData {
        entries: report
            .entries
            .into_iter()
            .map(|e| {
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
                HourlyEntry {
                    hour: e.hour,
                    clients: e.clients,
                    models: e.models,
                    input: e.input,
                    output: e.output,
                    cache_read: e.cache_read,
                    cache_write: e.cache_write,
                    reasoning: e.reasoning,
                    total,
                    message_count: e.message_count,
                    turn_count: e.turn_count,
                    cost: e.cost,
                }
            })
            .collect(),
        total_cost: report.total_cost,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #766 clamps corrupt Antigravity varints to `i64::MAX` per bucket. Two
    /// such buckets in one hourly entry must saturate the mapped `total`, not
    /// overflow it (a plain `+` panics in debug / wraps in release).
    fn entry(input: i64, output: i64, cache_read: i64, cache_write: i64, reasoning: i64) -> tokscale_core::HourlyUsage {
        tokscale_core::HourlyUsage {
            hour: "2026-07-12 00:00".to_string(),
            clients: vec!["antigravity_cli".to_string()],
            models: vec!["gemini-3-pro".to_string()],
            input,
            output,
            cache_read,
            cache_write,
            message_count: 1,
            turn_count: 1,
            reasoning,
            cost: 0.0,
        }
    }

    fn wrap(entries: Vec<tokscale_core::HourlyUsage>) -> tokscale_core::HourlyReport {
        tokscale_core::HourlyReport {
            entries,
            total_cost: 0.0,
            processing_time_ms: 0,
        }
    }

    #[test]
    fn total_saturates_on_overlarge_buckets() {
        let report = wrap(vec![entry(i64::MAX, i64::MAX, 0, 0, 0)]);
        let mapped = map_report(report);
        assert_eq!(mapped.entries[0].total, i64::MAX);
    }

    /// The two-MAX-field case above only pins `input`/`output` into the fold.
    /// Pin the other three fields too: nonzero `input`/`output`/`cache_write`
    /// plus clamped `cache_read`/`reasoning`, so those two are independently
    /// exercised, not just present-but-untested.
    #[test]
    fn total_saturates_when_cache_read_and_reasoning_are_overlarge() {
        let report = wrap(vec![entry(10, 20, i64::MAX, 5, i64::MAX)]);
        let mapped = map_report(report);
        assert_eq!(mapped.entries[0].total, i64::MAX);
    }

    /// The saturating cases can't catch a dropped operand (another MAX field
    /// keeps the total at MAX), so pin every field's inclusion with distinct
    /// powers of two: omitting any one operand changes the exact sum.
    #[test]
    fn total_includes_every_token_field() {
        let report = wrap(vec![entry(1, 2, 4, 8, 16)]);
        let mapped = map_report(report);
        assert_eq!(mapped.entries[0].total, 31);
    }

    #[test]
    fn run_rejects_an_invalid_year_filter_before_scanning() {
        let context =
            crate::LocalSourceContext::for_home(std::path::PathBuf::from("/nonexistent-020-home"));
        assert!(run(&context, "26", None).is_err());
    }

    #[test]
    fn normalize_year_accepts_only_four_digits() {
        assert_eq!(normalize_year(""), Ok(None));
        assert_eq!(normalize_year(" 2026 "), Ok(Some("2026".to_string())));
        assert!(normalize_year("26").is_err());
        assert!(normalize_year("20x6").is_err());
    }
}
