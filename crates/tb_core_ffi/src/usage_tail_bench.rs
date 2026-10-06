//! `cargo test --release --lib usage_tail::bench -- --ignored --nocapture --test-threads=1`
//! with HOME / TOKSCALE_CONFIG_DIR pointing at a disposable corpus. One test at
//! a time: CPU is read for the whole process, so a second bench running beside
//! it would be counted in its numbers.
//!
//!   BENCH_NOW_MS       fixed clock, normally the corpus capture time
//!   BENCH_TICKS        ticks after the first (default 20)
//!   BENCH_APPEND       file to append one synthesized line to per tick
//!   BENCH_APPEND_KIND  `claude` or `codex`
//!
//! A synthesized line carries a fresh identity (Claude ids; Codex cumulative
//! totals), because re-appending an existing line is dropped by dedup and would
//! never add an event.
use super::{UsageEvent, UsageTailer};
use chrono::{SecondsFormat, TimeZone, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::time::Instant;

#[repr(C)]
#[derive(Default)]
struct Timeval {
    sec: i64,
    usec: i32,
}
#[repr(C)]
#[derive(Default)]
struct Rusage {
    utime: Timeval,
    stime: Timeval,
    rest: [i64; 14],
}
extern "C" {
    fn getrusage(who: i32, usage: *mut Rusage) -> i32;
}

/// Process user+sys CPU in ms, every thread included.
fn cpu_ms() -> f64 {
    let mut r = Rusage::default();
    assert_eq!(unsafe { getrusage(0, &mut r) }, 0);
    let ms = |t: &Timeval| t.sec as f64 * 1000.0 + t.usec as f64 / 1000.0;
    ms(&r.utime) + ms(&r.stime)
}

/// Every field, totally ordered, then hashed: two windows match only when they
/// hold the same multiset of events.
fn digest(events: &[UsageEvent]) -> String {
    let mut rows: Vec<_> = events
        .iter()
        .map(|e| {
            (
                e.ts_ms,
                e.client.as_str(),
                e.agent.as_str(),
                e.model.as_str(),
                (e.input, e.output, e.reasoning, e.cache_read, e.cache_write),
                e.message_count,
            )
        })
        .collect();
    rows.sort();
    let hash = Sha256::digest(serde_json::to_vec(&rows).expect("serialize rows"));
    hash.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// One mutation per way a pruning, dedup or attribution change could alter the
/// window (must change the digest), plus a reorder (must not).
fn mutants(events: &[UsageEvent]) -> Vec<(&'static str, bool, Vec<UsageEvent>)> {
    let edit = |f: &dyn Fn(&mut UsageEvent)| {
        let mut v = events.to_vec();
        f(&mut v[0]);
        v
    };
    let mut removed = events.to_vec();
    removed.remove(0);
    let mut duplicated = events.to_vec();
    duplicated.push(events[0].clone());
    let mut reordered = events.to_vec();
    reordered.reverse();
    vec![
        ("remove one", true, removed),
        ("duplicate one", true, duplicated),
        ("client", true, edit(&|e| e.client.push('x'))),
        ("agent", true, edit(&|e| e.agent.push('x'))),
        ("model", true, edit(&|e| e.model.push('x'))),
        ("ts +1 ms", true, edit(&|e| e.ts_ms += 1)),
        ("tokens +1", true, edit(&|e| e.input += 1)),
        ("reorder", false, reordered),
    ]
}

fn last_line_with(path: &str, needles: &[&str], keep: impl Fn(&Value) -> bool) -> String {
    std::io::BufReader::new(std::fs::File::open(path).expect("open append target"))
        .lines()
        .map_while(Result::ok)
        .filter(|l| needles.iter().all(|n| l.contains(n)))
        .filter(|l| serde_json::from_str::<Value>(l).is_ok_and(|v| keep(&v)))
        .last()
        .unwrap_or_else(|| panic!("no matching line with {needles:?} in {path}"))
}

/// Appends `line` as its own record, adding the separator first when the
/// file's last record has no trailing newline (otherwise the two would join
/// into one invalid line).
fn append_record(path: &str, line: &str) {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .open(path)
        .expect("open append");
    let mut last = [0u8; 1];
    let unterminated = f.metadata().expect("stat append").len() > 0
        && f.seek(SeekFrom::End(-1)).is_ok()
        && f.read_exact(&mut last).is_ok()
        && last[0] != b'\n';
    if unterminated {
        writeln!(f).expect("append separator");
    }
    writeln!(f, "{line}").expect("append");
}

/// A new line built from the file's latest usage-bearing one, with a fresh
/// identity and `ts_ms` as its timestamp.
fn synthesize(path: &str, kind: &str, tick: usize, ts_ms: i64) -> String {
    let source = match kind {
        // Claude by parsed fields, so key order and spacing do not matter.
        "claude" => last_line_with(path, &["\"usage\""], |v| {
            v["type"] == "assistant" && v["message"]["usage"].is_object()
        }),
        "codex" => last_line_with(path, &["\"token_count\"", "\"total_token_usage\""], |_| true),
        other => panic!("BENCH_APPEND_KIND {other}"),
    };
    let mut v: Value = serde_json::from_str(&source).expect("parse last line");
    v["timestamp"] = Utc
        .timestamp_millis_opt(ts_ms)
        .single()
        .expect("timestamp")
        .to_rfc3339_opts(SecondsFormat::Millis, true)
        .into();
    if kind == "claude" {
        v["uuid"] = format!("bench-uuid-{tick}").into();
        v["requestId"] = format!("req_bench_{tick}").into();
        v["message"]["id"] = format!("msg_bench_{tick}").into();
    } else {
        if let Some(ordinal) = v.get("ordinal").and_then(Value::as_i64) {
            v["ordinal"] = (ordinal + 1).into();
        }
        let delta = [("input_tokens", 1000), ("output_tokens", 100), ("total_tokens", 1100)];
        let info = &mut v["payload"]["info"];
        for (key, add) in delta {
            let total = info["total_token_usage"][key].as_i64().unwrap_or(0);
            info["total_token_usage"][key] = (total + add).into();
        }
        for key in ["cached_input_tokens", "cache_write_input_tokens", "reasoning_output_tokens"] {
            info["last_token_usage"][key] = 0.into();
        }
        for (key, add) in delta {
            info["last_token_usage"][key] = add.into();
        }
    }
    serde_json::to_string(&v).expect("serialize line")
}

fn lanes(events: &[UsageEvent]) -> String {
    let mut by: BTreeMap<&str, (usize, i64)> = BTreeMap::new();
    for e in events {
        let slot = by.entry(e.client.as_str()).or_default();
        slot.0 += 1;
        slot.1 = slot.1.saturating_add(e.total());
    }
    by.iter()
        .map(|(client, (n, tokens))| format!("{client}={n}/{tokens}"))
        .collect::<Vec<_>>()
        .join(",")
}

#[test]
fn digest_sees_every_mutation_and_ignores_order() {
    let event = |ts, client: &str| UsageEvent {
        ts_ms: ts,
        client: client.into(),
        agent: "Main".into(),
        model: "m".into(),
        input: 10,
        output: 1,
        reasoning: 0,
        cache_read: 0,
        cache_write: 0,
        message_count: 1,
    };
    let events = vec![event(1, "claude"), event(2, "codex"), event(2, "grok")];
    let base = digest(&events);
    for (name, should_change, mutated) in mutants(&events) {
        assert_eq!(digest(&mutated) != base, should_change, "{name}");
    }
}

fn report(label: &str, tailer: &UsageTailer, cpu: f64, wall: f64, parses_before: usize) -> Vec<UsageEvent> {
    let events = tailer.events.lock().clone();
    println!(
        "{label}\tcpu_ms={cpu:.0}\twall_ms={wall:.0}\tparsed={}\tevents={}\tlanes={}\tdigest={}",
        tailer.parse_count() - parses_before,
        events.len(),
        lanes(&events),
        digest(&events)
    );
    events
}

#[test]
#[ignore]
fn bench_tail_tick() {
    let env = |k: &str| std::env::var(k).ok();
    let ticks: usize = env("BENCH_TICKS").and_then(|v| v.parse().ok()).unwrap_or(20);
    let fixed_now: Option<i64> = env("BENCH_NOW_MS").and_then(|v| v.parse().ok());
    let append = env("BENCH_APPEND");
    let kind = env("BENCH_APPEND_KIND").unwrap_or_else(|| "claude".into());
    let clock = || fixed_now.unwrap_or_else(super::now_ms);

    let context = crate::LocalSourceContext::current();
    let (c0, w0) = (cpu_ms(), Instant::now());
    let parsed = tokscale_core::parse_local_clients(context.parse_options(None, None));
    println!(
        "warmup\tcpu_ms={:.0}\twall_ms={:.0}\tmessages={}",
        cpu_ms() - c0,
        w0.elapsed().as_secs_f64() * 1000.0,
        parsed.map(|p| p.messages.len()).unwrap_or(0)
    );

    let tailer = UsageTailer::new();
    let (c, w) = (cpu_ms(), Instant::now());
    tailer.tick_with_clock(clock);
    let first = report("tick0", &tailer, cpu_ms() - c, w.elapsed().as_secs_f64() * 1000.0, 0);
    if !first.is_empty() {
        let base = digest(&first);
        for (name, should_change, mutated) in mutants(&first) {
            let changed = digest(&mutated) != base;
            let verdict = if changed == should_change { "ok" } else { "FAIL" };
            println!("mutation\t{name}\tchanged={changed}\t{verdict}");
        }
    }

    let mut cpus = Vec::with_capacity(ticks);
    let mut prev_events = first.len();
    for i in 1..=ticks {
        if let Some(path) = &append {
            let ts = clock() - ((ticks - i + 1) as i64) * 1000;
            let line = synthesize(path, &kind, i, ts);
            append_record(path, &line);
        }
        let parses_before = tailer.parse_count();
        let (c, w) = (cpu_ms(), Instant::now());
        tailer.tick_with_clock(clock);
        let cpu = cpu_ms() - c;
        let label = format!("tick{i}");
        let events = report(&label, &tailer, cpu, w.elapsed().as_secs_f64() * 1000.0, parses_before);
        if append.is_some() {
            let verdict = if events.len() > prev_events { "ok" } else { "FAIL" };
            println!("grow\t{label}\t{verdict}");
        }
        prev_events = events.len();
        cpus.push(cpu);
    }
    cpus.sort_by(f64::total_cmp);
    if !cpus.is_empty() {
        println!(
            "summary\tmode={}\tticks={ticks}\tmedian_cpu_ms={:.0}\tmax_cpu_ms={:.0}\tparses={}",
            if append.is_some() { "append" } else { "unchanged" },
            cpus[cpus.len() / 2],
            cpus[cpus.len() - 1],
            tailer.parse_count()
        );
    }
}

/// V4: one graph recompute, split the way `graph_compute` runs it (token probe,
/// then `usage_graph::run`), each bracketed by getrusage. Before every
/// iteration a synthesized line is appended so the token moves, as it does
/// while an agent is writing. `BENCH_GRAPH_ITERS` (default 10); the other
/// BENCH_* variables as for `bench_tail_tick`.
#[test]
#[ignore]
fn bench_graph_recompute() {
    let env = |k: &str| std::env::var(k).ok();
    let iters: usize = env("BENCH_GRAPH_ITERS").and_then(|v| v.parse().ok()).unwrap_or(10);
    let fixed_now: Option<i64> = env("BENCH_NOW_MS").and_then(|v| v.parse().ok());
    let append = env("BENCH_APPEND");
    let kind = env("BENCH_APPEND_KIND").unwrap_or_else(|| "claude".into());
    let clock = || fixed_now.unwrap_or_else(super::now_ms);
    let context = crate::LocalSourceContext::current();

    // Warm the cache once so every measured iteration is a warm recompute.
    let (c, w) = (cpu_ms(), Instant::now());
    let warm = crate::usage_graph::run(&context, "");
    println!(
        "graph-warmup\tcpu_ms={:.0}\twall_ms={:.0}\tok={}",
        cpu_ms() - c,
        w.elapsed().as_secs_f64() * 1000.0,
        warm.is_ok()
    );

    let (mut tokens, mut runs) = (Vec::new(), Vec::new());
    for i in 1..=iters {
        if let Some(path) = &append {
            let line = synthesize(path, &kind, 1000 + i, clock() - ((iters - i + 1) as i64) * 1000);
            append_record(path, &line);
        }
        let c = cpu_ms();
        let _ = tokscale_core::local_source_change_token(&context.parse_options(None, None));
        let token_cpu = cpu_ms() - c;
        let (c, w) = (cpu_ms(), Instant::now());
        let result = crate::usage_graph::run(&context, "");
        let run_cpu = cpu_ms() - c;
        println!(
            "graph\titer{i}\ttoken_cpu_ms={token_cpu:.0}\trun_cpu_ms={run_cpu:.0}\trun_wall_ms={:.0}\tok={}",
            w.elapsed().as_secs_f64() * 1000.0,
            result.is_ok()
        );
        tokens.push(token_cpu);
        runs.push(run_cpu);
    }
    for v in [&mut tokens, &mut runs] {
        v.sort_by(f64::total_cmp);
    }
    if iters > 0 {
        println!(
            "graph-summary\titers={iters}\ttoken_median_ms={:.0}\ttoken_max_ms={:.0}\trun_median_ms={:.0}\trun_max_ms={:.0}",
            tokens[iters / 2],
            tokens[iters - 1],
            runs[iters / 2],
            runs[iters - 1]
        );
    }
}

#[test]
fn append_record_keeps_records_on_separate_lines() {
    let dir = tempfile::tempdir().expect("tempdir");
    for (before, after) in [("a", "a\nb\n"), ("a\n", "a\nb\n"), ("", "b\n")] {
        let path = dir.path().join("f.jsonl");
        std::fs::write(&path, before).expect("write");
        append_record(path.to_str().expect("utf8 path"), "b");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), after, "{before:?}");
    }
}

#[test]
fn claude_source_line_is_chosen_by_fields_not_formatting() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("s.jsonl");
    let lines = [
        r#"{"type":"assistant","message":{"id":"m1","usage":{"input_tokens":1}}}"#,
        r#"{"message": {"usage": {"input_tokens": 2}, "id": "m2"}, "type": "assistant"}"#,
        r#"{"type":"user","message":{"usage":{"input_tokens":3}}}"#,
    ];
    std::fs::write(&path, lines.join("\n")).expect("write");
    let line = last_line_with(path.to_str().expect("utf8 path"), &["\"usage\""], |v| {
        v["type"] == "assistant" && v["message"]["usage"].is_object()
    });
    assert!(line.contains("\"m2\""), "{line}");
}
