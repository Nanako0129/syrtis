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
/// Items per page (whereToken's fixed size; the cap is unpublished).
const PAGE_SIZE: &str = "100";
const MAX_CURSOR_BYTES: usize = 4096;
/// Design 5: a background walk with no cutover ends after this many
/// consecutive calendar months that gave no rows.
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
    currency: Option<String>,
    /// In `currency` units, not cents.
    contracted: Option<Cents>,
}

/// A decimal amount (a JSON string or number such as `"0.0700000000"`) as
/// integer hundredths, rounded half up on the third decimal, computed from
/// its digits (no float). Anything else fails the page, like `Num`.
struct Cents(i64);

impl<'de> Deserialize<'de> for Cents {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let text = match serde_json::Value::deserialize(deserializer)? {
            serde_json::Value::String(text) => text,
            serde_json::Value::Number(number) => number.to_string(),
            _ => return Err(D::Error::custom("not a decimal")),
        };
        cents(text.trim())
            .map(Cents)
            .ok_or_else(|| D::Error::custom("not a decimal"))
    }
}

/// `-?digits(.digits)?` (at most 15 whole digits) -> hundredths.
fn cents(text: &str) -> Option<i64> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (whole, fraction) = match digits.split_once('.') {
        Some((whole, fraction)) if !fraction.is_empty() => (whole, fraction),
        Some(_) => return None,
        None => (digits, ""),
    };
    let all_digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    if whole.is_empty() || whole.len() > 15 || !all_digits(whole) || !all_digits(fraction) {
        return None;
    }
    let digit = |i: usize| fraction.as_bytes().get(i).map_or(0, |b| i64::from(b - b'0'));
    let mut hundredths = whole.parse::<i64>().ok()? * 100 + digit(0) * 10 + digit(1);
    if digit(2) >= 5 {
        hundredths += 1;
    }
    Some(if negative { -hundredths } else { hundredths })
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
    /// `cursor` rows left out: `eventTime` unreadable or no model (deduped
    /// by id; never a row, never counted against `billing_v2_no_items`).
    pub(crate) dropped: usize,
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
/// month ends 23:59:59Z on its last day, the current one now. Kept to the
/// second on purpose: whereToken measured that a month ending at the next
/// midnight or at 23:59:59.999 answers availability-only, while 23:59:59Z
/// pages (discovery §1; `billing_v2.go:51-60`). An event in the month's last
/// second is the accepted cost.
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

enum Mapped {
    Row(String, Row),
    /// A `cursor` row that cannot be written: counted (by id), not written.
    Dropped(String),
    /// Not one of ours to write (another product, no token meters, no id).
    Skipped,
}

/// The item as an engine row, with its id for dedupe. Skipped: another
/// product (D3), `hasTokenMeters:false`, no id. Dropped: an `eventTime`
/// that does not parse (S8), or no model (the engine drops a model-less row
/// anyway; dropping it here keeps it out of `events` and of D6).
fn map(item: Item) -> Mapped {
    if item.product.as_deref() != Some("cursor") {
        return Mapped::Skipped;
    }
    if item.has_token_meters == Some(false) {
        return Mapped::Skipped;
    }
    let Some(id) = item.billing_item_id else {
        return Mapped::Skipped;
    };
    let Some((ms, month)) = item
        .event_time
        .as_deref()
        .and_then(timestamp_ms)
        .and_then(|ms| Some((ms, month_index(ms)?)))
    else {
        return Mapped::Dropped(id);
    };
    let Some(model) = item
        .display_extras
        .and_then(|extras| extras.model)
        .filter(|model| !model.is_empty())
    else {
        return Mapped::Dropped(id);
    };
    // D2: a plan-included row (contracted 0) carries no cost, so the engine
    // prices it locally; so does a row in another currency. A paid USD row
    // is integer cents.
    let charged_cents = item
        .pricing
        .filter(|pricing| pricing.currency.as_deref() == Some("USD"))
        .and_then(|pricing| pricing.contracted)
        .map(|contracted| contracted.0)
        .filter(|cents| *cents > 0)
        .map(|cents| Num(serde_json::Number::from(cents)));
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
        model: Some(model),
        charged_cents,
        token_usage,
    };
    Mapped::Row(id, Row { month, ms, event })
}

/// Read `windows` (UTC month indexes, newest first). `user` is the cookie's
/// own user id: every item of every page must carry it as `principal.id`.
/// `stop_after_empty`: end after `EMPTY_WINDOWS_STOP` consecutive calendar
/// months with no rows (design 5); a gap between windows restarts the count.
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
    let mut previous: Option<i32> = None;
    let mut dropped = 0usize;
    for &month in windows {
        let Some((start, end)) = bounds(month, now_ms) else {
            continue;
        };
        let rows_before = rows.len();
        let mut cursor: Option<String> = None;
        let mut cursors: HashSet<String> = HashSet::new();
        loop {
            pages += 1;
            if pages > limits.max_v2_pages {
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
                match map(item) {
                    Mapped::Row(id, row) => {
                        if ids.insert(id) {
                            rows.push(row);
                        }
                    }
                    Mapped::Dropped(id) => {
                        if ids.insert(id) {
                            dropped += 1;
                        }
                    }
                    Mapped::Skipped => {}
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
        if previous.is_some_and(|previous| previous - 1 != month) {
            empty_run = 0;
        }
        previous = Some(month);
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
    Ok(Fetched {
        rows,
        months,
        dropped,
    })
}

#[cfg(test)]
mod tests {
    /// Integer cents from the decimal text, rounded half up. Mutation:
    /// `if digit(2) >= 5 {` -> `if digit(2) > 5 {`.
    #[test]
    fn cents_come_from_the_digits() {
        for (text, expected) in [
            ("0.0700000000", Some(7)),
            ("0.0000000000", Some(0)),
            ("0.075", Some(8)),
            ("0.0749", Some(7)),
            ("12", Some(1200)),
            ("3.1", Some(310)),
            ("-0.07", Some(-7)),
            ("1.", None),
            (".5", None),
            ("1e-5", None),
            ("abc", None),
            ("1234567890123456", None),
        ] {
            assert_eq!(super::cents(text), expected, "{text}");
        }
    }
}
