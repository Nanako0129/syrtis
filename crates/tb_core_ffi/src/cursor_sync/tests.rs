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
        max_body_bytes: 1024 * 1024,
    }
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
    let target = Target {
        db_path: db,
        url,
        https_only: false,
        file_stem: &stem,
    };
    crate::RUNTIME.block_on(run(&target, dir, limits, enabled))
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
    let dir = tmp.path().join("sync");
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
    Outcome {
        stop: Stop::new(state, reason),
        events: 0,
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
    assert_eq!(
        outcome,
        Outcome {
            stop: Stop::new(State::Ok, None),
            events: 3
        }
    );

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
            stop(State::Error, Some("http_status")),
        ),
        (
            json_ok(r#"{"totalUsageEventsCount":1}"#),
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
            stop(State::Error, Some("account_mismatch"))
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

/// A registry local to the test, so a sync test never turns on the takeover
/// for other tests' `LocalSourceContext`s through the process-wide one.
fn registry(enabled: bool, dir: &Path) -> RwLock<Config> {
    RwLock::new(Config {
        enabled,
        dir: Some(dir.to_path_buf()),
        cli_takeover_confirmed: false,
    })
}

fn sync_status(f: &Fixture, url: &str, registry: &RwLock<Config>) -> serde_json::Value {
    let target = Target {
        db_path: &f.db,
        url,
        https_only: false,
        file_stem: &stem,
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
        vec![json_ok(&format!(r#"{{"owningUser":"{OWNING_CANARY}"}}"#))],
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
        dir: tmp.path().join("sync"),
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

#[test]
fn set_validates_and_disable_deletes_only_usage_files() {
    let _guard = lock();
    set_for_test(Config::default());
    let home = tempfile::tempdir().unwrap();
    let dir = home
        .path()
        .join("Library/Application Support/x/cursor-cache");
    std::fs::create_dir_all(&dir).unwrap();
    let set = |json: serde_json::Value| set_from_json(&json.to_string(), Some(home.path()));

    for bad in [
        serde_json::json!({"enabled": true}),
        serde_json::json!({"enabled": true, "dir": "relative/dir"}),
        serde_json::json!({"enabled": true, "dir": "/"}),
        serde_json::json!({"enabled": true, "dir": format!("{}/../x", dir.display())}),
        serde_json::json!({"enabled": true, "dir": home.path().join(".config/tokscale/cursor-cache")}),
        serde_json::json!({"enabled": true, "dir": dir, "extra": 1}),
    ] {
        assert!(set(bad.clone()).is_err(), "{bad}");
        assert_eq!(
            config(),
            Config::default(),
            "rejected input changed the registry"
        );
    }

    let ok = set(serde_json::json!({"enabled": true, "dir": dir, "cliTakeoverConfirmed": true}))
        .unwrap();
    assert_eq!(ok["removedFiles"], 0);
    assert_eq!(
        config(),
        Config {
            enabled: true,
            dir: Some(dir.clone()),
            cli_takeover_confirmed: true
        }
    );

    // Not `usage.<64 hex>.json`: a complete-shaped file here would turn the
    // takeover on for other tests' contexts through the global registry.
    let synced = dir.join("usage.not-complete.json");
    let temp = dir.join(format!("{TEMP_FILE_PREFIX}1-1"));
    let unrelated = dir.join("keep.txt");
    for path in [&synced, &temp, &unrelated] {
        std::fs::write(path, "x").unwrap();
    }
    let off = set(serde_json::json!({"enabled": false, "dir": dir})).unwrap();
    assert_eq!(off["removedFiles"], 2);
    assert_eq!(off["cleanupFailed"], false);
    assert!(!synced.exists() && !temp.exists());
    assert!(
        unrelated.exists(),
        "disable must delete only Syrtis usage files"
    );
    assert!(!config().enabled);
    set_for_test(Config::default());
}

#[cfg(unix)]
#[test]
fn disable_reports_a_cleanup_it_could_not_finish() {
    use std::os::unix::fs::PermissionsExt as _;
    let _guard = lock();
    set_for_test(Config::default());
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("cursor-cache");
    std::fs::create_dir_all(&dir).unwrap();
    let set = |enabled: bool| {
        set_from_json(
            &serde_json::json!({"enabled": enabled, "dir": dir}).to_string(),
            Some(home.path()),
        )
        .unwrap()
    };
    set(true);
    let synced = dir.join("usage.not-complete.json");
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

    // Read-only, no lock file (it cannot be created) and nothing of ours.
    mode(&dir, 0o500);
    let empty = remove_usage_files_locked(&dir);
    mode(&dir, 0o700);
    assert_eq!(empty, (0, false), "nothing of ours: not a failure");

    // Same, with a synced file left behind (control).
    std::fs::write(dir.join("usage.x.json"), "x").unwrap();
    mode(&dir, 0o500);
    let stuck = remove_usage_files_locked(&dir);
    mode(&dir, 0o700);
    assert_eq!(stuck, (0, true));

    // A dir that cannot even be examined is a failure, not "missing".
    mode(home.path(), 0o000);
    let unreadable = remove_usage_files_locked(&dir);
    mode(home.path(), 0o700);
    assert_eq!(unreadable, (0, true));
    assert_eq!(
        remove_usage_files_locked(&home.path().join("absent")),
        (0, false)
    );
}

// --- takeover ------------------------------------------------------------------

fn write_complete_file(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join(format!("usage.{}.json", "c".repeat(64))),
        r#"{"totalUsageEventsCount":0,"usageEventsDisplay":[]}"#,
    )
    .unwrap();
}

fn claude_settings() -> tokscale_core::scanner::ScannerSettings {
    tokscale_core::scanner::ScannerSettings {
        extra_scan_paths: [("claude".to_string(), vec![PathBuf::from("/claude/extra")])].into(),
        ..Default::default()
    }
}

#[test]
fn takeover_matrix() {
    for bits in 0..16u8 {
        let (enabled, complete, cli_files, confirmed) =
            (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0, bits & 8 != 0);
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("sync");
        let cli = home.path().join(".config/tokscale/cursor-cache");
        std::fs::create_dir_all(&cli).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        if complete {
            write_complete_file(&dir);
        }
        if cli_files {
            std::fs::write(cli.join("usage.csv"), "x").unwrap();
        }
        let config = Config {
            enabled,
            dir: Some(dir.clone()),
            cli_takeover_confirmed: confirmed,
        };
        let mut settings = claude_settings();
        apply_takeover(&mut settings, &config, Some(home.path()));

        let on = enabled && complete && (!cli_files || confirmed);
        let label =
            format!("enabled={enabled} complete={complete} cli={cli_files} confirmed={confirmed}");
        assert_eq!(
            settings.extra_scan_paths["claude"],
            [PathBuf::from("/claude/extra")],
            "{label}"
        );
        assert_eq!(
            settings.extra_scan_paths.get("cursor").cloned(),
            on.then(|| vec![dir.clone()]),
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
    let dir = home.path().join("sync");
    write_complete_file(&dir);
    let context = crate::LocalSourceContext::for_home(home.path().to_path_buf());
    let cli = home.path().join(".config/tokscale/cursor-cache");

    set_for_test(Config {
        enabled: true,
        dir: Some(dir.clone()),
        cli_takeover_confirmed: false,
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
            std::slice::from_ref(&dir)
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
    set_thread_config_for_test(config);
    let options = tokscale_core::ReportOptions {
        group_by: tokscale_core::GroupBy::Session,
        ..crate::LocalSourceContext::for_home(home.to_path_buf())
            .report_options(None, Some(vec!["cursor".to_string()]))
    };
    set_thread_config_for_test(None);
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
        Outcome {
            stop: Stop::new(State::Ok, None),
            events: 2
        }
    );
    let synced = (300, 60, 30);
    let synced_sessions = vec!["conv-a".to_string(), "conv-b".to_string()];
    let on = |confirmed| Config {
        enabled: true,
        dir: Some(dir.clone()),
        cli_takeover_confirmed: confirmed,
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
