//! Cursor Billing V2 (`query-billing-items`) for an account whose old
//! `get-filtered-usage-events` page 1 is a 2xx JSON object without
//! `usageEventsDisplay` (plan CURSOR-BV2-M r4, design 6; the switch, the
//! history sidecar and the commit live in `cursor_sync`).
//!
//! Security contract (r4 S4-S8, S10, S11):
//! - The URL is a constant base plus `query_pairs_mut`; the server's cursor
//!   is at most 4096 bytes, never repeats, and never reaches a reason.
//! - The client (no redirects, https only) and the sensitive cookie are the
//!   old walk's. Pages and body bytes are capped across every window of one
//!   walk; any refusal (3xx/401/403/429/5xx, an `availability` answer,
//!   another principal) ends the walk, and the caller then writes nothing.
//! - Items are read into a typed struct with only the fields below:
//!   `displayExtras.userEmail` is never deserialised; `principal.id` and
//!   `billingItemId` live only in memory (comparison, dedupe). No log line.

use crate::cursor_sync::{
    body_stop, read_capped, refusal, send_stop, timestamp_ms, Event, Limits, Num, State, Stop,
    Timestamp, TokenUsage,
};
use chrono::{Datelike, TimeZone, Utc};
use serde::Deserialize;
use std::collections::HashSet;
use std::time::Instant;

pub(crate) const BILLING_ITEMS_URL: &str =
    "https://cursor.com/api/dashboard/v2/query-billing-items";
const PAGE_SIZE: &str = "100";
const MAX_CURSOR_BYTES: usize = 4096;
/// Design 5: with no cutover, a walk ends after this many windows in a row
/// that gave no rows.
const EMPTY_WINDOWS_STOP: usize = 3;

#[derive(Deserialize)]
struct Page {
    data: Option<Vec<Item>>,
    pagination: Option<Pagination>,
    availability: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pagination {
    next_cursor: Option<String>,
}

/// Only the fields the mapping reads (no whole-item value, no Debug).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Item {
    billing_item_id: Option<String>,
    event_time: Option<String>,
    product: Option<String>,
    display_extras: Option<Extras>,
    usage: Option<Meters>,
    pricing: Option<Pricing>,
    has_token_meters: Option<bool>,
    principal: Option<Principal>,
}

#[derive(Deserialize)]
struct Extras {
    model: Option<String>,
}

/// Decimal strings on the wire; `Num` accepts a number or a numeric string
/// and refuses free text (the page then fails as `unexpected_response`).
#[derive(Deserialize)]
struct Meters {
    #[serde(rename = "prompt-text-tokens")]
    prompt: Option<Num>,
    #[serde(rename = "cached-prompt-text-tokens")]
    cached: Option<Num>,
    #[serde(rename = "cache-write-text-tokens")]
    cache_write: Option<Num>,
    #[serde(rename = "completion-text-tokens")]
    completion: Option<Num>,
}

#[derive(Deserialize)]
struct Pricing {
    /// USD, not cents.
    contracted: Option<Num>,
}

#[derive(Deserialize)]
struct Principal {
    id: Option<String>,
}

/// One mapped row and the UTC month its `eventTime` falls in.
pub(crate) struct Row {
    pub(crate) month: i32,
    pub(crate) ms: i64,
    pub(crate) event: Event,
}

pub(crate) struct Fetched {
    /// Deduped by `billingItemId`; only rows whose month is in `months`.
    pub(crate) rows: Vec<Row>,
    /// The windows read to their last page, newest first.
    pub(crate) months: Vec<i32>,
}

/// UTC calendar month as `year * 12 + month0`.
pub(crate) fn month_index(ms: i64) -> Option<i32> {
    let time = Utc.timestamp_millis_opt(ms).single()?;
    Some(time.year() * 12 + time.month0() as i32)
}

fn month_start(index: i32) -> Option<chrono::DateTime<Utc>> {
    Utc.with_ymd_and_hms(
        index.div_euclid(12),
        index.rem_euclid(12) as u32 + 1,
        1,
        0,
        0,
        0,
    )
    .single()
}

/// One month's `startTime`/`endTime`, `yyyy-MM-ddTHH:mm:ssZ`: an earlier
/// month ends 23:59:59Z on its last day, the current one now.
fn bounds(index: i32, now_ms: i64) -> Option<(String, String)> {
    let start = month_start(index)?;
    let last = month_start(index + 1)? - chrono::Duration::seconds(1);
    let end = last.min(Utc.timestamp_millis_opt(now_ms).single()?);
    let format = "%Y-%m-%dT%H:%M:%SZ";
    Some((
        start.format(format).to_string(),
        end.format(format).to_string(),
    ))
}

/// The item as an engine row, with its id for dedupe; None for a row that is
/// not written: another product (D3), `hasTokenMeters:false`, no id, or an
/// `eventTime` that does not parse (S8).
fn map(item: Item) -> Option<(String, Row)> {
    if item.product.as_deref() != Some("cursor") {
        return None;
    }
    if item.has_token_meters == Some(false) {
        return None;
    }
    let id = item.billing_item_id?;
    let ms = timestamp_ms(&item.event_time?)?;
    let month = month_index(ms)?;
    // D2: a plan-included row (contracted 0) carries no cost, so the engine
    // prices it locally; a paid one is USD -> cents.
    let charged_cents = item
        .pricing
        .and_then(|pricing| pricing.contracted)
        .and_then(|usd| usd.0.as_f64())
        .filter(|usd| *usd > 0.0)
        .and_then(|usd| serde_json::Number::from_f64(usd * 100.0))
        .map(Num);
    let token_usage = item.usage.map(|usage| TokenUsage {
        input_tokens: usage.prompt,
        output_tokens: usage.completion,
        cache_read_tokens: usage.cached,
        cache_write_tokens: usage.cache_write,
        total_cents: None,
    });
    let event = Event {
        // D4: no conversation id in V2; the engine keys sessions per day.
        conversation_id: None,
        timestamp: Some(Timestamp::Text(ms.to_string())),
        model: item.display_extras.and_then(|extras| extras.model),
        charged_cents,
        token_usage,
    };
    Some((id, Row { month, ms, event }))
}

/// Read `windows` (UTC month indexes, newest first). `user` is the cookie's
/// own user id: every item of every page must carry it as `principal.id`.
/// `stop_after_empty`: end after `EMPTY_WINDOWS_STOP` windows in a row with
/// no rows (design 5).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn walk(
    client: &reqwest::Client,
    base: &str,
    cookie: &reqwest::header::HeaderValue,
    user: &str,
    limits: Limits,
    enabled: &dyn Fn() -> bool,
    windows: &[i32],
    stop_after_empty: bool,
    now_ms: i64,
) -> Result<Fetched, Stop> {
    use reqwest::header;
    let deadline = Instant::now() + limits.budget;
    let mut pages = 0u32;
    let mut bytes_read = 0usize;
    let mut ids: HashSet<String> = HashSet::new();
    let mut rows: Vec<Row> = Vec::new();
    let mut months: Vec<i32> = Vec::new();
    let mut empty_run = 0usize;
    for &month in windows {
        let Some((start, end)) = bounds(month, now_ms) else {
            continue;
        };
        let rows_before = rows.len();
        let mut cursor: Option<String> = None;
        let mut cursors: HashSet<String> = HashSet::new();
        loop {
            pages += 1;
            if pages > limits.max_pages {
                return Err(Stop::new(State::Partial, Some("too_many_pages")));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Stop::new(State::Partial, Some("budget_exhausted")));
            }
            // S-3: the switch is re-read before every send.
            if !enabled() {
                return Err(Stop::new(State::Disabled, None));
            }
            let mut url =
                reqwest::Url::parse(base).map_err(|_| Stop::error("client_unavailable"))?;
            {
                let mut query = url.query_pairs_mut();
                query
                    .append_pair("scope", "user")
                    .append_pair("startTime", &start)
                    .append_pair("endTime", &end)
                    .append_pair("pageSize", PAGE_SIZE);
                if let Some(cursor) = &cursor {
                    query.append_pair("cursor", cursor);
                }
            }
            let sent = client
                .get(url)
                .header(header::COOKIE, cookie.clone())
                .header(header::ORIGIN, "https://cursor.com")
                .header(header::REFERER, "https://cursor.com/dashboard")
                .header(header::ACCEPT, "application/json")
                .header(header::USER_AGENT, "TokenBar")
                .timeout(limits.page_timeout.min(remaining))
                .send()
                .await;
            let mut response = sent.map_err(|error| send_stop(&error))?;
            if let Some(stop) = refusal(&mut response).await {
                return Err(stop);
            }
            let cap = limits.max_body_bytes.saturating_sub(bytes_read);
            let body = read_capped(&mut response, cap).await.map_err(body_stop)?;
            bytes_read += body.len();
            // An object only: serde would also read these structs from an array.
            if body.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') {
                return Err(Stop::error("unexpected_response"));
            }
            let page: Page =
                serde_json::from_slice(&body).map_err(|_| Stop::error("unexpected_response"))?;
            drop(body);
            match page.availability.as_deref() {
                Some("missing_team_id") => return Err(Stop::error("billing_v2_team_required")),
                Some(_) => return Err(Stop::error("billing_v2_unavailable")),
                None => {}
            }
            let items = page.data.unwrap_or_default();
            // S7: every item, written or not, must be the cookie's own user.
            if items
                .iter()
                .any(|item| item.principal.as_ref().and_then(|p| p.id.as_deref()) != Some(user))
            {
                return Err(Stop::error("billing_v2_principal_mismatch"));
            }
            for item in items {
                if let Some((id, row)) = map(item) {
                    if ids.insert(id) {
                        rows.push(row);
                    }
                }
            }
            match page
                .pagination
                .and_then(|pagination| pagination.next_cursor)
                .filter(|next| !next.is_empty())
            {
                Some(next) => {
                    if next.len() > MAX_CURSOR_BYTES || !cursors.insert(next.clone()) {
                        return Err(Stop::new(State::Partial, Some("too_many_pages")));
                    }
                    cursor = Some(next);
                }
                None => break,
            }
        }
        months.push(month);
        empty_run = if rows.len() == rows_before {
            empty_run + 1
        } else {
            0
        };
        if stop_after_empty && empty_run >= EMPTY_WINDOWS_STOP {
            break;
        }
    }
    // A row dated in a month this walk did not read to the end would sit
    // beside that month's earlier rows (design 5b): dropped.
    rows.retain(|row| months.contains(&row.month));
    Ok(Fetched { rows, months })
}
