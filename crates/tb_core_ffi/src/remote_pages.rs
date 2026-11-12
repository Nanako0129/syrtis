//! The four dashboard payloads (graph, models, hourly, agents) with the usage
//! of paired producers folded in: the `tb_*_devices` entry points.
//!
//! Where each number comes from:
//! - the local part is exactly what the existing report modules produce
//!   (`usage_graph`, `model_report`, `hourly_report`, `agents_report`), from
//!   the engine; with only the local source selected, the existing entry
//!   point's body runs unchanged and nothing remote starts;
//! - each producer's part is computed on the producer by the same engine and
//!   arrives as validated records through `Runtime::pull` /
//!   `Runtime::last_good` (one selected producer at a time, never
//!   `Agent::pull_all`, which pulls every peer and returns them summed);
//! - the producers' parts are summed by the agent's frozen `merge::combine`.
//!
//! This module checks each producer's records against the request (dates and
//! hour buckets inside the asked range, record counts, memory), converts the
//! combined records into payload rows, adds them to the local payload with
//! integer saturating arithmetic, recomputes the derived values (graph
//! intensity, summary and years; model throughput; totals), and reports one
//! `devices` row per selected source. Nothing here is cached: no remote data
//! reaches `GRAPH_CACHE` or any snapshot.

use std::{
    collections::{BTreeSet, HashMap},
    sync::{LazyLock, Mutex, PoisonError},
    time::Duration,
};

use chrono::{DateTime, Days, NaiveDate};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use syrtis_agent::{
    merge::{classify, combine, FailureClass, PartStatus, PeerData, PeerPart, Report},
    peer_store::PeerRecord,
    report::{
        decode_response, AgentsRecord, Endpoint, GraphRecord, HourlyRecord, ModelsRecord,
        ReportRecord, MAX_RANGE_DAYS,
    },
    report_peer::{decode_reservation, Records, ReportQuery},
    security::{decode_fixed_hex, MemoryLedger, MemoryReservation},
};

use crate::remote_agent::RemoteAgent;

/// Most producers one call may select.
pub(crate) const PEERS_MAX: usize = 16;
/// Most records one producer may contribute to one call; more and its whole
/// part is left out.
pub(crate) const PEER_RECORDS_MAX: usize = 32_768;
/// Most records all selected producers together may contribute; the producer
/// whose part would pass it is left out whole.
pub(crate) const TOTAL_RECORDS_MAX: usize = 131_072;

/// One `refresh` at a time: the four pages queue here rather than compete
/// for the runtime's two fetch slots.
static SERIAL: Mutex<()> = Mutex::new(());
/// The memory charged for decoding cached bodies and for holding each
/// producer's records until the fold ends. The runtime's own ledger is
/// private to it, so this module keeps one per process with the same cap.
static LEDGER: LazyLock<MemoryLedger> = LazyLock::new(MemoryLedger::default);

/// `devices_json`: which sources to fold.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Selection {
    pub local: bool,
    /// Producer statics, in the caller's order.
    pub peers: Vec<[u8; 32]>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionJson {
    local: bool,
    peers: Vec<String>,
}

/// `{"local": bool, "peers": ["<64 lowercase hex>", …]}`: at least one
/// source, at most [`PEERS_MAX`] peers, none twice. Anything else `invalid`.
pub(crate) fn parse_selection(text: &str) -> Result<Selection, String> {
    let parsed: SelectionJson = serde_json::from_str(text).map_err(|_| "invalid")?;
    if parsed.peers.len() > PEERS_MAX || (!parsed.local && parsed.peers.is_empty()) {
        return Err("invalid".into());
    }
    let mut peers = Vec::with_capacity(parsed.peers.len());
    for peer in &parsed.peers {
        let key = decode_fixed_hex::<32>(peer).map_err(|_| "invalid")?;
        if peers.contains(&key) {
            return Err("invalid".into());
        }
        peers.push(key);
    }
    Ok(Selection {
        local: parsed.local,
        peers,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Mode {
    /// The local part and each producer's last good body; no connection.
    Cached,
    /// Pull each producer; a failure falls back as `merge::classify` says.
    Refresh,
}

pub(crate) fn parse_mode(text: &str) -> Result<Mode, String> {
    match text {
        "cached" => Ok(Mode::Cached),
        "refresh" => Ok(Mode::Refresh),
        _ => Err("invalid".into()),
    }
}

/// The remote date range of a page for `year` (empty: all time) and whether
/// it is narrower than the local one: a year is that calendar year; all time
/// is the [`MAX_RANGE_DAYS`] days up to `today` included (the end is
/// tomorrow, exclusive).
pub(crate) fn remote_range(year: &str, today: NaiveDate) -> Result<(String, String, bool), String> {
    let span = Days::new(MAX_RANGE_DAYS as u64);
    let (start, end, limited) = match crate::usage_graph::normalize_year(year)? {
        Some(year) => {
            let year: i32 = year.parse().map_err(|_| "invalid")?;
            let start = NaiveDate::from_ymd_opt(year, 1, 1).ok_or("invalid")?;
            let end = NaiveDate::from_ymd_opt(year + 1, 1, 1).ok_or("invalid")?;
            (start, end, false)
        }
        None => {
            let end = today.checked_add_days(Days::new(1)).ok_or("invalid")?;
            (end.checked_sub_days(span).ok_or("invalid")?, end, true)
        }
    };
    Ok((start.to_string(), end.to_string(), limited))
}

/// Whether the engine's remote query validation accepts `query` (its bundled
/// tzdb knows the timezone, the dates and clients are well formed): the same
/// check a producer runs first, run here before anything is sent.
pub(crate) fn query_accepted(query: &ReportQuery) -> bool {
    tokscale_core::aggregate_remote_usage_v1(
        &[],
        &tokscale_core::RemoteUsageQueryV1 {
            clients: query.clients.clone(),
            start_date: query.start_date.clone(),
            end_date_exclusive: query.end_date_exclusive.clone(),
            timezone: query.timezone.clone(),
            tzdb_revision: tokscale_core::REMOTE_TZDB_REVISION.to_owned(),
        },
    )
    .is_ok()
}

// ------------------------------------------------------------- gather ---

/// What one selected producer gave before validation.
pub(crate) enum Got {
    Fresh(Report),
    /// The last good body, decoded (`None`: none, or no memory to decode).
    Cached(Option<Report>),
    /// Refused or invalid reply: no data.
    Failed(FailureClass),
    /// Another record with the same host and port is newer (deferred item 3).
    Superseded,
    /// The query is not one the engine accepts.
    Incompatible,
    /// No agent, or the peer store could not be read.
    Unavailable,
}

pub(crate) struct Pulled {
    pub peer: [u8; 32],
    pub grant: [u8; 16],
    pub label: Option<String>,
    pub got: Got,
    /// [`RemoteAgent::last_success_at`] after the pull.
    pub last_success: Option<i64>,
}

/// The remote side of a page.
pub(crate) struct Remote {
    pub records: Option<Records>,
    pub devices: Vec<Value>,
    pub saturated: bool,
}

fn fingerprint(key: &[u8; 32]) -> String {
    syrtis_agent::security::hex_encode(&Sha256::digest(key)[..8])
}

/// The producer's address as the receiver stored it at pairing (the
/// producer cannot change it later).
fn label(record: &PeerRecord) -> String {
    if record.host.contains(':') {
        format!("[{}]:{}", record.host, record.port)
    } else {
        format!("{}:{}", record.host, record.port)
    }
}

fn device(id: String, label: Option<String>, state: &str, last_success: Option<i64>) -> Value {
    json!({"id": id, "label": label, "state": state, "lastSuccessAt": last_success})
}

pub(crate) fn local_device() -> Value {
    device("local".into(), None, "local", None)
}

/// Pulls (or reads the last good body of) every selected producer, one at a
/// time, then [`assemble`]. Only `refresh` takes [`SERIAL`]: a `cached` call
/// connects to nothing and must not wait behind a pull.
pub(crate) fn gather(
    agent: &RemoteAgent,
    peers: &[[u8; 32]],
    mode: Mode,
    query: &ReportQuery,
) -> Remote {
    if peers.is_empty() {
        return Remote {
            records: None,
            devices: Vec::new(),
            saturated: false,
        };
    }
    let _serial =
        (mode == Mode::Refresh).then(|| SERIAL.lock().unwrap_or_else(PoisonError::into_inner));
    let records = agent.peer_records();
    let accepted = query_accepted(query);
    let mut pulled = Vec::with_capacity(peers.len());
    for peer in peers {
        let record = records
            .as_ref()
            .ok()
            .and_then(|all| all.iter().find(|r| r.responder_static == *peer));
        let got = match &records {
            Err(_) => Got::Unavailable,
            Ok(_) if !accepted => Got::Incompatible,
            Ok(all) if record.is_some_and(|record| superseded(record, all)) => Got::Superseded,
            Ok(_) => fetch(agent, peer, mode, query),
        };
        pulled.push(Pulled {
            peer: *peer,
            grant: record.map_or([0; 16], |record| record.grant_id),
            label: record.map(label),
            got,
            last_success: agent.last_success_at(peer, query),
        });
    }
    let mut remote = assemble(query, pulled);
    if records.is_err() {
        // Deferred item (2): an unreadable peer store is said, not shown as
        // "no devices".
        remote
            .devices
            .insert(0, device("peers".into(), None, "unavailable", None));
    }
    remote
}

/// Deferred item (3): a record is superseded when another record names the
/// same host and port and was created later (the producer reset its
/// identity and was paired again). Its grant may still be active and its
/// cache still hold up to 30 days, so it is neither asked nor read.
fn superseded(record: &PeerRecord, all: &[PeerRecord]) -> bool {
    all.iter().any(|other| {
        other.responder_static != record.responder_static
            && other.host == record.host
            && other.port == record.port
            && other.created_at_ms > record.created_at_ms
    })
}

fn fetch(agent: &RemoteAgent, peer: &[u8; 32], mode: Mode, query: &ReportQuery) -> Got {
    let cached = || {
        Got::Cached(
            agent
                .last_good_peer(peer, query)
                .and_then(|body| decode(query.endpoint, body.body())),
        )
    };
    match mode {
        Mode::Cached => cached(),
        Mode::Refresh => match agent.pull_peer(peer, query) {
            None => Got::Unavailable,
            Some(Ok(fetched)) => Got::Fresh(Report {
                records: fetched.records,
                saturated: fetched.saturated,
            }),
            Some(Err(error)) => match classify(error) {
                FailureClass::NoAnswer => cached(),
                class => Got::Failed(class),
            },
        },
    }
}

/// A cached body, decoded by the frozen decoder after its decode cost is
/// reserved; `None` when the reservation or the decode fails.
fn decode(endpoint: Endpoint, body: &[u8]) -> Option<Report> {
    fn typed<R: ReportRecord>(body: &[u8], wrap: fn(Vec<R>) -> Records) -> Option<Report> {
        let response = decode_response::<R>(body, None).ok()?;
        Some(Report {
            records: wrap(response.records),
            saturated: response.saturated,
        })
    }
    let cost = decode_reservation(body, endpoint).ok()?;
    let _reservation = LEDGER.reserve(cost)?;
    match endpoint {
        Endpoint::Graph => typed::<GraphRecord>(body, Records::Graph),
        Endpoint::Models => typed::<ModelsRecord>(body, Records::Models),
        Endpoint::Hourly => typed::<HourlyRecord>(body, Records::Hourly),
        Endpoint::Agents => typed::<AgentsRecord>(body, Records::Agents),
    }
}

/// Why a producer's part was left out before `combine`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Refusal {
    /// Records outside the asked range, or not of the endpoint.
    OutOfRange,
    /// Over [`PEER_RECORDS_MAX`] / [`TOTAL_RECORDS_MAX`], or no memory.
    Limit,
}

/// The pure half of [`gather`]: checks each producer's records against
/// `query`, the record limits and the memory ledger, hands the parts that
/// pass to `merge::combine`, and builds the `devices` rows in the order of
/// `pulled`.
pub(crate) fn assemble(query: &ReportQuery, pulled: Vec<Pulled>) -> Remote {
    let mut total = 0usize;
    let mut held: Vec<MemoryReservation> = Vec::new();
    let mut refused: HashMap<[u8; 32], Refusal> = HashMap::new();
    let mut parts = Vec::new();
    /// A `devices` row before the merge outcome is known.
    struct Row {
        peer: [u8; 32],
        label: Option<String>,
        fixed: Option<&'static str>,
        last_success: Option<i64>,
    }
    let mut rows = Vec::with_capacity(pulled.len());
    for item in pulled {
        let fixed = match &item.got {
            Got::Superseded => Some("stale"),
            Got::Incompatible => Some("incompatible"),
            Got::Unavailable => Some("unavailable"),
            _ => None,
        };
        rows.push(Row {
            peer: item.peer,
            label: item.label.clone(),
            fixed,
            last_success: item.last_success,
        });
        let data = match item.got {
            Got::Fresh(report) => PeerData::Fresh(report),
            Got::Cached(Some(report)) => PeerData::LastGood(report),
            Got::Cached(None) => PeerData::None,
            Got::Failed(FailureClass::InvalidReply) => PeerData::InvalidReply,
            Got::Failed(_) => PeerData::Refused,
            Got::Superseded | Got::Incompatible | Got::Unavailable => continue,
        };
        if let PeerData::Fresh(report) | PeerData::LastGood(report) = &data {
            match admit(query, &report.records, &mut total) {
                Ok(reservation) => held.push(reservation),
                Err(refusal) => {
                    refused.insert(item.peer, refusal);
                    continue;
                }
            }
        }
        parts.push(PeerPart {
            producer_static: item.peer,
            grant_id: item.grant,
            data,
            error: None,
        });
    }
    let combined = combine(query.endpoint, None, parts);
    let status: HashMap<[u8; 32], PartStatus> = combined
        .peers
        .iter()
        .map(|outcome| (outcome.producer_static, outcome.status))
        .collect();
    let devices = rows
        .into_iter()
        .map(
            |Row {
                 peer,
                 label,
                 fixed,
                 last_success,
             }| {
                let state = fixed.unwrap_or_else(|| match refused.get(&peer) {
                    Some(Refusal::OutOfRange) => "invalidReply",
                    Some(Refusal::Limit) => "unavailable",
                    None => state_of(status.get(&peer).copied(), last_success),
                });
                let last_success = if state == "stale" && fixed.is_none() {
                    None
                } else {
                    last_success
                };
                device(fingerprint(&peer), label, state, last_success)
            },
        )
        .collect();
    drop(held);
    Remote {
        records: combined.records,
        devices,
        saturated: combined.saturated,
    }
}

/// The fixed state of a part `combine` considered. `LastGood` with no known
/// fresh time in this process (a body cached before a restart) is `stale`,
/// never `lastGood` or `fresh`.
pub(crate) fn state_of(status: Option<PartStatus>, last_success: Option<i64>) -> &'static str {
    match status {
        Some(PartStatus::Fresh) => "fresh",
        Some(PartStatus::LastGood) if last_success.is_some() => "lastGood",
        Some(PartStatus::LastGood) => "stale",
        Some(PartStatus::Refused) => "refused",
        Some(PartStatus::InvalidReply) | Some(PartStatus::Invalid) => "invalidReply",
        Some(PartStatus::Unavailable) | Some(PartStatus::Duplicate) | None => "unavailable",
    }
}

/// One producer's records against the request (the frozen record validation
/// does not check them against it): every graph date in
/// `[startDate, endDateExclusive)`; every hour bucket's local wall time
/// ([`local_ms`], the time its slot is built from) inside that range's
/// milliseconds, so a producer cannot move its numbers into slots of another
/// date with an offset (the frozen validation does not bound
/// `utcOffsetSeconds`); then the record limits; then a reservation for
/// holding them.
///
/// No slack is needed: the engine buckets a message by its local date in
/// the query timezone, keeps it only when that date is in the range, and
/// takes as the bucket start an instant whose local date and hour are the
/// message's and whose offset is the one it reports (`bucket_start` in
/// `tokscale-core/src/remote_report.rs`), so a correct record's local wall
/// time is on a requested date.
fn admit(
    query: &ReportQuery,
    records: &Records,
    total: &mut usize,
) -> Result<MemoryReservation, Refusal> {
    let count = match records {
        Records::Graph(records) => {
            let inside = |date: &str| {
                date >= query.start_date.as_str() && date < query.end_date_exclusive.as_str()
            };
            if !records.iter().all(|record| inside(&record.date)) {
                return Err(Refusal::OutOfRange);
            }
            records.len()
        }
        Records::Hourly(records) => {
            let window = day_ms(&query.start_date)
                .zip(day_ms(&query.end_date_exclusive))
                .ok_or(Refusal::OutOfRange)?;
            let inside = |record: &HourlyRecord| {
                local_ms(record).is_some_and(|local| (window.0..window.1).contains(&local))
                    && hour_slot(record).is_some()
            };
            if !records.iter().all(inside) {
                return Err(Refusal::OutOfRange);
            }
            records.len()
        }
        Records::Models(records) => records.len(),
        Records::Agents(records) => records.len(),
    };
    let next = total.checked_add(count).ok_or(Refusal::Limit)?;
    if count > PEER_RECORDS_MAX || next > TOTAL_RECORDS_MAX {
        return Err(Refusal::Limit);
    }
    let reservation = LEDGER.reserve(held_bytes(records)).ok_or(Refusal::Limit)?;
    *total = next;
    Ok(reservation)
}

/// UTC midnight of a `YYYY-MM-DD` date, in Unix milliseconds.
fn day_ms(date: &str) -> Option<i64> {
    NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .ok()?
        .and_hms_opt(0, 0, 0)?
        .and_utc()
        .timestamp_millis()
        .into()
}

/// What holding `records` costs: each record and its strings.
fn held_bytes(records: &Records) -> usize {
    fn text(value: &Option<String>) -> usize {
        value.as_ref().map_or(0, String::len)
    }
    let (count, size, strings) = match records {
        Records::Graph(r) => (
            r.len(),
            std::mem::size_of::<GraphRecord>(),
            r.iter()
                .map(|r| r.date.len() + r.client.len() + text(&r.model) + text(&r.provider))
                .sum::<usize>(),
        ),
        Records::Models(r) => (
            r.len(),
            std::mem::size_of::<ModelsRecord>(),
            r.iter()
                .map(|r| r.client.len() + text(&r.model) + text(&r.provider))
                .sum(),
        ),
        Records::Hourly(r) => (
            r.len(),
            std::mem::size_of::<HourlyRecord>(),
            r.iter()
                .map(|r| r.client.len() + text(&r.model) + text(&r.provider))
                .sum(),
        ),
        Records::Agents(r) => (
            r.len(),
            std::mem::size_of::<AgentsRecord>(),
            r.iter()
                .map(|r| r.agent.len() + r.client.len() + text(&r.model) + text(&r.provider))
                .sum(),
        ),
    };
    count.saturating_mul(size).saturating_add(strings)
}

/// The local `"YYYY-MM-DD HH:00"` slot of an hour bucket in the query's
/// timezone: the producer computed `bucketStartUnixMs` and the offset in that
/// timezone, so their sum is the local wall time. Checked: `None` on any
/// overflow.
pub(crate) fn hour_slot(record: &HourlyRecord) -> Option<String> {
    let local = local_ms(record)?;
    Some(
        DateTime::from_timestamp_millis(local)?
            .naive_utc()
            .format("%Y-%m-%d %H:00")
            .to_string(),
    )
}

/// `bucketStartUnixMs + utcOffsetSeconds`: the bucket's local wall time as
/// if it were UTC, in milliseconds. Checked: `None` on any overflow.
fn local_ms(record: &HourlyRecord) -> Option<i64> {
    let offset_ms = i64::from(record.utc_offset_seconds).checked_mul(1_000)?;
    record.bucket_start_unix_ms.checked_add(offset_ms)
}

// --------------------------------------------------------- arithmetic ---

fn to_i64(value: u64, saturated: &mut bool) -> i64 {
    i64::try_from(value).unwrap_or_else(|_| {
        *saturated = true;
        i64::MAX
    })
}

fn add_i64(target: &mut i64, value: u64, saturated: &mut bool) {
    let value = to_i64(value, saturated);
    *target = target.checked_add(value).unwrap_or_else(|| {
        *saturated = true;
        i64::MAX
    });
}

fn add_i32(target: &mut i32, value: u64, saturated: &mut bool) {
    let value = i32::try_from(value).unwrap_or_else(|_| {
        *saturated = true;
        i32::MAX
    });
    *target = target.checked_add(value).unwrap_or_else(|| {
        *saturated = true;
        i32::MAX
    });
}

/// `costNanoUsd` in US dollars: the one conversion to floating point, after
/// every integer sum.
fn usd(nano: u64) -> f64 {
    nano as f64 / 1e9
}

/// `None` and the empty string are the same model or provider: the local
/// reports carry an absent one as `""`.
fn text(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or("")
}

// --------------------------------------------------------------- graph ---

/// The engine's intensity rule (`calculate_intensities` in
/// `tokscale-core/src/aggregator.rs`, private there): each day's cost
/// against the highest day's, in quarters. Pinned to the engine by the
/// parity test.
pub(crate) fn intensities(days: &mut [tokscale_core::DailyContribution]) {
    let max_cost = days.iter().map(|day| day.totals.cost).fold(0.0, f64::max);
    if max_cost == 0.0 {
        return;
    }
    for day in days.iter_mut() {
        let ratio = day.totals.cost / max_cost;
        day.intensity = if ratio >= 0.75 {
            4
        } else if ratio >= 0.5 {
            3
        } else if ratio >= 0.25 {
            2
        } else if ratio > 0.0 {
            1
        } else {
            0
        };
    }
}

/// An empty engine graph (no local source selected).
pub(crate) fn empty_graph() -> tokscale_core::GraphResult {
    tokscale_core::GraphResult {
        meta: tokscale_core::GraphMeta {
            generated_at: chrono::Utc::now().to_rfc3339(),
            version: String::new(),
            date_range_start: String::new(),
            date_range_end: String::new(),
            processing_time_ms: 0,
        },
        summary: tokscale_core::calculate_summary(&[]),
        years: Vec::new(),
        contributions: Vec::new(),
        time_metrics: None,
    }
}

/// Adds graph records to the day they name: the row of the same client,
/// model and provider (or a new row), the day's breakdown, totals and turns
/// (or a new day); then intensity, summary, years and the date range again.
pub(crate) fn fold_graph(
    graph: &mut tokscale_core::GraphResult,
    records: &[GraphRecord],
    saturated: &mut bool,
) {
    let days = &mut graph.contributions;
    let mut index: HashMap<String, usize> = days
        .iter()
        .enumerate()
        .map(|(at, day)| (day.date.clone(), at))
        .collect();
    // (day, client, model, provider) -> row, so a day with many rows is not
    // scanned once per record.
    let mut rows: HashMap<(usize, String, String, String), usize> = HashMap::new();
    for (at, day) in days.iter().enumerate() {
        for (row, entry) in day.clients.iter().enumerate() {
            rows.entry((
                at,
                entry.client.clone(),
                entry.model_id.clone(),
                entry.provider_id.clone(),
            ))
            .or_insert(row);
        }
    }
    for record in records {
        let at = *index.entry(record.date.clone()).or_insert_with(|| {
            days.push(tokscale_core::DailyContribution {
                date: record.date.clone(),
                totals: Default::default(),
                intensity: 0,
                token_breakdown: Default::default(),
                clients: Vec::new(),
                active_time_ms: None,
                turns_by_client: Default::default(),
            });
            days.len() - 1
        });
        let day = &mut days[at];
        let (model, provider) = (text(&record.model), text(&record.provider));
        let key = (
            at,
            record.client.clone(),
            model.to_owned(),
            provider.to_owned(),
        );
        let row = match rows.get(&key) {
            Some(row) => *row,
            None => {
                rows.insert(key, day.clients.len());
                day.clients.push(tokscale_core::ClientContribution {
                    client: record.client.clone(),
                    model_id: model.to_owned(),
                    provider_id: provider.to_owned(),
                    tokens: Default::default(),
                    cost: 0.0,
                    messages: 0,
                });
                day.clients.len() - 1
            }
        };
        let cost = usd(record.cost_nano_usd);
        for tokens in [&mut day.clients[row].tokens, &mut day.token_breakdown] {
            add_i64(&mut tokens.input, record.input_tokens, saturated);
            add_i64(&mut tokens.output, record.output_tokens, saturated);
            add_i64(&mut tokens.cache_read, record.cache_read_tokens, saturated);
            add_i64(
                &mut tokens.cache_write,
                record.cache_write_tokens,
                saturated,
            );
            add_i64(&mut tokens.reasoning, record.reasoning_tokens, saturated);
        }
        day.clients[row].cost += cost;
        add_i32(
            &mut day.clients[row].messages,
            record.message_count,
            saturated,
        );
        add_i64(&mut day.totals.tokens, record.total_tokens, saturated);
        day.totals.cost += cost;
        add_i32(&mut day.totals.messages, record.message_count, saturated);
        if record.turn_count > 0 {
            add_i64(
                day.turns_by_client
                    .entry(record.client.clone())
                    .or_insert(0),
                record.turn_count,
                saturated,
            );
        }
    }
    days.sort_by(|a, b| a.date.cmp(&b.date));
    intensities(days);
    graph.summary = tokscale_core::calculate_summary(&graph.contributions);
    graph.years = tokscale_core::calculate_years(&graph.contributions);
    graph.meta.date_range_start = graph
        .contributions
        .first()
        .map(|d| d.date.clone())
        .unwrap_or_default();
    graph.meta.date_range_end = graph
        .contributions
        .last()
        .map(|d| d.date.clone())
        .unwrap_or_default();
}

// -------------------------------------------------------------- models ---

/// The engine's model order: cost descending, NaN last.
fn by_cost(a: f64, b: f64) -> std::cmp::Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => std::cmp::Ordering::Equal,
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        (false, false) => b.partial_cmp(&a).unwrap_or(std::cmp::Ordering::Equal),
    }
}

/// Adds model records to the row of the same client, model and provider (or
/// a new row), keeping the duration and timed tokens until after the sum so
/// `msPer1kTokens` is the merged rate; re-sorts and recomputes the totals.
/// Returns, per entry in the new order, whether it holds any remote part:
/// those rows get no `costEstimate` (it would price only the local tokens).
pub(crate) fn fold_models(
    report: &mut tokscale_core::ModelReport,
    records: &[ModelsRecord],
    saturated: &mut bool,
) -> Vec<bool> {
    let mut rows: Vec<(tokscale_core::ModelUsage, bool)> = report
        .entries
        .drain(..)
        .map(|entry| (entry, false))
        .collect();
    let mut index: HashMap<(String, String, String), usize> = HashMap::new();
    for (at, (row, _)) in rows.iter().enumerate() {
        index
            .entry((row.client.clone(), row.model.clone(), row.provider.clone()))
            .or_insert(at);
    }
    for record in records {
        let (model, provider) = (text(&record.model), text(&record.provider));
        let key = (record.client.clone(), model.to_owned(), provider.to_owned());
        let at = match index.get(&key) {
            Some(at) => *at,
            None => {
                index.insert(key, rows.len());
                rows.push((
                    tokscale_core::ModelUsage {
                        client: record.client.clone(),
                        merged_clients: None,
                        workspace_key: None,
                        workspace_label: None,
                        session_id: None,
                        model: model.to_owned(),
                        provider: provider.to_owned(),
                        input: 0,
                        output: 0,
                        cache_read: 0,
                        cache_write: 0,
                        reasoning: 0,
                        message_count: 0,
                        cost: 0.0,
                        performance: Default::default(),
                    },
                    true,
                ));
                rows.len() - 1
            }
        };
        let (row, touched) = &mut rows[at];
        *touched = true;
        add_i64(&mut row.input, record.input_tokens, saturated);
        add_i64(&mut row.output, record.output_tokens, saturated);
        add_i64(&mut row.cache_read, record.cache_read_tokens, saturated);
        add_i64(&mut row.cache_write, record.cache_write_tokens, saturated);
        add_i64(&mut row.reasoning, record.reasoning_tokens, saturated);
        add_i32(&mut row.message_count, record.message_count, saturated);
        row.cost += usd(record.cost_nano_usd);
        let performance = &mut row.performance;
        add_i64(
            &mut performance.total_duration_ms,
            record.duration_millis,
            saturated,
        );
        add_i64(
            &mut performance.timed_tokens,
            record.timed_tokens,
            saturated,
        );
        add_i32(
            &mut performance.sample_count,
            record.sample_count,
            saturated,
        );
        // The engine's `ModelPerformance::finalize` rate.
        performance.ms_per_1k_tokens =
            (performance.timed_tokens > 0 && performance.total_duration_ms > 0).then(|| {
                performance.total_duration_ms as f64 * 1000.0 / performance.timed_tokens as f64
            });
    }
    rows.sort_by(|(a, _), (b, _)| by_cost(a.cost, b.cost));
    let touched = rows.iter().map(|(_, touched)| *touched).collect();
    report.entries = rows.into_iter().map(|(row, _)| row).collect();
    let entries = &report.entries;
    let sum = |field: fn(&tokscale_core::ModelUsage) -> i64| {
        entries
            .iter()
            .fold(0i64, |total, entry| total.saturating_add(field(entry)))
    };
    report.total_input = sum(|e| e.input);
    report.total_output = sum(|e| e.output);
    report.total_cache_read = sum(|e| e.cache_read);
    report.total_cache_write = sum(|e| e.cache_write);
    report.total_messages = entries
        .iter()
        .map(|e| e.message_count)
        .fold(0, i32::saturating_add);
    report.total_cost = entries.iter().map(|e| e.cost).sum::<f64>() + 0.0;
    touched
}

// -------------------------------------------------------------- hourly ---

/// Adds hourly records to their local hour slot (or a new slot): tokens,
/// counts and cost, and the slot's clients and models as sorted unions.
pub(crate) fn fold_hourly(
    report: &mut tokscale_core::HourlyReport,
    records: &[HourlyRecord],
    saturated: &mut bool,
) {
    let entries = &mut report.entries;
    let mut index: HashMap<String, usize> = HashMap::new();
    for (at, entry) in entries.iter().enumerate() {
        index.entry(entry.hour.clone()).or_insert(at);
    }
    // The clients and models of each touched slot, written out once, sorted.
    let mut sets: HashMap<usize, (BTreeSet<String>, BTreeSet<String>)> = HashMap::new();
    for record in records {
        // `admit` refused any record without a slot.
        let Some(hour) = hour_slot(record) else {
            continue;
        };
        let at = match index.get(&hour) {
            Some(at) => *at,
            None => {
                index.insert(hour.clone(), entries.len());
                entries.push(tokscale_core::HourlyUsage {
                    hour,
                    clients: Vec::new(),
                    models: Vec::new(),
                    input: 0,
                    output: 0,
                    cache_read: 0,
                    cache_write: 0,
                    message_count: 0,
                    turn_count: 0,
                    reasoning: 0,
                    cost: 0.0,
                });
                entries.len() - 1
            }
        };
        let entry = &mut entries[at];
        add_i64(&mut entry.input, record.input_tokens, saturated);
        add_i64(&mut entry.output, record.output_tokens, saturated);
        add_i64(&mut entry.cache_read, record.cache_read_tokens, saturated);
        add_i64(&mut entry.cache_write, record.cache_write_tokens, saturated);
        add_i64(&mut entry.reasoning, record.reasoning_tokens, saturated);
        add_i32(&mut entry.message_count, record.message_count, saturated);
        add_i32(&mut entry.turn_count, record.turn_count, saturated);
        entry.cost += usd(record.cost_nano_usd);
        let (clients, models) = sets.entry(at).or_insert_with(|| {
            (
                entry.clients.iter().cloned().collect(),
                entry.models.iter().cloned().collect(),
            )
        });
        clients.insert(record.client.clone());
        if let Some(model) = &record.model {
            models.insert(model.clone());
        }
    }
    for (at, (clients, models)) in sets {
        entries[at].clients = clients.into_iter().collect();
        entries[at].models = models.into_iter().collect();
    }
    entries.sort_by(|a, b| a.hour.cmp(&b.hour));
    report.total_cost = entries.iter().map(|e| e.cost).sum::<f64>() + 0.0;
}

// -------------------------------------------------------------- agents ---

/// Adds agent records to the entry of the same agent (or a new one), the
/// clients as a sorted union; re-sorts as the engine does (cost, then token
/// total, descending) and recomputes the totals.
pub(crate) fn fold_agents(
    report: &mut tokscale_core::AgentReport,
    records: &[AgentsRecord],
    saturated: &mut bool,
) {
    let entries = &mut report.entries;
    let mut index: HashMap<String, usize> = HashMap::new();
    for (at, entry) in entries.iter().enumerate() {
        index.entry(entry.agent.clone()).or_insert(at);
    }
    // The clients of each entry that gained one, written out once, sorted (an
    // entry that gained none keeps its order, as before).
    let mut sets: HashMap<usize, BTreeSet<String>> = HashMap::new();
    for record in records {
        let at = match index.get(&record.agent) {
            Some(at) => *at,
            None => {
                index.insert(record.agent.clone(), entries.len());
                entries.push(tokscale_core::AgentReportEntry {
                    agent: record.agent.clone(),
                    clients: Vec::new(),
                    input: 0,
                    output: 0,
                    cache_read: 0,
                    cache_write: 0,
                    reasoning: 0,
                    cost: 0.0,
                    messages: 0,
                });
                entries.len() - 1
            }
        };
        let entry = &mut entries[at];
        add_i64(&mut entry.input, record.input_tokens, saturated);
        add_i64(&mut entry.output, record.output_tokens, saturated);
        add_i64(&mut entry.cache_read, record.cache_read_tokens, saturated);
        add_i64(&mut entry.cache_write, record.cache_write_tokens, saturated);
        add_i64(&mut entry.reasoning, record.reasoning_tokens, saturated);
        add_i32(&mut entry.messages, record.message_count, saturated);
        entry.cost += usd(record.cost_nano_usd);
        let known = sets.get(&at).map_or_else(
            || entry.clients.contains(&record.client),
            |set| set.contains(&record.client),
        );
        if !known {
            sets.entry(at)
                .or_insert_with(|| entry.clients.iter().cloned().collect())
                .insert(record.client.clone());
        }
    }
    for (at, clients) in sets {
        entries[at].clients = clients.into_iter().collect();
    }
    let total = |e: &tokscale_core::AgentReportEntry| {
        e.input
            .saturating_add(e.output)
            .saturating_add(e.cache_read)
            .saturating_add(e.cache_write)
            .saturating_add(e.reasoning)
    };
    entries.sort_by(|a, b| {
        b.cost
            .partial_cmp(&a.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(total(b).cmp(&total(a)))
    });
    report.total_cost = entries.iter().map(|e| e.cost).sum::<f64>() + 0.0;
    report.total_messages = entries
        .iter()
        .map(|e| e.messages)
        .fold(0, i32::saturating_add);
}

// --------------------------------------------------------------- pages ---

/// The new keys on every device-aware payload.
fn decorate(
    mut data: Value,
    devices: Vec<Value>,
    sources: usize,
    saturated: bool,
    limited: bool,
) -> Result<Value, String> {
    let object = data.as_object_mut().ok_or("internal")?;
    object.insert("devices".into(), Value::Array(devices));
    object.insert("duplicateWarning".into(), Value::Bool(sources >= 2));
    object.insert("saturated".into(), Value::Bool(saturated));
    object.insert("remoteRangeLimited".into(), Value::Bool(limited));
    Ok(data)
}

/// The arguments of one device-aware page call.
pub(crate) struct Call<'a> {
    pub agent: &'a RemoteAgent,
    pub context: &'a crate::LocalSourceContext,
    pub year: &'a str,
    pub clients: Option<Vec<String>>,
    pub selection: Selection,
    pub timezone: &'a str,
    pub mode: Mode,
    pub today: NaiveDate,
}

impl Call<'_> {
    /// The query sent to each producer, with whether its range is narrower
    /// than the local one. Clients are sorted and unique, as the request
    /// requires.
    fn query(&self, endpoint: Endpoint) -> Result<(ReportQuery, bool), String> {
        let (start_date, end_date_exclusive, limited) = remote_range(self.year, self.today)?;
        let mut clients = self.clients.clone().unwrap_or_default();
        clients.sort();
        clients.dedup();
        Ok((
            ReportQuery {
                endpoint,
                start_date,
                end_date_exclusive,
                timezone: self.timezone.to_owned(),
                clients,
            },
            limited,
        ))
    }

    fn sources(&self) -> usize {
        usize::from(self.selection.local) + self.selection.peers.len()
    }

    fn devices(&self, remote: Vec<Value>) -> Vec<Value> {
        let mut devices = Vec::with_capacity(remote.len() + 1);
        if self.selection.local {
            devices.push(local_device());
        }
        devices.extend(remote);
        devices
    }

    /// Only the local source: the existing body, then the new keys.
    fn local_only(&self, data: Value) -> Result<Value, String> {
        decorate(data, self.devices(Vec::new()), 1, false, false)
    }

    /// The remote part of a page, when any producer is selected.
    fn remote(&self, endpoint: Endpoint) -> Result<(Remote, bool), String> {
        let (query, limited) = self.query(endpoint)?;
        Ok((
            gather(self.agent, &self.selection.peers, self.mode, &query),
            limited,
        ))
    }
}

pub(crate) fn graph(call: &Call<'_>) -> Result<Value, String> {
    if call.selection.peers.is_empty() {
        let data = match crate::graph_cached(
            call.year,
            Duration::from_secs(crate::ONESHOT_MAX_AGE_SECS),
        ) {
            Some(data) => data,
            None => crate::graph_shared(call.year, true)?,
        };
        return call.local_only(data);
    }
    let mut graph = if call.selection.local {
        crate::usage_graph::compute(call.context, call.year)?
    } else {
        crate::usage_graph::normalize_year(call.year)?;
        empty_graph()
    };
    let (remote, limited) = call.remote(Endpoint::Graph)?;
    let mut saturated = remote.saturated;
    if let Some(Records::Graph(records)) = &remote.records {
        fold_graph(&mut graph, records, &mut saturated);
    }
    let data = crate::usage_graph::to_payload(graph)?;
    decorate(
        data,
        call.devices(remote.devices),
        call.sources(),
        saturated,
        limited,
    )
}

pub(crate) fn models(call: &Call<'_>) -> Result<Value, String> {
    if call.selection.peers.is_empty() {
        return call.local_only(crate::model_report::run(call.context, call.year)?);
    }
    let mut report = if call.selection.local {
        crate::model_report::compute(call.context, call.year)?
    } else {
        crate::usage_graph::normalize_year(call.year)?;
        tokscale_core::ModelReport {
            entries: Vec::new(),
            total_input: 0,
            total_output: 0,
            total_cache_read: 0,
            total_cache_write: 0,
            total_messages: 0,
            total_cost: 0.0,
            processing_time_ms: 0,
        }
    };
    let (remote, limited) = call.remote(Endpoint::Models)?;
    let mut saturated = remote.saturated;
    let touched = match &remote.records {
        Some(Records::Models(records)) => fold_models(&mut report, records, &mut saturated),
        _ => vec![false; report.entries.len()],
    };
    let pricing = tokscale_core::pricing::PricingService::load_cached_any_age();
    let data = models_payload(report, &touched, pricing.as_ref())?;
    decorate(
        data,
        call.devices(remote.devices),
        call.sources(),
        saturated,
        limited,
    )
}

/// The mapped model report, where a row holding any remote part has no
/// `costEstimate`: the estimate prices only local tokens with the local
/// table, so `cost / costEstimate` would misread a summed row (and a
/// producer could make the owner's row look implausible).
pub(crate) fn models_payload(
    report: tokscale_core::ModelReport,
    touched: &[bool],
    pricing: Option<&tokscale_core::pricing::PricingService>,
) -> Result<Value, String> {
    let mut data = crate::model_report::map_report(report, pricing);
    for (entry, touched) in data.entries.iter_mut().zip(touched) {
        if *touched {
            entry.cost_estimate = None;
        }
    }
    serde_json::to_value(data).map_err(|e| format!("serialize model report: {e}"))
}

pub(crate) fn hourly(call: &Call<'_>) -> Result<Value, String> {
    if call.selection.peers.is_empty() {
        return call.local_only(crate::hourly_report::run(
            call.context,
            call.year,
            call.clients.clone(),
        )?);
    }
    let mut report = if call.selection.local {
        crate::hourly_report::compute(call.context, call.year, call.clients.clone())?
    } else {
        crate::usage_graph::normalize_year(call.year)?;
        tokscale_core::HourlyReport {
            entries: Vec::new(),
            total_cost: 0.0,
            processing_time_ms: 0,
        }
    };
    let (remote, limited) = call.remote(Endpoint::Hourly)?;
    let mut saturated = remote.saturated;
    if let Some(Records::Hourly(records)) = &remote.records {
        fold_hourly(&mut report, records, &mut saturated);
    }
    let data = serde_json::to_value(crate::hourly_report::map_report(report))
        .map_err(|e| format!("serialize hourly report: {e}"))?;
    decorate(
        data,
        call.devices(remote.devices),
        call.sources(),
        saturated,
        limited,
    )
}

pub(crate) fn agents(call: &Call<'_>) -> Result<Value, String> {
    if call.selection.peers.is_empty() {
        return call.local_only(crate::agents_report::run(
            call.context,
            call.year,
            call.clients.clone(),
        )?);
    }
    let mut report = if call.selection.local {
        crate::agents_report::compute(call.context, call.year, call.clients.clone())?
    } else {
        crate::usage_graph::normalize_year(call.year)?;
        tokscale_core::AgentReport {
            entries: Vec::new(),
            total_cost: 0.0,
            total_messages: 0,
            processing_time_ms: 0,
        }
    };
    let (remote, limited) = call.remote(Endpoint::Agents)?;
    let mut saturated = remote.saturated;
    if let Some(Records::Agents(records)) = &remote.records {
        fold_agents(&mut report, records, &mut saturated);
    }
    let data = serde_json::to_value(crate::agents_report::map_report(report))
        .map_err(|e| format!("serialize agents report: {e}"))?;
    decorate(
        data,
        call.devices(remote.devices),
        call.sources(),
        saturated,
        limited,
    )
}

/// Today in the process's timezone, which is the one the app passes as
/// `timezone` (`TimeZone.current`).
pub(crate) fn today() -> NaiveDate {
    chrono::Local::now().date_naive()
}

#[cfg(test)]
mod tests {
    //! Hermetic. Every agent lives in a temp home below `/tmp` created
    //! exclusively `0700` and removed by its exact path. The tests that read
    //! the app's own local sources (`LocalSourceContext::current`, the graph
    //! cache) run in an env-cleared child ([`in_child`]) whose `HOME`,
    //! `TOKSCALE_CONFIG_DIR` and `TZ` are fixtures; the pure tests build their
    //! inputs. Nothing opens the real agent root.

    use super::*;
    use crate::remote_agent::{Inputs, MakeInputs, OVERRIDE_SUFFIX};
    use std::{
        ffi::{c_char, CStr, CString},
        fs,
        net::TcpListener,
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicI64, AtomicUsize, Ordering},
            Arc,
        },
        time::{SystemTime, UNIX_EPOCH},
    };
    use syrtis_agent::{
        agent::ScannerSettings, pairing::Clock, peer_store::PeerState, secure_fs::StorageError,
        security::hex_encode,
    };

    // ----------------------------------------------------------- harness ---

    struct TestClock {
        os: AtomicI64,
        mono: Mutex<Duration>,
    }

    impl TestClock {
        fn new() -> Arc<Self> {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            Arc::new(Self {
                os: AtomicI64::new(now as i64),
                mono: Mutex::new(Duration::ZERO),
            })
        }

        fn advance(&self, seconds: u64) {
            *self.mono.lock().unwrap() += Duration::from_secs(seconds);
            self.os.fetch_add(seconds as i64, Ordering::SeqCst);
        }
    }

    impl Clock for TestClock {
        fn monotonic(&self) -> Duration {
            *self.mono.lock().unwrap()
        }
        fn os_now_seconds(&self) -> Result<i64, StorageError> {
            Ok(self.os.load(Ordering::SeqCst))
        }
        fn sleep(&self, duration: Duration) {
            self.advance(duration.as_secs().max(1));
        }
    }

    /// A short temp dir below the canonical `/tmp`, created exclusively and
    /// `0700`; only it is removed.
    struct TempHome(PathBuf);

    impl TempHome {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = fs::canonicalize("/tmp").unwrap().join(format!(
                "sa3b-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::os::unix::fs::DirBuilderExt::mode(&mut fs::DirBuilder::new(), 0o700)
                .create(&path)
                .unwrap();
            Self(path)
        }

        fn dir(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir_all(&path).unwrap();
            path
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const CHILD: &str = "TB_REMOTE_PAGES_CHILD_ROOT";

    fn child_root() -> Option<PathBuf> {
        std::env::var_os(CHILD).map(PathBuf::from)
    }

    /// True in the child. In the parent, re-runs `test` in an env-cleared
    /// child (`HOME`=`<root>/home`, `TOKSCALE_CONFIG_DIR`=`<root>/config`,
    /// `TZ`=UTC, cache-only pricing), asserts it passed, returns false.
    fn in_child(test: &str) -> bool {
        if child_root().is_some() {
            return true;
        }
        let root = TempHome::new();
        root.dir("home");
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("remote_pages::tests::{test}"),
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env(CHILD, &root.0)
            .env("HOME", root.0.join("home"))
            .env("TOKSCALE_CONFIG_DIR", root.0.join("config"))
            .env("TOKSCALE_PRICING_CACHE_ONLY", "1")
            .env("TZ", "UTC")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        // Measurements the child printed, passed on.
        for line in stdout
            .lines()
            .filter_map(|line| line.find("SA3B-").map(|at| &line[at..]))
        {
            println!("{line}");
        }
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "child failed: {}\n{stdout}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        false
    }

    const PERIOD: Duration = Duration::from_millis(100);

    fn make(home: &Path, clock: &Arc<TestClock>, settings: ScannerSettings) -> MakeInputs {
        let (home, clock) = (home.to_path_buf(), Arc::clone(clock));
        Arc::new(move || {
            Ok(Inputs {
                home: home.clone(),
                state_suffix: OVERRIDE_SUFFIX,
                config_dir: home.join(".config"),
                data_dir: home.join(".local/share"),
                settings: settings.clone(),
                pricing_source: home.join("pricing-src"),
                pricing_period: PERIOD,
                clock: Arc::clone(&clock) as Arc<dyn Clock>,
            })
        })
    }

    fn agent_at(home: &Path, clock: &Arc<TestClock>) -> Arc<RemoteAgent> {
        let agent = Arc::new(RemoteAgent::default());
        agent
            .start(make(home, clock, ScannerSettings::default()))
            .unwrap();
        agent
    }

    /// One Claude Code assistant line.
    fn claude(id: &str, at: &str, model: &str, input: u64, output: u64, sidechain: bool) -> String {
        let side = if sidechain {
            r#""isSidechain":true,"agentId":"x","#
        } else {
            ""
        };
        format!(
            r#"{{"type":"assistant",{side}"timestamp":"{at}","requestId":"req_{id}","message":{{"id":"msg_{id}","model":"{model}","usage":{{"input_tokens":{input},"output_tokens":{output}}}}}}}"#
        )
    }

    fn write_session(home: &Path, file: &str, lines: &[String]) {
        let dir = home.join(".claude/projects/fixture");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(file), lines.join("\n") + "\n").unwrap();
    }

    /// A producer with sharing on, an approved scope and a listener.
    fn producer(home: &Path, clock: &Arc<TestClock>) -> Arc<RemoteAgent> {
        let agent = agent_at(home, clock);
        agent
            .listen_configure("127.0.0.1", free_port(), None)
            .unwrap();
        let digest = agent.scope_disclosure().unwrap()["ownerInputsSha256"]
            .as_str()
            .unwrap()
            .to_owned();
        agent.scope_approve(&digest).unwrap();
        agent.sharing_set(true).unwrap();
        agent
    }

    /// Offer → claim → approve → poll; returns the producer static.
    fn pair(producer: &RemoteAgent, receiver: &RemoteAgent) -> [u8; 32] {
        let offer = producer.offer_create(600).unwrap();
        let claim = receiver
            .pair_claim(offer["pairUrl"].as_str().unwrap())
            .unwrap();
        let pending = producer.pending_list().unwrap()[0]["pendingId"]
            .as_str()
            .unwrap()
            .to_owned();
        producer.decide(true, &pending, 1).unwrap();
        let paired = receiver
            .pair_poll(claim["handle"].as_u64().unwrap())
            .unwrap();
        decode_fixed_hex::<32>(paired["producerStatic"].as_str().unwrap()).unwrap()
    }

    fn free_port() -> u16 {
        TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// A listener that counts nothing but whether anyone connected.
    fn trap() -> TcpListener {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        listener
    }

    fn connected(listener: &TcpListener) -> bool {
        listener.accept().is_ok()
    }

    /// A usable record in `receiver`'s peer store for a producer that does
    /// not exist, at `port`; `created_at_ms` as given.
    fn fake_peer(receiver: &RemoteAgent, key: u8, port: u16, created_at_ms: i64) -> [u8; 32] {
        fake_peer_as(receiver, [key; 32], key, port, created_at_ms)
    }

    /// [`fake_peer`] with a given responder static.
    fn fake_peer_as(
        receiver: &RemoteAgent,
        responder: [u8; 32],
        key: u8,
        port: u16,
        created_at_ms: i64,
    ) -> [u8; 32] {
        receiver
            .with_agent(|agent| {
                let identity = agent
                    .runtime()
                    .store()
                    .state()
                    .unwrap()
                    .identity_public()
                    .unwrap();
                agent
                    .runtime()
                    .peers()
                    .upsert(PeerRecord {
                        auth_epoch: 1,
                        created_at_ms,
                        expires_at_ms: i64::MAX / 2,
                        grant_id: [key; 16],
                        host: "127.0.0.1".into(),
                        initiator_static: identity,
                        node_label: None,
                        pending_id: [key; 16],
                        port,
                        responder_static: responder,
                        source_scope_generation: 1,
                        state: PeerState::Active,
                    })
                    .unwrap()
            })
            .unwrap();
        responder
    }

    fn call<'a>(
        agent: &'a RemoteAgent,
        context: &'a crate::LocalSourceContext,
        local: bool,
        peers: &[[u8; 32]],
        timezone: &'a str,
        mode: Mode,
    ) -> Call<'a> {
        Call {
            agent,
            context,
            year: "2025",
            clients: None,
            selection: Selection {
                local,
                peers: peers.to_vec(),
            },
            timezone,
            mode,
            today: NaiveDate::from_ymd_opt(2025, 6, 1).unwrap(),
        }
    }

    fn states(data: &Value) -> Vec<(String, String)> {
        data["devices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| {
                (
                    d["id"].as_str().unwrap().to_owned(),
                    d["state"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }

    fn day<'a>(graph: &'a Value, date: &str) -> &'a Value {
        graph["contributions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["date"] == date)
            .unwrap_or(&Value::Null)
    }

    fn find<'a>(page: &'a Value, key: &str, value: &str) -> &'a Value {
        page["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e[key] == value)
            .unwrap_or(&Value::Null)
    }

    // ------------------------------------------------- pure: arguments ---

    #[test]
    fn selection_and_mode_are_validated() {
        let a = "a".repeat(64);
        assert_eq!(
            parse_selection(&format!(r#"{{"local":true,"peers":["{a}"]}}"#)),
            Ok(Selection {
                local: true,
                peers: vec![[0xaa; 32]]
            })
        );
        for bad in [
            r#"{"local":false,"peers":[]}"#.to_owned(),
            format!(r#"{{"local":true,"peers":["{a}","{a}"]}}"#),
            format!(r#"{{"local":true,"peers":["{}"]}}"#, "A".repeat(64)),
            r#"{"local":true,"peers":["ab"]}"#.to_owned(),
            r#"{"local":true,"peers":[],"x":1}"#.to_owned(),
            format!(
                r#"{{"local":false,"peers":[{}]}}"#,
                (0..17)
                    .map(|i| format!("\"{}\"", hex_encode(&[i as u8; 32])))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        ] {
            assert_eq!(parse_selection(&bad), Err("invalid".into()), "{bad}");
        }
        assert_eq!(parse_mode("cached"), Ok(Mode::Cached));
        assert_eq!(parse_mode("refresh"), Ok(Mode::Refresh));
        assert_eq!(parse_mode("Refresh"), Err("invalid".into()));
    }

    /// Acceptance (6): all time asks the 370 days up to today and says the
    /// remote range is limited; a year asks that calendar year.
    #[test]
    fn all_time_asks_the_last_370_days_and_a_year_its_calendar_year() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 12).unwrap();
        let (start, end, limited) = remote_range("", today).unwrap();
        assert_eq!((end.as_str(), limited), ("2026-11-13", true));
        let days = NaiveDate::parse_from_str(&end, "%Y-%m-%d").unwrap()
            - NaiveDate::parse_from_str(&start, "%Y-%m-%d").unwrap();
        assert_eq!(days.num_days(), 370);
        assert_eq!(
            remote_range("2024", today).unwrap(),
            ("2024-01-01".into(), "2025-01-01".into(), false)
        );
        assert!(remote_range("24", today).is_err());
    }

    #[test]
    fn the_engine_query_check_refuses_an_unknown_timezone() {
        let query = |timezone: &str| ReportQuery {
            endpoint: Endpoint::Graph,
            start_date: "2025-01-01".into(),
            end_date_exclusive: "2026-01-01".into(),
            timezone: timezone.into(),
            clients: Vec::new(),
        };
        assert!(query_accepted(&query("UTC")));
        assert!(query_accepted(&query("Asia/Tokyo")));
        assert!(!query_accepted(&query("Not/AZone")));
    }

    // ---------------------------------------------- pure: state and fold ---

    /// Acceptance (1c), the fixed mapping row by row.
    #[test]
    fn state_mapping_is_fixed() {
        assert_eq!(state_of(Some(PartStatus::Fresh), Some(1)), "fresh");
        assert_eq!(state_of(Some(PartStatus::LastGood), Some(1)), "lastGood");
        assert_eq!(state_of(Some(PartStatus::LastGood), None), "stale");
        assert_eq!(state_of(Some(PartStatus::Unavailable), None), "unavailable");
        assert_eq!(state_of(Some(PartStatus::Refused), Some(1)), "refused");
        assert_eq!(
            state_of(Some(PartStatus::InvalidReply), None),
            "invalidReply"
        );
        assert_eq!(state_of(Some(PartStatus::Invalid), None), "invalidReply");
    }

    fn graph_record(date: &str, input: u64, cost: u64) -> GraphRecord {
        GraphRecord {
            date: date.into(),
            client: "claude".into(),
            model: Some("m".into()),
            provider: Some("p".into()),
            input_tokens: input,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            total_tokens: input,
            message_count: 1,
            turn_count: 1,
            cost_nano_usd: cost,
        }
    }

    fn hourly_record(bucket: i64, offset: i32, input: u64) -> HourlyRecord {
        HourlyRecord {
            bucket_start_unix_ms: bucket,
            utc_offset_seconds: offset,
            client: "claude".into(),
            model: Some("m".into()),
            provider: Some("p".into()),
            input_tokens: input,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            total_tokens: input,
            message_count: 1,
            turn_count: 0,
            cost_nano_usd: 0,
        }
    }

    fn graph_query() -> ReportQuery {
        ReportQuery {
            endpoint: Endpoint::Graph,
            start_date: "2025-01-01".into(),
            end_date_exclusive: "2026-01-01".into(),
            timezone: "UTC".into(),
            clients: Vec::new(),
        }
    }

    fn pulled(key: u8, records: Records) -> Pulled {
        Pulled {
            peer: [key; 32],
            grant: [key; 16],
            label: Some(format!("10.0.0.{key}:4000")),
            got: Got::Fresh(Report {
                records,
                saturated: false,
            }),
            last_success: Some(7),
        }
    }

    /// Acceptance (4) and (9b): a producer whose records lie outside the asked
    /// range (a date of another year, an hour bucket near the i64 limit) is
    /// left out whole and marked `invalidReply`; the others are summed.
    #[test]
    fn a_producer_with_records_outside_the_range_is_left_out_alone() {
        let good = Records::Graph(vec![graph_record("2025-03-10", 5, 0)]);
        let also = Records::Graph(vec![graph_record("2025-03-10", 7, 0)]);
        // Sorted as the frozen validation requires, so only the range check
        // can refuse it.
        let outside = Records::Graph(vec![
            graph_record("2024-12-31", 1, 0),
            graph_record("2025-03-10", 1, 0),
        ]);
        assert!(
            syrtis_agent::report::validate_graph_records(match &outside {
                Records::Graph(r) => r,
                _ => unreachable!(),
            })
            .is_ok()
        );
        let remote = assemble(
            &graph_query(),
            vec![pulled(1, good), pulled(2, outside), pulled(3, also)],
        );
        assert_eq!(
            remote.records,
            Some(Records::Graph(
                vec![graph_record("2025-03-10", 12, 0)]
                    .into_iter()
                    .map(|mut r| {
                        r.message_count = 2;
                        r.turn_count = 2;
                        r
                    })
                    .collect()
            ))
        );
        let rows: Vec<_> = remote
            .devices
            .iter()
            .map(|d| d["state"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(rows, ["fresh", "invalidReply", "fresh"]);
        assert_eq!(remote.devices[0]["id"], fingerprint(&[1; 32]));
        assert_eq!(remote.devices[0]["label"], "10.0.0.1:4000");
        assert_eq!(remote.devices[0]["lastSuccessAt"], 7);

        let hourly = ReportQuery {
            endpoint: Endpoint::Hourly,
            ..graph_query()
        };
        let march = day_ms("2025-03-10").unwrap();
        let ok = Records::Hourly(vec![hourly_record(march, 0, 1)]);
        let extreme = Records::Hourly(vec![hourly_record(i64::MAX - 1, 3_600, 1)]);
        let early = Records::Hourly(vec![hourly_record(
            day_ms("2025-01-01").unwrap() - 3_600_000,
            0,
            1,
        )]);
        // 2025-12-31 23:00 at UTC-8: the bucket itself is past the range end,
        // its local wall time is not.
        let edge = Records::Hourly(vec![hourly_record(
            day_ms("2026-01-01").unwrap() + 7 * 3_600_000,
            -28_800,
            1,
        )]);
        let remote = assemble(
            &hourly,
            vec![
                pulled(1, ok),
                pulled(2, extreme),
                pulled(3, early),
                pulled(4, edge),
            ],
        );
        let rows: Vec<_> = remote
            .devices
            .iter()
            .map(|d| d["state"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(rows, ["fresh", "invalidReply", "invalidReply", "fresh"]);
        let Some(Records::Hourly(kept)) = remote.records else {
            panic!()
        };
        assert_eq!(kept.len(), 2);
    }

    /// Review fix 1: a bucket inside the range with an offset of a year moves
    /// that producer's numbers into the owner's older local slots; the
    /// producer is left out whole, the local slots and the other producer are
    /// unchanged.
    #[test]
    fn an_in_range_bucket_with_an_offset_of_a_year_is_left_out() {
        let hourly = ReportQuery {
            endpoint: Endpoint::Hourly,
            ..graph_query()
        };
        let five = day_ms("2025-03-10").unwrap() + 5 * 3_600_000;
        let shifted = Records::Hourly(vec![hourly_record(five, -365 * 86_400, 1_000)]);
        assert_eq!(
            hour_slot(&hourly_record(five, -365 * 86_400, 1)).unwrap(),
            "2024-03-10 05:00",
            "control: the offset alone moves the slot a year back"
        );
        let honest = Records::Hourly(vec![hourly_record(five, 0, 1)]);
        let remote = assemble(&hourly, vec![pulled(1, shifted), pulled(2, honest)]);
        let rows: Vec<_> = remote
            .devices
            .iter()
            .map(|d| d["state"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(rows, ["invalidReply", "fresh"]);
        let Some(Records::Hourly(kept)) = remote.records else {
            panic!()
        };
        let slot = |hour: &str, input| tokscale_core::HourlyUsage {
            hour: hour.into(),
            clients: vec!["codex".into()],
            models: vec![],
            input,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            message_count: 1,
            turn_count: 0,
            reasoning: 0,
            cost: 0.0,
        };
        let mut local = tokscale_core::HourlyReport {
            entries: vec![slot("2024-03-10 05:00", 100), slot("2025-03-10 05:00", 5)],
            total_cost: 0.0,
            processing_time_ms: 0,
        };
        fold_hourly(&mut local, &kept, &mut false);
        let inputs: Vec<_> = local
            .entries
            .iter()
            .map(|e| (e.hour.as_str(), e.input))
            .collect();
        assert_eq!(inputs, [("2024-03-10 05:00", 100), ("2025-03-10 05:00", 6)]);
    }

    /// Acceptance (9b): the record limits leave the producer out whole.
    #[test]
    fn a_producer_over_the_record_limits_is_left_out_whole() {
        let many = |count: usize| {
            Records::Graph(
                (0..count)
                    .map(|i| {
                        let mut r = graph_record("2025-03-10", 1, 0);
                        r.client = format!("c{i:06}");
                        r
                    })
                    .collect(),
            )
        };
        let remote = assemble(
            &graph_query(),
            vec![pulled(1, many(PEER_RECORDS_MAX + 1)), pulled(2, many(3))],
        );
        let rows: Vec<_> = remote
            .devices
            .iter()
            .map(|d| d["state"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(rows, ["unavailable", "fresh"]);
        let Some(Records::Graph(kept)) = remote.records else {
            panic!()
        };
        assert_eq!(kept.len(), 3);

        // Four at the per-producer cap fill the total; the fifth is out.
        let parts = (1..=5).map(|k| pulled(k, many(PEER_RECORDS_MAX))).collect();
        let remote = assemble(&graph_query(), parts);
        let rows: Vec<_> = remote
            .devices
            .iter()
            .map(|d| d["state"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(rows, ["fresh", "fresh", "fresh", "fresh", "unavailable"]);
    }

    /// Acceptance (8): sums that pass the payload's integer width saturate
    /// and say so.
    #[test]
    fn sums_past_the_payload_width_saturate_and_are_flagged() {
        let mut graph = empty_graph();
        let mut saturated = false;
        let mut big = graph_record("2025-03-10", (i64::MAX / 2) as u64, 0);
        big.message_count = u64::from(u32::MAX);
        fold_graph(&mut graph, &[big], &mut saturated);
        assert!(saturated, "a count above i32 must flag");
        let day = &graph.contributions[0];
        assert_eq!(day.token_breakdown.input, i64::MAX / 2);
        assert_eq!(day.totals.messages, i32::MAX);

        let mut saturated = false;
        fold_graph(
            &mut empty_graph(),
            &[graph_record("2025-03-10", u64::MAX, 0)],
            &mut saturated,
        );
        assert!(saturated, "a u64 above i64 must flag");

        let mut saturated = false;
        let mut near = graph_record("2025-03-10", (i64::MAX - 10) as u64, 0);
        near.message_count = 0;
        near.turn_count = 0;
        fold_graph(&mut graph, &[near], &mut saturated);
        assert!(saturated, "two near-limit values must flag");
        assert_eq!(graph.contributions[0].token_breakdown.input, i64::MAX);

        let mut saturated = false;
        fold_graph(
            &mut empty_graph(),
            &[graph_record("2025-03-10", 1, 0)],
            &mut saturated,
        );
        assert!(!saturated, "a small sum must not flag");
    }

    /// Acceptance (1e): the intensity copy gives, day by day, the engine's
    /// own intensity on local data (`aggregate_by_date` runs the engine's
    /// private rule). The costs sit on and around each quarter.
    #[test]
    fn intensity_matches_the_engine() {
        let costs = [
            0.0, 0.1, 0.24, 0.25, 0.26, 0.49, 0.5, 0.51, 0.74, 0.75, 0.76, 1.0,
        ];
        let messages = costs
            .iter()
            .enumerate()
            .map(|(day, cost)| {
                tokscale_core::UnifiedMessage::new(
                    "claude",
                    "m",
                    "p",
                    format!("s{day}"),
                    1_735_689_600_000 + day as i64 * 86_400_000,
                    tokscale_core::TokenBreakdown {
                        input: 1,
                        ..Default::default()
                    },
                    *cost,
                )
            })
            .collect();
        let engine = tokscale_core::aggregate_by_date(messages);
        assert_eq!(engine.len(), costs.len());
        let mut ours = engine.clone();
        for day in &mut ours {
            day.intensity = 9;
        }
        intensities(&mut ours);
        let pairs: Vec<_> = engine
            .iter()
            .zip(&ours)
            .map(|(e, o)| (e.intensity, o.intensity))
            .collect();
        assert!(pairs.iter().all(|(e, o)| e == o), "{pairs:?}");
        // Every level occurs, so a changed threshold shows.
        for level in 1..=4u8 {
            assert!(engine.iter().any(|d| d.intensity == level), "level {level}");
        }
    }

    fn model_usage(model: &str, input: i64, cost: f64) -> tokscale_core::ModelUsage {
        tokscale_core::ModelUsage {
            client: "opencode".into(),
            merged_clients: None,
            workspace_key: None,
            workspace_label: None,
            session_id: None,
            model: model.into(),
            provider: "deepseek".into(),
            input,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
            message_count: 1,
            cost,
            performance: tokscale_core::ModelPerformance {
                ms_per_1k_tokens: Some(1000.0),
                total_duration_ms: 1_000,
                timed_tokens: 1_000,
                sample_count: 1,
                token_coverage: 1.0,
            },
        }
    }

    fn models_record(model: &str, input: u64, cost_nano: u64) -> ModelsRecord {
        ModelsRecord {
            client: "opencode".into(),
            model: Some(model.into()),
            provider: Some("deepseek".into()),
            input_tokens: input,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            total_tokens: input,
            message_count: 1,
            turn_count: 0,
            cost_nano_usd: cost_nano,
            duration_millis: 3_000,
            timed_tokens: 1_000,
            sample_count: 1,
        }
    }

    /// Acceptance (1d): a local row with an estimate summed with a remote row
    /// of the same client, model and provider that carries a very high cost
    /// has no `costEstimate` (Swift's `implausibleCostRatio` then has no
    /// denominator); a local-only row keeps its estimate. The merged
    /// throughput is the merged rate.
    #[test]
    fn a_row_with_a_remote_part_has_no_cost_estimate() {
        let mut litellm = std::collections::HashMap::new();
        for model in ["m", "other"] {
            litellm.insert(
                model.to_string(),
                tokscale_core::pricing::litellm::ModelPricing {
                    input_cost_per_token: Some(1e-6),
                    ..Default::default()
                },
            );
        }
        let pricing = tokscale_core::pricing::PricingService::new(litellm, Default::default());
        let mut report = tokscale_core::ModelReport {
            entries: vec![
                model_usage("m", 1_000_000, 1.0),
                model_usage("other", 1_000_000, 0.5),
            ],
            total_input: 0,
            total_output: 0,
            total_cache_read: 0,
            total_cache_write: 0,
            total_messages: 0,
            total_cost: 0.0,
            processing_time_ms: 0,
        };
        let mut saturated = false;
        let touched = fold_models(
            &mut report,
            &[models_record("m", 1_000, 5_000_000_000_000)],
            &mut saturated,
        );
        let mut data = models_payload(report, &touched, Some(&pricing)).unwrap();
        let merged = find(&data, "model", "m").clone();
        assert_eq!(merged["costEstimate"], Value::Null);
        assert_eq!(merged["input"], 1_001_000);
        assert_eq!(merged["cost"], 5001.0);
        // (1000 + 3000) ms over (1000 + 1000) timed tokens.
        assert_eq!(merged["msPer1kTokens"], 2000.0);
        assert_eq!(find(&data, "model", "other")["costEstimate"], 1.0);
        assert_eq!(data["totalInput"], 2_001_000);
        assert_eq!(data["entries"][0]["model"], "m", "re-sorted by cost");
        data.as_object_mut().unwrap().remove("pricingUpdatedAt");
        assert!(!saturated);
    }

    #[test]
    fn hour_slots_are_the_local_wall_hour_of_the_query_timezone() {
        // 2025-03-12T20:00Z in UTC+9 is 05:00 the next day.
        let utc = day_ms("2025-03-12").unwrap() + 20 * 3_600_000;
        assert_eq!(
            hour_slot(&hourly_record(utc, 32_400, 1)).unwrap(),
            "2025-03-13 05:00"
        );
        assert_eq!(
            hour_slot(&hourly_record(utc, 0, 1)).unwrap(),
            "2025-03-12 20:00"
        );
        assert_eq!(hour_slot(&hourly_record(i64::MAX, 1, 1)), None);
    }

    #[test]
    fn agents_and_hourly_fold_by_their_keys() {
        let mut agents = tokscale_core::AgentReport {
            entries: vec![tokscale_core::AgentReportEntry {
                agent: "Main".into(),
                clients: vec!["codex".into()],
                input: 10,
                output: 0,
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
        let record = |agent: &str, input| AgentsRecord {
            agent: agent.into(),
            client: "claude".into(),
            model: None,
            provider: None,
            input_tokens: input,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            total_tokens: input,
            message_count: 2,
            turn_count: 0,
            cost_nano_usd: 0,
        };
        let mut saturated = false;
        fold_agents(
            &mut agents,
            &[record("Main", 5), record("reviewer", 100)],
            &mut saturated,
        );
        assert_eq!(
            agents.entries[0].agent, "reviewer",
            "re-sorted by tokens at equal cost"
        );
        let main = agents.entries.iter().find(|e| e.agent == "Main").unwrap();
        assert_eq!((main.input, main.messages), (15, 3));
        assert_eq!(main.clients, ["claude", "codex"]);
        assert_eq!(agents.total_messages, 5);
    }

    /// Review fix 3: one producer's 32,768 distinct records under one day,
    /// one model key space, one hour slot and one agent fold in time that
    /// grows about linearly. Bound: each page under `FOLD_BOUND`; the timings
    /// are printed (`SA3B-FOLD`).
    #[test]
    fn folds_of_32768_distinct_records_are_not_quadratic() {
        const FOLD_BOUND: Duration = Duration::from_secs(2);
        let n = PEER_RECORDS_MAX;
        let client = |i: usize| format!("c{i:06}");
        let graph_records: Vec<_> = (0..n)
            .map(|i| {
                let mut r = graph_record("2025-03-10", 1, 0);
                r.client = client(i);
                r
            })
            .collect();
        let model_records: Vec<_> = (0..n)
            .map(|i| {
                let mut r = models_record("m", 1, 0);
                r.client = client(i);
                r
            })
            .collect();
        let bucket = day_ms("2025-03-10").unwrap();
        let hourly_records: Vec<_> = (0..n)
            .map(|i| {
                let mut r = hourly_record(bucket, 0, 1);
                r.client = client(i);
                r.model = Some(format!("m{i:06}"));
                r
            })
            .collect();
        let agent_records: Vec<_> = (0..n)
            .map(|i| AgentsRecord {
                agent: "Main".into(),
                client: client(i),
                model: None,
                provider: None,
                input_tokens: 1,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
                total_tokens: 1,
                message_count: 1,
                turn_count: 0,
                cost_nano_usd: 0,
            })
            .collect();
        let mut saturated = false;
        let started = std::time::Instant::now();
        let mut graph = empty_graph();
        fold_graph(&mut graph, &graph_records, &mut saturated);
        let graph_time = started.elapsed();
        let started = std::time::Instant::now();
        let mut models = tokscale_core::ModelReport {
            entries: Vec::new(),
            total_input: 0,
            total_output: 0,
            total_cache_read: 0,
            total_cache_write: 0,
            total_messages: 0,
            total_cost: 0.0,
            processing_time_ms: 0,
        };
        fold_models(&mut models, &model_records, &mut saturated);
        let models_time = started.elapsed();
        let started = std::time::Instant::now();
        let mut hourly = tokscale_core::HourlyReport {
            entries: Vec::new(),
            total_cost: 0.0,
            processing_time_ms: 0,
        };
        fold_hourly(&mut hourly, &hourly_records, &mut saturated);
        let hourly_time = started.elapsed();
        let started = std::time::Instant::now();
        let mut agents = tokscale_core::AgentReport {
            entries: Vec::new(),
            total_cost: 0.0,
            total_messages: 0,
            processing_time_ms: 0,
        };
        fold_agents(&mut agents, &agent_records, &mut saturated);
        let agents_time = started.elapsed();
        println!(
            "SA3B-FOLD graph={graph_time:?} models={models_time:?} hourly={hourly_time:?} agents={agents_time:?}"
        );
        // Control: every record reached its page.
        assert_eq!(graph.contributions[0].clients.len(), n);
        assert_eq!(models.entries.len(), n);
        assert_eq!(
            (
                hourly.entries.len(),
                hourly.entries[0].clients.len(),
                hourly.entries[0].models.len()
            ),
            (1, n, n)
        );
        assert_eq!(
            (agents.entries.len(), agents.entries[0].clients.len()),
            (1, n)
        );
        assert!(hourly.entries[0].clients.windows(2).all(|w| w[0] < w[1]));
        assert!(agents.entries[0].clients.windows(2).all(|w| w[0] < w[1]));
        for (page, time) in [
            ("graph", graph_time),
            ("models", models_time),
            ("hourly", hourly_time),
            ("agents", agents_time),
        ] {
            assert!(time < FOLD_BOUND, "{page} fold took {time:?}");
        }
    }

    // ------------------------------------------------------ integration ---

    /// Acceptance (1): with only the local source, each new entry's data is
    /// the existing entry's, byte for byte, apart from the four new keys;
    /// the new keys say one local device and nothing else.
    #[test]
    fn local_only_is_the_existing_output_byte_for_byte() {
        if !in_child("local_only_is_the_existing_output_byte_for_byte") {
            return;
        }
        let root = child_root().unwrap();
        write_session(
            &root.join("home"),
            "r.jsonl",
            &[
                claude(
                    "l1",
                    "2025-03-10T05:30:00.000Z",
                    "claude-3-5-sonnet",
                    100,
                    10,
                    false,
                ),
                claude(
                    "l2",
                    "2025-03-11T06:00:00.000Z",
                    "claude-3-5-sonnet",
                    50,
                    5,
                    false,
                ),
            ],
        );
        fn take(raw: *mut c_char) -> String {
            let text = unsafe { CStr::from_ptr(raw) }.to_str().unwrap().to_owned();
            unsafe { crate::tb_free(raw) };
            text
        }
        let c = |text: &str| CString::new(text).unwrap();
        let (year, devices, tz, mode) = (
            c("2025"),
            c(r#"{"local":true,"peers":[]}"#),
            c("UTC"),
            c("refresh"),
        );
        let clients = c("claude");
        use crate::remote_pages_ffi as new;
        let pairs = unsafe {
            [
                (
                    take(crate::tb_graph(year.as_ptr())),
                    take(new::tb_graph_devices(
                        year.as_ptr(),
                        devices.as_ptr(),
                        tz.as_ptr(),
                        mode.as_ptr(),
                    )),
                ),
                (
                    take(crate::tb_model_report(year.as_ptr())),
                    take(new::tb_model_report_devices(
                        year.as_ptr(),
                        devices.as_ptr(),
                        tz.as_ptr(),
                        mode.as_ptr(),
                    )),
                ),
                (
                    take(crate::tb_hourly_report(year.as_ptr(), clients.as_ptr())),
                    take(new::tb_hourly_report_devices(
                        year.as_ptr(),
                        clients.as_ptr(),
                        devices.as_ptr(),
                        tz.as_ptr(),
                        mode.as_ptr(),
                    )),
                ),
                (
                    take(crate::tb_agents_report(year.as_ptr(), clients.as_ptr())),
                    take(new::tb_agents_report_devices(
                        year.as_ptr(),
                        clients.as_ptr(),
                        devices.as_ptr(),
                        tz.as_ptr(),
                        mode.as_ptr(),
                    )),
                ),
            ]
        };
        for (old, new) in pairs {
            let old: Value = serde_json::from_str(&old).unwrap();
            let mut new: Value = serde_json::from_str(&new).unwrap();
            assert_eq!(old["ok"], true, "{old}");
            let data = new["data"].as_object_mut().unwrap();
            assert_eq!(
                data.remove("devices"),
                Some(
                    json!([{"id": "local", "label": null, "state": "local", "lastSuccessAt": null}])
                )
            );
            assert_eq!(data.remove("duplicateWarning"), Some(json!(false)));
            assert_eq!(data.remove("saturated"), Some(json!(false)));
            assert_eq!(data.remove("remoteRangeLimited"), Some(json!(false)));
            assert_eq!(old["data"].to_string(), new["data"].to_string());
            // Control: the fixture reached the report.
            assert!(old["data"].to_string().contains("claude"), "{old}");
        }
        let bad = c(r#"{"local":false,"peers":[]}"#);
        let reply = take(unsafe {
            new::tb_graph_devices(year.as_ptr(), bad.as_ptr(), tz.as_ptr(), mode.as_ptr())
        });
        assert_eq!(reply, r#"{"err":"invalid","ok":false}"#);
    }

    /// Acceptance (2), (3), (5), (7), (1b), (1c), (4), (9c), (9e): two producers
    /// paired with a receiver, through the page functions.
    #[test]
    fn producers_fold_into_the_four_pages() {
        if !in_child("producers_fold_into_the_four_pages") {
            return;
        }
        let root = child_root().unwrap();
        let clock = TestClock::new();
        let context = crate::LocalSourceContext::current();
        let sonnet = "claude-3-5-sonnet";
        write_session(
            &root.join("home"),
            "r.jsonl",
            &[
                claude("l1", "2025-03-10T05:30:00.000Z", sonnet, 100, 10, false),
                claude("l2", "2025-03-11T06:00:00.000Z", sonnet, 50, 5, false),
            ],
        );
        let (a_home, b_home, r_home) = (root.join("pa"), root.join("pb"), root.join("r"));
        write_session(
            &a_home,
            "a.jsonl",
            &[
                claude("a1", "2025-03-10T05:45:00.000Z", sonnet, 7, 3, false),
                claude(
                    "a2",
                    "2025-03-12T20:30:00.000Z",
                    "claude-3-opus",
                    11,
                    1,
                    false,
                ),
            ],
        );
        write_session(
            &a_home,
            "agent-x.jsonl",
            &[claude(
                "a3",
                "2025-03-12T09:00:00.000Z",
                sonnet,
                13,
                2,
                true,
            )],
        );
        write_session(
            &b_home,
            "b.jsonl",
            &[claude(
                "b1",
                "2025-03-10T05:50:00.000Z",
                sonnet,
                1000,
                100,
                false,
            )],
        );
        for home in [&a_home, &b_home, &r_home] {
            fs::create_dir_all(home).unwrap();
        }
        let (a, b) = (producer(&a_home, &clock), producer(&b_home, &clock));
        let receiver = agent_at(&r_home, &clock);
        let (ka, kb) = (pair(&a, &receiver), pair(&b, &receiver));
        let unselected = trap();
        let kc = fake_peer(&receiver, 0xcc, unselected.local_addr().unwrap().port(), 1);

        // Local only, for the sums below.
        let local_graph =
            graph(&call(&receiver, &context, true, &[], "UTC", Mode::Cached)).unwrap();
        let local_models =
            models(&call(&receiver, &context, true, &[], "UTC", Mode::Cached)).unwrap();
        let local_hourly =
            hourly(&call(&receiver, &context, true, &[], "UTC", Mode::Cached)).unwrap();
        let local_agents =
            agents(&call(&receiver, &context, true, &[], "UTC", Mode::Cached)).unwrap();
        assert_eq!(
            day(&local_graph, "2025-03-10")["tokenBreakdown"]["input"],
            100,
            "control: local fixture read"
        );

        // (1b) Only A selected: A's numbers, not B's; the unselected fake
        // peer gets no connection.
        let page =
            |f: fn(&Call<'_>) -> Result<Value, String>, local, peers: &[[u8; 32]], tz, mode| {
                if mode == Mode::Refresh {
                    clock.advance(61);
                }
                f(&call(&receiver, &context, local, peers, tz, mode)).unwrap()
            };
        let g = page(graph, true, &[ka], "UTC", Mode::Refresh);
        assert_eq!(
            states(&g),
            [
                ("local".into(), "local".into()),
                (fingerprint(&ka), "fresh".into())
            ]
        );
        assert_eq!(g["duplicateWarning"], true, "(5) two sources");
        assert_eq!(g["remoteRangeLimited"], false);
        assert!(g["devices"][1]["lastSuccessAt"].is_i64());
        assert_eq!(
            g["devices"][1]["label"],
            format!("127.0.0.1:{}", a.status().unwrap()["bindPort"])
        );
        // (2) overlapping day and row: local + A; A-only day; local-only day.
        let d10 = day(&g, "2025-03-10");
        assert_eq!(d10["tokenBreakdown"]["input"], 107);
        assert_eq!(d10["totals"]["messages"], 2);
        let row = d10["clients"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["client"] == "claude")
            .unwrap();
        assert_eq!(row["tokens"]["input"], 107, "{d10}");
        assert_eq!(day(&g, "2025-03-12")["tokenBreakdown"]["input"], 11 + 13);
        assert_eq!(day(&g, "2025-03-11")["tokenBreakdown"]["input"], 50);
        assert_eq!(
            g["summary"]["totalTokens"],
            local_graph["summary"]["totalTokens"].as_i64().unwrap() + 7 + 3 + 11 + 1 + 13 + 2
        );
        assert!(g["summary"]["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m == "claude-3-opus"));
        assert!(
            !connected(&unselected),
            "(9e) the unselected peer got a connection"
        );

        let m = page(models, true, &[ka], "UTC", Mode::Refresh);
        let local_sonnet = find(&local_models, "model", sonnet);
        let merged = find(&m, "model", sonnet);
        assert_eq!(
            merged["input"],
            local_sonnet["input"].as_i64().unwrap() + 7 + 13,
            "{m}"
        );
        assert_eq!(find(&m, "model", "claude-3-opus")["input"], 11);
        assert_eq!(merged["costEstimate"], Value::Null);

        let h = page(hourly, true, &[ka], "UTC", Mode::Refresh);
        assert_eq!(find(&h, "hour", "2025-03-10 05:00")["input"], 107);
        assert_eq!(find(&h, "hour", "2025-03-12 20:00")["input"], 11);
        assert_eq!(find(&h, "hour", "2025-03-11 06:00")["input"], 50);
        assert_eq!(
            find(&local_hourly, "hour", "2025-03-10 05:00")["input"],
            100
        );

        let ag = page(agents, true, &[ka], "UTC", Mode::Refresh);
        let main = find(&ag, "agent", "Main");
        assert_eq!(
            main["input"],
            find(&local_agents, "agent", "Main")["input"]
                .as_i64()
                .unwrap()
                + 7
                + 11
        );
        let sub: Vec<_> = ag["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["agent"] != "Main")
            .collect();
        assert_eq!(sub.len(), 1, "{ag}");
        assert_eq!(sub[0]["input"], 13, "A's sub-agent, absent locally");
        assert!(!connected(&unselected));

        // (3) Only B, no local: B alone.
        let only_b = page(graph, false, &[kb], "UTC", Mode::Refresh);
        assert_eq!(states(&only_b), [(fingerprint(&kb), "fresh".into())]);
        assert_eq!(only_b["duplicateWarning"], false, "(5) one source");
        assert_eq!(only_b["contributions"].as_array().unwrap().len(), 1);
        assert_eq!(day(&only_b, "2025-03-10")["tokenBreakdown"]["input"], 1000);
        // Local + A + B.
        let all = page(hourly, true, &[ka, kb], "UTC", Mode::Refresh);
        assert_eq!(find(&all, "hour", "2025-03-10 05:00")["input"], 1107);

        // (7) Asia/Tokyo: A's 20:30Z message lands at 05:00 the next day.
        let tokyo = page(hourly, false, &[ka], "Asia/Tokyo", Mode::Refresh);
        assert_eq!(
            find(&tokyo, "hour", "2025-03-13 05:00")["input"],
            11,
            "{tokyo}"
        );
        assert_eq!(find(&tokyo, "hour", "2025-03-10 14:00")["input"], 7);
        let tokyo_graph = page(graph, false, &[ka], "Asia/Tokyo", Mode::Refresh);
        assert_eq!(
            day(&tokyo_graph, "2025-03-13")["tokenBreakdown"]["input"],
            11
        );

        // (1c) incompatible: nothing sent, local intact.
        let incompatible = page(graph, true, &[ka, kc], "Not/AZone", Mode::Refresh);
        assert_eq!(
            states(&incompatible)[1..],
            [
                (fingerprint(&ka), "incompatible".into()),
                (fingerprint(&kc), "incompatible".into())
            ]
        );
        assert!(!connected(&unselected), "an incompatible query was sent");
        assert_eq!(
            day(&incompatible, "2025-03-10")["tokenBreakdown"]["input"],
            100
        );

        // (1c) unavailable: cached mode, a query never fetched (all time).
        let mut never = call(&receiver, &context, false, &[ka], "UTC", Mode::Cached);
        never.year = "";
        let never = graph(&never).unwrap();
        assert_eq!(states(&never), [(fingerprint(&ka), "unavailable".into())]);
        assert_eq!(never["remoteRangeLimited"], true, "(6)");

        // (4) A refused (revoked), B fresh, local intact.
        let grant = a.grants().unwrap()[0]["grantId"]
            .as_str()
            .unwrap()
            .to_owned();
        a.revoke(&grant).unwrap();
        let refused = page(graph, true, &[ka, kb], "UTC", Mode::Refresh);
        assert_eq!(
            states(&refused)[1..],
            [
                (fingerprint(&ka), "refused".into()),
                (fingerprint(&kb), "fresh".into())
            ]
        );
        assert_eq!(day(&refused, "2025-03-10")["tokenBreakdown"]["input"], 1100);

        // (2) cached after B stops: B's last good body, `lastGood`, no
        // connection to the selected fake peer either.
        b.stop().unwrap();
        let cached = page(hourly, true, &[kb, kc], "UTC", Mode::Cached);
        assert_eq!(
            states(&cached)[1..],
            [
                (fingerprint(&kb), "lastGood".into()),
                (fingerprint(&kc), "unavailable".into())
            ]
        );
        assert_eq!(find(&cached, "hour", "2025-03-10 05:00")["input"], 1100);
        // A body never fetched (B's models) is `unavailable`.
        let never_models = page(models, true, &[kb], "UTC", Mode::Cached);
        assert_eq!(
            states(&never_models)[1..],
            [(fingerprint(&kb), "unavailable".into())]
        );
        assert_eq!(
            find(&never_models, "model", sonnet)["input"],
            local_sonnet["input"]
        );
        assert!(!connected(&unselected), "cached mode connected");
        // Refresh with B stopped: no answer, so the last good body.
        let fallback = page(graph, false, &[kb], "UTC", Mode::Refresh);
        assert_eq!(states(&fallback), [(fingerprint(&kb), "lastGood".into())]);
        assert_eq!(
            day(&fallback, "2025-03-10")["tokenBreakdown"]["input"],
            1000
        );

        // (9c) After a restart of the process (a new RemoteAgent on the same
        // root), the cached body has no known age: `stale`, null time.
        drop(receiver);
        let restarted = agent_at(&r_home, &clock);
        let stale = graph(&call(
            &restarted,
            &context,
            false,
            &[kb],
            "UTC",
            Mode::Cached,
        ))
        .unwrap();
        assert_eq!(
            stale["devices"],
            json!([{"id": fingerprint(&kb), "label": format!("127.0.0.1:{}", stale["devices"][0]["label"].as_str().unwrap().rsplit(':').next().unwrap()), "state": "stale", "lastSuccessAt": null}])
        );
        assert_eq!(day(&stale, "2025-03-10")["tokenBreakdown"]["input"], 1000);
        drop(a);
    }

    /// Review fix 2: a producer that completes the handshake and then never
    /// answers holds a `refresh` in its fetch. Meanwhile a `cached` call for
    /// the same page and a management call (`peer_remove`) both return
    /// within `BOUND`; the refresh is still waiting when they do.
    #[test]
    fn a_silent_producer_blocks_neither_cached_calls_nor_management() {
        if !in_child("a_silent_producer_blocks_neither_cached_calls_nor_management") {
            return;
        }
        const BOUND: Duration = Duration::from_secs(2);
        let root = child_root().unwrap();
        let clock = TestClock::new();
        let r_home = root.join("r");
        fs::create_dir_all(&r_home).unwrap();
        let receiver = agent_at(&r_home, &clock);
        let key = syrtis_agent::noise::StaticKey::generate().unwrap();
        let silent = *key.public();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (handshook, handshake) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let session = syrtis_agent::noise::accept(stream, &key);
            handshook.send(session.is_ok()).unwrap();
            let _ = released.recv_timeout(Duration::from_secs(60));
            drop(session);
        });
        fake_peer_as(&receiver, silent, 0x51, port, 1);
        let other = fake_peer(&receiver, 0x52, free_port(), 1);

        let pulling = Arc::clone(&receiver);
        let refresh = std::thread::spawn(move || {
            let context = crate::LocalSourceContext::current();
            graph(&call(
                &pulling,
                &context,
                false,
                &[silent],
                "UTC",
                Mode::Refresh,
            ))
            .unwrap()
        });
        assert!(
            handshake.recv_timeout(Duration::from_secs(10)).unwrap(),
            "the refresh did not complete the handshake"
        );
        let context = crate::LocalSourceContext::current();
        let started = std::time::Instant::now();
        let cached = graph(&call(
            &receiver,
            &context,
            false,
            &[silent],
            "UTC",
            Mode::Cached,
        ))
        .unwrap();
        let cached_time = started.elapsed();
        let started = std::time::Instant::now();
        receiver.peer_remove(&hex_encode(&other)).unwrap();
        let remove_time = started.elapsed();
        let still_waiting = !refresh.is_finished();
        release.send(()).unwrap();
        server.join().unwrap();
        let refreshed = refresh.join().unwrap();
        println!("SA3B-LOCK cached={cached_time:?} remove={remove_time:?}");
        assert!(still_waiting, "control: the refresh had already ended");
        assert_eq!(
            states(&cached),
            [(fingerprint(&silent), "unavailable".into())]
        );
        assert!(cached_time < BOUND, "cached call took {cached_time:?}");
        assert!(remove_time < BOUND, "peer_remove took {remove_time:?}");
        assert!(receiver
            .peer_records()
            .unwrap()
            .iter()
            .all(|r| r.responder_static != other));
        assert_ne!(states(&refreshed)[0].1, "fresh");
    }

    /// Deferred items (3) and (9d): a record superseded by a newer one at the
    /// same host and port is neither read (cached) nor asked (refresh).
    #[test]
    fn a_superseded_record_is_never_read_or_asked() {
        if !in_child("a_superseded_record_is_never_read_or_asked") {
            return;
        }
        let root = child_root().unwrap();
        let clock = TestClock::new();
        let context = crate::LocalSourceContext::current();
        let (a_home, r_home) = (root.join("pa"), root.join("r"));
        write_session(
            &a_home,
            "a.jsonl",
            &[claude("a1", "2025-03-10T05:45:00.000Z", "m", 7, 3, false)],
        );
        fs::create_dir_all(&r_home).unwrap();
        let a = producer(&a_home, &clock);
        let port = a.status().unwrap()["bindPort"].as_u64().unwrap() as u16;
        let receiver = agent_at(&r_home, &clock);
        let ka = pair(&a, &receiver);
        clock.advance(61);
        let first = graph(&call(
            &receiver,
            &context,
            false,
            &[ka],
            "UTC",
            Mode::Refresh,
        ))
        .unwrap();
        assert_eq!(
            states(&first),
            [(fingerprint(&ka), "fresh".into())],
            "control"
        );

        // The producer resets and is paired again at the same address: here a
        // newer record at that host and port.
        a.stop().unwrap();
        let trap = TcpListener::bind(("127.0.0.1", port)).unwrap();
        trap.set_nonblocking(true).unwrap();
        fake_peer(&receiver, 0xdd, port, i64::MAX / 4);
        for mode in [Mode::Cached, Mode::Refresh] {
            clock.advance(61);
            let page = graph(&call(&receiver, &context, false, &[ka], "UTC", mode)).unwrap();
            assert_eq!(
                states(&page),
                [(fingerprint(&ka), "stale".into())],
                "{mode:?}"
            );
            assert_eq!(
                page["contributions"],
                json!([]),
                "{mode:?} merged the old record"
            );
        }
        assert!(!connected(&trap), "the superseded record was asked");
    }

    /// Deferred item (2): a peer store that cannot be read is a row, not "no
    /// devices"; the local part is unaffected.
    #[test]
    fn an_unreadable_peer_store_is_said() {
        if !in_child("an_unreadable_peer_store_is_said") {
            return;
        }
        let root = child_root().unwrap();
        let clock = TestClock::new();
        let context = crate::LocalSourceContext::current();
        write_session(
            &root.join("home"),
            "r.jsonl",
            &[claude(
                "l1",
                "2025-03-10T05:30:00.000Z",
                "m",
                100,
                10,
                false,
            )],
        );
        let r_home = root.join("r");
        fs::create_dir_all(&r_home).unwrap();
        drop(agent_at(&r_home, &clock));
        fs::write(r_home.join("a/peers.json"), b"{not json").unwrap();
        let receiver = Arc::new(RemoteAgent::default());
        assert!(receiver
            .start(make(&r_home, &clock, ScannerSettings::default()))
            .is_err());
        let peer = [0x11; 32];
        let page = graph(&call(
            &receiver,
            &context,
            true,
            &[peer],
            "UTC",
            Mode::Cached,
        ))
        .unwrap();
        assert_eq!(
            states(&page),
            [
                ("local".into(), "local".into()),
                ("peers".into(), "unavailable".into()),
                (fingerprint(&peer), "unavailable".into()),
            ]
        );
        assert_eq!(day(&page, "2025-03-10")["tokenBreakdown"]["input"], 100);
    }

    /// Deferred item (4): a refusal with code 2 after another grant's revoke
    /// renumbered this one's `authEpoch` asks Grant status once (`active`)
    /// and stores the new epoch; this call is `refused`, the next is fresh.
    #[test]
    fn check_grant_status_updates_the_record() {
        if !in_child("check_grant_status_updates_the_record") {
            return;
        }
        let clock = TestClock::new();
        let (a_home, r_home, s_home) = (TempHome::new(), TempHome::new(), TempHome::new());
        let context = crate::LocalSourceContext::for_home(PathBuf::from("/nonexistent-sa3b"));
        let a = producer(&a_home.0, &clock);
        let receiver = agent_at(&r_home.0, &clock);
        let second = agent_at(&s_home.0, &clock);
        let ka = pair(&a, &receiver);
        pair(&a, &second);
        let epoch = |agent: &RemoteAgent| {
            agent
                .peer_records()
                .unwrap()
                .iter()
                .find(|r| r.responder_static == ka)
                .unwrap()
                .auth_epoch
        };
        let before = epoch(&receiver);
        let other = a
            .grants()
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["grantId"].as_str().unwrap().to_owned())
            .find(|id| {
                let record = second.peer_records().unwrap();
                hex_encode(&record[0].grant_id) == *id
            })
            .unwrap();
        a.revoke(&other).unwrap();
        clock.advance(61);
        let page = agents(&call(
            &receiver,
            &context,
            false,
            &[ka],
            "UTC",
            Mode::Refresh,
        ))
        .unwrap();
        assert_eq!(states(&page), [(fingerprint(&ka), "refused".into())]);
        assert!(
            epoch(&receiver) > before,
            "Grant status did not update the record"
        );
        clock.advance(61);
        let page = agents(&call(
            &receiver,
            &context,
            false,
            &[ka],
            "UTC",
            Mode::Refresh,
        ))
        .unwrap();
        assert_eq!(states(&page), [(fingerprint(&ka), "fresh".into())]);
    }

    /// Deferred item (5), measured: the producer's pricing copy older than an
    /// hour (by the dataset's own timestamp) is refused by the engine's remote
    /// snapshot, and a client that records no cost of its own (Claude Code)
    /// is then shared with cost 0. A fresh copy prices the same tokens.
    #[test]
    fn shared_cost_with_fresh_and_hour_old_pricing() {
        if !in_child("shared_cost_with_fresh_and_hour_old_pricing") {
            return;
        }
        let clock = TestClock::new();
        let (a_home, r_home) = (TempHome::new(), TempHome::new());
        let context = crate::LocalSourceContext::for_home(PathBuf::from("/nonexistent-sa3b"));
        write_session(
            &a_home.0,
            "a.jsonl",
            &[claude(
                "a1",
                "2025-03-10T05:45:00.000Z",
                "claude-3-5-sonnet",
                1_000_000,
                0,
                false,
            )],
        );
        let source = a_home.dir("pricing-src");
        let write = |age: u64, bump: u64| {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let dataset = |data: Value| json!({"timestamp": now - age, "data": data}).to_string();
            fs::write(
                source.join("pricing-litellm.json"),
                dataset(json!({"claude-3-5-sonnet": {"input_cost_per_token": 0.000001}})),
            )
            .unwrap();
            fs::write(source.join("pricing-openrouter.json"), dataset(json!({}))).unwrap();
            for name in crate::remote_agent::PRICING_FILES {
                fs::File::options()
                    .write(true)
                    .open(source.join(name))
                    .unwrap()
                    .set_modified(SystemTime::now() + Duration::from_secs(bump))
                    .unwrap();
            }
        };
        write(0, 0);
        let a = producer(&a_home.0, &clock);
        let receiver = agent_at(&r_home.0, &clock);
        let ka = pair(&a, &receiver);
        let cost = |year: &str| {
            clock.advance(61);
            let mut c = call(&receiver, &context, false, &[ka], "UTC", Mode::Refresh);
            c.year = year;
            let page = models(&c).unwrap();
            assert_eq!(
                states(&page),
                [(fingerprint(&ka), "fresh".into())],
                "{page}"
            );
            page["totalCost"].as_f64().unwrap()
        };
        let fresh = cost("2025");
        write(3_700, 10);
        std::thread::sleep(PERIOD * 5);
        let stale = cost("2025");
        println!("SA3B-PRICING fresh={fresh} stale={stale}");
        assert!((fresh - 1.0).abs() < 1e-9, "fresh pricing: {fresh}");
        assert_eq!(stale, 0.0, "hour-old pricing");
    }
}
