//! Hermetic: temp SQLite fixtures, temp dirs, an in-process HTTP server on
//! 127.0.0.1. No real HOME, no real Cursor database, no network.
//!
//! End to end (engine pin 8fc63ced and later, which parses usage-events
//! JSON): `synced_file_reaches_the_report_once` at the bottom.

use super::*;
use base64::Engine as _;
use std::io::Read as _;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

const USER: &str = "user_CANARYuser0123456789AB";
const OTHER_USER: &str = "user_OTHERuser01234567890CD";
const THIRD_USER: &str = "user_THIRDuser01234567890EF";
const SIGNATURE: &str = "CANARYsignatureXYZ";
const OWNING_CANARY: &str = "OWNINGUSERCANARY42";
const KEY: [u8; 32] = [7; 32];

fn jwt(sub: &str, exp: Option<f64>) -> String {
    let mut claims = serde_json::json!({ "sub": sub, "type": "session" });
    if let Some(exp) = exp {
        claims["exp"] = exp.into();
    }
    let encode = |v: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
    format!(
        "{}.{}.{SIGNATURE}",
        encode(r#"{"alg":"HS256"}"#),
        encode(&claims.to_string())
    )
}

fn valid_token() -> String {
    jwt(&format!("auth0|{USER}"), Some(now_secs() + 3600.0))
}

fn state_db(dir: &Path, rows: &[(&str, &str)]) -> PathBuf {
    let path = dir.join("state.vscdb");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT)")
        .unwrap();
    for (key, value) in rows {
        conn.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value],
        )
        .unwrap();
    }
    path
}

fn signed_in_db(dir: &Path, token: &str) -> PathBuf {
    state_db(
        dir,
        &[
            ("cursorAuth/accessToken", &format!("\"{token}\"")),
            ("glass.lastSignedInAuthId", &format!("glass-{USER}")),
        ],
    )
}

// --- local server ---------------------------------------------------------

#[derive(Clone)]
enum Reply {
    Raw(Vec<u8>),
    Delayed(Duration, Vec<u8>),
    /// Headers sent, body never finishes.
    Hang,
}

#[derive(Clone, Debug)]
struct Seen {
    head: String,
    body: String,
}

fn http(status: &str, content_type: &str, body: &str) -> Reply {
    Reply::Raw(
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes(),
    )
}

fn json_ok(body: &str) -> Reply {
    http("200 OK", "application/json", body)
}

/// The Billing V2 URL on the same loopback server (which answers in order,
/// whatever the path), so a test can tell the two endpoints apart.
fn v2_url(url: &str) -> String {
    url.replace("/api/usage", "/api/dashboard/v2/query-billing-items")
}

fn read_request(stream: &mut TcpStream) -> Seen {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let n = stream.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            break buf.len();
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let length = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    while buf.len() < head_end + length {
        let n = stream.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Seen {
        head,
        body: String::from_utf8_lossy(&buf[head_end..]).to_string(),
    }
}

/// Serves `replies` in order (then 404s) forever; records every request.
fn serve(replies: Vec<Reply>) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/api/usage", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        let mut replies = replies.into_iter();
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let request = read_request(&mut stream);
            log.lock().unwrap().push(request);
            match replies.next() {
                Some(Reply::Raw(bytes)) => {
                    let _ = stream.write_all(&bytes);
                }
                Some(Reply::Delayed(delay, bytes)) => {
                    std::thread::sleep(delay);
                    let _ = stream.write_all(&bytes);
                }
                Some(Reply::Hang) => {
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100000\r\n\r\n{\"usageEventsDisplay\":[",
                    );
                    std::thread::sleep(Duration::from_secs(3));
                }
                None => {
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                }
            }
        }
    });
    (url, seen)
}

fn event(id: &str) -> serde_json::Value {
    serde_json::json!({
        "timestamp": "1788171994001",
        "model": "claude-4-sonnet",
        "kind": "USAGE_EVENT_KIND_INCLUDED_IN_PRO",
        "tokenUsage": {"inputTokens": 10, "outputTokens": 5, "cacheReadTokens": 2,
                       "totalCents": 1.25, "cacheWriteTokens": 9},
        "chargedCents": 1.25,
        "usageBasedCosts": "$0.01",
        "requestsCosts": 0,
        "cursorTokenFee": 0,
        "isChargeable": true,
        "isTokenBasedCall": true,
        "isHeadless": false,
        "conversationId": id,
        "owningUser": OWNING_CANARY,
        "serviceAccountId": "SERVICEACCOUNTCANARY",
        "subscriptionProductId": "SUBPRODUCTCANARY",
        "customSubscriptionName": "CUSTOMSUBCANARY",
    })
}

fn page(total: usize, ids: &[&str]) -> String {
    serde_json::json!({
        "totalUsageEventsCount": total,
        "usageEventsDisplay": ids.iter().map(|id| event(id)).collect::<Vec<_>>(),
    })
    .to_string()
}

fn limits() -> Limits {
    Limits {
        budget: Duration::from_secs(20),
        page_timeout: Duration::from_millis(700),
        page_size: 2,
        max_pages: 10,
        max_v2_pages: 64,
        max_body_bytes: 1024 * 1024,
        explicit: false,
    }
}

/// Plaintext tests: no Cursor Keychain consent (a sealed token would stop at
/// `keychain_consent` or, with no key source, `token_sealed_unsupported`).
fn no_consent() -> bool {
    false
}

fn stem(user: &str) -> Option<String> {
    crate::agent_account_scope::keyed_digest_hex(&KEY, FILE_NAME_DOMAIN, user.as_bytes()).ok()
}

fn expected_file(dir: &Path) -> PathBuf {
    dir.join(format!("usage.{}.json", stem(USER).unwrap()))
}

fn run_with(
    db: &Path,
    dir: &Path,
    url: &str,
    limits: Limits,
    enabled: &dyn Fn() -> bool,
) -> Outcome {
    let billing = v2_url(url);
    let target = Target {
        db_path: db,
        url,
        billing_url: &billing,
        https_only: false,
        file_stem: &stem,
        key_fetch: None,
        consent: &no_consent,
    };
    let slot = Slot {
        id: DEFAULT_PROFILE_ID.to_string(),
        root: None,
        dir: dir.to_path_buf(),
        db: db.to_path_buf(),
    };
    crate::RUNTIME.block_on(run(
        &target,
        &slot,
        limits,
        enabled,
        &mut |_, _| None,
        &mut SealedRound::new(false, &|| true),
    ))
}

struct Fixture {
    _tmp: tempfile::TempDir,
    db: PathBuf,
    dir: PathBuf,
    token: String,
}

fn fixture_with_token(token: &str) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let db = signed_in_db(tmp.path(), token);
    let dir = tmp.path().join("app").join(DEFAULT_DIR_NAME);
    Fixture {
        _tmp: tmp,
        db,
        dir,
        token: token.to_string(),
    }
}

fn fixture() -> Fixture {
    fixture_with_token(&valid_token())
}

fn stop(state: State, reason: Option<&'static str>) -> Outcome {
    Outcome::stopped(Stop::new(state, reason))
}

/// A walk that wrote a file with new bytes.
fn ok(events: usize) -> Outcome {
    Outcome {
        events,
        changed: true,
        ..stop(State::Ok, None)
    }
}

/// A complete file from an earlier sync, to prove failures leave it alone.
fn seed_old_file(dir: &Path) -> Vec<u8> {
    std::fs::create_dir_all(dir).unwrap();
    let old = br#"{"totalUsageEventsCount":0,"usageEventsDisplay":[]}"#.to_vec();
    std::fs::write(expected_file(dir), &old).unwrap();
    old
}

// --- walk ------------------------------------------------------------------

#[test]
fn walks_every_page_and_writes_the_complete_history() {
    let f = fixture();
    let (url, seen) = serve(vec![
        json_ok(&page(3, &["a", "b"])),
        json_ok(&page(3, &["c"])),
    ]);
    let outcome = run_with(&f.db, &f.dir, &url, limits(), &|| true);
    assert_eq!(outcome, ok(3));

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    for (i, request) in seen.iter().enumerate() {
        let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
        assert_eq!(body["page"], i as u64 + 1);
        assert_eq!(body["pageSize"], 2);
        assert_eq!(body["startDate"], "0");
        assert!(body["endDate"].as_str().unwrap().parse::<u128>().is_ok());
        assert!(body.get("teamId").is_none(), "no teamId (#1397)");
        assert!(request.head.starts_with("POST /api/usage "));
        assert_eq!(
            header(&request.head, "cookie").as_deref(),
            Some(format!("WorkosCursorSessionToken={USER}%3A%3A{}", f.token).as_str())
        );
        let head = request.head.to_ascii_lowercase();
        assert!(head.contains("origin: https://cursor.com\r\n"));
        assert!(head.contains("referer: https://cursor.com/dashboard\r\n"));
        assert!(head.contains("content-type: application/json\r\n"));
    }

    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(expected_file(&f.dir)).unwrap()).unwrap();
    assert_eq!(written["totalUsageEventsCount"], 3);
    let ids: Vec<_> = written["usageEventsDisplay"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["conversationId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["a", "b", "c"]);
}

fn header(head: &str, name: &str) -> Option<String> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name)
            .then(|| value.trim().to_string())
    })
}

#[test]
fn stops_at_the_reported_total_without_an_extra_page() {
    let f = fixture();
    let (url, seen) = serve(vec![json_ok(&page(2, &["a", "b"]))]);
    let outcome = run_with(&f.db, &f.dir, &url, limits(), &|| true);
    assert_eq!(outcome.events, 2);
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[test]
fn byte_cap_is_an_error_and_keeps_the_old_file() {
    let small = Limits {
        max_body_bytes: 2000,
        ..limits()
    };
    let big = page(1, &[&"x".repeat(4000)]);
    // Content-Length precheck, unknown-length streaming, and the cap being
    // cumulative across pages (page 1 fits, page 2 does not).
    let streamed = Reply::Raw(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{big}"
        )
        .into_bytes(),
    );
    let page1 = page(4, &["a", "b"]);
    assert!(page1.len() < 2000 && page1.len() * 2 > 2000);
    for replies in [
        vec![json_ok(&big)],
        vec![streamed],
        vec![json_ok(&page1), json_ok(&page1)],
    ] {
        let f = fixture();
        let old = seed_old_file(&f.dir);
        let (url, _) = serve(replies);
        let outcome = run_with(&f.db, &f.dir, &url, small, &|| true);
        assert_eq!(outcome, stop(State::Error, Some("body_too_large")));
        assert_eq!(std::fs::read(expected_file(&f.dir)).unwrap(), old);
    }
    // Control: the same page under the default cap is accepted.
    let f = fixture();
    let (url, _) = serve(vec![json_ok(&big)]);
    assert_eq!(
        run_with(&f.db, &f.dir, &url, limits(), &|| true).stop.state,
        State::Ok
    );
}

#[test]
fn timeout_is_partial_and_keeps_the_old_file() {
    let f = fixture();
    let old = seed_old_file(&f.dir);
    let (url, seen) = serve(vec![json_ok(&page(4, &["a", "b"])), Reply::Hang]);
    let outcome = run_with(&f.db, &f.dir, &url, limits(), &|| true);
    assert_eq!(outcome, stop(State::Partial, Some("timeout")));
    assert_eq!(seen.lock().unwrap().len(), 2);
    assert_eq!(std::fs::read(expected_file(&f.dir)).unwrap(), old);
}

#[test]
fn spent_budget_is_partial() {
    let f = fixture();
    let (url, seen) = serve(vec![json_ok(&page(4, &["a", "b"]))]);
    let outcome = run_with(
        &f.db,
        &f.dir,
        &url,
        Limits {
            budget: Duration::ZERO,
            ..limits()
        },
        &|| true,
    );
    assert_eq!(outcome, stop(State::Partial, Some("budget_exhausted")));
    assert!(seen.lock().unwrap().is_empty());
}

#[test]
fn redirect_is_expired_and_never_followed() {
    let f = fixture();
    let (elsewhere, elsewhere_seen) = serve(vec![json_ok(&page(1, &["a"]))]);
    let (url, seen) = serve(vec![Reply::Raw(
        format!(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: {elsewhere}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .into_bytes(),
    )]);
    assert_eq!(
        run_with(&f.db, &f.dir, &url, limits(), &|| true),
        stop(State::Expired, None)
    );
    assert_eq!(seen.lock().unwrap().len(), 1);
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        elsewhere_seen.lock().unwrap().is_empty(),
        "the redirect was followed"
    );
}

#[test]
fn http_statuses_map_to_states() {
    let cases: Vec<(Reply, Outcome)> = vec![
        (
            http("401 Unauthorized", "application/json", "{}"),
            stop(State::Expired, None),
        ),
        (
            http(
                "403 Forbidden",
                "application/json",
                r#"{"error":"not_authenticated"}"#,
            ),
            stop(State::Expired, None),
        ),
        (
            http("403 Forbidden", "text/html", "<html>blocked</html>"),
            stop(State::Error, Some("forbidden_non_json")),
        ),
        (
            http("429 Too Many Requests", "application/json", "{}"),
            stop(State::Error, Some("rate_limited")),
        ),
        (
            http("500 Internal Server Error", "text/plain", "x"),
            stop(State::Error, Some("server_error")),
        ),
        (
            json_ok(r#"{"totalUsageEventsCount":1,"usageEventsDisplay":"x"}"#),
            stop(State::Error, Some("unexpected_response")),
        ),
        (
            json_ok("[]"),
            stop(State::Error, Some("unexpected_response")),
        ),
        (
            json_ok("<html>"),
            stop(State::Error, Some("unexpected_response")),
        ),
    ];
    for (reply, expected) in cases {
        let f = fixture();
        let old = seed_old_file(&f.dir);
        let (url, seen) = serve(vec![reply]);
        assert_eq!(run_with(&f.db, &f.dir, &url, limits(), &|| true), expected);
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(std::fs::read(expected_file(&f.dir)).unwrap(), old);
    }
}

#[test]
fn unreachable_server_is_offline() {
    let f = fixture();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let url = format!("http://127.0.0.1:{port}/api/usage");
    assert_eq!(
        run_with(&f.db, &f.dir, &url, limits(), &|| true),
        stop(State::Offline, Some("unreachable"))
    );
}

// --- gates before any send ---------------------------------------------------

#[test]
fn switch_off_sends_nothing_and_on_sends() {
    let f = fixture();
    let (url, seen) = serve(vec![json_ok(&page(1, &["a"]))]);
    assert_eq!(
        run_with(&f.db, &f.dir, &url, limits(), &|| false),
        stop(State::Disabled, None)
    );
    assert!(seen.lock().unwrap().is_empty());
    // Off is decided before the login is even read: an absent database still
    // reports `disabled`, not `notSignedIn`.
    assert_eq!(
        run_with(&f.dir.join("absent.vscdb"), &f.dir, &url, limits(), &|| {
            false
        }),
        stop(State::Disabled, None)
    );
    // Control: the same fixture with the switch on does reach the server.
    assert_eq!(
        run_with(&f.db, &f.dir, &url, limits(), &|| true).stop.state,
        State::Ok
    );
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[test]
fn switch_turned_off_mid_walk_stops_before_the_next_page() {
    let f = fixture();
    let (url, seen) = serve(vec![
        json_ok(&page(4, &["a", "b"])),
        json_ok(&page(4, &["c", "d"])),
    ]);
    let calls = std::cell::Cell::new(0);
    // run's own check, then page 1's; off from page 2 on.
    let enabled = || {
        calls.set(calls.get() + 1);
        calls.get() <= 2
    };
    assert_eq!(
        run_with(&f.db, &f.dir, &url, limits(), &enabled),
        stop(State::Disabled, None)
    );
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert!(!expected_file(&f.dir).exists());
}

#[test]
fn expired_or_unverifiable_token_sends_nothing() {
    for token in [
        jwt(&format!("auth0|{USER}"), Some(now_secs() - 10.0)),
        jwt(&format!("auth0|{USER}"), Some(now_secs() + 30.0)),
        jwt(&format!("auth0|{USER}"), None),
    ] {
        let f = fixture_with_token(&token);
        let (url, seen) = serve(vec![json_ok(&page(1, &["a"]))]);
        assert_eq!(
            run_with(&f.db, &f.dir, &url, limits(), &|| true),
            stop(State::Expired, None)
        );
        assert!(seen.lock().unwrap().is_empty());
    }
    // Control: an hour of validity is sent.
    let f = fixture();
    let (url, seen) = serve(vec![json_ok(&page(1, &["a"]))]);
    assert_eq!(
        run_with(&f.db, &f.dir, &url, limits(), &|| true).stop.state,
        State::Ok
    );
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[test]
fn stored_account_that_differs_from_the_token_is_refused() {
    let token = valid_token();
    for rows in [
        vec![("glass.lastSignedInAuthId", format!("glass-{OTHER_USER}"))],
        vec![
            ("glass.lastSignedInAuthId", format!("glass-{USER}")),
            (
                "cursorAuth/cachedScopedProfile",
                format!(r#"{{"userId":"{OTHER_USER}"}}"#),
            ),
        ],
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let mut all: Vec<(&str, String)> = vec![("cursorAuth/accessToken", token.clone())];
        all.extend(rows.iter().map(|(k, v)| (*k, v.clone())));
        let refs: Vec<(&str, &str)> = all.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let db = state_db(tmp.path(), &refs);
        let (url, seen) = serve(vec![json_ok(&page(1, &["a"]))]);
        assert_eq!(
            run_with(&db, &tmp.path().join("sync"), &url, limits(), &|| true),
            Outcome::refused(Stop::error("account_mismatch"))
        );
        assert!(seen.lock().unwrap().is_empty());
    }
    // Controls: matching stored ids, and no stored id at all, are both sent.
    for rows in [
        vec![(
            "cursorAuth/cachedScopedProfile",
            format!(r#"{{"userId":"{USER}"}}"#),
        )],
        vec![],
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let mut all: Vec<(&str, String)> = vec![("cursorAuth/accessToken", token.clone())];
        all.extend(rows.iter().map(|(k, v)| (*k, v.clone())));
        let refs: Vec<(&str, &str)> = all.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let db = state_db(tmp.path(), &refs);
        let (url, seen) = serve(vec![json_ok(&page(1, &["a"]))]);
        assert_eq!(
            run_with(&db, &tmp.path().join("sync"), &url, limits(), &|| true)
                .stop
                .state,
            State::Ok
        );
        assert_eq!(seen.lock().unwrap().len(), 1);
    }
}

#[test]
fn missing_login_is_not_signed_in() {
    let tmp = tempfile::tempdir().unwrap();
    let (url, seen) = serve(vec![]);
    for db in [tmp.path().join("absent.vscdb"), state_db(tmp.path(), &[])] {
        assert_eq!(
            run_with(&db, &tmp.path().join("sync"), &url, limits(), &|| true),
            stop(State::NotSignedIn, None)
        );
    }
    assert!(seen.lock().unwrap().is_empty());
}

// --- what is written ---------------------------------------------------------

/// Exactly what tokscale-core's Cursor JSON parser reads (`CursorUsageEvent`,
/// `CursorTokenUsage` at pin 8fc63ced).
const ALLOWED_EVENT_FIELDS: &[&str] = &[
    "conversationId",
    "timestamp",
    "model",
    "chargedCents",
    "tokenUsage",
];
const ALLOWED_TOKEN_FIELDS: &[&str] = &[
    "inputTokens",
    "outputTokens",
    "cacheReadTokens",
    "cacheWriteTokens",
    "totalCents",
];

#[test]
fn written_file_holds_only_the_parser_fields() {
    let f = fixture();
    let (url, _) = serve(vec![json_ok(&page(1, &["a"]))]);
    assert_eq!(
        run_with(&f.db, &f.dir, &url, limits(), &|| true).stop.state,
        State::Ok
    );
    let text = std::fs::read_to_string(expected_file(&f.dir)).unwrap();
    let written: serde_json::Value = serde_json::from_str(&text).unwrap();
    let mut top: Vec<_> = written.as_object().unwrap().keys().cloned().collect();
    top.sort();
    assert_eq!(top, ["totalUsageEventsCount", "usageEventsDisplay"]);
    let event = written["usageEventsDisplay"][0].as_object().unwrap();
    let mut keys: Vec<&str> = event.keys().map(String::as_str).collect();
    keys.sort();
    let mut allowed = ALLOWED_EVENT_FIELDS.to_vec();
    allowed.sort();
    // Exact: every allowed field the fixture carries survives (control), and
    // nothing else does.
    assert_eq!(keys, allowed);
    let mut token_keys: Vec<&str> = event["tokenUsage"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    token_keys.sort();
    let mut allowed_tokens = ALLOWED_TOKEN_FIELDS.to_vec();
    allowed_tokens.sort();
    assert_eq!(token_keys, allowed_tokens);
    assert_eq!(event["tokenUsage"]["totalCents"], 1.25);
    assert_eq!(event["chargedCents"], 1.25);
    for forbidden in [
        "owningUser",
        "serviceAccountId",
        "subscriptionProductId",
        "customSubscriptionName",
        "kind",
        "usageBasedCosts",
        "requestsCosts",
        "cursorTokenFee",
        "isChargeable",
        "isTokenBasedCall",
        "isHeadless",
    ] {
        assert!(!text.contains(forbidden), "{forbidden}");
    }
}

/// The engine coerces numeric strings, so a page carrying one must still
/// sync (written back as a number); free text in a numeric field must not
/// reach the file.
#[test]
fn numeric_strings_are_accepted_and_free_text_is_refused() {
    let mut lenient = event("a");
    lenient["chargedCents"] = "2.5".into();
    lenient["tokenUsage"]["inputTokens"] = "10".into();
    let body = serde_json::json!({"totalUsageEventsCount": 1, "usageEventsDisplay": [lenient]});
    let f = fixture();
    let (url, _) = serve(vec![json_ok(&body.to_string())]);
    assert_eq!(
        run_with(&f.db, &f.dir, &url, limits(), &|| true).stop.state,
        State::Ok
    );
    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(expected_file(&f.dir)).unwrap()).unwrap();
    assert_eq!(written["usageEventsDisplay"][0]["chargedCents"], 2.5);
    assert_eq!(
        written["usageEventsDisplay"][0]["tokenUsage"]["inputTokens"],
        10.0
    );

    let mut junk = event("a");
    junk["chargedCents"] = "FREETEXTCANARY".into();
    let body = serde_json::json!({"totalUsageEventsCount": 1, "usageEventsDisplay": [junk]});
    let f = fixture();
    let (url, _) = serve(vec![json_ok(&body.to_string())]);
    assert_eq!(
        run_with(&f.db, &f.dir, &url, limits(), &|| true),
        stop(State::Error, Some("unexpected_response"))
    );
    assert!(!expected_file(&f.dir).exists());
}

#[cfg(unix)]
#[test]
fn files_are_owner_only_and_no_temp_file_remains() {
    use std::os::unix::fs::PermissionsExt as _;
    let f = fixture();
    let (url, _) = serve(vec![json_ok(&page(1, &["a"]))]);
    assert_eq!(
        run_with(&f.db, &f.dir, &url, limits(), &|| true).stop.state,
        State::Ok
    );
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&f.dir), 0o700);
    assert_eq!(mode(&expected_file(&f.dir)), 0o600);
    let names: Vec<String> = std::fs::read_dir(&f.dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().all(|n| !n.starts_with(TEMP_FILE_PREFIX)),
        "{names:?}"
    );
}

#[test]
fn temp_file_name_can_never_be_scanned_as_usage() {
    let name = temp_file_name();
    assert!(name.starts_with('.'));
    assert!(!name.starts_with("usage"));
    assert!(!is_sync_file_name(&name));
}

#[test]
fn a_complete_walk_drops_other_usage_files_and_a_failed_one_does_not() {
    let f = fixture();
    std::fs::create_dir_all(&f.dir).unwrap();
    let orphan = f.dir.join(format!("usage.{}.json", "a".repeat(64)));
    let unrelated = f.dir.join("notes.txt");
    std::fs::write(&orphan, "{}").unwrap();
    std::fs::write(&unrelated, "keep").unwrap();

    let (url, _) = serve(vec![http("500 Internal Server Error", "text/plain", "")]);
    assert_eq!(
        run_with(&f.db, &f.dir, &url, limits(), &|| true).stop.state,
        State::Error
    );
    assert!(orphan.exists(), "a failed walk must not clean up");

    let (url, _) = serve(vec![json_ok(&page(1, &["a"]))]);
    assert_eq!(
        run_with(&f.db, &f.dir, &url, limits(), &|| true).stop.state,
        State::Ok
    );
    assert!(!orphan.exists());
    assert!(unrelated.exists());
    assert!(expected_file(&f.dir).exists());
}

#[test]
fn file_name_is_keyed_and_domain_separated() {
    let name = stem(USER).unwrap();
    assert_eq!(name.len(), 64);
    assert!(is_sync_file_name(&format!("usage.{name}.json")));
    assert_ne!(Some(name.clone()), stem(OTHER_USER));
    assert_ne!(
        Some(name),
        crate::agent_account_scope::keyed_digest_hex(&KEY, "other-domain", USER.as_bytes()).ok()
    );
}

// --- FFI-level: registry, single flight, canaries ---------------------------

fn lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// The config whose default profile writes to `dir` (`<appDir>/cursor-cache`).
fn config_for(enabled: bool, dir: &Path) -> Config {
    assert!(dir.ends_with(DEFAULT_DIR_NAME));
    Config {
        enabled,
        app_dir: dir.parent().map(Path::to_path_buf),
        ..Config::default()
    }
}

/// A registry local to the test, so a sync test never turns on the takeover
/// for other tests' `LocalSourceContext`s through the process-wide one.
fn registry(enabled: bool, dir: &Path) -> RwLock<Config> {
    RwLock::new(config_for(enabled, dir))
}

fn sync_status(f: &Fixture, url: &str, registry: &RwLock<Config>) -> serde_json::Value {
    let billing = v2_url(url);
    let target = Target {
        db_path: &f.db,
        url,
        billing_url: &billing,
        https_only: false,
        file_stem: &stem,
        key_fetch: None,
        consent: &no_consent,
    };
    let source = || registry.read().unwrap().clone();
    sync_with(true, None, &target, &source, Some(limits())).0
}

#[test]
fn ffi_switch_off_sends_nothing_even_when_explicit() {
    let _guard = lock();
    let f = fixture();
    let (url, seen) = serve(vec![json_ok(&page(1, &["a"]))]);
    let status = sync_status(&f, &url, &registry(false, &f.dir));
    assert_eq!(status["state"], "disabled");
    assert!(seen.lock().unwrap().is_empty());
    // Control.
    let status = sync_status(&f, &url, &registry(true, &f.dir));
    assert_eq!(status["state"], "ok");
    assert_eq!(status["events"], 1);
    assert!(status["lastSuccessMs"].as_u64().unwrap() > 0);
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[test]
fn single_flight_second_caller_waits_and_shares_the_result() {
    let _guard = lock();
    let f = fixture();
    let (url, seen) = serve(vec![Reply::Delayed(
        Duration::from_millis(300),
        match json_ok(&page(1, &["a"])) {
            Reply::Raw(bytes) => bytes,
            _ => unreachable!(),
        },
    )]);
    let on = registry(true, &f.dir);
    let first = std::thread::scope(|scope| {
        let first = scope.spawn(|| sync_status(&f, &url, &on));
        let started = Instant::now();
        while seen.lock().unwrap().is_empty() {
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
        let second = sync_status(&f, &url, &on);
        let first = first.join().unwrap();
        assert_eq!(second, first);
        first
    });
    assert_eq!(first["state"], "ok");
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "the second caller started a walk"
    );
}

#[test]
fn canaries_never_reach_the_file_names_status_or_errors() {
    let _guard = lock();
    let token = valid_token();
    let canaries = [
        token.as_str(),
        SIGNATURE,
        USER,
        OWNING_CANARY,
        "SERVICEACCOUNTCANARY",
    ];
    let replies = vec![
        vec![json_ok(&page(1, &["a"]))],
        vec![http("401 Unauthorized", "application/json", "{}")],
        vec![http("403 Forbidden", "text/html", "<html>blocked</html>")],
        vec![http("429 Too Many Requests", "application/json", "{}")],
        vec![json_ok(&format!(
            r#"{{"usageEventsDisplay":{{"owningUser":"{OWNING_CANARY}"}}}}"#
        ))],
        vec![json_ok(&page(4, &["a", "b"])), Reply::Hang],
    ];
    let mut outputs = Vec::new();
    for reply in replies {
        let f = fixture();
        let (url, _) = serve(reply);
        outputs.push(sync_status(&f, &url, &registry(true, &f.dir)).to_string());
        for entry in std::fs::read_dir(&f.dir).into_iter().flatten().flatten() {
            outputs.push(entry.file_name().to_string_lossy().into_owned());
            if entry.file_type().unwrap().is_file() {
                outputs.push(std::fs::read_to_string(entry.path()).unwrap());
            }
        }
    }
    // Mismatch refusal, through the same entry point.
    let tmp = tempfile::tempdir().unwrap();
    let db = state_db(
        tmp.path(),
        &[
            ("cursorAuth/accessToken", &token),
            ("glass.lastSignedInAuthId", OTHER_USER),
        ],
    );
    let mismatch = Fixture {
        db,
        dir: tmp.path().join("app").join(DEFAULT_DIR_NAME),
        _tmp: tmp,
        token: token.clone(),
    };
    outputs.push(
        sync_status(
            &mismatch,
            "http://127.0.0.1:9/never",
            &registry(true, &mismatch.dir),
        )
        .to_string(),
    );

    // Control: the scenarios above did produce the outputs being searched.
    let all = outputs.join("\n");
    for expected in [
        "\"ok\"",
        "\"expired\"",
        "forbidden_non_json",
        "rate_limited",
        "unexpected_response",
        "\"partial\"",
        "account_mismatch",
        "usage.",
    ] {
        assert!(all.contains(expected), "missing {expected} in {all}");
    }
    for canary in canaries {
        assert!(!all.contains(canary), "canary leaked");
    }
}

// --- registry ------------------------------------------------------------------

const BUNDLE: &str = "com.example.syrtis.selftest";
const PROFILE_A: &str = "0A1B2C3D-0000-4000-8000-00000000000A";
const PROFILE_B: &str = "0A1B2C3D-0000-4000-8000-00000000000B";
const PROFILE_C: &str = "0A1B2C3D-0000-4000-8000-00000000000C";

fn app_dir_in(home: &Path) -> PathBuf {
    home.join("Library/Application Support").join(BUNDLE)
}

fn set_in(home: &Path, json: serde_json::Value) -> Result<serde_json::Value, String> {
    set_from_json(&json.to_string(), Some(home))
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name != LOCK_FILE_NAME)
        .collect();
    names.sort();
    names
}

fn hex_name(c: char) -> String {
    format!("usage.{}.json", c.to_string().repeat(64))
}

/// 3b ✱: the bundle id is the only directory input, and the appDir derived
/// from it must be a real directory outside the tokscale CLI's tree.
#[test]
fn base_validation_rejects_bad_bundle_ids_and_app_dirs() {
    let _guard = lock();
    set_for_test(Config::default());
    let home = tempfile::tempdir().unwrap();
    let set = |json: serde_json::Value| set_in(home.path(), json);
    for bad in [
        serde_json::json!({"enabled": true}),
        serde_json::json!({"enabled": true, "dir": "/x/cursor-cache"}),
        serde_json::json!({"enabled": true, "bundleId": BUNDLE, "dir": "/x"}),
        serde_json::json!({"enabled": true, "bundleId": ""}),
        serde_json::json!({"enabled": true, "bundleId": "syrtis"}),
        serde_json::json!({"enabled": true, "bundleId": "com..syrtis"}),
        serde_json::json!({"enabled": true, "bundleId": ".."}),
        serde_json::json!({"enabled": true, "bundleId": "com.x/../y"}),
        serde_json::json!({"enabled": true, "bundleId": "com/x"}),
        serde_json::json!({"enabled": true, "bundleId": "com.x."}),
        serde_json::json!({"enabled": true, "bundleId": "com.x y"}),
        serde_json::json!({"enabled": true, "bundleId": BUNDLE, "extra": 1}),
    ] {
        assert!(set(bad.clone()).is_err(), "{bad}");
        assert_eq!(
            config(),
            Config::default(),
            "rejected {bad} changed the registry"
        );
    }
    assert!(set_from_json(
        &serde_json::json!({"enabled": true, "bundleId": BUNDLE}).to_string(),
        None
    )
    .is_err());

    // A file in the appDir's place, a symlink to a file, and (unless root)
    // a symlink to a directory owned by someone else are refused for an on
    // push.
    let support = home.path().join("Library/Application Support");
    std::fs::create_dir_all(&support).unwrap();
    std::fs::write(support.join("com.example.file"), "x").unwrap();
    let mut refused = vec!["com.example.file"];
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            support.join("com.example.file"),
            support.join("com.example.tofile"),
        )
        .unwrap();
        refused.push("com.example.tofile");
        if crate::cursor_desktop::current_uid() != 0 {
            std::os::unix::fs::symlink("/usr", support.join("com.example.foreign")).unwrap();
            refused.push("com.example.foreign");
        }
    }
    for bundle in refused {
        assert!(
            set(serde_json::json!({"enabled": true, "bundleId": bundle})).is_err(),
            "{bundle}"
        );
        assert_eq!(config(), Config::default());
    }
    // An own-uid symlinked appDir is accepted, as earlier releases did.
    #[cfg(unix)]
    {
        let elsewhere = home.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, support.join("com.example.linked")).unwrap();
        let on =
            set(serde_json::json!({"enabled": true, "bundleId": "com.example.linked"})).unwrap();
        assert_eq!(on["enabled"], true);
        assert_eq!(config().app_dir, Some(support.join("com.example.linked")));
        set_for_test(Config::default());
    }

    // An appDir that resolves under ~/.config/tokscale through a symlinked
    // ancestor.
    let tricky = tempfile::tempdir().unwrap();
    let inside = tricky.path().join(".config/tokscale/library");
    std::fs::create_dir_all(&inside).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&inside, tricky.path().join("Library")).unwrap();
    assert!(set_in(
        tricky.path(),
        serde_json::json!({"enabled": true, "bundleId": BUNDLE})
    )
    .is_err());
    assert_eq!(config(), Config::default());

    // Controls: valid ids are accepted, and two ids never share a dir.
    let one = set(serde_json::json!({"enabled": false, "bundleId": "com.example.one"})).unwrap();
    let two =
        set(serde_json::json!({"enabled": false, "bundleId": "com.example.one.selftest"})).unwrap();
    let (one, two) = (
        PathBuf::from(one["appDir"].as_str().unwrap()),
        PathBuf::from(two["appDir"].as_str().unwrap()),
    );
    assert_eq!(one, support.join("com.example.one"));
    assert!(
        !one.starts_with(&two) && !two.starts_with(&one),
        "{one:?} {two:?}"
    );
    // An off push there is accepted but cleans nothing; with no earlier
    // appDir nothing of ours can be left, so no failure is reported.
    let off = set_in(
        tricky.path(),
        serde_json::json!({"enabled": false, "bundleId": "com.example.x"}),
    )
    .unwrap();
    assert_eq!(off["enabled"], false);
    assert_eq!(off["appDir"], serde_json::Value::Null);
    assert_eq!(off["cleanupFailed"], false);

    // Item 6: a real (not symlinked) appDir owned by another user is refused
    // like a symlink to one. Reached through a symlinked ancestor onto a
    // root-owned framework dir whose name is a valid bundle id.
    #[cfg(target_os = "macos")]
    {
        let framework = Path::new("/System/Library/Frameworks/AppKit.framework");
        if crate::cursor_desktop::current_uid() != 0 && framework.is_dir() {
            let foreign = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(foreign.path().join("Library")).unwrap();
            std::os::unix::fs::symlink(
                "/System/Library/Frameworks",
                foreign.path().join("Library/Application Support"),
            )
            .unwrap();
            assert!(set_in(
                foreign.path(),
                serde_json::json!({"enabled": true, "bundleId": "AppKit.framework"})
            )
            .is_err());
            assert_eq!(config(), Config::default());
        }
    }
    set_for_test(Config::default());
}

/// FIX 3: an off push is always accepted and turns the takeover off,
/// whatever its bundle id, profiles or appDir; it cleans only an appDir that
/// passes the on-push checks and reports the rest.
#[test]
fn an_off_push_is_always_accepted() {
    let _guard = lock();
    let home = tempfile::tempdir().unwrap();
    let app = app_dir_in(home.path());
    let support = app.parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&support).unwrap();
    let on = || {
        set_for_test(Config {
            enabled: true,
            app_dir: Some(app.clone()),
            ..Config::default()
        })
    };
    for off in [
        serde_json::json!({"enabled": false, "bundleId": BUNDLE,
            "profiles": [{"id": "/", "root": "relative"}]}),
        serde_json::json!({"enabled": false, "bundleId": ".."}),
        serde_json::json!({"enabled": false}),
        serde_json::json!({"enabled": false, "dir": "/x/cursor-cache"}),
    ] {
        on();
        let result = set_in(home.path(), off.clone()).unwrap_or_else(|e| panic!("{off}: {e}"));
        assert_eq!(result["enabled"], false, "{off}");
        assert!(!config().enabled, "{off}");
        assert_eq!(takeover(&config(), Some(home.path())), Takeover::Off);
    }
    // The invalid-profile push still cleaned its (valid) appDir.
    on();
    let default = app.join("cursor-cache");
    std::fs::create_dir_all(&default).unwrap();
    let temp = default.join(format!("{TEMP_FILE_PREFIX}1-1"));
    std::fs::write(&temp, "x").unwrap();
    let result = set_in(
        home.path(),
        serde_json::json!({"enabled": false, "bundleId": BUNDLE,
            "profiles": [{"id": "/", "root": "relative"}]}),
    )
    .unwrap();
    assert_eq!(result["removedFiles"], 1);
    assert!(!temp.exists());

    // A symlinked appDir pointing at a file: accepted, nothing cleaned there.
    // The appDir in use before (a valid, absent one) was cleaned: no failure.
    #[cfg(unix)]
    {
        let target = home.path().join("plain-file");
        std::fs::write(&target, "x").unwrap();
        std::os::unix::fs::symlink(&target, support.join("com.example.bad")).unwrap();
        on();
        let result = set_in(
            home.path(),
            serde_json::json!({"enabled": false, "bundleId": "com.example.bad"}),
        )
        .unwrap();
        assert!(!config().enabled);
        assert_eq!(result["appDir"], serde_json::Value::Null);
        assert_eq!(result["cleanupFailed"], false);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "x");
    }
    set_for_test(Config::default());
}

/// 6 ✱: profile ids are UUIDs Swift made; roots are absolute.
#[test]
fn bad_profile_ids_and_roots_are_rejected() {
    let _guard = lock();
    set_for_test(Config::default());
    let home = tempfile::tempdir().unwrap();
    let with = |profiles: serde_json::Value| {
        set_in(
            home.path(),
            serde_json::json!({"enabled": true, "bundleId": BUNDLE, "profiles": profiles}),
        )
    };
    for bad in [
        serde_json::json!([{"id": "/", "root": "/Users/x/cursor-b"}]),
        serde_json::json!([{"id": "..", "root": "/Users/x/cursor-b"}]),
        serde_json::json!([{"id": "../0A1B2C3D-0000-4000-8000-00000000000A", "root": "/Users/x/b"}]),
        serde_json::json!([{"id": "0a1b2c3d-0000-4000-8000-00000000000a", "root": "/Users/x/b"}]),
        serde_json::json!([{"id": "0A1B2C3D00004000800000000000000A", "root": "/Users/x/b"}]),
        serde_json::json!([{"id": "0A1B2C3D-0000-4000-8000-00000000000/", "root": "/Users/x/b"}]),
        serde_json::json!([{"id": "default", "root": "/Users/x/b"}]),
        serde_json::json!([{"id": PROFILE_A, "root": "/Users/x/b"}, {"id": PROFILE_A, "root": "/Users/x/c"}]),
        serde_json::json!([{"id": PROFILE_A, "root": "relative"}]),
        serde_json::json!([{"id": PROFILE_A, "root": "/"}]),
        serde_json::json!([{"id": PROFILE_A, "root": "/Users/x/../y"}]),
        serde_json::json!([{"id": PROFILE_A, "root": "/Users/x/b", "dir": "/tmp"}]),
        serde_json::json!((0..=MAX_PROFILES)
            .map(|i| serde_json::json!({"id": format!("0A1B2C3D-0000-4000-8000-{i:012}"), "root": "/r"}))
            .collect::<Vec<_>>()),
    ] {
        assert!(with(bad.clone()).is_err(), "{bad}");
        assert_eq!(config(), Config::default(), "rejected {bad} changed the registry");
    }
    // Control.
    let ok = with(serde_json::json!([{"id": PROFILE_A, "root": "/Users/x/cursor-b"}])).unwrap();
    let app = app_dir_in(home.path());
    assert_eq!(
        ok["profiles"],
        serde_json::json!([
            {"id": "default", "dir": app.join("cursor-cache")},
            {"id": PROFILE_A, "dir": app.join("cursor-profiles").join(PROFILE_A)},
        ])
    );
    assert_eq!(
        config(),
        Config {
            enabled: true,
            app_dir: Some(app),
            profiles: vec![Profile {
                id: PROFILE_A.to_string(),
                root: PathBuf::from("/Users/x/cursor-b")
            }],
            cli_takeover_confirmed: false,
        }
    );
    set_for_test(Config::default());
}

/// Turning sync off deletes only our files (B2: `usage.<64 hex>.json` and
/// temp files) from every profile dir, and removes a profile dir it emptied.
#[test]
fn disable_deletes_only_our_files_in_every_profile_dir() {
    let _guard = lock();
    set_for_test(Config::default());
    let home = tempfile::tempdir().unwrap();
    let app = app_dir_in(home.path());
    let off = serde_json::json!({"enabled": false, "bundleId": BUNDLE,
        "profiles": [{"id": PROFILE_A, "root": "/Users/x/a"}]});
    // Files are created while the registry is off, so no complete-shaped
    // file is ever visible to other tests' takeover.
    set_in(home.path(), off.clone()).unwrap();
    let default = app.join("cursor-cache");
    let profile = app.join("cursor-profiles").join(PROFILE_A);
    for dir in [&default, &profile] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(hex_name('a')), "x").unwrap();
        std::fs::write(dir.join(format!("{TEMP_FILE_PREFIX}1-1")), "x").unwrap();
    }
    std::fs::write(default.join("keep.txt"), "x").unwrap();
    std::fs::write(default.join("usage.not-ours.json"), "x").unwrap();
    std::fs::write(default.join(format!("usage.{}.json", "a".repeat(63))), "x").unwrap();

    let result = set_in(home.path(), off).unwrap();
    assert_eq!(result["removedFiles"], 4);
    assert_eq!(result["cleanupFailed"], false);
    assert_eq!(
        names_in(&default),
        [
            "keep.txt".to_string(),
            format!("usage.{}.json", "a".repeat(63)),
            "usage.not-ours.json".to_string()
        ]
    );
    assert!(!profile.exists(), "an emptied profile dir is removed");
    assert!(!config().enabled);
    set_for_test(Config::default());
}

/// 5 ✱: removing a profile (or finding one left by an earlier run) deletes
/// only our files in that profile's dir; other profiles, foreign files,
/// symlinked and malformed dirs are left alone.
#[test]
fn removed_and_orphaned_profiles_are_cleaned_and_nothing_else() {
    let _guard = lock();
    set_for_test(Config::default());
    let home = tempfile::tempdir().unwrap();
    let app = app_dir_in(home.path());
    let profiles = app.join("cursor-profiles");
    let a = profiles.join(PROFILE_A);
    let b = profiles.join(PROFILE_B);
    let temp = format!("{TEMP_FILE_PREFIX}9-9");
    // A (kept) holds a temp file of ours and a foreign file. No complete
    // file in a configured dir, so the global takeover stays off.
    std::fs::create_dir_all(&a).unwrap();
    std::fs::write(a.join(&temp), "x").unwrap();
    std::fs::write(a.join("foreign.txt"), "x").unwrap();
    // B (orphan) holds ours plus a foreign file named like usage.
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(b.join(hex_name('b')), "x").unwrap();
    std::fs::write(b.join(&temp), "x").unwrap();
    std::fs::write(b.join("usage.foreign.json"), "x").unwrap();
    // C is a symlink to a dir holding a file of ours: not followed.
    let outside = home.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join(hex_name('c')), "x").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, profiles.join(PROFILE_C)).unwrap();
    // Malformed names: not ours.
    for name in ["not-a-uuid", "0a1b2c3d-0000-4000-8000-00000000000d"] {
        std::fs::create_dir_all(profiles.join(name)).unwrap();
        std::fs::write(profiles.join(name).join(hex_name('d')), "x").unwrap();
    }

    // A fresh process (restart) configures A only.
    let result = set_in(
        home.path(),
        serde_json::json!({"enabled": true, "bundleId": BUNDLE,
            "profiles": [{"id": PROFILE_A, "root": "/Users/x/a"}]}),
    )
    .unwrap();
    assert_eq!(result["removedFiles"], 2);
    assert_eq!(result["cleanupFailed"], false);
    assert_eq!(names_in(&a), [temp.clone(), "foreign.txt".to_string()]);
    assert_eq!(
        names_in(&b),
        ["usage.foreign.json".to_string()],
        "B: only ours went"
    );
    assert_eq!(
        names_in(&outside),
        [hex_name('c')],
        "the symlink was followed"
    );
    assert!(std::fs::symlink_metadata(profiles.join(PROFILE_C))
        .unwrap()
        .file_type()
        .is_symlink());
    for name in ["not-a-uuid", "0a1b2c3d-0000-4000-8000-00000000000d"] {
        assert_eq!(names_in(&profiles.join(name)), [hex_name('d')], "{name}");
    }

    // Removing A while on: its files of ours go, its foreign file stays.
    let result = set_in(
        home.path(),
        serde_json::json!({"enabled": true, "bundleId": BUNDLE, "profiles": []}),
    )
    .unwrap();
    assert_eq!(result["removedFiles"], 1);
    assert_eq!(names_in(&a), ["foreign.txt".to_string()]);

    // A symlinked `cursor-profiles` is never entered.
    set_for_test(Config::default());
    let other = tempfile::tempdir().unwrap();
    let other_app = app_dir_in(other.path());
    std::fs::create_dir_all(&other_app).unwrap();
    let target = other.path().join("target");
    std::fs::create_dir_all(target.join(PROFILE_B)).unwrap();
    std::fs::write(target.join(PROFILE_B).join(hex_name('e')), "x").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, other_app.join("cursor-profiles")).unwrap();
    set_in(
        other.path(),
        serde_json::json!({"enabled": false, "bundleId": BUNDLE}),
    )
    .unwrap();
    assert_eq!(names_in(&target.join(PROFILE_B)), [hex_name('e')]);
    set_for_test(Config::default());
}

#[cfg(unix)]
#[test]
fn disable_reports_a_cleanup_it_could_not_finish() {
    use std::os::unix::fs::PermissionsExt as _;
    let _guard = lock();
    set_for_test(Config::default());
    let home = tempfile::tempdir().unwrap();
    let dir = app_dir_in(home.path()).join("cursor-cache");
    std::fs::create_dir_all(&dir).unwrap();
    let set = |enabled: bool| {
        set_in(
            home.path(),
            serde_json::json!({"enabled": enabled, "bundleId": BUNDLE}),
        )
        .unwrap()
    };
    set(true);
    // A temp file is ours but never complete, so the global takeover stays off.
    let synced = dir.join(format!("{TEMP_FILE_PREFIX}1-1"));
    std::fs::write(&synced, "x").unwrap();
    // The lock file exists already, so only the delete itself is refused.
    std::fs::write(dir.join(LOCK_FILE_NAME), "").unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let off = set(false);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        synced.exists(),
        "fixture: the delete must have been refused"
    );
    assert_eq!(off["cleanupFailed"], true);
    assert_eq!(off["cleanupFailedProfiles"], serde_json::json!(["default"]));
    assert_eq!(off["removedFiles"], 0);

    // Turning it on and off again retries, and a clean pass clears the flag.
    assert_eq!(set(true)["cleanupFailed"], false);
    let retried = set(false);
    assert_eq!(retried["cleanupFailed"], false);
    assert_eq!(retried["removedFiles"], 1);
    assert!(!synced.exists());
    set_for_test(Config::default());
}

#[cfg(unix)]
#[test]
fn cleanup_without_a_lock_fails_only_when_files_of_ours_remain() {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = |path: &Path, mode: u32| {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap()
    };
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("cursor-cache");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("keep.txt"), "x").unwrap();
    std::fs::write(dir.join("usage.x.json"), "x").unwrap();

    // Read-only, no lock file (it cannot be created) and nothing of ours.
    mode(&dir, 0o500);
    let empty = remove_usage_files_locked(&dir, false);
    mode(&dir, 0o700);
    assert_eq!(empty, (0, false), "nothing of ours: not a failure");

    // Same, with a synced file left behind (control).
    std::fs::write(dir.join(hex_name('f')), "x").unwrap();
    mode(&dir, 0o500);
    let stuck = remove_usage_files_locked(&dir, false);
    mode(&dir, 0o700);
    assert_eq!(stuck, (0, true));

    // A dir that cannot even be examined is a failure, not "missing".
    mode(home.path(), 0o000);
    let unreadable = remove_usage_files_locked(&dir, false);
    mode(home.path(), 0o700);
    assert_eq!(unreadable, (0, true));
    assert_eq!(
        remove_usage_files_locked(&home.path().join("absent"), false),
        (0, false)
    );
}

// --- takeover ------------------------------------------------------------------

fn write_complete_file(dir: &Path) -> PathBuf {
    write_named_file(dir, 'c')
}

fn write_named_file(dir: &Path, c: char) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(hex_name(c));
    std::fs::write(
        &path,
        r#"{"totalUsageEventsCount":0,"usageEventsDisplay":[]}"#,
    )
    .unwrap();
    std::fs::canonicalize(path).unwrap()
}

fn claude_settings() -> tokscale_core::scanner::ScannerSettings {
    tokscale_core::scanner::ScannerSettings {
        extra_scan_paths: [("claude".to_string(), vec![PathBuf::from("/claude/extra")])].into(),
        ..Default::default()
    }
}

/// 13: the takeover rule over the default and an added profile.
#[test]
fn takeover_matrix() {
    for bits in 0..32u8 {
        let (enabled, in_default, in_added, cli_files, confirmed) = (
            bits & 1 != 0,
            bits & 2 != 0,
            bits & 4 != 0,
            bits & 8 != 0,
            bits & 16 != 0,
        );
        let home = tempfile::tempdir().unwrap();
        let app = app_dir_in(home.path());
        let cli = home.path().join(".config/tokscale/cursor-cache");
        std::fs::create_dir_all(&cli).unwrap();
        std::fs::create_dir_all(app.join("cursor-cache")).unwrap();
        let mut files = Vec::new();
        if in_default {
            files.push(write_named_file(&app.join("cursor-cache"), 'a'));
        }
        if in_added {
            files.push(write_named_file(
                &app.join("cursor-profiles").join(PROFILE_A),
                'b',
            ));
        }
        if cli_files {
            std::fs::write(cli.join("usage.csv"), "x").unwrap();
        }
        let config = Config {
            enabled,
            app_dir: Some(app.clone()),
            profiles: vec![Profile {
                id: PROFILE_A.to_string(),
                root: PathBuf::from("/Users/x/a"),
            }],
            cli_takeover_confirmed: confirmed,
        };
        let mut settings = claude_settings();
        apply_takeover(&mut settings, &config, Some(home.path()));

        let complete = in_default || in_added;
        let on = enabled && complete && (!cli_files || confirmed);
        let label = format!(
            "enabled={enabled} default={in_default} added={in_added} cli={cli_files} confirmed={confirmed}"
        );
        assert_eq!(
            settings.extra_scan_paths["claude"],
            [PathBuf::from("/claude/extra")],
            "{label}"
        );
        assert_eq!(
            settings.extra_scan_paths.get("cursor").cloned(),
            on.then(|| files.clone()),
            "{label}"
        );
        assert_eq!(
            settings.excluded_scan_paths.get("cursor").cloned(),
            on.then(|| vec![cli.clone()]),
            "{label}"
        );
        let blocked = enabled && complete && cli_files && !confirmed;
        assert_eq!(
            takeover(&config, Some(home.path())) == Takeover::Blocked,
            blocked,
            "{label}"
        );
    }
}

#[test]
fn cli_file_detection_errs_toward_asking() {
    let home = tempfile::tempdir().unwrap();
    let cli = home.path().join("cursor-cache");
    assert!(!cli_has_cursor_files(&cli), "a missing root holds nothing");
    std::fs::create_dir_all(cli.join("archive")).unwrap();
    std::fs::write(cli.join("usage.last-sync-attempt"), "x").unwrap();
    assert!(!cli_has_cursor_files(&cli), "control: no usage data file");
    std::fs::write(cli.join("archive/usage.backup-1.csv"), "x").unwrap();
    assert!(cli_has_cursor_files(&cli));
    std::fs::remove_file(cli.join("archive/usage.backup-1.csv")).unwrap();
    std::fs::write(cli.join("usage.json"), "{}").unwrap();
    assert!(cli_has_cursor_files(&cli));
}

/// The one test that sets the global registry with a complete file. That file
/// holds zero events, and other tests' homes have no CLI Cursor files, so the
/// moment it is visible to a concurrent test it changes no total.
#[test]
fn local_source_context_carries_the_takeover_and_keeps_claude_roots() {
    let _guard = lock();
    let _extra = crate::extra_scan_paths::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let home = tempfile::tempdir().unwrap();
    let claude = home.path().join("claude-extra");
    crate::extra_scan_paths::set_from_json(&serde_json::json!({ "claude": [claude] }).to_string())
        .unwrap();
    let app = app_dir_in(home.path());
    let file = write_complete_file(&app.join("cursor-cache"));
    let context = crate::LocalSourceContext::for_home(home.path().to_path_buf());
    let cli = home.path().join(".config/tokscale/cursor-cache");

    set_for_test(Config {
        enabled: true,
        app_dir: Some(app.clone()),
        ..Config::default()
    });
    for settings in [
        context.report_options(None, None).scanner_settings,
        context.parse_options(None, None).scanner_settings,
    ] {
        assert_eq!(
            settings.extra_scan_paths["claude"],
            std::slice::from_ref(&claude)
        );
        assert_eq!(
            settings.extra_scan_paths["cursor"],
            std::slice::from_ref(&file)
        );
        assert_eq!(
            settings.excluded_scan_paths["cursor"],
            std::slice::from_ref(&cli)
        );
    }
    // Control: sync off → the context is exactly the registry's.
    set_for_test(Config::default());
    let settings = context.report_options(None, None).scanner_settings;
    assert_eq!(
        settings.extra_scan_paths["claude"],
        std::slice::from_ref(&claude)
    );
    assert!(!settings.extra_scan_paths.contains_key("cursor"));
    assert!(settings.excluded_scan_paths.is_empty());
    crate::extra_scan_paths::reset_for_test();
}

// --- end to end: synced file → engine report ---------------------------------

/// Points the engine's cache and pricing at a temp dir for one test.
/// `TOKSCALE_CONFIG_DIR` keeps the source cache out of the real
/// `~/.config/tokscale`; `TOKSCALE_PRICING_CACHE_ONLY` keeps the report from
/// fetching pricing, so every cost below is the one the files report.
struct EngineEnv {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EngineEnv {
    fn set(config_dir: &Path) -> Self {
        let keys = ["TOKSCALE_CONFIG_DIR", "TOKSCALE_PRICING_CACHE_ONLY"];
        let saved = keys.iter().map(|k| (*k, std::env::var_os(k))).collect();
        std::env::set_var("TOKSCALE_CONFIG_DIR", config_dir);
        std::env::set_var("TOKSCALE_PRICING_CACHE_ONLY", "1");
        Self { saved }
    }
}

impl Drop for EngineEnv {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// Two events of one usage history. `chargedCents` differs from `totalCents`
/// so the cost proves the engine took `totalCents`.
fn e2e_event(
    id: &str,
    ts: &str,
    input: i64,
    output: i64,
    cache_read: i64,
    total_cents: f64,
) -> serde_json::Value {
    serde_json::json!({
        "conversationId": id, "timestamp": ts, "model": "gpt-5",
        "chargedCents": 1,
        "tokenUsage": {"inputTokens": input, "outputTokens": output,
                       "cacheReadTokens": cache_read, "totalCents": total_cents},
        "owningUser": OWNING_CANARY,
    })
}

/// The CLI's CSV of the same two events.
const SAME_USAGE_CSV: &str = "Date,Kind,Model,Max Mode,Input (w/ Cache Write),Input (w/o Cache Write),Cache Read,Output Tokens,Total Tokens,Cost\n\
\"2026-09-01T10:00:00.000Z\",\"Included\",\"gpt-5\",\"No\",\"0\",\"100\",\"30\",\"20\",\"150\",\"0.50\"\n\
\"2026-09-01T11:00:00.000Z\",\"Included\",\"gpt-5\",\"No\",\"0\",\"200\",\"0\",\"40\",\"240\",\"0.25\"\n";

/// (input, output, cache_read, cost, sorted session ids) of the Cursor rows.
fn cursor_report(home: &Path, config: Option<Config>) -> (i64, i64, i64, f64, Vec<String>) {
    cursor_report_with(home, config, None)
}

/// `legacy_dir`: register that directory as the Cursor root the way builds
/// before CMP-520 did (rollback check), instead of the takeover's files.
fn cursor_report_with(
    home: &Path,
    config: Option<Config>,
    legacy_dir: Option<&Path>,
) -> (i64, i64, i64, f64, Vec<String>) {
    set_thread_config_for_test(config);
    let mut options = tokscale_core::ReportOptions {
        group_by: tokscale_core::GroupBy::Session,
        ..crate::LocalSourceContext::for_home(home.to_path_buf())
            .report_options(None, Some(vec!["cursor".to_string()]))
    };
    set_thread_config_for_test(None);
    if let Some(dir) = legacy_dir {
        let settings = &mut options.scanner_settings;
        settings
            .extra_scan_paths
            .insert("cursor".to_string(), vec![dir.to_path_buf()]);
        settings.excluded_scan_paths.insert(
            "cursor".to_string(),
            vec![home.join(".config/tokscale/cursor-cache")],
        );
    }
    let report = crate::RUNTIME
        .block_on(tokscale_core::get_model_report(options))
        .expect("model report");
    let mut sessions: Vec<String> = report
        .entries
        .iter()
        .map(|e| e.session_id.clone().unwrap_or_default())
        .collect();
    sessions.sort();
    (
        report.total_input,
        report.total_output,
        report.total_cache_read,
        report.total_cost,
        sessions,
    )
}

#[test]
fn synced_file_reaches_the_report_once() {
    let _guard = lock();
    let tmp = tempfile::tempdir().unwrap();
    let _env = EngineEnv::set(&tmp.path().join("tokscale-config"));
    let home = tmp.path().join("home");
    let dir = home.join("Library/Application Support/test/cursor-cache");
    std::fs::create_dir_all(&home).unwrap();
    let db = signed_in_db(tmp.path(), &valid_token());

    // Sync: two events through the local server into the real file format.
    let body = serde_json::json!({
        "totalUsageEventsCount": 2,
        "usageEventsDisplay": [
            e2e_event("conv-a", "1788256800000", 100, 20, 30, 50.0),
            e2e_event("conv-b", "1788260400000", 200, 40, 0, 25.0),
        ],
    });
    let (url, _) = serve(vec![json_ok(&body.to_string())]);
    assert_eq!(
        run_with(
            &db,
            &dir,
            &url,
            Limits {
                page_size: 500,
                ..limits()
            },
            &|| true
        ),
        ok(2)
    );
    let synced = (300, 60, 30);
    let synced_sessions = vec!["conv-a".to_string(), "conv-b".to_string()];
    let on = |confirmed| Config {
        cli_takeover_confirmed: confirmed,
        ..config_for(true, &dir)
    };

    // No CLI files: the synced file alone, cost from totalCents (0.75), not
    // chargedCents (0.02), and not estimated (pricing is unavailable here).
    let (input, output, cache_read, cost, sessions) = cursor_report(&home, Some(on(false)));
    assert_eq!((input, output, cache_read), synced);
    assert!((cost - 0.75).abs() < 1e-9, "cost {cost}");
    assert_eq!(sessions, synced_sessions);

    // The CLI's CSV of the same usage appears.
    let cli = home.join(".config/tokscale/cursor-cache");
    std::fs::create_dir_all(&cli).unwrap();
    std::fs::write(cli.join("usage.csv"), SAME_USAGE_CSV).unwrap();

    // Sync off: the CSV alone, and it is the same usage (control for "once").
    let csv = cursor_report(&home, Some(Config::default()));
    assert_eq!(
        (csv.0, csv.1, csv.2),
        synced,
        "the CSV fixture is not the same usage"
    );
    assert!((csv.3 - 0.75).abs() < 1e-9, "csv cost {}", csv.3);
    assert!(
        csv.4.iter().all(|s| !synced_sessions.contains(s)),
        "{:?}",
        csv.4
    );

    // Not confirmed (cliPresent): takeover off, the synced file is not added.
    assert_eq!(cursor_report(&home, Some(on(false))), csv);

    // Confirmed: counted once, and that once is the synced file.
    let (input, output, cache_read, cost, sessions) = cursor_report(&home, Some(on(true)));
    assert_eq!(
        (input, output, cache_read),
        synced,
        "double counted or lost"
    );
    assert!((cost - 0.75).abs() < 1e-9, "cost {cost}");
    assert_eq!(
        sessions, synced_sessions,
        "takeover did not switch to the synced file"
    );
}

// --- several Cursor profiles (CMP-520) ---------------------------------------

/// A usage file as the sync writes it, named for `account` (stands in for
/// `HMAC(sub)`), `age_secs` old.
fn usage_file(dir: &Path, account: char, events: &[serde_json::Value], age_secs: u64) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(hex_name(account));
    let body =
        serde_json::json!({"totalUsageEventsCount": events.len(), "usageEventsDisplay": events});
    std::fs::write(&path, body.to_string()).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(age_secs))
        .unwrap();
    path
}

fn profile(id: &str, root: &Path) -> Profile {
    Profile {
        id: id.to_string(),
        root: root.to_path_buf(),
    }
}

fn multi(home: &Path, enabled: bool, profiles: Vec<Profile>) -> Config {
    Config {
        enabled,
        app_dir: Some(app_dir_in(home)),
        profiles,
        cli_takeover_confirmed: false,
    }
}

fn profile_dir(home: &Path, id: &str) -> PathBuf {
    app_dir_in(home).join("cursor-profiles").join(id)
}

fn default_sync_dir(home: &Path) -> PathBuf {
    app_dir_in(home).join("cursor-cache")
}

fn x_event() -> serde_json::Value {
    e2e_event("conv-x", "1788256800000", 100, 20, 30, 50.0)
}

fn y_event() -> serde_json::Value {
    e2e_event("conv-y", "1788260400000", 200, 40, 0, 25.0)
}

fn z_event() -> serde_json::Value {
    e2e_event("conv-z", "1788264000000", 400, 80, 0, 10.0)
}

fn engine_home(tmp: &Path) -> (EngineEnv, PathBuf) {
    let env = EngineEnv::set(&tmp.join("tokscale-config"));
    let home = tmp.join("home");
    std::fs::create_dir_all(&home).unwrap();
    (env, home)
}

/// 1 ✱: one account synced into two profiles (same file name) is counted
/// once. Mutation: key `newest_per_account` by path instead of file name.
#[test]
fn same_account_in_two_profiles_counts_once() {
    let _guard = lock();
    let tmp = tempfile::tempdir().unwrap();
    let (_env, home) = engine_home(tmp.path());
    let config = multi(
        &home,
        true,
        vec![profile(PROFILE_A, Path::new("/Users/x/a"))],
    );
    usage_file(&default_sync_dir(&home), 'a', &[x_event()], 10);
    let copy = usage_file(&profile_dir(&home, PROFILE_A), 'a', &[x_event()], 0);

    let (input, output, _, cost, sessions) = cursor_report(&home, Some(config.clone()));
    assert_eq!((input, output), (100, 20), "double counted");
    assert!((cost - 0.5).abs() < 1e-9, "cost {cost}");
    assert_eq!(sessions, ["conv-x"]);

    // Control: a different account in the added profile counts as well.
    std::fs::remove_file(copy).unwrap();
    usage_file(&profile_dir(&home, PROFILE_A), 'b', &[y_event()], 0);
    let (input, output, _, cost, sessions) = cursor_report(&home, Some(config));
    assert_eq!((input, output), (300, 60));
    assert!((cost - 0.75).abs() < 1e-9, "cost {cost}");
    assert_eq!(sessions, ["conv-x", "conv-y"]);
}

/// 2 ✱: dir A holds X (newest) and an old Y left by a failed delete; dir B
/// holds the newest Y; dir C holds another account Z. Each counts once, and
/// Z is not hidden. Mutation: register the dirs instead of the files.
#[test]
fn a_stale_copy_in_another_profile_is_never_scanned() {
    let _guard = lock();
    let tmp = tempfile::tempdir().unwrap();
    let (_env, home) = engine_home(tmp.path());
    let config = multi(
        &home,
        true,
        vec![
            profile(PROFILE_A, Path::new("/Users/x/a")),
            profile(PROFILE_B, Path::new("/Users/x/b")),
        ],
    );
    usage_file(&default_sync_dir(&home), 'a', &[x_event()], 0);
    usage_file(&default_sync_dir(&home), 'b', &[y_event()], 100);
    let newest_y = usage_file(&profile_dir(&home, PROFILE_A), 'b', &[y_event()], 0);
    usage_file(&profile_dir(&home, PROFILE_B), 'c', &[z_event()], 50);

    let (input, output, _, cost, sessions) = cursor_report(&home, Some(config.clone()));
    assert_eq!((input, output), (700, 140), "a stale copy was counted");
    assert!((cost - 0.85).abs() < 1e-9, "cost {cost}");
    assert_eq!(sessions, ["conv-x", "conv-y", "conv-z"]);
    // The chosen Y is the newest copy, by path.
    let mut settings = tokscale_core::scanner::ScannerSettings::default();
    apply_takeover(&mut settings, &config, Some(&home));
    assert!(settings.extra_scan_paths["cursor"].contains(&std::fs::canonicalize(newest_y).unwrap()));
    assert_eq!(settings.extra_scan_paths["cursor"].len(), 3);
}

/// 3: the default profile's existing file keeps counting once, alone or with
/// an added profile on the same account; only files are registered.
#[test]
fn upgrade_keeps_the_existing_file_and_registers_no_directory() {
    let _guard = lock();
    let tmp = tempfile::tempdir().unwrap();
    let (_env, home) = engine_home(tmp.path());
    usage_file(&default_sync_dir(&home), 'a', &[x_event(), y_event()], 0);
    for config in [
        multi(&home, true, vec![]),
        multi(
            &home,
            true,
            vec![profile(PROFILE_A, Path::new("/Users/x/a"))],
        ),
    ] {
        if !config.profiles.is_empty() {
            usage_file(
                &profile_dir(&home, PROFILE_A),
                'a',
                &[x_event(), y_event()],
                5,
            );
        }
        let (input, _, _, _, sessions) = cursor_report(&home, Some(config.clone()));
        assert_eq!(input, 300, "{config:?}");
        assert_eq!(sessions, ["conv-x", "conv-y"]);
        let mut settings = tokscale_core::scanner::ScannerSettings::default();
        apply_takeover(&mut settings, &config, Some(&home));
        let cursor = &settings.extra_scan_paths["cursor"];
        assert!(!cursor.is_empty());
        assert!(
            cursor.iter().all(|p| p.is_file()),
            "a directory was registered: {cursor:?}"
        );
    }
}

/// Rollback: a build from before CMP-520 registers `cursor-cache` as a dir.
/// The added profile's copy of the same account lives in the sibling
/// `cursor-profiles`, which that build never walks: counted once.
#[test]
fn rollback_scan_of_cursor_cache_counts_the_account_once() {
    let _guard = lock();
    let tmp = tempfile::tempdir().unwrap();
    let (_env, home) = engine_home(tmp.path());
    usage_file(&default_sync_dir(&home), 'a', &[x_event()], 0);
    usage_file(&profile_dir(&home, PROFILE_A), 'a', &[x_event()], 0);
    usage_file(&profile_dir(&home, PROFILE_B), 'b', &[y_event()], 0);
    let (input, _, _, _, sessions) = cursor_report_with(
        &home,
        Some(Config::default()),
        Some(&default_sync_dir(&home)),
    );
    assert_eq!(input, 100);
    assert_eq!(sessions, ["conv-x"]);
}

// Sync rounds over several profiles.

fn login_db(dir: &Path, token: &str, user: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    state_db(
        dir,
        &[
            ("cursorAuth/accessToken", &format!("\"{token}\"")),
            ("glass.lastSignedInAuthId", &format!("glass-{user}")),
        ],
    )
}

/// A `--user-data-dir` root whose login is `token` for `user`.
fn profile_root(base: &Path, name: &str, token: &str, user: &str) -> PathBuf {
    let root = base.join(name);
    login_db(&root.join("User/globalStorage"), token, user);
    root
}

fn other_token() -> String {
    jwt(&format!("auth0|{OTHER_USER}"), Some(now_secs() + 3600.0))
}

fn sync_round(default_db: &Path, url: &str, registry: &RwLock<Config>) -> serde_json::Value {
    sync_round_with(default_db, url, registry, limits())
}

fn sync_round_with(
    default_db: &Path,
    url: &str,
    registry: &RwLock<Config>,
    limits: Limits,
) -> serde_json::Value {
    sync_round_in(default_db, url, registry, limits, None)
}

fn sync_round_in(
    default_db: &Path,
    url: &str,
    registry: &RwLock<Config>,
    limits: Limits,
    home: Option<&Path>,
) -> serde_json::Value {
    let billing = v2_url(url);
    let target = Target {
        db_path: default_db,
        url,
        billing_url: &billing,
        https_only: false,
        file_stem: &stem,
        key_fetch: None,
        consent: &no_consent,
    };
    let source = || registry.read().unwrap().clone();
    sync_with(true, home, &target, &source, Some(limits)).0
}

fn states(status: &serde_json::Value) -> Vec<String> {
    status["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            let reason = p["reason"]
                .as_str()
                .map(|r| format!(":{r}"))
                .unwrap_or_default();
            format!("{}{reason}", p["state"].as_str().unwrap())
        })
        .collect()
}

/// The same account in a second profile is not walked again this round;
/// its status names the profile that did walk it, never the account.
#[test]
fn a_second_profile_on_the_same_account_is_not_walked_again() {
    let _guard = lock();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let token = valid_token();
    let default_db = login_db(&tmp.path().join("default"), &token, USER);
    let root = profile_root(tmp.path(), "b", &token, USER);
    let registry = RwLock::new(multi(&home, true, vec![profile(PROFILE_A, &root)]));
    let (url, seen) = serve(vec![json_ok(&page(1, &["a"])), json_ok(&page(1, &["a"]))]);
    let status = sync_round(&default_db, &url, &registry);
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "the account was walked twice"
    );
    assert_eq!(states(&status), ["ok", "duplicate"]);
    assert_eq!(status["profiles"][1]["duplicateOf"], "default");
    assert_eq!(status["state"], "ok");
    assert!(expected_file(&default_sync_dir(&home)).exists());
    assert!(!profile_dir(&home, PROFILE_A).exists());
    // Control: a different account in the second profile is walked.
    let other = profile_root(tmp.path(), "c", &other_token(), OTHER_USER);
    *registry.write().unwrap() = multi(&home, true, vec![profile(PROFILE_A, &other)]);
    let (url, seen) = serve(vec![json_ok(&page(1, &["a"])), json_ok(&page(1, &["b"]))]);
    let status = sync_round(&default_db, &url, &registry);
    assert_eq!(seen.lock().unwrap().len(), 2);
    assert_eq!(states(&status), ["ok", "ok"]);
    assert_eq!(status["events"], 2);
}

/// 4: an account switch in one profile leaves only the new account's file,
/// and the report shows only the new account's usage there.
#[test]
fn account_switch_in_a_profile_leaves_only_the_new_file() {
    let _guard = lock();
    let tmp = tempfile::tempdir().unwrap();
    let (_env, home) = engine_home(tmp.path());
    let old = usage_file(&profile_dir(&home, PROFILE_A), 'a', &[x_event()], 0);
    let root = profile_root(tmp.path(), "b", &other_token(), OTHER_USER);
    let registry = RwLock::new(multi(&home, true, vec![profile(PROFILE_A, &root)]));
    let body = serde_json::json!({"totalUsageEventsCount": 1, "usageEventsDisplay": [y_event()]});
    let (url, _) = serve(vec![json_ok(&body.to_string())]);
    let status = sync_round(&tmp.path().join("absent.vscdb"), &url, &registry);
    assert_eq!(states(&status), ["notSignedIn", "ok"]);
    assert_eq!(
        status["state"], "ok",
        "no default login is not a problem here"
    );
    assert!(!old.exists());
    assert_eq!(
        names_in(&profile_dir(&home, PROFILE_A)),
        [format!("usage.{}.json", stem(OTHER_USER).unwrap())]
    );
    let (input, _, _, _, sessions) = cursor_report(&home, Some(registry.read().unwrap().clone()));
    assert_eq!(input, 200);
    assert_eq!(sessions, ["conv-y"]);
}

/// 7 ✱: a profile database that is a symlink, not a regular file, or owned
/// by someone else is refused with a fixed reason before any request.
#[cfg(unix)]
#[test]
fn unsafe_profile_databases_send_nothing() {
    let _guard = lock();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let real = profile_root(tmp.path(), "real", &valid_token(), USER);
    let linked = tmp.path().join("linked");
    std::fs::create_dir_all(linked.join("User/globalStorage")).unwrap();
    std::os::unix::fs::symlink(
        real.join("User/globalStorage/state.vscdb"),
        linked.join("User/globalStorage/state.vscdb"),
    )
    .unwrap();
    let directory = tmp.path().join("directory");
    std::fs::create_dir_all(directory.join("User/globalStorage/state.vscdb")).unwrap();
    // (A foreign owner: `cursor_desktop::checked_open_tests`.)
    let registry = RwLock::new(multi(
        &home,
        true,
        vec![profile(PROFILE_A, &linked), profile(PROFILE_B, &directory)],
    ));
    let (url, seen) = serve(vec![json_ok(&page(1, &["a"]))]);
    let status = sync_round(&tmp.path().join("absent.vscdb"), &url, &registry);
    assert!(
        seen.lock().unwrap().is_empty(),
        "an unsafe database was sent"
    );
    assert_eq!(
        states(&status),
        ["notSignedIn", "error:db_symlink", "error:db_not_file"]
    );
    // Control: the real file behind the symlink is sent.
    *registry.write().unwrap() = multi(&home, true, vec![profile(PROFILE_A, &real)]);
    let status = sync_round(&tmp.path().join("absent.vscdb"), &url, &registry);
    assert_eq!(states(&status), ["notSignedIn", "ok"]);
    assert_eq!(seen.lock().unwrap().len(), 1);
}

/// 8 ✱: each profile's token goes only in its own requests, and no token
/// or account id reaches a file name, a file, or the status.
#[test]
fn each_profile_token_reaches_only_its_own_requests() {
    let _guard = lock();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let (token_a, token_b) = (valid_token(), other_token());
    let default_db = login_db(&tmp.path().join("default"), &token_a, USER);
    let root = profile_root(tmp.path(), "b", &token_b, OTHER_USER);
    let registry = RwLock::new(multi(&home, true, vec![profile(PROFILE_A, &root)]));
    let (url, seen) = serve(vec![
        json_ok(&page(3, &["a", "b"])),
        json_ok(&page(3, &["c"])),
        json_ok(&page(1, &["d"])),
    ]);
    let status = sync_round(&default_db, &url, &registry);
    assert_eq!(states(&status), ["ok", "ok"]);
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 3);
    for (i, request) in seen.iter().enumerate() {
        let cookie = header(&request.head, "cookie").unwrap();
        let (own, other) = if i < 2 {
            (&token_a, &token_b)
        } else {
            (&token_b, &token_a)
        };
        assert!(
            cookie.contains(own.as_str()),
            "request {i} lacks its own token"
        );
        assert!(!request.head.contains(other.as_str()) && !request.body.contains(other.as_str()));
    }
    let mut outputs = vec![status.to_string()];
    for dir in [default_sync_dir(&home), profile_dir(&home, PROFILE_A)] {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            outputs.push(entry.file_name().to_string_lossy().into_owned());
            outputs.push(std::fs::read_to_string(entry.path()).unwrap_or_default());
        }
    }
    let all = outputs.join("\n");
    assert!(all.contains("usage.") && all.contains("conversationId") && all.contains("\"ok\""));
    for canary in [
        token_a.as_str(),
        token_b.as_str(),
        SIGNATURE,
        USER,
        OTHER_USER,
    ] {
        assert!(!all.contains(canary), "canary leaked");
    }
}

/// 9: the profile's root changes while its walk runs → nothing is written
/// for that walk (Swift's controller then reruns on the new config).
#[test]
fn root_changed_mid_walk_writes_nothing() {
    let _guard = lock();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let root = profile_root(tmp.path(), "b", &valid_token(), USER);
    let registry = RwLock::new(multi(&home, true, vec![profile(PROFILE_A, &root)]));
    let reply = match json_ok(&page(1, &["a"])) {
        Reply::Raw(bytes) => bytes,
        _ => unreachable!(),
    };
    let (url, seen) = serve(vec![Reply::Delayed(Duration::from_millis(300), reply)]);
    let status = std::thread::scope(|scope| {
        let walk = scope.spawn(|| sync_round(&tmp.path().join("absent.vscdb"), &url, &registry));
        let started = Instant::now();
        while seen.lock().unwrap().is_empty() {
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
        registry.write().unwrap().profiles[0].root = tmp.path().join("moved");
        walk.join().unwrap()
    });
    assert_eq!(states(&status), ["notSignedIn", "disabled"]);
    assert!(sync_files(&profile_dir(&home, PROFILE_A)).is_empty());
}

/// 10 ✱: sync off sends nothing for any profile; turned off during the
/// first profile's walk, the next profile sends nothing.
#[test]
fn switch_off_sends_nothing_for_every_profile() {
    let _guard = lock();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let default_db = login_db(&tmp.path().join("default"), &valid_token(), USER);
    let root = profile_root(tmp.path(), "b", &other_token(), OTHER_USER);
    let registry = RwLock::new(multi(&home, false, vec![profile(PROFILE_A, &root)]));
    let reply = match json_ok(&page(1, &["a"])) {
        Reply::Raw(bytes) => bytes,
        _ => unreachable!(),
    };
    let (url, seen) = serve(vec![
        Reply::Delayed(Duration::from_millis(300), reply.clone()),
        Reply::Raw(reply),
    ]);
    let status = sync_round(&default_db, &url, &registry);
    assert_eq!(status["state"], "disabled");
    assert!(seen.lock().unwrap().is_empty());

    registry.write().unwrap().enabled = true;
    let status = std::thread::scope(|scope| {
        let walk = scope.spawn(|| sync_round(&default_db, &url, &registry));
        let started = Instant::now();
        while seen.lock().unwrap().is_empty() {
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
        registry.write().unwrap().enabled = false;
        walk.join().unwrap()
    });
    assert_eq!(states(&status), ["disabled", "disabled"]);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "the next profile sent after off"
    );
    assert!(!expected_file(&default_sync_dir(&home)).exists());
}

/// 12: an S-1 mismatch in one profile refuses only that profile.
#[test]
fn account_mismatch_refuses_only_its_own_profile() {
    let _guard = lock();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let mismatched = login_db(&tmp.path().join("default"), &valid_token(), OTHER_USER);
    let root = profile_root(tmp.path(), "b", &other_token(), OTHER_USER);
    let registry = RwLock::new(multi(&home, true, vec![profile(PROFILE_A, &root)]));
    let (url, seen) = serve(vec![json_ok(&page(1, &["a"]))]);
    let status = sync_round(&mismatched, &url, &registry);
    assert_eq!(states(&status), ["error:account_mismatch", "ok"]);
    assert_eq!(status["state"], "error");
    // FIX 1: a file was written although the headline is an error.
    assert_eq!(status["changed"], true);
    assert_eq!(status["reason"], "account_mismatch");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert!(header(&seen[0].head, "cookie")
        .unwrap()
        .contains(OTHER_USER));
}

/// One budget for the whole round: a spent one starts no profile.
#[test]
fn one_budget_covers_every_profile() {
    let _guard = lock();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let default_db = login_db(&tmp.path().join("default"), &valid_token(), USER);
    let root = profile_root(tmp.path(), "b", &other_token(), OTHER_USER);
    let registry = RwLock::new(multi(&home, true, vec![profile(PROFILE_A, &root)]));
    let (url, seen) = serve(vec![json_ok(&page(1, &["a"])), json_ok(&page(1, &["b"]))]);
    let status = sync_round_with(
        &default_db,
        &url,
        &registry,
        Limits {
            budget: Duration::ZERO,
            ..limits()
        },
    );
    assert_eq!(
        states(&status),
        ["partial:budget_exhausted", "partial:budget_exhausted"]
    );
    assert!(seen.lock().unwrap().is_empty());
}

/// FIX 2 ✱: an account is covered only by a walk that synced. The default
/// profile's session for X is refused (401); profile B on the same account
/// then tries its own session, and X still counts once.
#[test]
fn a_failed_walk_does_not_cover_the_account_for_later_profiles() {
    let _guard = lock();
    let tmp = tempfile::tempdir().unwrap();
    let (_env, home) = engine_home(tmp.path());
    let default_db = login_db(&tmp.path().join("default"), &valid_token(), USER);
    let root = profile_root(tmp.path(), "b", &valid_token(), USER);
    let registry = RwLock::new(multi(&home, true, vec![profile(PROFILE_A, &root)]));
    let body = serde_json::json!({"totalUsageEventsCount": 1, "usageEventsDisplay": [x_event()]});
    let (url, seen) = serve(vec![
        http("401 Unauthorized", "application/json", "{}"),
        json_ok(&body.to_string()),
    ]);
    let status = sync_round(&default_db, &url, &registry);
    assert_eq!(states(&status), ["expired", "ok"]);
    assert_eq!(
        seen.lock().unwrap().len(),
        2,
        "B never tried its own session"
    );
    assert!(expected_file(&profile_dir(&home, PROFILE_A)).exists());
    let (input, _, _, _, sessions) = cursor_report(&home, Some(registry.read().unwrap().clone()));
    assert_eq!(input, 100);
    assert_eq!(sessions, ["conv-x"]);
}

/// FIX 6 ✱: the default profile follows a symlinked `state.vscdb` (as every
/// release did, including through `TOKENBAR_CURSOR_STATE_VSCDB`); an added
/// profile's symlinked database is still refused.
#[cfg(unix)]
#[test]
fn a_symlinked_default_database_still_syncs() {
    let _guard = lock();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let real = login_db(&tmp.path().join("real"), &valid_token(), USER);
    let link = tmp.path().join("link.vscdb");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let added = tmp.path().join("added");
    std::fs::create_dir_all(added.join("User/globalStorage")).unwrap();
    std::os::unix::fs::symlink(
        login_db(&tmp.path().join("other"), &other_token(), OTHER_USER),
        added.join("User/globalStorage/state.vscdb"),
    )
    .unwrap();
    let registry = RwLock::new(multi(&home, true, vec![profile(PROFILE_A, &added)]));
    let (url, seen) = serve(vec![json_ok(&page(1, &["a"])), json_ok(&page(1, &["b"]))]);
    let status = sync_round(&link, &url, &registry);
    assert_eq!(states(&status), ["ok", "error:db_symlink"]);
    assert_eq!(seen.lock().unwrap().len(), 1);
    // A default symlink to a directory is "not signed in", as `read_login`
    // treated it (owner: `cursor_desktop::checked_open_tests`).
    let to_dir = tmp.path().join("dir-link.vscdb");
    std::os::unix::fs::symlink(tmp.path(), &to_dir).unwrap();
    let status = sync_round(&to_dir, &url, &registry);
    assert_eq!(states(&status)[0], "notSignedIn");
    assert_eq!(seen.lock().unwrap().len(), 1);
}

/// FIX 5: when the round becomes `cliPresent`, no other profile's reason
/// rides along.
#[test]
fn cli_present_carries_no_other_reason() {
    let _guard = lock();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let cli = home.join(".config/tokscale/cursor-cache");
    std::fs::create_dir_all(&cli).unwrap();
    std::fs::write(cli.join("usage.csv"), "x").unwrap();
    let mismatched = login_db(&tmp.path().join("default"), &valid_token(), OTHER_USER);
    let root = profile_root(tmp.path(), "b", &other_token(), OTHER_USER);
    let registry = RwLock::new(multi(&home, true, vec![profile(PROFILE_A, &root)]));
    let (url, _) = serve(vec![json_ok(&page(1, &["a"]))]);
    let status = sync_round_in(&mismatched, &url, &registry, limits(), Some(&home));
    assert_eq!(states(&status), ["error:account_mismatch", "ok"]);
    assert_eq!(
        (status["state"].as_str(), status.get("reason")),
        (Some("cliPresent"), None)
    );
}

/// Round 2, items 1 and 2: the appDir in use until now is re-checked before
/// an off push cleans it. Replaced by a symlink into a tokscale-like folder,
/// nothing there is touched (no delete, no lock file) and the skipped
/// cleanup is reported. With no earlier appDir, a refused one (dangling
/// symlink, or no HOME) reports no failure.
#[cfg(unix)]
#[test]
fn an_off_push_never_cleans_a_previous_app_dir_that_now_fails_the_checks() {
    let _guard = lock();
    set_for_test(Config::default());
    let home = tempfile::tempdir().unwrap();
    let app = app_dir_in(home.path());
    std::fs::create_dir_all(&app).unwrap();
    set_in(
        home.path(),
        serde_json::json!({"enabled": true, "bundleId": BUNDLE}),
    )
    .unwrap();
    // Keep the old appDir but off while the outside file exists, so no other
    // test's takeover can see it; the cleanup branch reads only the appDir.
    set_for_test(Config {
        enabled: false,
        ..config()
    });
    let outside = home.path().join(".config/tokscale/scratch");
    let outside_file = outside.join("cursor-cache").join(hex_name('a'));
    std::fs::create_dir_all(outside_file.parent().unwrap()).unwrap();
    std::fs::write(&outside_file, "x").unwrap();
    std::fs::remove_dir(&app).unwrap();
    std::os::unix::fs::symlink(&outside, &app).unwrap();

    let result = set_in(
        home.path(),
        serde_json::json!({"enabled": false, "bundleId": BUNDLE}),
    )
    .unwrap();
    assert!(!config().enabled);
    assert_eq!(result["appDir"], serde_json::Value::Null);
    assert_eq!(result["cleanupFailed"], true);
    assert_eq!(
        result["cleanupFailedProfiles"],
        serde_json::json!(["appDir"])
    );
    assert!(
        outside_file.exists(),
        "a file outside the checked appDir was deleted"
    );
    assert_eq!(
        std::fs::read_dir(outside_file.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        [hex_name('a')],
        "something (a lock file) was created outside"
    );

    // No earlier appDir: a dangling symlink, then no HOME at all.
    for (home, bundle) in [(Some(home.path()), "com.example.dangling"), (None, BUNDLE)] {
        set_for_test(Config::default());
        if let Some(home) = home {
            std::os::unix::fs::symlink(
                home.join("nowhere"),
                home.join("Library/Application Support/com.example.dangling"),
            )
            .unwrap();
        }
        let result = set_from_json(
            &serde_json::json!({"enabled": false, "bundleId": bundle}).to_string(),
            home,
        )
        .unwrap();
        assert_eq!(result["cleanupFailed"], false, "{bundle}");
        assert_eq!(result["cleanupFailedProfiles"], serde_json::json!([]));
    }
    set_for_test(Config::default());
}

/// Round 2, item 4: an account-wide failure (429) still covers the account
/// for the round, so a second profile on it sends nothing.
#[test]
fn a_rate_limited_account_is_not_retried_from_another_profile() {
    let _guard = lock();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let default_db = login_db(&tmp.path().join("default"), &valid_token(), USER);
    let root = profile_root(tmp.path(), "b", &valid_token(), USER);
    let registry = RwLock::new(multi(&home, true, vec![profile(PROFILE_A, &root)]));
    let (url, seen) = serve(vec![
        http("429 Too Many Requests", "application/json", "{}"),
        json_ok(&page(1, &["a"])),
    ]);
    let status = sync_round(&default_db, &url, &registry);
    assert_eq!(states(&status), ["error:rate_limited", "duplicate"]);
    assert_eq!(seen.lock().unwrap().len(), 1, "the 429 was repeated from B");
}

/// Round 3, item 1: the appDir is re-checked at the start of every round.
/// Accepted, then replaced by a symlink into a tokscale-like folder: the
/// round writes nothing there, creates no lock, registers no takeover file,
/// and reports `app_dir_rejected`.
#[cfg(unix)]
#[test]
fn a_round_writes_nothing_into_an_app_dir_that_now_fails_the_checks() {
    let _guard = lock();
    set_for_test(Config::default());
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let app = app_dir_in(&home);
    std::fs::create_dir_all(&app).unwrap();
    set_in(
        &home,
        serde_json::json!({"enabled": true, "bundleId": BUNDLE}),
    )
    .unwrap();
    let registry = RwLock::new(config());
    set_for_test(Config::default());
    let outside = home.join(".config/tokscale/scratch");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::remove_dir(&app).unwrap();
    std::os::unix::fs::symlink(&outside, &app).unwrap();
    let default_db = login_db(&tmp.path().join("default"), &valid_token(), USER);
    let (url, seen) = serve(vec![json_ok(&page(1, &["a"]))]);
    let status = sync_round_in(&default_db, &url, &registry, limits(), Some(&home));
    assert_eq!(states(&status), ["error:app_dir_rejected"]);
    assert_eq!(status["reason"], "app_dir_rejected");
    assert_eq!(status["changed"], false);
    assert!(seen.lock().unwrap().is_empty());
    assert!(
        names_in(&outside).is_empty(),
        "something was created outside"
    );
    assert!(!outside.join(LOCK_FILE_NAME).exists());
    // A complete file placed there is never registered either.
    write_complete_file(&outside.join("cursor-cache"));
    assert_eq!(
        takeover(&registry.read().unwrap(), Some(&home)),
        Takeover::Off
    );
}

/// Round 3, item 3: a profile removed mid-walk (`disabled`) does not cover
/// its account, so the next profile on it still walks.
#[test]
fn a_profile_removed_mid_walk_does_not_cover_its_account() {
    let _guard = lock();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let a = profile_root(tmp.path(), "a", &valid_token(), USER);
    let b = profile_root(tmp.path(), "b", &valid_token(), USER);
    let registry = RwLock::new(multi(
        &home,
        true,
        vec![profile(PROFILE_A, &a), profile(PROFILE_B, &b)],
    ));
    let first_page = match json_ok(&page(4, &["a", "b"])) {
        Reply::Raw(bytes) => bytes,
        _ => unreachable!(),
    };
    let (url, seen) = serve(vec![
        Reply::Delayed(Duration::from_millis(300), first_page),
        json_ok(&page(1, &["c"])),
    ]);
    let status = std::thread::scope(|scope| {
        let walk = scope.spawn(|| sync_round(&tmp.path().join("absent.vscdb"), &url, &registry));
        let started = Instant::now();
        while seen.lock().unwrap().is_empty() {
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
        registry.write().unwrap().profiles.remove(0);
        walk.join().unwrap()
    });
    assert_eq!(states(&status), ["notSignedIn", "disabled", "ok"]);
    assert_eq!(
        seen.lock().unwrap().len(),
        2,
        "B was skipped as a duplicate"
    );
    assert!(covers_account(Stop::error("rate_limited")));
    assert!(!covers_account(Stop::new(State::Disabled, None)));
}

// --- multi-account audit fixes (macOS slice M, item 4) ------------------------

fn target<'a>(db: &'a Path, url: &'a str) -> Target<'a> {
    Target {
        db_path: db,
        url,
        billing_url: url,
        https_only: false,
        file_stem: &stem,
        key_fetch: None,
        consent: &no_consent,
    }
}

/// A profile root with no login database: it reports `notSignedIn`.
fn empty_root(base: &Path, name: &str) -> PathBuf {
    let root = base.join(name);
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// MA-22: a profile that now resolves to `duplicate` (signed in to the account
/// an earlier profile covered) drops the usage file of the account it held
/// before, under its lock, so that account is no longer registered. Its current
/// account's copy stays (same file name; the takeover keeps the newest).
#[test]
fn a_duplicate_profile_drops_its_old_accounts_file() {
    let _guard = lock();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let default_db = login_db(&tmp.path().join("default"), &valid_token(), USER);
    let root = profile_root(tmp.path(), "b", &valid_token(), USER);
    let registry = RwLock::new(multi(&home, true, vec![profile(PROFILE_A, &root)]));
    let a_dir = profile_dir(&home, PROFILE_A);
    let old = usage_file(&a_dir, 'e', &[], 0);
    let current = a_dir.join(format!("usage.{}.json", stem(USER).unwrap()));
    std::fs::write(&current, "{}").unwrap();
    assert!(
        newest_per_account(&registry.read().unwrap())
            .iter()
            .any(|p| p.ends_with(old.file_name().unwrap())),
        "control: the old account's file is registered before the round"
    );
    let (url, _) = serve(vec![json_ok(&page(1, &["a"])), json_ok(&page(1, &["a"]))]);
    let status = sync_round(&default_db, &url, &registry);
    assert_eq!(states(&status), ["ok", "duplicate"]);
    assert!(!old.exists(), "the old account's file stayed");
    assert!(current.exists(), "the current account's copy was removed");
    assert_eq!(status["cleanupFailedProfiles"], serde_json::json!([]));
    let registered = newest_per_account(&registry.read().unwrap());
    assert!(
        registered
            .iter()
            .all(|p| !p.ends_with(old.file_name().unwrap())),
        "{registered:?}"
    );
    assert_eq!(registered.len(), 1);
}

/// MA-22: a duplicate whose old file cannot be deleted says so (by profile id,
/// never a path), and a removal that did happen still counts as a change.
#[cfg(unix)]
#[test]
fn a_duplicates_failed_cleanup_is_reported() {
    use std::os::unix::fs::PermissionsExt as _;
    let _guard = lock();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let default_db = login_db(&tmp.path().join("default"), &valid_token(), USER);
    let root = profile_root(tmp.path(), "b", &valid_token(), USER);
    let registry = RwLock::new(multi(&home, true, vec![profile(PROFILE_A, &root)]));
    let a_dir = profile_dir(&home, PROFILE_A);
    let stuck = usage_file(&a_dir, 'e', &[], 0);
    let mode =
        |m: u32| std::fs::set_permissions(&a_dir, std::fs::Permissions::from_mode(m)).unwrap();
    mode(0o500);
    let (url, _) = serve(vec![json_ok(&page(1, &["a"])), json_ok(&page(1, &["a"]))]);
    let status = sync_round(&default_db, &url, &registry);
    mode(0o700);
    assert_eq!(states(&status), ["ok", "duplicate"]);
    assert!(stuck.exists(), "fixture: the delete was not refused");
    assert_eq!(
        status["cleanupFailedProfiles"],
        serde_json::json!([PROFILE_A])
    );
    // Control: once the file can go, nothing is reported.
    let status = sync_round(&default_db, &url, &registry);
    assert!(!stuck.exists());
    assert_eq!(status["cleanupFailedProfiles"], serde_json::json!([]));
}

/// MA-30: the 60 s background budget applies once every profile that could
/// write a file has one. A profile whose last round could not write one however
/// long it ran (no login, a duplicate, a refusal by the login reader) no longer
/// holds every later round at the full budget. "Sync Now" always takes the full
/// one, and so does a profile changed since (another root).
#[test]
fn a_profile_that_cannot_write_a_file_does_not_hold_the_full_budget() {
    let _guard = lock();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let default_db = login_db(&tmp.path().join("default"), &valid_token(), USER);
    let empty = empty_root(tmp.path(), "empty");
    let same = profile_root(tmp.path(), "same", &valid_token(), USER);
    // The login reader refuses a database that is not a regular file.
    let refused = tmp.path().join("refused");
    std::fs::create_dir_all(refused.join("User/globalStorage/state.vscdb")).unwrap();
    let registry = RwLock::new(multi(
        &home,
        true,
        vec![
            profile(PROFILE_A, &empty),
            profile(PROFILE_B, &same),
            profile(PROFILE_C, &refused),
        ],
    ));
    let (url, _) = serve(vec![json_ok(&page(1, &["a"])); 8]);
    let round = |explicit: bool| {
        let source = || registry.read().unwrap().clone();
        let status = sync_with(
            explicit,
            None,
            &target(&default_db, &url),
            &source,
            Some(limits()),
        )
        .0;
        (
            states(&status),
            ROUND_BUDGET.with(std::cell::Cell::get).unwrap(),
        )
    };
    let (seen_states, budget) = round(false);
    assert_eq!(
        seen_states,
        ["ok", "notSignedIn", "duplicate", "error:db_not_file"]
    );
    assert_eq!(budget, FULL_BUDGET, "the default had no file yet");
    assert_eq!(round(false).1, BACKGROUND_BUDGET);
    assert_eq!(round(true).1, FULL_BUDGET, "Sync Now");
    assert_eq!(round(false).1, BACKGROUND_BUDGET);
    registry.write().unwrap().profiles[0].root = tmp.path().join("moved");
    assert_eq!(
        round(false).1,
        FULL_BUDGET,
        "a changed profile's first round"
    );
    // Control: an outage is not a profile that cannot write; it keeps the full
    // budget until a file exists.
    let lonely = tmp.path().join("lonely");
    let down = profile_root(tmp.path(), "down", &other_token(), OTHER_USER);
    *registry.write().unwrap() = multi(&lonely, true, vec![profile(PROFILE_A, &down)]);
    let unreachable = "http://127.0.0.1:1/api/usage";
    let source = || registry.read().unwrap().clone();
    for _ in 0..2 {
        let status = sync_with(
            false,
            None,
            &target(&tmp.path().join("absent.vscdb"), unreachable),
            &source,
            Some(limits()),
        )
        .0;
        assert_eq!(states(&status), ["notSignedIn", "offline:unreachable"]);
        assert_eq!(ROUND_BUDGET.with(std::cell::Cell::get), Some(FULL_BUDGET));
    }
}

/// MA-32: `commit_file` asks `still_wanted` before it creates the dir or the
/// lock, so a profile removed or switched off during its last page creates
/// nothing. (The check is not atomic with the creation; the second one, under
/// the lock, is what stops a write.)
#[test]
fn a_commit_no_longer_wanted_creates_no_dir_and_no_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp
        .path()
        .join("app")
        .join(PROFILES_DIR_NAME)
        .join(PROFILE_A);
    let stop = commit_file(&dir, &stem(USER).unwrap(), &[], &|| false).unwrap_err();
    assert_eq!(stop, Stop::new(State::Disabled, None));
    assert!(!tmp.path().join("app").exists(), "something was created");
    // Control: wanted, it creates the dir, lock and file.
    assert!(
        commit_file(&dir, &stem(USER).unwrap(), &[], &|| true)
            .unwrap()
            .changed
    );
    assert!(dir.join(LOCK_FILE_NAME).exists());
}

/// MA-34: `changed` only when a usage file's bytes changed. A walk whose bytes
/// equal the file's rewrites nothing and is not `changed`, but its modified
/// time still moves so `lastSuccessMs` dates this walk. Same-length different
/// bytes ("a" then "b") are a change, so the length check alone is not enough.
#[test]
fn an_identical_walk_changes_nothing() {
    let _guard = lock();
    let f = fixture();
    let (url, _) = serve(vec![
        json_ok(&page(1, &["a"])),
        json_ok(&page(1, &["a"])),
        json_ok(&page(1, &["b"])),
    ]);
    let on = registry(true, &f.dir);
    let first = sync_status(&f, &url, &on);
    assert_eq!(first["changed"], true);
    let path = expected_file(&f.dir);
    let bytes = std::fs::read(&path).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    let again = sync_status(&f, &url, &on);
    assert_eq!(states(&again), ["ok"]);
    assert_eq!(
        again["changed"], false,
        "an identical rewrite read as a change"
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert!(
        again["lastSuccessMs"].as_u64() > first["lastSuccessMs"].as_u64(),
        "{again} {first}"
    );
    let new = sync_status(&f, &url, &on);
    assert_eq!(new["changed"], true);
    assert_ne!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(
        std::fs::read(&path).unwrap().len(),
        bytes.len(),
        "fixture: same length"
    );
}

/// MA-8: the core refuses Cursor's own folder (the default profile) and a
/// folder already listed, by directory identity as well as by path, so the
/// Swift check at add time is only a pre-check. Per entry: the refused profile
/// is left out and reported by index with its reason, the default and the
/// other profiles stay registered and sync (an earlier entry wins a repeat).
#[cfg(unix)]
#[test]
fn the_core_refuses_cursors_own_folder_and_a_duplicate_folder_per_entry() {
    let _guard = lock();
    set_for_test(Config::default());
    let home = tempfile::tempdir().unwrap();
    let own = home.path().join("Library/Application Support/Cursor");
    std::fs::create_dir_all(&own).unwrap();
    let other = home.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let alias = home.path().join("alias");
    std::os::unix::fs::symlink(&own, &alias).unwrap();
    let other_alias = home.path().join("other-alias");
    std::os::unix::fs::symlink(&other, &other_alias).unwrap();
    let push = |profiles: serde_json::Value| {
        set_in(
            home.path(),
            serde_json::json!({"enabled": true, "bundleId": BUNDLE, "profiles": profiles}),
        )
        .unwrap()
    };
    let entry = |id: &str, root: &Path| serde_json::json!({"id": id, "root": root});
    let third = home.path().join("third");
    for (name, profiles, rejected, kept) in [
        (
            "own folder by path",
            serde_json::json!([entry(PROFILE_A, &own), entry(PROFILE_B, &third)]),
            serde_json::json!([{"index": 0, "reason": "cursor_default"}]),
            vec![PROFILE_B],
        ),
        (
            "own folder by symlink",
            serde_json::json!([entry(PROFILE_A, &third), entry(PROFILE_B, &alias)]),
            serde_json::json!([{"index": 1, "reason": "cursor_default"}]),
            vec![PROFILE_A],
        ),
        (
            "listed twice by path",
            serde_json::json!([entry(PROFILE_A, &other), entry(PROFILE_B, &other)]),
            serde_json::json!([{"index": 1, "reason": "duplicate"}]),
            vec![PROFILE_A],
        ),
        (
            "listed twice by symlink",
            serde_json::json!([entry(PROFILE_A, &other), entry(PROFILE_B, &other_alias)]),
            serde_json::json!([{"index": 1, "reason": "duplicate"}]),
            vec![PROFILE_A],
        ),
    ] {
        let result = push(profiles);
        assert_eq!(result["rejectedProfiles"], rejected, "{name}");
        // The default profile and the valid profile are registered.
        let ids: Vec<String> = config().profiles.iter().map(|p| p.id.clone()).collect();
        assert_eq!(ids, kept, "{name}: the valid profile was dropped");
        assert!(config().enabled, "{name}: the config was refused whole");
        let listed: Vec<&str> = result["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_str().unwrap())
            .collect();
        assert_eq!(listed[0], "default", "{name}");
        assert_eq!(&listed[1..], &kept[..], "{name}");
    }
    // Control: nothing refused, nothing reported.
    let ok = push(serde_json::json!([
        entry(PROFILE_A, &other),
        entry(PROFILE_B, &third)
    ]));
    assert_eq!(ok["rejectedProfiles"], serde_json::json!([]));
    assert_eq!(config().profiles.len(), 2);
    // A folder that does not exist yet can only match by path.
    let twice = push(serde_json::json!([
        entry(PROFILE_A, &third),
        entry(PROFILE_B, &third)
    ]));
    assert_eq!(
        twice["rejectedProfiles"],
        serde_json::json!([{"index": 1, "reason": "duplicate"}])
    );
    set_for_test(Config::default());
}

/// A mixed push syncs the default and the valid profile: the refused profile
/// is not walked and gets no dir, and a round writes both kept files.
#[cfg(unix)]
#[test]
fn a_mixed_push_keeps_the_default_and_the_valid_profile_syncing() {
    let _guard = lock();
    set_for_test(Config::default());
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let own = home.join("Library/Application Support/Cursor");
    std::fs::create_dir_all(&own).unwrap();
    let default_db = login_db(&tmp.path().join("default"), &valid_token(), USER);
    let valid = profile_root(tmp.path(), "b", &other_token(), OTHER_USER);
    let pushed = set_in(
        &home,
        serde_json::json!({
            "enabled": true, "bundleId": BUNDLE,
            "profiles": [
                {"id": PROFILE_A, "root": own},
                {"id": PROFILE_B, "root": valid},
            ],
        }),
    )
    .unwrap();
    assert_eq!(
        pushed["rejectedProfiles"],
        serde_json::json!([{"index": 0, "reason": "cursor_default"}])
    );
    let registry = RwLock::new(config());
    set_for_test(Config::default());
    let (url, seen) = serve(vec![json_ok(&page(1, &["a"])), json_ok(&page(1, &["b"]))]);
    let status = sync_round_in(&default_db, &url, &registry, limits(), Some(&home));
    assert_eq!(states(&status), ["ok", "ok"]);
    assert_eq!(seen.lock().unwrap().len(), 2, "one walk per kept profile");
    assert!(expected_file(&default_sync_dir(&home)).exists());
    assert!(
        !profile_dir(&home, PROFILE_A).exists(),
        "the refused profile got a dir"
    );
    assert_eq!(names_in(&profile_dir(&home, PROFILE_B)).len(), 1);
}

/// A successful commit that could not delete a superseded usage file (an
/// entry named like one that is not a deletable file) reports it in
/// `cleanupFailedProfiles` like a duplicate's cleanup does, so the app can tell
/// the stale file is still there. Control: nothing planted, nothing reported.
#[cfg(unix)]
#[test]
fn a_commit_that_could_not_drop_a_superseded_file_reports_it() {
    let _guard = lock();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let default_db = login_db(&tmp.path().join("default"), &valid_token(), USER);
    let registry = RwLock::new(multi(&home, true, vec![]));
    let (url, _) = serve(vec![json_ok(&page(1, &["a"])), json_ok(&page(1, &["a"]))]);
    let clean = sync_round(&default_db, &url, &registry);
    assert_eq!(states(&clean), ["ok"]);
    assert_eq!(clean["cleanupFailedProfiles"], serde_json::json!([]));
    let stuck = default_sync_dir(&home).join(hex_name('e'));
    std::fs::create_dir(&stuck).unwrap();
    let status = sync_round(&default_db, &url, &registry);
    assert_eq!(states(&status), ["ok"]);
    assert_eq!(
        status["cleanupFailedProfiles"],
        serde_json::json!(["default"])
    );
}

/// `cliPresent` follows a completed walk, not a byte change: with tokscale CLI
/// Cursor files present and the takeover unconfirmed, a second round that
/// writes the identical bytes (`changed: false`) still reports `cliPresent`,
/// so the prompt does not vanish after a quiet round. Control: with no CLI
/// files the same rounds report `ok`.
#[test]
fn an_unchanged_round_still_reports_cli_present() {
    let _guard = lock();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let default_db = login_db(&tmp.path().join("default"), &valid_token(), USER);
    let config = multi(&home, true, vec![]);
    set_for_test(config.clone());
    let registry = RwLock::new(config);
    let (url, _) = serve(vec![json_ok(&page(1, &["a"])); 4]);
    let state = |status: &serde_json::Value| status["state"].as_str().unwrap().to_string();
    let first = sync_round_in(&default_db, &url, &registry, limits(), Some(&home));
    assert_eq!(
        (state(&first), first["changed"].clone()),
        ("ok".into(), true.into())
    );
    let cli = home.join(".config/tokscale/cursor-cache");
    std::fs::create_dir_all(&cli).unwrap();
    std::fs::write(cli.join("usage.csv"), "x").unwrap();
    let again = sync_round_in(&default_db, &url, &registry, limits(), Some(&home));
    assert_eq!(again["changed"], false, "fixture: identical bytes");
    assert_eq!(
        state(&again),
        "cliPresent",
        "the prompt vanished after a quiet round"
    );
    // Control: without the CLI files the identical round is plain ok.
    std::fs::remove_file(cli.join("usage.csv")).unwrap();
    let control = sync_round_in(&default_db, &url, &registry, limits(), Some(&home));
    assert_eq!(
        (state(&control), control["changed"].clone()),
        ("ok".into(), false.into())
    );
    set_for_test(Config::default());
}

/// A previous appDir that could not be cleaned stays reported by every
/// accepted push until it is clean: a later push with the same appDir does not
/// hide it. The retry also removes the files once the cause is gone.
#[cfg(unix)]
#[test]
fn a_previous_app_dir_that_could_not_be_cleaned_is_retried_until_clean() {
    use std::os::unix::fs::PermissionsExt as _;
    let _guard = lock();
    set_for_test(Config::default());
    let home = tempfile::tempdir().unwrap();
    let first = home
        .path()
        .join("Library/Application Support/com.example.first");
    let cache = first.join("cursor-cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join(hex_name('a')), "{}").unwrap();
    std::fs::write(cache.join(LOCK_FILE_NAME), "").unwrap();
    let push = |bundle: &str| {
        set_in(
            home.path(),
            serde_json::json!({"enabled": true, "bundleId": bundle}),
        )
        .unwrap()
    };
    push("com.example.first");
    // Move to another appDir while the old one cannot be cleaned.
    std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o500)).unwrap();
    let moved = push("com.example.second");
    assert_eq!(moved["cleanupFailed"], true);
    assert_eq!(
        moved["cleanupFailedProfiles"],
        serde_json::json!(["appDir"])
    );
    // The same appDir again: still reported, not forgotten.
    let again = push("com.example.second");
    assert_eq!(again["cleanupFailed"], true, "a later push hid the failure");
    assert!(cache.join(hex_name('a')).exists());
    // The cause goes away: the next push cleans it and reports nothing.
    std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o700)).unwrap();
    let healed = push("com.example.second");
    assert_eq!(healed["cleanupFailed"], false);
    assert_eq!(healed["removedFiles"], 1);
    assert!(!cache.join(hex_name('a')).exists());
    assert_eq!(push("com.example.second")["cleanupFailed"], false);
    set_for_test(Config::default());
}

// --- sealed tokens (plan CURSOR-SS-M, Acceptance 1) ---------------------------
//
// Hermetic: the Keychain is a closure (`Target::key_fetch`), consent a closure
// (or the in-memory registry for test 16); never the real Keychain, never
// Security.framework.
//
// Fixture: a synthetic session JWT for `USER` (exp 4102444800 = 2100-01-01),
// sealed the way Electron's macOS safeStorage does, generated independently
// of this code with Python hashlib + OpenSSL (as the `macos_safe_storage`
// fixture), never from a real Keychain or a real Cursor database:
//
//   key=$(python3 -c 'import hashlib,sys;print(hashlib.pbkdf2_hmac("sha1",
//         sys.argv[1].encode(),b"saltysalt",1003,16).hex())' "$PASSWORD")
//   { printf 'v10'; printf '%s' "$SEALED_JWT" |
//     openssl enc -aes-128-cbc -K "$key" -iv 20202020202020202020202020202020; } | base64
//
// (OpenSSL 4.1.0, PKCS7 padding by default.) `SEALED_B64` uses
// `SAFE_STORAGE_PASSWORD`, `OTHER_SEALED_B64` uses `OTHER_PASSWORD`; both
// contain `+` and `/`, so a URL_SAFE decode fails on them.

const SEALED_JWT: &str = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJhdXRoMHx1c2VyX0NBTkFSWXVzZXIwMTIzNDU2Nzg5QUIiLCJ0eXBlIjoic2Vzc2lvbiIsImV4cCI6NDEwMjQ0NDgwMH0.CANARYsignatureXYZ";
const SAFE_STORAGE_PASSWORD: &str = "test-cursor-safe-storage-password";
const OTHER_PASSWORD: &str = "test-other-profile-password";
const SEALED_B64: &str = "djEw8+TB1Z9KP2LSnhnAOkoG/rZDMNM60WA8aBrDF0Lq8b14ETjZU4oAM+GymBTJs96k6GThmCKnEb/p0ZUgRWybj9Rib1g/ETjaK1eV89hF9gFgNu5njPzKseH2vAwHut3sNkaXvPaj8914JEU6bGWqafewhtl0WpfESqAMUcBhkpx/K8NnNPDYgsoK1K2uDDwX";
const OTHER_SEALED_B64: &str = "djEwQvaqV+D/2m6qRhxCu/YxXFYbILo+sCU/IN/wqAabT85DoYAIQYMGxpsw8Yp6ftdjD+Cet4SZ46hB5AJlpLV+8ZMfNFrDv9RixDfWZBSfh71ZspdCFBm1irpRFIcpzvZc2GbKSOfDK1hfsEVKeVIA0nl7hx55Cny7YFe+uq//rEdUijB1HfmPf4W/kZ/7rOgG";

fn sealed(b64: &str) -> String {
    format!("cursor-safe-storage:v1:{b64}")
}

/// Both registries' test locks (the consent registry drops the cached key),
/// then a clean sealed-token state.
fn sealed_lock() -> (
    std::sync::MutexGuard<'static, ()>,
    std::sync::MutexGuard<'static, ()>,
) {
    let guards = (
        lock(),
        crate::keychain_consent::TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner()),
    );
    crate::keychain_consent::reset_for_test();
    reset_sealed_for_test();
    guards
}

/// A Keychain stand-in: records every account asked, answers with `answer`.
fn keychain<'a>(
    calls: &'a std::cell::RefCell<Vec<String>>,
    answer: impl Fn(&str) -> Result<Vec<u8>, i32> + 'a,
) -> impl Fn(&str) -> Result<Vec<u8>, i32> + 'a {
    move |account| {
        calls.borrow_mut().push(account.to_string());
        answer(account)
    }
}

fn password(_: &str) -> Result<Vec<u8>, i32> {
    Ok(SAFE_STORAGE_PASSWORD.as_bytes().to_vec())
}

fn granted() -> bool {
    true
}

fn sealed_round(
    explicit: bool,
    default_db: &Path,
    url: &str,
    registry: &RwLock<Config>,
    key_fetch: Option<&KeyFetch<'_>>,
    consent: &dyn Fn() -> bool,
) -> serde_json::Value {
    let target = Target {
        db_path: default_db,
        url,
        billing_url: url,
        https_only: false,
        file_stem: &stem,
        key_fetch,
        consent,
    };
    let source = || registry.read().unwrap().clone();
    sync_with(explicit, None, &target, &source, Some(limits())).0
}

/// One sealed default profile (`token` stored JSON-quoted, as Cursor does).
struct SealedFixture {
    tmp: tempfile::TempDir,
    db: PathBuf,
    registry: RwLock<Config>,
}

fn sealed_fixture(token: &str) -> SealedFixture {
    let tmp = tempfile::tempdir().unwrap();
    let db = login_db(&tmp.path().join("default"), token, USER);
    let registry = RwLock::new(multi(&tmp.path().join("home"), true, Vec::new()));
    SealedFixture { tmp, db, registry }
}

fn cookie_of(seen: &Arc<Mutex<Vec<Seen>>>) -> Option<String> {
    header(&seen.lock().unwrap().first()?.head, "cookie")
}

/// 1: a sealed token, with consent and the key, sends exactly what its
/// plaintext twin sends (same user, same token, so the same exp).
#[test]
fn sealed_token_with_consent_syncs_like_its_plaintext_twin() {
    let _guard = sealed_lock();
    let calls = std::cell::RefCell::new(Vec::new());
    let fetch = keychain(&calls, password);
    let twin = sealed_fixture(SEALED_JWT);
    let (url, plain_seen) = serve(vec![json_ok(&page(1, &["a"]))]);
    let plain = sealed_round(true, &twin.db, &url, &twin.registry, Some(&fetch), &granted);
    assert_eq!(plain["state"], "ok", "{plain}");
    assert!(
        calls.borrow().is_empty(),
        "a plaintext token read the Keychain"
    );

    let f = sealed_fixture(&sealed(SEALED_B64));
    let (url, sealed_seen) = serve(vec![json_ok(&page(1, &["a"]))]);
    let status = sealed_round(true, &f.db, &url, &f.registry, Some(&fetch), &granted);
    assert_eq!(status["state"], "ok", "{status}");
    assert_eq!(*calls.borrow(), ["Cursor Key"]);
    let (plain_cookie, sealed_cookie) = (cookie_of(&plain_seen), cookie_of(&sealed_seen));
    assert!(plain_cookie
        .as_deref()
        .is_some_and(|c| c.contains(SEALED_JWT)));
    assert_eq!(sealed_cookie, plain_cookie);
    let user = token_user(&CursorLogin {
        access_token: SEALED_JWT.to_string(),
        glass_user_id: None,
        profile_user_id: None,
    })
    .unwrap();
    assert_eq!(user, (USER.to_string(), 4102444800.0));
}

/// 2: a plaintext token without consent syncs and never reaches the key
/// source.
#[test]
fn plaintext_token_needs_no_consent_and_no_keychain() {
    let _guard = sealed_lock();
    let calls = std::cell::RefCell::new(Vec::new());
    let fetch = keychain(&calls, password);
    let asked = std::cell::Cell::new(0);
    let consent = || {
        asked.set(asked.get() + 1);
        false
    };
    let f = sealed_fixture(&valid_token());
    let (url, seen) = serve(vec![json_ok(&page(1, &["a"]))]);
    let status = sealed_round(true, &f.db, &url, &f.registry, Some(&fetch), &consent);
    assert_eq!(status["state"], "ok", "{status}");
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert!(calls.borrow().is_empty(), "plaintext reached the Keychain");
    assert_eq!(asked.get(), 0, "plaintext consulted the consent");
}

/// 3: a sealed token without consent stops at `keychain_consent`, with no
/// Keychain call and nothing sent.
#[test]
fn sealed_token_without_consent_reads_nothing() {
    let _guard = sealed_lock();
    let calls = std::cell::RefCell::new(Vec::new());
    let fetch = keychain(&calls, password);
    let f = sealed_fixture(&sealed(SEALED_B64));
    let (url, seen) = serve(vec![json_ok(&page(1, &["a"]))]);
    let status = sealed_round(true, &f.db, &url, &f.registry, Some(&fetch), &no_consent);
    assert_eq!(states(&status), ["error:keychain_consent"]);
    assert_eq!(status["reason"], "keychain_consent");
    assert!(calls.borrow().is_empty());
    assert!(seen.lock().unwrap().is_empty());
}

/// Two sealed profiles: the default (`default_b64`) and one added profile
/// (`added_b64`), same account.
fn two_sealed(default_b64: &str, added_b64: &str) -> SealedFixture {
    let f = sealed_fixture(&sealed(default_b64));
    let root = profile_root(f.tmp.path(), "b", &sealed(added_b64), USER);
    *f.registry.write().unwrap() = multi(
        &f.tmp.path().join("home"),
        true,
        vec![profile(PROFILE_A, &root)],
    );
    f
}

/// 4: Deny is `keychain_denied`, the legacy account is not tried after it,
/// and a second sealed profile of the same round reuses the result (one
/// dialog per round).
#[test]
fn a_denied_read_is_not_retried_in_the_round() {
    let _guard = sealed_lock();
    let calls = std::cell::RefCell::new(Vec::new());
    let fetch = keychain(&calls, |_| Err(-128));
    let f = two_sealed(SEALED_B64, SEALED_B64);
    let (url, seen) = serve(vec![json_ok(&page(1, &["a"]))]);
    let status = sealed_round(true, &f.db, &url, &f.registry, Some(&fetch), &granted);
    assert_eq!(
        states(&status),
        ["error:keychain_denied", "error:keychain_denied"]
    );
    assert_eq!(*calls.borrow(), ["Cursor Key"], "a second dialog");
    assert!(seen.lock().unwrap().is_empty());
}

/// 5: NotFound on `Cursor Key` falls back to `Cursor`, in that order; other
/// statuses stop at the first account (table in `macos_safe_storage`).
#[test]
fn the_legacy_account_is_tried_only_when_the_current_one_is_missing() {
    let _guard = sealed_lock();
    let cases: [(&KeyFetch<'_>, &str, &[&str]); 4] = [
        (
            &|account| match account {
                "Cursor Key" => Err(-25300),
                _ => password(account),
            },
            "ok",
            &["Cursor Key", "Cursor"],
        ),
        (
            &|_| Err(-25300),
            "error:keychain_missing",
            &["Cursor Key", "Cursor"],
        ),
        (
            &|_| Err(-25308),
            "error:keychain_unavailable",
            &["Cursor Key"],
        ),
        (&|_| Err(-25293), "error:keychain_denied", &["Cursor Key"]),
    ];
    for (answer, expected, accounts) in cases {
        reset_sealed_for_test();
        let calls = std::cell::RefCell::new(Vec::new());
        let fetch = keychain(&calls, answer);
        let f = sealed_fixture(&sealed(SEALED_B64));
        let (url, _) = serve(vec![json_ok(&page(1, &["a"]))]);
        let status = sealed_round(true, &f.db, &url, &f.registry, Some(&fetch), &granted);
        assert_eq!(states(&status), [expected], "{accounts:?}");
        assert_eq!(*calls.borrow(), accounts);
    }
}

/// 6: one read serves both profiles; a later background round uses the
/// cached key with no read; removing the grant, or turning sync off, empties
/// the cache at once (no round in between).
#[test]
fn the_key_is_read_once_kept_and_dropped_with_the_grant_or_the_switch() {
    let _guard = sealed_lock();
    let calls = std::cell::RefCell::new(Vec::new());
    let fetch = keychain(&calls, password);
    let f = two_sealed(SEALED_B64, SEALED_B64);
    let replies = (0..4).map(|_| json_ok(&page(1, &["a"]))).collect();
    let (url, _) = serve(replies);
    let round = |explicit| sealed_round(explicit, &f.db, &url, &f.registry, Some(&fetch), &granted);
    assert_eq!(states(&round(true)), ["ok", "duplicate"]);
    assert_eq!(calls.borrow().len(), 1, "one read per round");
    assert_eq!(states(&round(false)), ["ok", "duplicate"]);
    assert_eq!(calls.borrow().len(), 1, "a background round read again");
    assert!(sealed_key_cached_for_test());

    // A payload that does not grant "cursor" drops the key immediately.
    crate::keychain_consent::set_from_json(r#"{"grok-bot":true}"#).unwrap();
    assert!(!sealed_key_cached_for_test(), "consent removed, key kept");
    // Control: a payload that grants "cursor" keeps it.
    assert_eq!(states(&round(true)), ["ok", "duplicate"]);
    crate::keychain_consent::set_from_json(r#"{"cursor":true}"#).unwrap();
    assert!(sealed_key_cached_for_test());

    // Turning sync off drops it immediately too.
    set_for_test(Config::default());
    set_from_json(r#"{"enabled":false,"bundleId":"com.example.off"}"#, None).unwrap();
    assert!(!sealed_key_cached_for_test(), "sync off, key kept");
    set_for_test(Config::default());
    crate::keychain_consent::reset_for_test();
}

/// 7: every malformed sealed token is a fixed reason, and no fixture secret
/// reaches any output; a decrypt failure with the cached key empties the
/// cache.
#[test]
fn malformed_sealed_tokens_are_fixed_reasons_without_secrets() {
    let _guard = sealed_lock();
    let blob = base64::engine::general_purpose::STANDARD
        .decode(SEALED_B64)
        .unwrap();
    let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
    let mut v11 = blob.clone();
    v11[2] = b'1';
    let cases = [
        (sealed(OTHER_SEALED_B64), "token_sealed_unreadable"),
        (sealed(&encode(&blob[..11])), "token_sealed_unreadable"),
        (sealed(&encode(&v11)), "token_sealed_unreadable"),
        (sealed("not*base64"), "token_sealed_unreadable"),
        (
            format!("cursor-safe-storage:v2:{SEALED_B64}"),
            "token_sealed_unsupported",
        ),
    ];
    let mut outputs = Vec::new();
    for (token, reason) in &cases {
        reset_sealed_for_test();
        let calls = std::cell::RefCell::new(Vec::new());
        let fetch = keychain(&calls, password);
        let f = sealed_fixture(token);
        let (url, seen) = serve(vec![json_ok(&page(1, &["a"]))]);
        let status = sealed_round(true, &f.db, &url, &f.registry, Some(&fetch), &granted);
        assert_eq!(states(&status), [format!("error:{reason}")], "{status}");
        assert!(seen.lock().unwrap().is_empty());
        outputs.push(status.to_string());
        for entry in walkdir(f.tmp.path()) {
            outputs.push(entry);
        }
    }
    let all = outputs.join("\n");
    for canary in [
        SEALED_JWT,
        SIGNATURE,
        SAFE_STORAGE_PASSWORD,
        OTHER_PASSWORD,
        &SEALED_B64[4..24],
        &OTHER_SEALED_B64[4..24],
    ] {
        assert!(!all.contains(canary), "a fixture secret reached an output");
    }

    // A decrypt failure with a cached key empties the cache.
    reset_sealed_for_test();
    let calls = std::cell::RefCell::new(Vec::new());
    let fetch = keychain(&calls, password);
    let f = sealed_fixture(&sealed(SEALED_B64));
    let (url, _) = serve(vec![json_ok(&page(1, &["a"]))]);
    let round = |explicit| sealed_round(explicit, &f.db, &url, &f.registry, Some(&fetch), &granted);
    assert_eq!(states(&round(true)), ["ok"]);
    assert!(sealed_key_cached_for_test());
    // The stored token now needs another key (a rotated Safe Storage key).
    let rotated = login_db(
        &f.tmp.path().join("rotated"),
        &sealed(OTHER_SEALED_B64),
        USER,
    );
    std::fs::copy(rotated, &f.db).unwrap();
    assert_eq!(states(&round(false)), ["error:token_sealed_unreadable"]);
    assert!(
        !sealed_key_cached_for_test(),
        "a key that opens nothing was kept"
    );
    assert_eq!(calls.borrow().len(), 1);
    assert_eq!(states(&round(false)), ["error:keychain_unlock"]);
}

/// Names and contents of every file under `dir`.
fn walkdir(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        out.push(entry.file_name().to_string_lossy().into_owned());
        if path.is_dir() {
            out.extend(walkdir(&path));
        } else if path.extension().is_some_and(|e| e == "json") {
            out.push(std::fs::read_to_string(&path).unwrap_or_default());
        }
    }
    out
}

/// 9: the opened token goes through the same checks as a plaintext one: a
/// stored account that differs is `account_mismatch`, a non-JWT plaintext
/// is `token_unreadable`.
#[test]
fn an_opened_token_is_checked_like_a_plaintext_one() {
    let _guard = sealed_lock();
    let calls = std::cell::RefCell::new(Vec::new());
    let fetch = keychain(&calls, password);
    let tmp = tempfile::tempdir().unwrap();
    let db = login_db(&tmp.path().join("other"), &sealed(SEALED_B64), OTHER_USER);
    let registry = RwLock::new(multi(&tmp.path().join("home"), true, Vec::new()));
    let (url, seen) = serve(vec![json_ok(&page(1, &["a"]))]);
    let status = sealed_round(true, &db, &url, &registry, Some(&fetch), &granted);
    assert_eq!(states(&status), ["error:account_mismatch"]);
    // Synthetic non-JWT plaintext from the `macos_safe_storage` fixture
    // (Python hashlib + OpenSSL, password `test-keychain-password`).
    reset_sealed_for_test();
    let calls = std::cell::RefCell::new(Vec::new());
    let fetch = keychain(&calls, |_| Ok(b"test-keychain-password".to_vec()));
    let f = sealed_fixture(&sealed("djEwAO+8/FAQYBswbLPYv43X0Q=="));
    let status = sealed_round(true, &f.db, &url, &f.registry, Some(&fetch), &granted);
    assert_eq!(states(&status), ["error:token_unreadable"]);
    assert_eq!(calls.borrow().len(), 1, "the fixture never reached the key");
    assert!(seen.lock().unwrap().is_empty());
}

/// 11: only the first round of the process, or a round the user started,
/// may read. A first round with a plaintext token (or none) still counts.
#[test]
fn only_the_first_or_an_explicit_round_reads_the_keychain() {
    let _guard = sealed_lock();
    let calls = std::cell::RefCell::new(Vec::new());
    let fetch = keychain(&calls, password);
    let f = sealed_fixture(&valid_token());
    let (url, _) = serve((0..3).map(|_| json_ok(&page(1, &["a"]))).collect());
    let round = |explicit| sealed_round(explicit, &f.db, &url, &f.registry, Some(&fetch), &granted);
    assert_eq!(states(&round(false)), ["ok"], "first round, plaintext");
    // Cursor now stores the token sealed.
    let resealed = login_db(&f.tmp.path().join("resealed"), &sealed(SEALED_B64), USER);
    std::fs::copy(resealed, &f.db).unwrap();
    assert_eq!(states(&round(false)), ["error:keychain_unlock"]);
    assert!(
        calls.borrow().is_empty(),
        "a background round read the Keychain"
    );
    assert_eq!(states(&round(true)), ["ok"]);
    assert_eq!(calls.borrow().len(), 1);

    // No token at all in the first round counts as well.
    reset_sealed_for_test();
    let tmp = tempfile::tempdir().unwrap();
    let absent = tmp.path().join("absent.vscdb");
    let registry = RwLock::new(multi(&tmp.path().join("home"), true, Vec::new()));
    let status = sealed_round(false, &absent, &url, &registry, Some(&fetch), &granted);
    assert_eq!(states(&status), ["notSignedIn"]);
    assert_eq!(states(&round(false)), ["error:keychain_unlock"]);
    assert_eq!(
        calls.borrow().len(),
        1,
        "a background round read the Keychain"
    );
    // Control: in a fresh process the first background round does read.
    reset_sealed_for_test();
    assert_eq!(states(&round(false)), ["ok"]);
    assert_eq!(calls.borrow().len(), 2);
}

/// 12: a key whose read was overtaken (consent removed, sync turned off, a
/// drop) while the dialog was up is not kept, and the next round needs
/// consent again.
#[test]
fn a_key_read_overtaken_by_a_change_is_not_kept() {
    let _guard = sealed_lock();
    let f = sealed_fixture(&sealed(SEALED_B64));
    let (url, seen) = serve(vec![json_ok(&page(1, &["a"]))]);

    // Consent removed while the dialog is up.
    let consent = std::sync::atomic::AtomicBool::new(true);
    let calls = std::cell::RefCell::new(Vec::new());
    let fetch = keychain(&calls, |account| {
        consent.store(false, Ordering::SeqCst);
        password(account)
    });
    let live = || consent.load(Ordering::SeqCst);
    let status = sealed_round(true, &f.db, &url, &f.registry, Some(&fetch), &live);
    assert_eq!(states(&status), ["error:keychain_consent"]);
    assert!(!sealed_key_cached_for_test());
    let status = sealed_round(false, &f.db, &url, &f.registry, Some(&fetch), &live);
    assert_eq!(states(&status), ["error:keychain_consent"]);
    assert_eq!(calls.borrow().len(), 1);

    // Sync turned off while the dialog is up.
    reset_sealed_for_test();
    let calls = std::cell::RefCell::new(Vec::new());
    let fetch = keychain(&calls, |account| {
        f.registry.write().unwrap().enabled = false;
        password(account)
    });
    let status = sealed_round(true, &f.db, &url, &f.registry, Some(&fetch), &granted);
    assert_eq!(states(&status), ["disabled"]);
    assert!(!sealed_key_cached_for_test());
    f.registry.write().unwrap().enabled = true;

    // A drop (here: a consent payload without "cursor", while the injected
    // consent still says yes) between the read and the store.
    reset_sealed_for_test();
    let calls = std::cell::RefCell::new(Vec::new());
    let fetch = keychain(&calls, |account| {
        crate::keychain_consent::set_from_json(r#"{"grok-bot":true}"#).unwrap();
        password(account)
    });
    let status = sealed_round(true, &f.db, &url, &f.registry, Some(&fetch), &granted);
    assert_eq!(states(&status), ["error:keychain_consent"]);
    assert!(!sealed_key_cached_for_test());
    assert!(seen.lock().unwrap().is_empty(), "an overtaken key was used");
    // Control: the same read with nothing changing is kept and used.
    reset_sealed_for_test();
    let fetch = keychain(&calls, password);
    let status = sealed_round(true, &f.db, &url, &f.registry, Some(&fetch), &granted);
    assert_eq!(states(&status), ["ok"]);
    assert!(sealed_key_cached_for_test());
    crate::keychain_consent::reset_for_test();
}

/// 13: with no key source (the non-macOS build), a sealed token is
/// `token_sealed_unsupported` before any consent lookup.
#[test]
fn no_key_source_means_unsupported_before_consent() {
    let _guard = sealed_lock();
    let asked = std::cell::Cell::new(0);
    let consent = || {
        asked.set(asked.get() + 1);
        true
    };
    let f = sealed_fixture(&sealed(SEALED_B64));
    let (url, seen) = serve(vec![json_ok(&page(1, &["a"]))]);
    let status = sealed_round(true, &f.db, &url, &f.registry, None, &consent);
    assert_eq!(states(&status), ["error:token_sealed_unsupported"]);
    assert_eq!(asked.get(), 0);
    assert!(seen.lock().unwrap().is_empty());
}

/// 15: an added profile sealed with another key fails on its own; the key
/// that opened the default profile is kept for the next background round.
/// Only when no profile opens is it dropped.
#[test]
fn one_profile_failing_to_open_keeps_the_key_for_the_others() {
    let _guard = sealed_lock();
    let calls = std::cell::RefCell::new(Vec::new());
    let fetch = keychain(&calls, password);
    let f = two_sealed(SEALED_B64, OTHER_SEALED_B64);
    let (url, seen) = serve((0..2).map(|_| json_ok(&page(1, &["a"]))).collect());
    let round = |explicit| sealed_round(explicit, &f.db, &url, &f.registry, Some(&fetch), &granted);
    assert_eq!(
        states(&round(true)),
        ["ok", "error:token_sealed_unreadable"]
    );
    assert_eq!(calls.borrow().len(), 1);
    assert_eq!(
        states(&round(false)),
        ["ok", "error:token_sealed_unreadable"]
    );
    assert_eq!(calls.borrow().len(), 1, "the key was dropped");
    assert_eq!(seen.lock().unwrap().len(), 2, "the default did not sync");

    // Every profile failing: the key goes.
    let g = two_sealed(OTHER_SEALED_B64, OTHER_SEALED_B64);
    let status = sealed_round(false, &g.db, &url, &g.registry, Some(&fetch), &granted);
    assert_eq!(
        states(&status),
        [
            "error:token_sealed_unreadable",
            "error:token_sealed_unreadable"
        ]
    );
    assert!(!sealed_key_cached_for_test());
}

/// 16: the production consent check reads the registry: a payload with
/// `"cursor":false` stops an explicit round at `keychain_consent`.
#[test]
fn the_registry_decides_cursor_consent() {
    let _guard = sealed_lock();
    let calls = std::cell::RefCell::new(Vec::new());
    let fetch = keychain(&calls, password);
    let consent = || crate::keychain_consent::allowed("cursor");
    let f = sealed_fixture(&sealed(SEALED_B64));
    let (url, _) = serve(vec![json_ok(&page(1, &["a"]))]);
    let result =
        crate::keychain_consent::set_from_json(r#"{"grok-bot":true,"cursor":false}"#).unwrap();
    assert!(result["rejected"].as_array().unwrap().is_empty());
    let status = sealed_round(true, &f.db, &url, &f.registry, Some(&fetch), &consent);
    assert_eq!(states(&status), ["error:keychain_consent"]);
    assert!(calls.borrow().is_empty());
    // Control: granting "cursor" lets the same round read.
    crate::keychain_consent::set_from_json(r#"{"grok-bot":true,"cursor":true}"#).unwrap();
    let status = sealed_round(true, &f.db, &url, &f.registry, Some(&fetch), &consent);
    assert_eq!(states(&status), ["ok"]);
    assert_eq!(calls.borrow().len(), 1);
    crate::keychain_consent::reset_for_test();
}

// --- review round 1 ---------------------------------------------------------

/// R1: the consent answer a cached key is used under is read while the
/// key-cache lock is held, so a revoke (registry write, then a drop under
/// that lock) cannot slip between the check and the decrypt. A withdrawn
/// answer there is `keychain_consent` with nothing sent.
#[test]
fn consent_is_read_under_the_key_cache_lock() {
    let _guard = sealed_lock();
    let calls = std::cell::RefCell::new(Vec::new());
    let fetch = keychain(&calls, password);
    let f = sealed_fixture(&sealed(SEALED_B64));
    let (url, seen) = serve((0..2).map(|_| json_ok(&page(1, &["a"]))).collect());
    assert_eq!(
        states(&sealed_round(
            true,
            &f.db,
            &url,
            &f.registry,
            Some(&fetch),
            &granted
        )),
        ["ok"]
    );
    assert!(sealed_key_cached_for_test());
    // `try_lock` from the thread that holds the lock fails without blocking.
    let held = std::cell::RefCell::new(Vec::new());
    let answer = std::cell::Cell::new(true);
    let consent = || {
        held.borrow_mut().push(SEALED_KEY.try_lock().is_err());
        answer.get()
    };
    let status = sealed_round(false, &f.db, &url, &f.registry, Some(&fetch), &consent);
    assert_eq!(states(&status), ["ok"]);
    assert_eq!(
        *held.borrow(),
        [true],
        "consent read outside the key-cache lock"
    );
    answer.set(false);
    let status = sealed_round(false, &f.db, &url, &f.registry, Some(&fetch), &consent);
    assert_eq!(states(&status), ["error:keychain_consent"]);
    assert_eq!(
        seen.lock().unwrap().len(),
        2,
        "a withdrawn grant opened the token"
    );
    assert_eq!(calls.borrow().len(), 1);
}

/// R3: time spent waiting on the Keychain dialog comes off the round's
/// budget; the walk does not get a fresh one.
#[test]
fn the_keychain_wait_counts_against_the_round_budget() {
    let _guard = sealed_lock();
    let calls = std::cell::RefCell::new(Vec::new());
    let fetch = keychain(&calls, |account| {
        std::thread::sleep(Duration::from_millis(1200));
        password(account)
    });
    let f = sealed_fixture(&sealed(SEALED_B64));
    let (url, seen) = serve(vec![json_ok(&page(1, &["a"]))]);
    let target = Target {
        db_path: &f.db,
        url: &url,
        billing_url: &url,
        https_only: false,
        file_stem: &stem,
        key_fetch: Some(&fetch),
        consent: &granted,
    };
    let source = || f.registry.read().unwrap().clone();
    let short = Limits {
        budget: Duration::from_secs(1),
        ..limits()
    };
    let status = sync_with(true, None, &target, &source, Some(short)).0;
    assert_eq!(states(&status), ["partial:budget_exhausted"]);
    assert!(
        seen.lock().unwrap().is_empty(),
        "the walk ran past the budget"
    );
    // Control: with time left after the wait, the same round syncs.
    reset_sealed_for_test();
    let roomy = Limits {
        budget: Duration::from_secs(10),
        ..limits()
    };
    let status = sync_with(true, None, &target, &source, Some(roomy)).0;
    assert_eq!(states(&status), ["ok"]);
}

/// R4: a profile removed while the dialog was up skips itself, but the key
/// the user just allowed is kept for the round's other profiles.
#[test]
fn a_profile_removed_during_the_dialog_keeps_the_key_for_the_others() {
    let _guard = sealed_lock();
    let f = sealed_fixture(&valid_token());
    let home = f.tmp.path().join("home");
    let a = profile_root(f.tmp.path(), "a", &sealed(SEALED_B64), USER);
    let b = profile_root(f.tmp.path(), "b", &sealed(SEALED_B64), USER);
    *f.registry.write().unwrap() = multi(
        &home,
        true,
        vec![profile(PROFILE_A, &a), profile(PROFILE_B, &b)],
    );
    let calls = std::cell::RefCell::new(Vec::new());
    let fetch = keychain(&calls, |account| {
        // Profile A is removed while macOS shows its dialog.
        f.registry
            .write()
            .unwrap()
            .profiles
            .retain(|p| p.id != PROFILE_A);
        password(account)
    });
    let (url, _) = serve(vec![json_ok(&page(1, &["a"]))]);
    let status = sealed_round(true, &f.db, &url, &f.registry, Some(&fetch), &granted);
    assert_eq!(states(&status), ["ok", "disabled", "duplicate"]);
    assert_eq!(calls.borrow().len(), 1);
    assert!(
        sealed_key_cached_for_test(),
        "the allowed key was thrown away"
    );
}

// --- Billing V2 (plan CURSOR-BV2-M r4, Acceptance 1) ---------------------------
//
// Fixtures: the discovery's §1 third-party shapes (t3code #28511's redacted
// item, `{}`, `{"data":[]}`, cursor pagination) and G1's observed
// `{"availability":"missing_team_id"}`. Months are relative to the real UTC
// now, so the windows the walk asks for are the ones these replies answer.

const EMAIL_CANARY: &str = "EMAILCANARY@example.invalid";
const ITEM_ID_CANARY: &str = "BILLINGITEMIDCANARY";
const HOUR: i64 = 3_600_000;
/// The four meters of `item`: 1263 + 46400 + 0 + 312.
const ITEM_TOKENS: f64 = 47975.0;
/// `event`'s four token counts: 10 + 5 + 2 + 9.
const OLD_ROW_TOKENS: f64 = 26.0;

fn now_ms_i64() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// Start (Unix ms) of the UTC month `back` months before the current one.
fn month_start_ms(back: i32) -> i64 {
    use chrono::TimeZone as _;
    let month = cursor_billing_v2::month_index(now_ms_i64()).unwrap() - back;
    chrono::Utc
        .with_ymd_and_hms(
            month.div_euclid(12),
            month.rem_euclid(12) as u32 + 1,
            1,
            0,
            0,
            0,
        )
        .unwrap()
        .timestamp_millis()
}

fn iso(ms: i64) -> String {
    use chrono::TimeZone as _;
    chrono::Utc
        .timestamp_millis_opt(ms)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// A V2 billing item shaped like t3code #28511's redacted live sample.
fn item(id: &str, ms: i64) -> serde_json::Value {
    serde_json::json!({
        "billingItemId": id,
        "eventTime": iso(ms),
        "product": "cursor",
        "chargeCategory": "usage",
        "usageType": "included",
        "principal": {"type": "user", "id": USER},
        "usage": {"prompt-text-tokens": "1263", "cached-prompt-text-tokens": "46400",
                  "cache-write-text-tokens": "0", "completion-text-tokens": "312"},
        "totalTokens": "47975",
        "hasTokenMeters": true,
        "pricing": {"currency": "USD", "contracted": "0.0000000000"},
        "displayExtras": {"model": "composer-2.5", "userEmail": EMAIL_CANARY,
                          "displayLabel": "Composer 2.5"},
    })
}

fn v2_page(items: &[serde_json::Value], next: Option<&str>) -> Reply {
    let pagination = match next {
        Some(next) => serde_json::json!({"nextCursor": next, "hasMore": true}),
        None => serde_json::json!({}),
    };
    json_ok(&serde_json::json!({"data": items, "pagination": pagination}).to_string())
}

/// `{}`: t3code reads it as an idle slice (discovery §2).
fn v2_empty() -> Reply {
    json_ok("{}")
}

/// The old endpoint's page 1 for a migrated account (#520): an object
/// without `usageEventsDisplay`.
fn old_migrated() -> Reply {
    json_ok("{}")
}

fn old_page(rows: &[serde_json::Value]) -> Reply {
    json_ok(
        &serde_json::json!({"totalUsageEventsCount": rows.len(), "usageEventsDisplay": rows})
            .to_string(),
    )
}

/// An old-endpoint row at `ms`.
fn old_row(id: &str, ms: i64) -> serde_json::Value {
    let mut row = event(id);
    row["timestamp"] = ms.to_string().into();
    row
}

fn v2_limits(explicit: bool) -> Limits {
    Limits {
        max_pages: 64,
        max_v2_pages: 64,
        explicit,
        ..limits()
    }
}

fn run_v2(f: &Fixture, replies: Vec<Reply>, limits: Limits) -> (Outcome, Vec<Seen>) {
    let (url, seen) = serve(replies);
    let outcome = run_with(&f.db, &f.dir, &url, limits, &|| true);
    let seen = seen.lock().unwrap().clone();
    (outcome, seen)
}

fn v2_requests(seen: &[Seen]) -> Vec<Seen> {
    seen.iter()
        .filter(|s| {
            s.head
                .starts_with("GET /api/dashboard/v2/query-billing-items?")
        })
        .cloned()
        .collect()
}

fn query(seen: &Seen) -> std::collections::BTreeMap<String, String> {
    let target = seen.head.split(' ').nth(1).unwrap();
    reqwest::Url::parse(&format!("http://h{target}"))
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
}

fn sidecar_path(dir: &Path) -> PathBuf {
    dir.join(sidecar_name(&stem(USER).unwrap()))
}

fn sidecar_json(dir: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(sidecar_path(dir)).unwrap()).unwrap()
}

fn live_rows(dir: &Path) -> Vec<serde_json::Value> {
    let file: serde_json::Value =
        serde_json::from_slice(&std::fs::read(expected_file(dir)).unwrap()).unwrap();
    file["usageEventsDisplay"].as_array().unwrap().clone()
}

fn row_ms(row: &serde_json::Value) -> i64 {
    timestamp_ms(row["timestamp"].as_str().unwrap()).unwrap()
}

fn row_tokens(row: &serde_json::Value) -> f64 {
    [
        "inputTokens",
        "outputTokens",
        "cacheReadTokens",
        "cacheWriteTokens",
    ]
    .iter()
    .map(|key| row["tokenUsage"][key].as_f64().unwrap_or(0.0))
    .sum()
}

fn tokens_in_month(dir: &Path, back: i32) -> f64 {
    let (start, end) = (month_start_ms(back), month_start_ms(back - 1));
    live_rows(dir)
        .iter()
        .filter(|row| (start..end).contains(&row_ms(row)))
        .map(row_tokens)
        .sum()
}

/// The live file as the old walk writes it, from `rows`.
fn seed_by_old_walk(f: &Fixture, rows: &[serde_json::Value]) {
    let (outcome, _) = run_v2(f, vec![old_page(rows)], v2_limits(false));
    assert_eq!(outcome.stop.state, State::Ok);
}

/// The cutover the first V2 round below leaves: the newest old row.
fn v2_cutover() -> i64 {
    month_start_ms(1) + 2 * HOUR
}

/// Two old rows in M-1 (the newer one is the cutover), then a first V2
/// round (windows M, M-1): `v-current` in M; in M-1 one item before the
/// cutover, one at it, `v-after` after it.
fn first_v2_round(f: &Fixture) -> Outcome {
    let cutover = v2_cutover();
    seed_by_old_walk(
        f,
        &[
            old_row("a", month_start_ms(1) + HOUR),
            old_row("b", cutover),
        ],
    );
    let (outcome, seen) = run_v2(
        f,
        vec![
            old_migrated(),
            v2_page(&[item("v-current", month_start_ms(0))], None),
            v2_page(
                &[
                    item("v-before", cutover - HOUR / 2),
                    item("v-at", cutover),
                    item("v-after", cutover + HOUR),
                ],
                None,
            ),
        ],
        v2_limits(false),
    );
    assert_eq!(
        v2_requests(&seen).len(),
        2,
        "range: the cutover's month to now"
    );
    outcome
}

/// D1a control: an account the old endpoint still answers never reaches V2.
/// Mutation: `if page == 1 && lacks_usage_events(&body)` -> `if page == 1`.
#[test]
fn unmigrated_account_never_calls_billing_v2() {
    let f = fixture();
    let (outcome, seen) = run_v2(&f, vec![json_ok(&page(1, &["a"]))], v2_limits(true));
    assert_eq!(outcome, ok(1));
    assert_eq!(seen.len(), 1);
    assert!(seen[0].head.starts_with("POST /api/usage "));
    assert!(!sidecar_path(&f.dir).exists());
}

/// Nothing is frozen at the switch: a refused V2 round leaves the old file
/// alone and creates no sidecar, and a later old success writes only its
/// own rows. Mutation: in `billing_v2`, after `let (windows,
/// stop_after_empty) = …;` add `let _ = commit_v2(dir, stem, Fetched { rows:
/// Vec::new(), months: Vec::new() }, now_ms, enabled);` (freeze at switch).
#[test]
fn a_refused_switch_freezes_nothing_and_old_success_writes_only_its_rows() {
    let f = fixture();
    let t = month_start_ms(1) + HOUR;
    seed_by_old_walk(&f, &[old_row("a", t), old_row("b", t + 1000)]);
    let before = std::fs::read(expected_file(&f.dir)).unwrap();
    let (outcome, seen) = run_v2(
        &f,
        vec![
            old_migrated(),
            json_ok(r#"{"availability":"missing_team_id"}"#),
        ],
        v2_limits(false),
    );
    assert_eq!(
        outcome,
        stop(State::Error, Some("billing_v2_team_required"))
    );
    assert_eq!(v2_requests(&seen).len(), 1);
    assert!(!sidecar_path(&f.dir).exists(), "frozen at the switch");
    assert_eq!(std::fs::read(expected_file(&f.dir)).unwrap(), before);

    let rows = [
        old_row("a", t),
        old_row("b", t + 1000),
        old_row("c", t + 2000),
    ];
    let (outcome, _) = run_v2(&f, vec![old_page(&rows)], v2_limits(false));
    assert_eq!(outcome, ok(3));
    assert!(!sidecar_path(&f.dir).exists());
    assert_eq!(live_rows(&f.dir).len(), 3);
    let total: f64 = live_rows(&f.dir).iter().map(row_tokens).sum();
    assert_eq!(total, 3.0 * OLD_ROW_TOKENS);
}

/// D5: the first V2 commit keeps the old rows (pre-cutover total unchanged)
/// and adds only V2 rows after the cutover. Mutations: (plain replace)
/// `let mut events = history.frozen;` -> `let mut events: Vec<Event> =
/// Vec::new();`; (no cutover filter) `.filter(|row| cutover.is_none_or(|cutover|
/// row.ms > cutover))` -> `.filter(|_| true)`.
#[test]
fn the_first_v2_commit_keeps_old_rows_and_adds_only_rows_after_the_cutover() {
    let f = fixture();
    let outcome = first_v2_round(&f);
    assert_eq!(outcome, ok(4));
    let cutover = v2_cutover();
    let rows = live_rows(&f.dir);
    assert_eq!(rows.len(), 4);
    let pre: f64 = rows
        .iter()
        .filter(|row| row_ms(row) <= cutover)
        .map(row_tokens)
        .sum();
    assert_eq!(pre, 2.0 * OLD_ROW_TOKENS, "pre-cutover total changed");
    let after: Vec<i64> = rows
        .iter()
        .filter(|row| row_ms(row) > cutover)
        .map(row_ms)
        .collect();
    assert_eq!(after.len(), 2);
    assert!(after.contains(&(cutover + HOUR)) && after.contains(&month_start_ms(0)));
    let history = sidecar_json(&f.dir);
    assert_eq!(history["cutoverMs"], cutover);
    assert_eq!(history["frozen"].as_array().unwrap().len(), 2);
}

/// Design 4: a second V2 commit keeps the sidecar, its cutover and frozen
/// rows byte for byte, and writes an item first seen in round 2.
/// Mutation: `remove_usage_files(dir, &[&final_name, &sidecar_name])` ->
/// `remove_usage_files(dir, &[&final_name])`.
#[test]
fn a_second_v2_commit_keeps_the_sidecar_and_its_cutover() {
    let f = fixture();
    assert_eq!(first_v2_round(&f).stop.state, State::Ok);
    assert!(
        sidecar_path(&f.dir).exists(),
        "the first commit dropped its sidecar"
    );
    let first = sidecar_json(&f.dir);
    let cutover = v2_cutover();
    let new_ms = (now_ms_i64() - 60_000).max(month_start_ms(0) + 1);
    let (outcome, _) = run_v2(
        &f,
        vec![
            old_migrated(),
            v2_page(
                &[item("v-current", month_start_ms(0)), item("v-new", new_ms)],
                None,
            ),
            v2_page(&[item("v-after", cutover + HOUR)], None),
        ],
        v2_limits(false),
    );
    assert_eq!(outcome.stop.state, State::Ok);
    assert!(sidecar_path(&f.dir).exists(), "the sidecar was dropped");
    let second = sidecar_json(&f.dir);
    assert_eq!(
        second["cutoverMs"].to_string(),
        first["cutoverMs"].to_string()
    );
    assert_eq!(second["frozen"].to_string(), first["frozen"].to_string());
    let rows = live_rows(&f.dir);
    assert_eq!(rows.len(), 5);
    assert!(rows.iter().any(|row| row_ms(row) == new_ms));
}

/// Design 5b: a background round reads only M-2..M; month M-5's V2 rows
/// stay, and an item dated M-5 inside window M (a month this round did not
/// read) is not written. Mutations: (A) delete `events.extend(live_v2
/// .into_iter().filter(|event| !replaced(event)));`; (B) delete
/// `rows.retain(|row| months.contains(&row.month));`.
#[test]
fn a_background_round_keeps_the_months_it_did_not_re_read() {
    let f = fixture();
    let at = |back: i32| month_start_ms(back) + if back == 0 { 0 } else { HOUR };
    let mut replies = vec![old_migrated()];
    replies.extend((0..12).map(|back| v2_page(&[item(&format!("m{back}"), at(back))], None)));
    let (outcome, seen) = run_v2(&f, replies, v2_limits(true));
    assert_eq!(outcome, ok(12));
    assert_eq!(v2_requests(&seen).len(), 12);
    assert_eq!(tokens_in_month(&f.dir, 5), ITEM_TOKENS);

    let mut leak = item("leak", at(5) + HOUR);
    leak["usage"]["prompt-text-tokens"] = "999999".into();
    let (outcome, seen) = run_v2(
        &f,
        vec![
            old_migrated(),
            v2_page(&[item("m0", at(0)), leak], None),
            v2_empty(),
            v2_empty(),
        ],
        v2_limits(false),
    );
    assert_eq!(outcome.stop.state, State::Ok);
    assert_eq!(
        v2_requests(&seen).len(),
        3,
        "a background round reads M-2..M"
    );
    assert_eq!(tokens_in_month(&f.dir, 5), ITEM_TOKENS, "month M-5 changed");
    assert_eq!(live_rows(&f.dir).len(), 12);
}

/// Design 6: a month that had rows and now answers `{}` keeps them; the
/// rest of the file is updated. Mutation: in `replaced`,
/// `with_rows.contains_key(&month)` -> `months.contains(&month)`.
#[test]
fn a_month_answering_empty_keeps_its_rows() {
    let f = fixture();
    assert_eq!(first_v2_round(&f).stop.state, State::Ok);
    let cutover = v2_cutover();
    let new_ms = (now_ms_i64() - 60_000).max(month_start_ms(0) + 1);
    let (outcome, _) = run_v2(
        &f,
        vec![
            old_migrated(),
            v2_page(
                &[item("v-current", month_start_ms(0)), item("v-new", new_ms)],
                None,
            ),
            v2_empty(),
        ],
        v2_limits(true),
    );
    assert_eq!(outcome.stop.state, State::Ok);
    let times: Vec<i64> = live_rows(&f.dir).iter().map(row_ms).collect();
    assert!(
        times.contains(&(cutover + HOUR)),
        "M-1's row was dropped on {{}}"
    );
    assert!(times.contains(&new_ms), "the file was not updated");
    assert_eq!(times.len(), 5);
}

/// An item on the M-1/M boundary that both windows return is written once.
/// Mutation: `if ids.insert(id) { rows.push(row); }` -> `ids.insert(id);
/// rows.push(row);`.
#[test]
fn an_item_on_a_window_boundary_is_written_once() {
    let f = fixture();
    let boundary = item(ITEM_ID_CANARY, month_start_ms(0) - 500);
    let (outcome, _) = run_v2(
        &f,
        vec![
            old_migrated(),
            v2_page(&[boundary.clone(), item("c", month_start_ms(0))], None),
            v2_page(&[boundary], None),
            v2_empty(),
            v2_empty(),
            v2_empty(),
        ],
        v2_limits(false),
    );
    assert_eq!(outcome.stop.state, State::Ok);
    let at_boundary = live_rows(&f.dir)
        .iter()
        .filter(|row| row_ms(row) == month_start_ms(0) - 500)
        .count();
    assert_eq!(at_boundary, 1);
}

/// Refusals and partial walks leave the live file and the sidecar as they
/// were, with fixed reasons; a 429 stops the whole walk. Mutations: swap
/// the `Some("missing_team_id")` arm's reason with the `Some(_)` arm's;
/// delete `|| !cursors.insert(next.clone())`; delete `next.len() >
/// MAX_CURSOR_BYTES ||`; delete the `if status.as_u16() == 429 {…}` block
/// in `refusal`.
#[test]
fn refusals_leave_the_file_and_sidecar_unchanged() {
    let f = fixture();
    assert_eq!(first_v2_round(&f).stop.state, State::Ok);
    let live = std::fs::read(expected_file(&f.dir)).unwrap();
    let side = std::fs::read(sidecar_path(&f.dir)).unwrap();
    let current = || item("v-current", month_start_ms(0));
    let long = "c".repeat(4097);
    let cases: Vec<(Vec<Reply>, Outcome, usize)> = vec![
        (
            vec![json_ok(r#"{"availability":"missing_team_id"}"#)],
            stop(State::Error, Some("billing_v2_team_required")),
            1,
        ),
        (
            vec![json_ok(r#"{"availability":"not_enabled"}"#)],
            stop(State::Error, Some("billing_v2_unavailable")),
            1,
        ),
        (
            vec![v2_page(&[current()], Some("c1")), v2_page(&[], Some("c1"))],
            stop(State::Partial, Some("too_many_pages")),
            2,
        ),
        (
            vec![v2_page(&[current()], Some(&long))],
            stop(State::Partial, Some("too_many_pages")),
            1,
        ),
        (
            vec![
                v2_page(&[current()], None),
                http("429 Too Many Requests", "application/json", "{}"),
            ],
            stop(State::Error, Some("rate_limited")),
            2,
        ),
        (
            vec![http("401 Unauthorized", "application/json", "{}")],
            stop(State::Expired, None),
            1,
        ),
        (
            vec![http("403 Forbidden", "text/html", "<html>blocked</html>")],
            stop(State::Error, Some("forbidden_non_json")),
            1,
        ),
        (
            vec![http("500 Internal Server Error", "text/plain", "x")],
            stop(State::Error, Some("server_error")),
            1,
        ),
        (
            vec![json_ok("[]")],
            stop(State::Error, Some("unexpected_response")),
            1,
        ),
        (
            vec![json_ok("<html>")],
            stop(State::Error, Some("unexpected_response")),
            1,
        ),
    ];
    for (replies, expected, requests) in cases {
        let mut all = vec![old_migrated()];
        all.extend(replies);
        let (outcome, seen) = run_v2(&f, all, v2_limits(false));
        assert_eq!(outcome, expected);
        let v2 = v2_requests(&seen);
        assert_eq!(v2.len(), requests, "{expected:?}");
        if requests == 2 && expected.stop.reason == Some("too_many_pages") {
            assert_eq!(query(&v2[1]).get("cursor").map(String::as_str), Some("c1"));
        }
        assert_eq!(std::fs::read(expected_file(&f.dir)).unwrap(), live);
        assert_eq!(std::fs::read(sidecar_path(&f.dir)).unwrap(), side);
    }
}

/// Design 5: no old file and rows only in the current month: the first
/// background round (full budget: no file yet) reads M (two pages), M-1,
/// M-2, M-3 and commits. The request is the GET the plan names. Mutation:
/// delete `if stop_after_empty && empty_run >= EMPTY_WINDOWS_STOP { break; }`.
#[test]
fn no_old_file_and_rows_only_this_month_commit_after_four_windows() {
    let _guard = lock();
    // The default profile's key is the same in every test: forget what an
    // earlier test's last round left, so this background round sees a
    // profile with no file (full budget).
    IN_FLIGHT
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .no_file
        .clear();
    let f = fixture();
    let (url, seen) = serve(vec![
        old_migrated(),
        v2_page(&[item("x1", month_start_ms(0))], Some("n1")),
        v2_page(&[item("x2", month_start_ms(0) + 1)], None),
        v2_empty(),
        json_ok(r#"{"data":[]}"#),
        json_ok(r#"{"data":[],"pagination":{}}"#),
    ]);
    let status = sync_background(&f, &url, &registry(true, &f.dir));
    assert_eq!(status["state"], "ok", "{status}");
    assert_eq!(status["events"], 2);
    assert_eq!(ROUND_BUDGET.with(|b| b.get()), Some(FULL_BUDGET));
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 6);
    let v2 = v2_requests(&seen);
    assert_eq!(v2.len(), 5);
    let first = query(&v2[0]);
    assert_eq!(first.get("scope").map(String::as_str), Some("user"));
    assert_eq!(first.get("pageSize").map(String::as_str), Some("100"));
    assert_eq!(
        first.get("startTime"),
        Some(&iso(month_start_ms(0)).replace(".000Z", "Z"))
    );
    assert!(first.get("endTime").unwrap().ends_with('Z'));
    assert!(!first.contains_key("cursor"));
    assert_eq!(query(&v2[1]).get("cursor").map(String::as_str), Some("n1"));
    assert_eq!(
        query(&v2[2]).get("endTime"),
        Some(&iso(month_start_ms(0) - 1000).replace(".000Z", "Z"))
    );
    for request in &v2 {
        assert_eq!(
            header(&request.head, "cookie").as_deref(),
            Some(format!("WorkosCursorSessionToken={USER}%3A%3A{}", f.token).as_str())
        );
        let head = request.head.to_ascii_lowercase();
        assert!(head.contains("origin: https://cursor.com\r\n"));
        assert!(head.contains("accept: application/json\r\n"));
    }
    assert_eq!(sidecar_json(&f.dir)["cutoverMs"], serde_json::Value::Null);
}

/// D6: no old file and no rows anywhere: nothing is written. Mutation:
/// delete `if events.is_empty() { return Err(Stop::error("billing_v2_no_items")); }`.
#[test]
fn no_old_file_and_no_rows_writes_nothing() {
    let f = fixture();
    let mut grok = item("g", month_start_ms(1) + HOUR);
    grok["product"] = "grok-bot".into();
    let (outcome, seen) = run_v2(
        &f,
        vec![
            old_migrated(),
            v2_empty(),
            v2_page(&[grok], None),
            json_ok(r#"{"data":[]}"#),
        ],
        v2_limits(false),
    );
    assert_eq!(outcome, stop(State::Error, Some("billing_v2_no_items")));
    assert_eq!(v2_requests(&seen).len(), 3);
    assert!(!expected_file(&f.dir).exists());
    assert!(!sidecar_path(&f.dir).exists());
}

/// S4: the page cap counts every window of one walk. Mutation: move `let
/// mut pages = 0u32;` into the `for &month in windows` loop (per window).
#[test]
fn the_page_cap_counts_across_windows() {
    let f = fixture();
    let mut replies = vec![old_migrated()];
    replies.extend(
        (0..12).map(|back| v2_page(&[item(&format!("p{back}"), month_start_ms(back) + 1)], None)),
    );
    let (outcome, seen) = run_v2(
        &f,
        replies,
        Limits {
            max_v2_pages: 3,
            ..v2_limits(true)
        },
    );
    assert_eq!(outcome, stop(State::Partial, Some("too_many_pages")));
    assert_eq!(v2_requests(&seen).len(), 3);
    assert!(!expected_file(&f.dir).exists());
}

/// Disable, profile removal and an account switch delete live files and
/// sidecars; a sidecar of another stem is never read and is cleaned by a V2
/// commit. Mutation: `|| is_sidecar_name(name)` deleted from `is_ours`.
#[test]
fn cleanup_deletes_sidecars_and_ignores_another_stems() {
    let sidecar_of = |c: char| format!("{SIDECAR_PREFIX}{}.json", c.to_string().repeat(64));
    {
        let _guard = lock();
        set_for_test(Config::default());
        let home = tempfile::tempdir().unwrap();
        let app = app_dir_in(home.path());
        let off = serde_json::json!({"enabled": false, "bundleId": BUNDLE});
        set_in(home.path(), off.clone()).unwrap();
        // Profile removal: A is not configured, so an on push removes it.
        // (Nothing complete-shaped sits in a configured dir while sync is
        // on, so no other test's takeover sees it.)
        let orphan = app.join("cursor-profiles").join(PROFILE_A);
        std::fs::create_dir_all(&orphan).unwrap();
        std::fs::write(orphan.join(hex_name('a')), "x").unwrap();
        std::fs::write(orphan.join(sidecar_of('a')), "x").unwrap();
        let on = serde_json::json!({"enabled": true, "bundleId": BUNDLE, "profiles": []});
        set_in(home.path(), on).unwrap();
        assert!(!orphan.exists(), "the orphan's sidecar was left");
        // Disable.
        set_in(home.path(), off.clone()).unwrap();
        let default = app.join("cursor-cache");
        std::fs::create_dir_all(&default).unwrap();
        std::fs::write(default.join(hex_name('a')), "x").unwrap();
        std::fs::write(default.join(sidecar_of('a')), "x").unwrap();
        set_in(home.path(), off).unwrap();
        assert!(names_in(&default).is_empty(), "{:?}", names_in(&default));
        set_for_test(Config::default());
    }

    // Account switch: another account's live file and sidecar go with the
    // next commit for this account.
    let f = fixture();
    std::fs::create_dir_all(&f.dir).unwrap();
    std::fs::write(f.dir.join(hex_name('b')), "x").unwrap();
    std::fs::write(f.dir.join(sidecar_of('b')), "x").unwrap();
    seed_by_old_walk(&f, &[old_row("a", month_start_ms(1) + HOUR)]);
    assert_eq!(
        names_in(&f.dir),
        [expected_file(&f.dir)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()]
    );

    // Another stem's sidecar is never read by a V2 commit, and goes.
    let f = fixture();
    std::fs::create_dir_all(&f.dir).unwrap();
    let foreign = serde_json::json!({"frozen": [old_row("x", 1)], "cutoverMs": 1, "fetched": {}});
    std::fs::write(f.dir.join(sidecar_of('c')), foreign.to_string()).unwrap();
    let (outcome, _) = run_v2(
        &f,
        vec![
            old_migrated(),
            v2_page(&[item("only", month_start_ms(0))], None),
            v2_empty(),
            v2_empty(),
            v2_empty(),
        ],
        v2_limits(false),
    );
    assert_eq!(outcome, ok(1));
    assert!(!f.dir.join(sidecar_of('c')).exists());
    assert_eq!(live_rows(&f.dir).len(), 1);
    assert!(sidecar_json(&f.dir)["frozen"]
        .as_array()
        .unwrap()
        .is_empty());
}

/// S5/S7: `userEmail`, `principal.id`, `billingItemId`, the token and the
/// cookie never reach a file or the status; a page carrying two users
/// other than the cookie's, or an item without a principal, stops the walk
/// with nothing written (one other user is accepted:
/// `v2_one_user_principal_other_than_the_cookie_user`). Mutations: in
/// `struct Extras`, put `#[serde(rename = "userEmail")]` on `model`; make
/// the principal refusal in `walk` `if false`.
#[test]
fn v2_canaries_stay_out_and_a_second_other_principal_writes_nothing() {
    let _guard = lock();
    let mut outputs = Vec::new();
    let mut collect = |f: &Fixture, status: serde_json::Value| {
        outputs.push(status.to_string());
        for entry in std::fs::read_dir(&f.dir).into_iter().flatten().flatten() {
            outputs.push(entry.file_name().to_string_lossy().into_owned());
            if entry.file_type().unwrap().is_file() {
                outputs.push(std::fs::read_to_string(entry.path()).unwrap());
            }
        }
    };
    let f = fixture();
    let mut replies = vec![
        old_migrated(),
        v2_page(&[item(ITEM_ID_CANARY, month_start_ms(0))], None),
    ];
    replies.extend((0..11).map(|_| v2_empty()));
    let (url, _) = serve(replies);
    let status = sync_status(&f, &url, &registry(true, &f.dir));
    assert_eq!(status["state"], "ok", "{status}");
    collect(&f, status);

    let mut other = item("o", month_start_ms(0));
    other["principal"]["id"] = OTHER_USER.into();
    let mut third = item("t", month_start_ms(0));
    third["principal"]["id"] = THIRD_USER.into();
    let mut missing = item("m", month_start_ms(0));
    missing.as_object_mut().unwrap().remove("principal");
    for bad in [vec![other, third], vec![missing]] {
        let f = fixture();
        let mut page = vec![item("fine", month_start_ms(0))];
        page.extend(bad);
        let (url, _) = serve(vec![old_migrated(), v2_page(&page, None)]);
        let status = sync_status(&f, &url, &registry(true, &f.dir));
        assert_eq!(status["state"], "error");
        assert_eq!(status["reason"], "billing_v2_principal_mismatch");
        assert!(!expected_file(&f.dir).exists());
        assert!(!sidecar_path(&f.dir).exists());
        collect(&f, status);
    }

    let all = outputs.join("\n");
    for canary in [
        EMAIL_CANARY,
        USER,
        OTHER_USER,
        THIRD_USER,
        ITEM_ID_CANARY,
        SIGNATURE,
        f.token.as_str(),
    ] {
        assert!(!all.contains(canary), "canary leaked");
    }
    // Control: the outputs searched hold the written row and the refusal.
    assert!(all.contains("composer-2.5") && all.contains("billing_v2_principal_mismatch"));
}

/// #520: a migrated account's items can carry one user id that is not the
/// cookie's (reporter: 100 of 100 items, one distinct id). The walk accepts
/// it across pages and windows and keeps it out of the files and the
/// status; it accepts it beside the cookie's own id (months before the
/// move), whose `type` is not read, as on main. After the first other id it
/// refuses a third id on a later page or window, a `team`, `organization`
/// or untyped principal on another id, and a missing principal; it refuses a first
/// `team` principal on another id and items whose ids are all empty.
/// Mutations: in `walk`, refuse `Owner::OtherUser`; accept it without the
/// comparison; declare `other` inside the window loop; accept
/// `Owner::Refused`; in `owner`, drop the `type` test; refuse only `team`;
/// let an untyped other id pass; require `type: user` for the cookie's id; pass an empty id.
#[test]
fn v2_one_user_principal_other_than_the_cookie_user() {
    let _guard = lock();
    let as_user = |id: &str, ms: i64, user: &str| {
        let mut item = item(id, ms);
        item["principal"]["id"] = user.into();
        item
    };
    let f = fixture();
    let mut replies = vec![
        old_migrated(),
        v2_page(&[as_user("o1", month_start_ms(0), OTHER_USER)], Some("n1")),
        v2_page(&[as_user("o2", month_start_ms(0) + 1, OTHER_USER)], None),
        v2_page(&[as_user("o3", month_start_ms(1), OTHER_USER)], None),
    ];
    replies.extend((0..10).map(|_| v2_empty()));
    let (url, _) = serve(replies);
    let status = sync_status(&f, &url, &registry(true, &f.dir));
    assert_eq!(status["state"], "ok", "{status}");
    assert_eq!(status["events"], 3, "{status}");
    let mut outputs = vec![status.to_string()];
    for entry in std::fs::read_dir(&f.dir).unwrap().flatten() {
        outputs.push(entry.file_name().to_string_lossy().into_owned());
        if entry.file_type().unwrap().is_file() {
            outputs.push(std::fs::read_to_string(entry.path()).unwrap());
        }
    }
    let all = outputs.join("\n");
    assert!(
        !all.contains(OTHER_USER) && !all.contains(USER),
        "principal leaked"
    );
    // Control: the outputs searched hold the written rows.
    assert!(expected_file(&f.dir).exists() && all.contains("composer-2.5"));

    // The move inside one walk: the newest month on the other id, older
    // items on the cookie's id with `type: user`, none, or another spelling.
    let f = fixture();
    let mut untyped = item("c2", month_start_ms(0) + 2);
    untyped["principal"].as_object_mut().unwrap().remove("type");
    let mut spelled = item("c3", month_start_ms(0) + 3);
    spelled["principal"]["type"] = "USER".into();
    let mut replies = vec![
        old_migrated(),
        v2_page(&[as_user("o1", month_start_ms(0), OTHER_USER)], Some("n1")),
        v2_page(&[untyped, spelled], None),
        v2_page(&[item("c4", month_start_ms(1))], None),
    ];
    replies.extend((0..10).map(|_| v2_empty()));
    let (url, _) = serve(replies);
    let status = sync_status(&f, &url, &registry(true, &f.dir));
    assert_eq!(
        status["state"], "ok",
        "cookie id beside the other: {status}"
    );
    assert_eq!(status["events"], 4, "{status}");

    let pinned = || as_user("a", month_start_ms(0), OTHER_USER);
    let later = |change: &dyn Fn(&mut serde_json::Value)| {
        let mut item = as_user("b", month_start_ms(0) + 1, OTHER_USER);
        change(&mut item);
        vec![v2_page(&[pinned()], Some("n1")), v2_page(&[item], None)]
    };
    let mut team = as_user("t", month_start_ms(0), OTHER_USER);
    team["principal"]["type"] = "team".into();
    let cases = [
        (
            "third id on a later page",
            later(&|i| i["principal"]["id"] = THIRD_USER.into()),
        ),
        (
            "third id in a later window",
            vec![
                v2_page(&[pinned()], None),
                v2_page(&[as_user("b", month_start_ms(1), THIRD_USER)], None),
            ],
        ),
        (
            "team after the pin",
            later(&|i| i["principal"]["type"] = "team".into()),
        ),
        (
            "organization after the pin",
            later(&|i| i["principal"]["type"] = "organization".into()),
        ),
        (
            "untyped other after the pin",
            later(&|i| {
                i["principal"].as_object_mut().unwrap().remove("type");
            }),
        ),
        (
            "missing after the pin",
            later(&|i| {
                i.as_object_mut().unwrap().remove("principal");
            }),
        ),
        (
            "every id empty",
            vec![v2_page(
                &[
                    as_user("a", month_start_ms(0), ""),
                    as_user("b", month_start_ms(0) + 1, ""),
                ],
                None,
            )],
        ),
        ("first item a team", vec![v2_page(&[team], None)]),
    ];
    for (case, pages) in cases {
        let f = fixture();
        let mut replies = vec![old_migrated()];
        replies.extend(pages);
        replies.extend((0..12).map(|_| v2_empty()));
        let (url, _) = serve(replies);
        let status = sync_status(&f, &url, &registry(true, &f.dir));
        assert_eq!(status["state"], "error", "{case}: {status}");
        assert_eq!(status["reason"], "billing_v2_principal_mismatch", "{case}");
        assert!(!expected_file(&f.dir).exists(), "{case}");
        assert!(!sidecar_path(&f.dir).exists(), "{case}");
    }
}

/// S8: an unparseable `eventTime` is dropped; a future newest old row
/// clamps the cutover to now. Mutations: (drop) `let ms =
/// timestamp_ms(&item.event_time?)?;` -> `let ms =
/// timestamp_ms(&item.event_time?).unwrap_or(0);` together with deleting
/// `rows.retain(|row| months.contains(&row.month));`; (clamp)
/// `.map_or(now_ms, |newest| newest.min(now_ms))` -> `.map_or(now_ms,
/// |newest| newest)`.
#[test]
fn bad_event_times_are_dropped_and_a_future_cutover_is_clamped() {
    let f = fixture();
    let mut bad = item("bad", month_start_ms(0));
    bad["eventTime"] = "yesterday".into();
    let (outcome, _) = run_v2(
        &f,
        vec![
            old_migrated(),
            v2_page(&[bad, item("good", month_start_ms(0))], None),
            v2_empty(),
            v2_empty(),
            v2_empty(),
        ],
        v2_limits(false),
    );
    assert_eq!(
        outcome,
        Outcome {
            dropped: 1,
            ..ok(1)
        }
    );

    let f = fixture();
    let future = 4_102_444_800_000; // 2100-01-01
    seed_by_old_walk(&f, &[old_row("a", month_start_ms(1)), old_row("z", future)]);
    let before = now_ms_i64();
    let (outcome, _) = run_v2(
        &f,
        vec![
            old_migrated(),
            v2_page(&[item("v", month_start_ms(0))], None),
        ],
        v2_limits(true),
    );
    let after = now_ms_i64();
    assert_eq!(outcome.stop.state, State::Ok);
    let cutover = sidecar_json(&f.dir)["cutoverMs"].as_i64().unwrap();
    assert!((before..=after).contains(&cutover), "cutover {cutover}");
}

/// Discovery §4 mapping: meters, model, timestamp, no conversation id; D2
/// (contracted 0 -> no chargedCents, USD > 0 -> integer cents, another
/// currency -> none); D3 (only `cursor` rows); `hasTokenMeters:false`
/// skipped; only the engine's fields written. Mutations: see the report.
#[test]
fn v2_items_map_to_engine_rows() {
    let f = fixture();
    let t = month_start_ms(0);
    let included = item("included", t);
    let mut paid = item("paid", t + 1);
    paid["pricing"]["contracted"] = "0.0700000000".into();
    let mut euro = item("euro", t + 4);
    euro["pricing"] = serde_json::json!({"currency": "EUR", "contracted": "0.0700000000"});
    let mut garbage = item("garbage", t + 5);
    garbage["pricing"]["contracted"] = "FREETEXTCANARY".into();
    let mut number = item("number", t + 6);
    number["pricing"]["contracted"] = serde_json::json!(4e-3);
    let mut grok = item("grok", t + 2);
    grok["product"] = "grok-bot".into();
    let mut meterless = item("meterless", t + 3);
    meterless["hasTokenMeters"] = false.into();
    let (outcome, _) = run_v2(
        &f,
        vec![
            old_migrated(),
            v2_page(
                &[included, paid, grok, meterless, euro, garbage, number],
                None,
            ),
            v2_empty(),
            v2_empty(),
            v2_empty(),
        ],
        v2_limits(false),
    );
    assert_eq!(outcome, ok(5));
    let rows = live_rows(&f.dir);
    assert_eq!(rows.len(), 5);
    let garbage = rows.iter().find(|r| row_ms(r) == t + 5).unwrap();
    assert!(
        garbage.get("chargedCents").is_none(),
        "free text priced the row"
    );
    assert!(!live_rows(&f.dir)
        .iter()
        .any(|r| r.to_string().contains("FREETEXTCANARY")));
    let number = rows.iter().find(|r| row_ms(r) == t + 6).unwrap();
    assert_eq!(
        number["chargedCents"].to_string(),
        "1",
        "a paid row floors at 1 cent"
    );
    let euro = rows.iter().find(|r| row_ms(r) == t + 4).unwrap();
    assert!(
        euro.get("chargedCents").is_none(),
        "a EUR amount was written as cents"
    );
    let included = rows.iter().find(|r| row_ms(r) == t).unwrap();
    let paid = rows.iter().find(|r| row_ms(r) == t + 1).unwrap();
    assert_eq!(included["timestamp"], t.to_string());
    assert_eq!(included["model"], "composer-2.5");
    assert!(included.get("conversationId").is_none());
    assert!(
        included.get("chargedCents").is_none(),
        "D2: included row priced"
    );
    assert_eq!(
        paid["chargedCents"].to_string(),
        "7",
        "integer cents, no float"
    );
    let usage = &included["tokenUsage"];
    assert_eq!(usage["inputTokens"], 1263.0);
    assert_eq!(usage["cacheReadTokens"], 46400.0);
    assert_eq!(usage["cacheWriteTokens"], 0.0);
    assert_eq!(usage["outputTokens"], 312.0);
    assert!(usage.get("totalCents").is_none());
    for row in &rows {
        for key in row.as_object().unwrap().keys() {
            assert!(ALLOWED_EVENT_FIELDS.contains(&key.as_str()), "{key}");
        }
        for key in row["tokenUsage"].as_object().unwrap().keys() {
            assert!(ALLOWED_TOKEN_FIELDS.contains(&key.as_str()), "{key}");
        }
    }
}

/// Design 4: a successful old walk after V2 commits brings the full old
/// history back: the sidecar goes and the live file is the old rows alone.
/// Mutation (both, since either deletion alone suffices): delete `let _ =
/// std::fs::remove_file(dir.join(sidecar_name(stem)));` in `commit_file`,
/// and delete `|| is_sidecar_name(name)` from `is_ours`.
#[test]
fn an_old_success_after_v2_deletes_the_sidecar() {
    let f = fixture();
    assert_eq!(first_v2_round(&f).stop.state, State::Ok);
    assert!(sidecar_path(&f.dir).exists());
    let rows = [old_row("a", month_start_ms(1) + HOUR)];
    let (outcome, seen) = run_v2(&f, vec![old_page(&rows)], v2_limits(false));
    assert_eq!(outcome, ok(1));
    assert!(v2_requests(&seen).is_empty());
    assert!(
        !sidecar_path(&f.dir).exists(),
        "the sidecar outlived an old success"
    );
    assert_eq!(live_rows(&f.dir).len(), 1);
}

// --- Billing V2, review round 2 ------------------------------------------------

/// `sync_status` as a background round (not Sync Now).
fn sync_background(f: &Fixture, url: &str, registry: &RwLock<Config>) -> serde_json::Value {
    let billing = v2_url(url);
    let target = Target {
        db_path: &f.db,
        url,
        billing_url: &billing,
        https_only: false,
        file_stem: &stem,
        key_fetch: None,
        consent: &no_consent,
    };
    let source = || registry.read().unwrap().clone();
    sync_with(false, None, &target, &source, Some(limits())).0
}

fn month_of(back: i32) -> i64 {
    i64::from(cursor_billing_v2::month_index(month_start_ms(back)).unwrap())
}

/// A profile that is a duplicate of an earlier one keeps its own live file
/// and that account's sidecar; another account's sidecar goes. Mutation:
/// `locked_removal(dir, &[&live_name(&stem), &sidecar_name(&stem)], false)`
/// -> `locked_removal(dir, &[&live_name(&stem)], false)`.
#[test]
fn a_duplicate_profile_keeps_its_own_sidecar() {
    let _guard = lock();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let token = valid_token();
    let default_db = login_db(&tmp.path().join("default"), &token, USER);
    let root = profile_root(tmp.path(), "b", &token, USER);
    let registry = RwLock::new(multi(&home, true, vec![profile(PROFILE_A, &root)]));
    let dir = profile_dir(&home, PROFILE_A);
    std::fs::create_dir_all(&dir).unwrap();
    let own_sidecar = sidecar_path(&dir);
    let other_sidecar = dir.join(format!("{SIDECAR_PREFIX}{}.json", "e".repeat(64)));
    std::fs::write(
        expected_file(&dir),
        r#"{"totalUsageEventsCount":0,"usageEventsDisplay":[]}"#,
    )
    .unwrap();
    std::fs::write(&own_sidecar, "{}").unwrap();
    std::fs::write(&other_sidecar, "{}").unwrap();
    let (url, _) = serve(vec![json_ok(&page(1, &["a"]))]);
    let status = sync_round(&default_db, &url, &registry);
    assert_eq!(states(&status), ["ok", "duplicate"]);
    assert!(expected_file(&dir).exists());
    assert!(
        own_sidecar.exists(),
        "the duplicate dropped its own sidecar"
    );
    assert!(!other_sidecar.exists());
}

/// V2 pages are a fifth of the old size, so the V2 walk has its own cap: a
/// walk over the old 256 pages commits under production limits. Mutation:
/// in `Limits::production`, `max_v2_pages: MAX_V2_PAGES,` -> `max_v2_pages:
/// MAX_PAGES,`.
#[test]
fn a_v2_walk_past_the_old_page_cap_commits() {
    let f = fixture();
    let pages = MAX_PAGES as usize + 1;
    let mut replies = vec![old_migrated()];
    replies.extend((0..pages).map(|i| {
        let next = format!("c{i}");
        v2_page(
            &[item(&format!("p{i}"), month_start_ms(0))],
            (i + 1 < pages).then_some(next.as_str()),
        )
    }));
    replies.extend((0..3).map(|_| v2_empty()));
    let (outcome, seen) = run_v2(&f, replies, Limits::production(Duration::from_secs(120)));
    assert_eq!(outcome, ok(pages));
    assert_eq!(v2_requests(&seen).len(), pages + 3);
}

/// D5: a same-stem sidecar Syrtis cannot read (a directory, or a file that
/// does not parse) stops the round before any request, old or V2.
/// Mutation: delete the `if read_history(dir, &stem).is_err() { return
/// stopped(Stop::error("write_failed")); }` block in `run`.
#[test]
fn an_unreadable_sidecar_fails_before_any_request() {
    for make in [
        (|path: &Path| std::fs::create_dir_all(path).unwrap()) as fn(&Path),
        |path: &Path| std::fs::write(path, "not json").unwrap(),
    ] {
        let f = fixture();
        std::fs::create_dir_all(&f.dir).unwrap();
        make(&sidecar_path(&f.dir));
        let mut replies = vec![old_migrated()];
        replies
            .extend((0..12).map(|i| v2_page(&[item(&format!("u{i}"), month_start_ms(0))], None)));
        let (outcome, seen) = run_v2(&f, replies, v2_limits(true));
        assert_eq!(outcome, stop(State::Error, Some("write_failed")));
        assert!(seen.is_empty(), "requests sent before failing");
        assert!(!expected_file(&f.dir).exists());
    }
}

/// The old walk's commit is unchanged by a directory at another stem's
/// sidecar name (no cleanup failure; the entry stays), while disable still
/// tries it and reports it. Mutations: delete the `if spare_sidecar_dirs &&
/// is_sidecar_name(&name) && entry.file_type().is_ok_and(|kind|
/// kind.is_dir()) { continue; }` block in `remove_usage_files`; in
/// `locked_removal`, `let result = remove_usage_files(dir, keep, false);`
/// -> `let result = remove_usage_files(dir, keep, true);`.
#[test]
fn a_directory_at_a_sidecar_name_spares_only_the_old_commit() {
    let other = format!("{SIDECAR_PREFIX}{}.json", "d".repeat(64));
    let f = fixture();
    std::fs::create_dir_all(f.dir.join(&other)).unwrap();
    let (outcome, _) = run_v2(&f, vec![json_ok(&page(1, &["a"]))], v2_limits(false));
    assert_eq!(outcome, ok(1));
    assert!(f.dir.join(&other).is_dir());
    assert_eq!(live_rows(&f.dir).len(), 1);

    let _guard = lock();
    set_for_test(Config::default());
    let home = tempfile::tempdir().unwrap();
    let off = serde_json::json!({"enabled": false, "bundleId": BUNDLE});
    set_in(home.path(), off.clone()).unwrap();
    let default = app_dir_in(home.path()).join("cursor-cache");
    std::fs::create_dir_all(default.join(&other).join("inside")).unwrap();
    let result = set_in(home.path(), off).unwrap();
    assert_eq!(result["cleanupFailed"], true, "{result}");
    set_for_test(Config::default());
}

/// Item 1 (round 3): a V2 commit, then an old walk whose sidecar delete
/// failed (the stale sidecar is put back), then V2 again. The stale sidecar
/// is not trusted: the commit freezes the old walk's file (every old row
/// kept), the cutover moves to its newest row, the months are read again,
/// and nothing is counted twice. Mutation: `Some(history) if trusted =>` ->
/// `Some(history) =>`.
#[test]
fn a_stale_sidecar_is_frozen_again_from_the_old_walks_file() {
    let f = fixture();
    assert_eq!(first_v2_round(&f).stop.state, State::Ok);
    let stale = std::fs::read(sidecar_path(&f.dir)).unwrap();
    let cutover = v2_cutover();
    let newest = cutover + 3 * HOUR;
    let rows = [
        old_row("a", month_start_ms(1) + HOUR),
        old_row("b", cutover),
        old_row("c", newest),
    ];
    let (outcome, _) = run_v2(&f, vec![old_page(&rows)], v2_limits(false));
    assert_eq!(outcome, ok(3));
    std::fs::write(sidecar_path(&f.dir), &stale).unwrap();
    let (outcome, seen) = run_v2(
        &f,
        vec![
            old_migrated(),
            v2_page(&[item("v-current", month_start_ms(0))], None),
            v2_page(
                &[
                    item("v-after", cutover + HOUR),
                    item("v-late", newest + HOUR),
                ],
                None,
            ),
        ],
        v2_limits(false),
    );
    assert_eq!(outcome, ok(5));
    assert_eq!(v2_requests(&seen).len(), 2);
    let times: Vec<i64> = live_rows(&f.dir).iter().map(row_ms).collect();
    for old in [month_start_ms(1) + HOUR, cutover, newest] {
        assert!(times.contains(&old), "old row {old} lost");
    }
    assert!(
        !times.contains(&(cutover + HOUR)),
        "counted twice: before the new cutover"
    );
    let total: f64 = live_rows(&f.dir).iter().map(row_tokens).sum();
    assert_eq!(total, 3.0 * OLD_ROW_TOKENS + 2.0 * ITEM_TOKENS);
    let history = sidecar_json(&f.dir);
    assert_eq!(history["cutoverMs"], newest);
    assert_eq!(history["frozen"].as_array().unwrap().len(), 3);
    assert_eq!(
        history["fetched"],
        serde_json::json!([month_of(1), month_of(0)])
    );
    assert!(history["liveSha256"]
        .as_str()
        .is_some_and(|d| d.len() == 64));
}

/// Item 2 (round 3): no cutover, and the live file is gone (an older
/// build's disable deletes it but not the sidecar). The background round
/// reads the full range, with no 3-empty stop, so a month V2 had read is
/// not dropped. Mutation: `let stop_after_empty = cutover.is_none() &&
/// !explicit && !stale;` -> `let stop_after_empty = cutover.is_none() &&
/// !explicit;`.
#[test]
fn a_stale_sidecar_without_a_cutover_reads_the_full_range() {
    let f = fixture();
    let months = |replies: &mut Vec<Reply>| {
        replies.push(v2_page(&[item("m0", month_start_ms(0))], None));
        replies.extend((0..4).map(|_| v2_empty()));
        replies.push(v2_page(&[item("m5", month_start_ms(5) + HOUR)], None));
        replies.extend((0..6).map(|_| v2_empty()));
    };
    let mut replies = vec![old_migrated()];
    months(&mut replies);
    let (outcome, _) = run_v2(&f, replies, v2_limits(true));
    assert_eq!(outcome, ok(2));
    std::fs::remove_file(expected_file(&f.dir)).unwrap();
    let mut replies = vec![old_migrated()];
    months(&mut replies);
    let (outcome, seen) = run_v2(&f, replies, v2_limits(false));
    assert_eq!(outcome, ok(2));
    assert_eq!(v2_requests(&seen).len(), 12);
    assert_eq!(tokens_in_month(&f.dir, 5), ITEM_TOKENS);
}

/// Item 6 (round 3): a dropped copy never hides a complete copy of the same
/// id, and only drops that would otherwise have been written count (a read
/// month, after the cutover). Mutations: in `walk`, `                        id,\n
/// month: ms.and_then(month_index).unwrap_or(month),` -> `id: {
/// ids.insert(id.clone()); id },\n month: …`; delete `.filter(|item|
/// self.months.contains(&item.month))`; delete the `.filter(|item| { item.ms
/// .is_none_or(…) })` filter in `dropped_after`.
#[test]
fn only_drops_that_would_have_been_written_count() {
    let f = fixture();
    let t = month_start_ms(0);
    let mut partial = item("same", t);
    partial["displayExtras"] = serde_json::json!({});
    // No cutover here, so only the month filter keeps this one out: M-6 is
    // a month this background walk never reads.
    let mut unread = item("unread", month_start_ms(6) + HOUR);
    unread["displayExtras"] = serde_json::json!({});
    let (outcome, _) = run_v2(
        &f,
        vec![
            old_migrated(),
            v2_page(&[partial, unread], Some("n")),
            v2_page(&[item("same", t)], None),
            v2_empty(),
            v2_empty(),
            v2_empty(),
        ],
        v2_limits(false),
    );
    assert_eq!(
        outcome,
        ok(1),
        "the complete copy was not written, or a drop that would not be written was counted"
    );

    let f = fixture();
    assert_eq!(first_v2_round(&f).stop.state, State::Ok);
    let cutover = v2_cutover();
    let model_less = |id: &str, ms: i64| {
        let mut row = item(id, ms);
        row["displayExtras"] = serde_json::json!({});
        row
    };
    let (outcome, _) = run_v2(
        &f,
        vec![
            old_migrated(),
            v2_page(
                &[
                    model_less("after", (now_ms_i64() - 60_000).max(t + 1)),
                    model_less("unread-month", month_start_ms(5) + HOUR),
                ],
                None,
            ),
            v2_page(&[model_less("before", cutover - HOUR / 2)], None),
        ],
        v2_limits(false),
    );
    assert_eq!(outcome.stop.state, State::Ok);
    assert_eq!(outcome.dropped, 1);
}

/// Rows with an unreadable `eventTime` or no model are dropped and counted
/// (`droppedRows`, a number only); they never count toward `events` or
/// against `billing_v2_no_items`. Mutations N8a-c: see the executor report.
#[test]
fn rows_without_a_time_or_model_are_dropped_and_counted() {
    let _guard = lock();
    let f = fixture();
    let t = month_start_ms(0);
    let mut bad_time = item("bad-time", t);
    bad_time["eventTime"] = "not a time".into();
    let mut no_model = item("no-model", t + 1);
    no_model["displayExtras"] = serde_json::json!({"userEmail": EMAIL_CANARY});
    let mut empty_model = item("empty-model", t + 2);
    empty_model["displayExtras"]["model"] = "".into();
    let (url, _) = serve(vec![
        old_migrated(),
        v2_page(
            &[item("good", t + 3), bad_time, no_model, empty_model.clone()],
            None,
        ),
        v2_empty(),
        v2_empty(),
        v2_empty(),
    ]);
    let status = sync_background(&f, &url, &registry(true, &f.dir));
    assert_eq!(status["state"], "ok", "{status}");
    assert_eq!(status["events"], 1);
    assert_eq!(status["profiles"][0]["droppedRows"], 3);
    assert_eq!(live_rows(&f.dir).len(), 1);

    let f = fixture();
    let (outcome, _) = run_v2(
        &f,
        vec![
            old_migrated(),
            v2_page(&[empty_model], None),
            v2_empty(),
            v2_empty(),
        ],
        v2_limits(false),
    );
    assert_eq!(
        outcome,
        Outcome {
            dropped: 1,
            ..stop(State::Error, Some("billing_v2_no_items"))
        }
    );
    assert!(!expected_file(&f.dir).exists());
}

/// No cutover: Sync Now reads all 12 months; a background round stops after
/// 3 consecutive empty calendar months, and a gap between its windows
/// restarts that count. Mutations: `let stop_after_empty =
/// cutover.is_none() && !explicit;` -> `let stop_after_empty =
/// cutover.is_none();`; delete the `if previous.is_some_and(|previous|
/// previous - 1 != month) { empty_run = 0; }` block.
#[test]
fn explicit_rounds_read_every_month_and_background_rounds_stop_on_calendar_runs() {
    let only_m5 = |empties_after: usize| {
        let mut replies = vec![old_migrated()];
        replies.extend((0..5).map(|_| v2_empty()));
        replies.push(v2_page(&[item("m5", month_start_ms(5) + HOUR)], None));
        replies.extend((0..empties_after).map(|_| v2_empty()));
        replies
    };
    let f = fixture();
    let (outcome, seen) = run_v2(&f, only_m5(6), v2_limits(false));
    assert_eq!(outcome, stop(State::Error, Some("billing_v2_no_items")));
    assert_eq!(v2_requests(&seen).len(), 3);

    let f = fixture();
    let (outcome, seen) = run_v2(&f, only_m5(6), v2_limits(true));
    assert_eq!(outcome, ok(1));
    assert_eq!(v2_requests(&seen).len(), 12);
    assert_eq!(tokens_in_month(&f.dir, 5), ITEM_TOKENS);

    // Round 1 reads M..M-3; round 2 reads M, M-1, M-2, then the never-read
    // M-4, M-5, …: M-4 follows a gap, so the empty run restarts there.
    let f = fixture();
    let m0 = || v2_page(&[item("m0", month_start_ms(0))], None);
    let (outcome, _) = run_v2(
        &f,
        vec![old_migrated(), m0(), v2_empty(), v2_empty(), v2_empty()],
        v2_limits(false),
    );
    assert_eq!(outcome, ok(1));
    let (outcome, seen) = run_v2(
        &f,
        vec![
            old_migrated(),
            m0(),
            v2_empty(),
            v2_empty(),
            v2_empty(),
            v2_page(&[item("m5", month_start_ms(5) + HOUR)], None),
            v2_empty(),
            v2_empty(),
            v2_empty(),
        ],
        v2_limits(false),
    );
    assert_eq!(outcome, ok(2));
    assert_eq!(v2_requests(&seen).len(), 8);
    assert_eq!(tokens_in_month(&f.dir, 5), ITEM_TOKENS);
}
