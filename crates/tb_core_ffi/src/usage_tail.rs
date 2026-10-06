//! Live tokens/min + per-(client, agent, model) trace for the popover and the
//! menu-bar cat animation.
//!
//! Each `tick()` re-parses only files whose mtime falls inside the event
//! window plus a small margin (`modified_after`), so steady-state cost is a
//! stat sweep plus parsing the handful of currently-active session files.
//! The event window is replaced wholesale each tick — snapshot-replace rather
//! than incremental append means tokscale's own dedup handles duplicates and
//! cross-tick state never accumulates.

use chrono::{Duration, Local, TimeZone};
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Retain a generous window so any `rate_in_window` / `trace` query (max 10m in
/// practice) is satisfiable, and so the window stays correct across midnight.
const EVENT_WINDOW_SECS: i64 = 3600;

#[derive(Debug, Clone, Serialize)]
pub struct UsageEvent {
    pub ts_ms: i64,
    pub client: String,
    pub agent: String,
    pub model: String,
    pub input: i64,
    pub output: i64,
    pub reasoning: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub message_count: i32,
}

impl UsageEvent {
    fn total(&self) -> i64 {
        // saturating_add so #766's i64::MAX-clamped buckets (corrupt
        // Antigravity DB) can't overflow this always-on live-rate total in
        // debug/release (see agents_report.rs's map_report for the same
        // pattern).
        self.input
            .saturating_add(self.output)
            .saturating_add(self.reasoning)
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TraceBucket {
    pub client: String,
    pub agent: String,
    pub model: String,
    pub tokens: i64,
    pub messages: u32,
    pub tokens_per_min: f32,
}

pub struct UsageTailer {
    events: Mutex<Vec<UsageEvent>>,
    /// Topology-sensitive source token from the last parse; when it hasn't
    /// changed, the event window is still correct (rate queries re-filter by
    /// timestamp on read) and the tick skips the parse entirely.
    last_source_token: Mutex<Option<u64>>,
    /// Ticks that attempted the parse (whether it succeeded or failed) rather
    /// than taking the unchanged-token skip; lets a benchmark tell a paid parse
    /// from a skip without a second token probe.
    parses: AtomicUsize,
}

impl UsageTailer {
    pub fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            last_source_token: Mutex::new(None),
            parses: AtomicUsize::new(0),
        }
    }

    // Only the benchmark reads it (macOS-only, see the `bench` mount below);
    // anywhere else the method would be unused.
    #[cfg(all(test, target_os = "macos"))]
    pub fn parse_count(&self) -> usize {
        self.parses.load(Ordering::Relaxed)
    }

    /// Re-parse recent local sessions via tokscale-core and replace the event
    /// window. Returns the number of events now in the window (cheap to compute
    /// and only used as a "did anything happen" hint by callers).
    pub fn tick(&self) -> usize {
        self.tick_with_clock(now_ms)
    }

    /// `tick` with the wall clock injected, so a benchmark on a captured corpus
    /// sees the same window on every run. Production passes `now_ms`, read at
    /// the same three points as before: the `since` date, `modified_after`, and
    /// the event cutoff after the parse.
    pub fn tick_with_clock(&self, clock: impl Fn() -> i64) -> usize {
        // `since` is date-granular; reach back one day so a sub-hour window that
        // straddles midnight still sees yesterday's tail.
        let now_local = Local
            .timestamp_millis_opt(clock())
            .single()
            .unwrap_or_else(Local::now);
        let since = (now_local - Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        // `modified_after` is what bounds the per-tick parse cost: a session
        // log whose mtime predates the event window can't contain in-window
        // events (logs are append-only, mtime >= last event's timestamp), so
        // only files active within the window — plus a small margin for write
        // latency and clock skew — are re-parsed each tick.
        let window_reach_ms = (EVENT_WINDOW_SECS + 300) * 1000;
        let context = crate::LocalSourceContext::current();
        let mut options = context.parse_options(None, None);
        options.since = Some(since);
        options.modified_after = Some((clock() - window_reach_ms) as u64);

        // No source changed since the last parse → the window is already
        // correct; skip the parse. Probe failure falls through to a parse.
        let token = tokscale_core::local_source_change_token(&options).ok();
        if token.is_some() && *self.last_source_token.lock() == token {
            return self.events.lock().len();
        }

        self.parses.fetch_add(1, Ordering::Relaxed);
        let parsed = match tokscale_core::parse_local_clients(options) {
            Ok(parsed) => parsed,
            Err(_) => return self.events.lock().len(),
        };
        *self.last_source_token.lock() = token;

        let cutoff = clock() - EVENT_WINDOW_SECS * 1000;
        let mut next: Vec<UsageEvent> = parsed
            .messages
            .into_iter()
            // ParsedMessage.timestamp is unix milliseconds (see
            // tokscale-core sessions::mod::timestamp_to_date, which feeds it to
            // chrono's timestamp_millis_opt).
            .filter(|m| m.timestamp >= cutoff)
            .map(|m| {
                let agent = m.agent.clone().unwrap_or_else(|| m.client.clone());
                UsageEvent {
                    ts_ms: m.timestamp,
                    client: m.client,
                    agent,
                    model: m.model_id,
                    input: m.input,
                    output: m.output,
                    reasoning: m.reasoning,
                    cache_read: m.cache_read,
                    cache_write: m.cache_write,
                    message_count: m.message_count,
                }
            })
            .collect();
        next.sort_by_key(|e| e.ts_ms);

        let len = next.len();
        *self.events.lock() = next;
        len
    }

    #[allow(dead_code)] // kept for API symmetry with rate_in_window
    pub fn rate_per_min(&self) -> f32 {
        self.window_total(60) as f32
    }

    pub fn rate_in_window(&self, window_secs: i64) -> f32 {
        if window_secs <= 0 {
            return 0.0;
        }
        let total = self.window_total(window_secs) as f32;
        let window_min = window_secs as f32 / 60.0;
        total / window_min
    }

    fn window_total(&self, secs: i64) -> i64 {
        // saturating so a pathological `secs` can't overflow the cutoff.
        let cutoff = now_ms().saturating_sub(secs.saturating_mul(1000));
        let events = self.events.lock();
        // saturating_add: each event's total() is already saturated, but the
        // cross-event fold over a window can still overflow the same way.
        events
            .iter()
            .filter(|e| e.ts_ms >= cutoff)
            .map(|e| e.total())
            .fold(0i64, |acc, t| acc.saturating_add(t))
    }

    /// Per-(client, agent, model) breakdown over `window_secs`. Frontend
    /// decides whether to collapse rows by client based on the user's
    /// "detailed trace" setting.
    pub fn trace(&self, window_secs: i64) -> Vec<TraceBucket> {
        // Mirror rate_in_window's contract: a non-positive window has no events.
        // saturating_* so a garbage window_secs from the C side can't overflow
        // the cutoff; window_secs itself is left intact for the window_min
        // divisor below, so the per-minute rate is never distorted by a clamp.
        if window_secs <= 0 {
            return Vec::new();
        }
        let cutoff = now_ms().saturating_sub(window_secs.saturating_mul(1000));
        let events = self.events.lock();
        let mut groups: HashMap<(String, String, String), (i64, u32)> = HashMap::new();
        for e in events.iter() {
            if e.ts_ms < cutoff {
                continue;
            }
            let key = (e.client.clone(), e.agent.clone(), e.model.clone());
            let slot = groups.entry(key).or_insert((0, 0));
            // saturating_add: same cross-event overflow class as window_total.
            slot.0 = slot.0.saturating_add(e.total());
            slot.1 = slot.1.saturating_add(e.message_count.max(0) as u32);
        }
        let window_min = (window_secs as f32 / 60.0).max(1.0 / 60.0);
        let mut out: Vec<TraceBucket> = groups
            .into_iter()
            .map(|((client, agent, model), (tokens, messages))| TraceBucket {
                client,
                agent,
                model,
                tokens,
                messages,
                tokens_per_min: tokens as f32 / window_min,
            })
            .collect();
        out.sort_by_key(|b| std::cmp::Reverse(b.tokens));
        out
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// Tail measurement harness for the CPU work. The two benches are `#[ignore]`;
// three small unit tests (digest mutations, record appending, source-line
// selection) run in the normal suite. macOS only: it reads CPU through a
// hand-declared getrusage with Darwin's layout.
#[cfg(all(test, target_os = "macos"))]
#[path = "usage_tail_bench.rs"]
mod bench;

#[cfg(test)]
mod tests {
    use super::*;

    /// #766 clamps corrupt Antigravity varints to `i64::MAX` per bucket. Two
    /// such events in the same window must saturate `window_total` /
    /// `rate_in_window` and the per-bucket `trace` fold, not overflow them (a
    /// plain `+`/`.sum()`/`+=` panics in debug / wraps in release).
    fn overlarge_event(ts_ms: i64, client: &str) -> UsageEvent {
        UsageEvent {
            ts_ms,
            client: client.to_string(),
            agent: "Main".to_string(),
            model: "gemini-3-pro".to_string(),
            input: i64::MAX,
            output: 0,
            reasoning: 0,
            cache_read: i64::MAX,
            cache_write: 0,
            message_count: 1,
        }
    }

    #[test]
    fn usage_event_total_saturates_on_overlarge_buckets() {
        let e = overlarge_event(0, "antigravity-cli");
        assert_eq!(e.total(), i64::MAX);
    }

    #[test]
    fn window_total_saturates_across_overlarge_events() {
        let tailer = UsageTailer::new();
        let now = now_ms();
        *tailer.events.lock() = vec![
            overlarge_event(now, "antigravity-cli"),
            overlarge_event(now, "antigravity-cli"),
        ];

        let total = tailer.window_total(3600);
        assert_eq!(total, i64::MAX);

        let rate = tailer.rate_in_window(3600);
        assert!(rate.is_finite(), "rate_in_window must not produce NaN/inf");
    }

    #[test]
    fn trace_counts_messages_without_dropping_zero_message_loop_tokens() {
        let tailer = UsageTailer::new();
        let now = now_ms();
        *tailer.events.lock() = vec![
            UsageEvent {
                ts_ms: now,
                client: "grok".to_string(),
                agent: "grok".to_string(),
                model: "grok-build".to_string(),
                input: 10,
                output: 2,
                reasoning: 0,
                cache_read: 3,
                cache_write: 0,
                message_count: 1,
            },
            UsageEvent {
                ts_ms: now,
                client: "grok".to_string(),
                agent: "grok".to_string(),
                model: "grok-build".to_string(),
                input: 0,
                output: 0,
                reasoning: 30,
                cache_read: 0,
                cache_write: 0,
                message_count: 0,
            },
        ];

        let buckets = tailer.trace(3600);
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].tokens, 45);
        assert_eq!(buckets[0].messages, 1);
        assert_eq!(tailer.window_total(3600), 45);
    }

    #[test]
    fn trace_saturates_across_overlarge_events_in_same_bucket() {
        let tailer = UsageTailer::new();
        let now = now_ms();
        *tailer.events.lock() = vec![
            overlarge_event(now, "antigravity-cli"),
            overlarge_event(now, "antigravity-cli"),
        ];

        let buckets = tailer.trace(3600);
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].tokens, i64::MAX);
        assert_eq!(buckets[0].messages, 2);
    }
}
