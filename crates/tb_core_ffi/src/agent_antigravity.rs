//! Antigravity (Google Code Assist) usage/quota — ported from codexbar's
//! Antigravity provider. Antigravity 2.0 (the IDE and the `antigravity-cli`
//! that replaced `gemini-cli`) does NOT persist per-message token counts
//! locally, so a contribution-graph style breakdown isn't derivable; the quota
//! API is the only usable signal. We mirror codexbar's `auto` source:
//!
//! 1. **Local IDE API (`cli`)** — when Antigravity is running, find its
//!    `language_server` process (carrying a `--csrf_token`), discover its
//!    listening ports with platform-native process tools, and call the local
//!    Connect-RPC `GetUserStatus` over loopback TLS. Live, no token refresh,
//!    no disk writes.
//! 2. **OAuth remote (`oauth`)** — otherwise read the shared Google creds under
//!    `GEMINI_CLI_HOME` (falling back to `~/.gemini`), refresh against Google
//!    (client id/secret scanned from the installed Antigravity.app binary, with
//!    the `agy` CLI as a fallback when the IDE is not installed), and
//!    hit the `cloudcode-pa.googleapis.com` Code Assist quota endpoints.
//!
//! Both yield per-model "remaining fraction + reset" which map to `UsageWindow`s.

use crate::agent_account_scope::{
    self, AccountScope, AccountScopeError, AuthoritativeIdKind, HistoryScope, RefreshCheckpoint,
    RefreshScopeTransaction,
};
use crate::agent_usage::{
    clean_plan, parse_datetime, percent_encode, provider_http_client_builder, read_response_body,
    request_after_verified_binding, AgentIdentity, ProviderCacheBinding, ProviderFetchFailure,
    ResponseReadFailure, SafeTransportDiagnostic, TransportErrorFacts, TransportPhase, UsageWindow,
};
use crate::agent_quota_duration::DurationEvidence;
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::Deserialize;
use serde_json::{json, value::RawValue, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsStr;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Marker error for "none of the three routes has a credential to use". The
/// local IDE route needs a running language server, the `agy` route needs the
/// CLI's own login, and both are tried before this failure can be returned, so
/// reaching it means nothing is set up. `agent_usage::fetch_antigravity` pairs
/// it with `source == "unconfigured"` — see `required_card_source` there.
pub(crate) const ANTIGRAVITY_UNCONFIGURED_ERROR: &str =
    "Antigravity is not logged in. Re-login in Antigravity.";

/// A Code Assist 401. Written for the primary, whose fix is signing in to
/// Antigravity again; `fetch_captured_with` replaces it with
/// `CAPTURED_AUTH_EXPIRED`, because a captured account is not the one
/// Antigravity is signed in to.
const ANTIGRAVITY_AUTH_EXPIRED: &str = "Antigravity Google auth expired. Re-login in Antigravity.";

const LANG_SERVICE: &str = "/exa.language_server_pb.LanguageServerService/GetUserStatus";
const CODE_ASSIST_BASE: &str = "https://cloudcode-pa.googleapis.com/v1internal";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const REFRESH_SAFETY_SECS: i64 = 60;

#[derive(Debug, Clone)]
pub(crate) struct Fetched {
    pub source: String,
    pub identity: Option<AgentIdentity>,
    pub account_scope: Result<AccountScope, AccountScopeError>,
    pub history_scope: Result<HistoryScope, AccountScopeError>,
    pub cache_binding: Option<ProviderCacheBinding>,
    pub windows: Vec<UsageWindow>,
    /// Set by the agy route (the marker read before the run) and by the bound
    /// substitution (`BoundOAuth::substitute`, the marker read before this
    /// fetch): see `AgentUsageSnapshot::agy_login_marker`. `None` elsewhere.
    pub agy_login_marker: Option<String>,
    /// Set ONLY by `BoundOAuth::substitute`: the captured account key whose
    /// OAuth result filled the primary. See
    /// `AgentUsageSnapshot::bound_account_key`.
    pub bound_account_key: Option<String>,
}

#[derive(Debug)]
enum LocalAttempt {
    Success(Fetched),
    RouteMiss,
}

#[derive(Debug)]
enum PrimaryAttempt<Context> {
    Success(Fetched),
    Forbidden(Context),
    SchemaContradiction {
        context: Context,
        failure: ProviderFetchFailure,
    },
    Transient {
        context: Context,
        failure: ProviderFetchFailure,
    },
    FinalFailure(ProviderFetchFailure),
}

async fn fetch_with<
    Local,
    LocalFuture,
    Primary,
    PrimaryFuture,
    Secondary,
    SecondaryFuture,
    Context,
>(
    local_attempt: Local,
    primary_remote_attempt: Primary,
    secondary_remote_attempt: Secondary,
) -> Result<Fetched, ProviderFetchFailure>
where
    Local: FnOnce() -> LocalFuture,
    LocalFuture: std::future::Future<Output = LocalAttempt>,
    Primary: FnOnce() -> PrimaryFuture,
    PrimaryFuture: std::future::Future<Output = PrimaryAttempt<Context>>,
    Secondary: FnOnce(Context) -> SecondaryFuture,
    SecondaryFuture: std::future::Future<Output = Result<Fetched, ProviderFetchFailure>>,
{
    if let LocalAttempt::Success(fetched) = local_attempt().await {
        return Ok(fetched);
    }

    match primary_remote_attempt().await {
        PrimaryAttempt::Success(fetched) => Ok(fetched),
        PrimaryAttempt::Forbidden(context) => secondary_remote_attempt(context).await,
        PrimaryAttempt::SchemaContradiction { context, failure } => {
            match secondary_remote_attempt(context).await {
                Ok(fetched) => Ok(fetched),
                Err(_) => Err(failure),
            }
        }
        PrimaryAttempt::Transient { context, failure } => {
            match secondary_remote_attempt(context).await {
                Ok(fetched) => Ok(fetched),
                Err(secondary_failure @ ProviderFetchFailure::Transient { .. }) => {
                    Err(secondary_failure)
                }
                Err(ProviderFetchFailure::Terminal { .. }) => Err(failure),
            }
        }
        PrimaryAttempt::FinalFailure(failure) => Err(failure),
    }
}

/// Auto: prefer the live Local IDE API, then the OAuth remote API, and finally
/// the optional `agy` CLI usage command when the earlier routes are unavailable.
/// With `bound` (agy's current account is a captured account bound under agy's
/// current login), the agy leg is replaced by that account's OAuth result when
/// the login is still the same at the decision point (`with_agy_fallback`).
pub(crate) async fn fetch(
    now: DateTime<Utc>,
    bound: Option<BoundOAuth>,
) -> Result<Fetched, ProviderFetchFailure> {
    let primary = fetch_with(
        || async move {
            match fetch_local_ide(now).await {
                Ok(local) if !local.windows.is_empty() => LocalAttempt::Success(local),
                Ok(_) | Err(_) => LocalAttempt::RouteMiss,
            }
        },
        || fetch_oauth_primary(now),
        |context| fetch_oauth_secondary(context, now),
    )
    .await;
    with_agy_fallback(primary, bound, live_agy_marker, || fetch_agy_cli(now)).await
}

/// The `agy` route's arbitration, separated from the route itself so the
/// property that decides the card's `source` can be tested without a CLI, a
/// login shell or the network.
///
/// That property is the `Err(_)` arm: when the CLI cannot answer, the caller
/// sees the ORIGINAL failure rather than the CLI's. It is what carries
/// `ANTIGRAVITY_UNCONFIGURED_ERROR` out of `fetch` on a machine where nothing is
/// set up, and it is unreachable on a machine that has Antigravity — there the
/// CLI answers and the card is configured, which is why #345's Antigravity half
/// is pinned here instead of by running the app.
///
/// Plan E: only where agy would run (the earlier routes ended in a Terminal
/// failure), a `bound` captured OAuth result replaces the run when agy's live
/// login marker, read here and now (`post_marker`), still equals the one read
/// before this fetch. An `Ok` or Transient primary is returned exactly as
/// before, and `post_marker` is not read without `bound`. A substituted poll
/// never calls `agy`, so it neither sets nor clears `AGY_LATCH`.
async fn with_agy_fallback<Marker, MarkerFuture, Agy, AgyFuture>(
    primary: Result<Fetched, ProviderFetchFailure>,
    bound: Option<BoundOAuth>,
    post_marker: Marker,
    agy: Agy,
) -> Result<Fetched, ProviderFetchFailure>
where
    Marker: FnOnce() -> MarkerFuture,
    MarkerFuture: std::future::Future<Output = Option<String>>,
    Agy: FnOnce() -> AgyFuture,
    AgyFuture: std::future::Future<Output = Result<Fetched, ProviderFetchFailure>>,
{
    match primary {
        Ok(fetched) => Ok(fetched),
        Err(primary_failure) if should_try_agy_fallback(&primary_failure) => {
            if let Some(bound) = bound {
                if let Some(fetched) = bound.substitute(post_marker().await) {
                    return Ok(fetched);
                }
            }
            match agy().await {
                Ok(fetched) => Ok(fetched),
                // When the other routes simply found no login, agy is the
                // route that actually ran, so its failure is the real answer.
                // Showing "not logged in" over an agy timeout hid the cause
                // (observed 2026-10-03). agy itself missing or signed out
                // keeps the primary's message, which says what to do.
                Err(ProviderFetchFailure::Terminal { display })
                    if matches!(&primary_failure, ProviderFetchFailure::Terminal { display: primary } if primary == ANTIGRAVITY_UNCONFIGURED_ERROR)
                        && display != AGY_NOT_FOUND
                        && display != AGY_NOT_SIGNED_IN =>
                {
                    Err(ProviderFetchFailure::terminal(display))
                }
                Err(_) => Err(primary_failure),
            }
        }
        Err(primary_failure) => Err(primary_failure),
    }
}

// ── Plan E: the bound captured account instead of an agy run ───────────────
//
// agy's current account, once captured, is fetched anyway as its own card
// through its OAuth copy. When the host has bound that account to agy's login
// marker (Swift `AntigravityAutoCapture.currentAgyKey`/`currentAgyMarker`),
// the primary takes that result instead of spawning agy, under conditions 1-5
// of the plan: a binding whose marker is one parsed `mdat`, its key
// registered, the live marker equal to it before the fetch and again at the
// decision point, and the captured fetch's RAW result Ok with windows. Any
// miss: today's agy route. No secret is involved: the binding is a key hash
// and a Keychain modification date.

/// The host's binding (`tb_set_antigravity_binding`): agy's current account
/// and the login marker it was confirmed under. Never logged (no `Debug`).
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct AntigravityBinding {
    pub key: String,
    pub marker: String,
}

static ANTIGRAVITY_BINDING: std::sync::Mutex<Option<AntigravityBinding>> =
    std::sync::Mutex::new(None);

fn lock_binding() -> std::sync::MutexGuard<'static, Option<AntigravityBinding>> {
    ANTIGRAVITY_BINDING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The binding as last set. Read ONCE per agent-usage fetch, at the start of
/// `fetch_antigravity_accounts`, and passed down from there (S3a): a setter
/// call during the fetch cannot change which key the fetch stamps.
pub(crate) fn antigravity_binding() -> Option<AntigravityBinding> {
    lock_binding().clone()
}

/// The time inside an agy login marker, or `None` when the marker is not
/// exactly the `mdat` value `parse_keychain_mdat` extracts from
/// `security find-generic-password` attributes:
/// `0x<hex>  "<YYYYMMDDhhmmss>Z\000"` (two spaces; `\000` is the four literal
/// characters `security` prints). Anchored at both ends, so `"present"`,
/// `"absent"`, `""` and any junk before or after a real value are refused:
/// only a marker naming one login write may bind. The hex is not
/// cross-checked against the digits (some real outputs truncate it).
pub(crate) fn mdat_marker_time(marker: &str) -> Option<NaiveDateTime> {
    let (hex, quoted) = marker.strip_prefix("0x")?.split_once("  \"")?;
    if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let digits = quoted.strip_suffix("Z\\000\"")?;
    if digits.len() != 14 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    NaiveDateTime::parse_from_str(digits, "%Y%m%d%H%M%S").ok()
}

const BINDING_INVALID_JSON: &str = "invalid_binding_json";
const BINDING_INVALID_KEY: &str = "invalid_key";
const BINDING_INVALID_MARKER: &str = "invalid_marker";

/// `tb_set_antigravity_binding`. `None` (a NULL pointer) or `{"key":null}`
/// clears with `{"bound":false}`. Anything that is not a valid
/// `{"key","marker"}` clears too, before one fixed error code is returned:
/// a bad input never leaves an older binding in force. The input is never
/// echoed, and nothing here can panic on it.
pub(crate) fn set_antigravity_binding(raw: Option<&[u8]>) -> Result<Value, String> {
    let mut stored = lock_binding();
    *stored = None;
    let Some(raw) = raw else {
        return Ok(json!({ "bound": false }));
    };
    let parsed = parse_binding(raw).map_err(str::to_string)?;
    let bound = parsed.is_some();
    *stored = parsed;
    Ok(json!({ "bound": bound }))
}

fn parse_binding(raw: &[u8]) -> Result<Option<AntigravityBinding>, &'static str> {
    // `from_slice` also rejects bytes that are not UTF-8.
    let input: Value = serde_json::from_slice(raw).map_err(|_| BINDING_INVALID_JSON)?;
    let object = input.as_object().ok_or(BINDING_INVALID_JSON)?;
    match (object.get("key"), object.get("marker")) {
        (Some(Value::Null), None) => Ok(None),
        (Some(Value::Null), Some(_)) => Err(BINDING_INVALID_KEY),
        (Some(Value::String(key)), _) if !valid_captured_key(key) => Err(BINDING_INVALID_KEY),
        (Some(Value::String(key)), Some(Value::String(marker)))
            if mdat_marker_time(marker).is_some() =>
        {
            Ok(Some(AntigravityBinding {
                key: key.clone(),
                marker: marker.clone(),
            }))
        }
        (Some(Value::String(_)), Some(Value::String(_)) | None) => Err(BINDING_INVALID_MARKER),
        _ => Err(BINDING_INVALID_JSON),
    }
}

/// Conditions 1-3, once per fetch: the binding's key and marker are valid
/// (the marker re-checked with `mdat_marker_time`, so a binding that bypassed
/// the setter still cannot bind on `"present"`), the key is a registered
/// captured account, and agy's live marker, read only then, equals the bound
/// one. Returns that account and the marker as read (the pre-fetch marker).
/// An unreadable live marker (`None`) never matches.
pub(crate) async fn bound_account<Marker, MarkerFuture>(
    binding: Option<&AntigravityBinding>,
    accounts: &[CapturedAccount],
    live_marker: Marker,
) -> Option<(CapturedAccount, String)>
where
    Marker: FnOnce() -> MarkerFuture,
    MarkerFuture: std::future::Future<Output = Option<String>>,
{
    let binding = binding.filter(|binding| {
        valid_captured_key(&binding.key) && mdat_marker_time(&binding.marker).is_some()
    })?;
    let account = accounts.iter().find(|account| account.key == binding.key)?;
    let marker_pre = live_marker().await?;
    (marker_pre == binding.marker).then(|| (account.clone(), marker_pre))
}

/// The bound account's captured OAuth result, carried to the primary's agy
/// leg. No `Debug`: it is never logged (S8).
pub(crate) struct BoundOAuth {
    key: String,
    marker_pre: String,
    fetched: Fetched,
}

impl BoundOAuth {
    /// Condition 4: the RAW captured result decides, Ok with at least one
    /// window. Never the card after last-good, which may be a stand-in.
    pub(crate) fn from_raw(
        key: String,
        marker_pre: String,
        raw: &Result<Fetched, ProviderFetchFailure>,
    ) -> Option<Self> {
        let fetched = raw.as_ref().ok().filter(|fetched| !fetched.windows.is_empty())?;
        Some(Self {
            key,
            marker_pre,
            fetched: fetched.clone(),
        })
    }

    /// Condition 5 at the decision point: the live marker still equals the
    /// pre-fetch one. The result reads as the captured card does (`oauth`),
    /// carries the pre-fetch marker and the bound key (what lets Swift's
    /// dedup tell it from the `oauth_creds.json` route and tie it to one
    /// captured card), has no cache binding (the primary slot's last-good is
    /// cleared, as after an agy-route success), and has the agy route's
    /// scopes: no account scope and no history scope, so history is recorded
    /// once, by the captured card (S7).
    pub(crate) fn substitute(self, post_marker: Option<String>) -> Option<Fetched> {
        if post_marker.as_deref() != Some(self.marker_pre.as_str()) {
            return None;
        }
        let mut fetched = self.fetched;
        fetched.source = "oauth".to_string();
        fetched.agy_login_marker = Some(self.marker_pre);
        fetched.bound_account_key = Some(self.key);
        fetched.cache_binding = None;
        fetched.account_scope = Err(AccountScopeError::NoTrustedEvidence);
        fetched.history_scope = Err(AccountScopeError::NoTrustedEvidence);
        Some(fetched)
    }
}

/// agy's live login marker for plan E's pre and post reads: the same
/// attributes-only query as the agy route (`agy_login_marker`, no secret
/// requested). Off macOS there is no agy Keychain item, so nothing matches.
pub(crate) async fn live_agy_marker() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        agy_login_marker().await
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

fn should_try_agy_fallback(failure: &ProviderFetchFailure) -> bool {
    matches!(failure, ProviderFetchFailure::Terminal { .. })
}

/// The host whose DNS failure precedes the CLI's interactive escalation.
///
/// `agy --print` is documented as non-interactive, and it is not: with an
/// expired cached token and no resolver it logs
/// `lookup oauth2.googleapis.com: no such host`, promotes that network failure
/// to an authentication failure, and opens a browser tab for a consumer OAuth
/// flow (#329). A quota poll must not be able to start that.
#[cfg(target_os = "macos")]
const OAUTH_TOKEN_HOST: &str = "oauth2.googleapis.com";

/// Whether the CLI's token endpoint resolves right now.
///
/// DNS only. The escalation in #329 is gated on resolution failing, so a
/// connect would buy no additional signal and would spend a real round trip on
/// every poll. Two seconds because this runs ahead of a subprocess that is
/// already allowed 35, and a resolver that has not answered in two is the
/// just-woken state this guards.
#[cfg(target_os = "macos")]
async fn oauth_endpoint_resolves() -> bool {
    match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::net::lookup_host((OAUTH_TOKEN_HOST, 443)),
    )
    .await
    {
        // An empty answer is not a usable resolution, so it is treated as a
        // failure rather than as "resolved with no addresses".
        Ok(Ok(mut addresses)) => addresses.next().is_some(),
        Ok(Err(_)) | Err(_) => false,
    }
}

/// Refuse to invoke the CLI when its own token endpoint cannot be resolved.
///
/// The guard has to sit in front of the invocation rather than around it. The
/// call site already wraps the subprocess in a 35-second `timeout` with
/// `kill_on_drop`, and #329 still recorded a process that started at 13:05:57
/// and escalated at 13:23:23 — eighteen minutes, across a sleep. A monotonic
/// timer does not advance while the machine is asleep, so the deadline that
/// should have killed that process had barely moved when it woke and carried on
/// to the browser. Anything that manages the child after spawning it is subject
/// to the same freeze; only not spawning it is not.
///
/// The same freeze applies to the logged-out case below: with `agy` signed out,
/// `agy --print /usage --output-format json --print-timeout 30s </dev/null` was
/// measured on macOS to open a browser tab on Google's OAuth consent page and
/// wait there. That is exactly the command this route runs unattended, so the
/// login check also has to happen before the spawn, not after it.
///
/// Taking the runner, the login marker and the latch as parameters is what
/// lets a test assert the child is never spawned, rather than inferring it
/// from the returned error.
///
/// The latch trade-off: once a real attempt has spawned `agy` and failed, the
/// route stays off for as long as the login marker is unchanged. A transient
/// failure after a real spawn therefore disables the `agy` route until the
/// Keychain item is rewritten (a re-login) or the app restarts, and meanwhile
/// the card shows the primary route's error, because `with_agy_fallback`
/// discards this route's failure. Expiring the latch on a timer is
/// deliberately not implemented: a spawned attempt that failed may be one that
/// opened the browser, and the failure seen here does not say which, so a
/// timed retry could repeat it every period.
async fn fetch_agy_cli_gated<Run, RunFuture>(
    now: DateTime<Utc>,
    endpoint_resolves: bool,
    marker: Option<String>,
    latch: &std::sync::Mutex<AgyLatch>,
    run: Run,
) -> Result<Fetched, ProviderFetchFailure>
where
    Run: FnOnce(DateTime<Utc>) -> RunFuture,
    RunFuture: std::future::Future<Output = Result<Fetched, AgyRunError>>,
{
    if !endpoint_resolves {
        return Err(ProviderFetchFailure::terminal(
            "Antigravity quota is unavailable while the network is unreachable.",
        ));
    }
    let Some(marker) = marker else {
        return Err(ProviderFetchFailure::terminal(AGY_NOT_SIGNED_IN));
    };
    {
        // Scoped so the guard is released before the await below: polls can
        // overlap (there is no per-provider single-flight upstream), and a
        // std mutex held across an await would block the other poll's thread.
        let mut state = lock_agy_latch(latch);
        match &*state {
            AgyLatch::InFlight => {
                return Err(ProviderFetchFailure::terminal(
                    "Antigravity CLI usage is already running.",
                ));
            }
            AgyLatch::Failed { marker: failed, retry_at } if *failed == marker => {
                match retry_at {
                    Some(at) if now >= *at => *state = AgyLatch::InFlight,
                    Some(_) => {
                        return Err(ProviderFetchFailure::terminal(AGY_TIMED_OUT_RETRYING));
                    }
                    None => {
                        return Err(ProviderFetchFailure::terminal(
                            "Antigravity CLI usage is paused after a failed attempt.",
                        ));
                    }
                }
            }
            AgyLatch::Idle | AgyLatch::Failed { .. } => *state = AgyLatch::InFlight,
        }
    }
    // Created with no await between it and the InFlight write, so a panic in
    // `run` or a drop of this future mid-flight still leaves the latch Idle.
    let mut release = AgyLatchRelease {
        latch,
        next: AgyLatch::Idle,
    };
    let fetched_under = marker.clone();
    match run(now).await {
        Ok(mut fetched) => {
            fetched.agy_login_marker = Some(fetched_under);
            Ok(fetched)
        }
        Err(AgyRunError { failure, spawned }) => {
            if spawned {
                // A timeout is transient (agy slow right after an update was
                // observed 2026-10-03, and the route then stayed latched for
                // the rest of the process): retry it after a cooldown. Any
                // other failure of a run that started may be agy asking for
                // re-authentication, which can open a browser, so it stays
                // latched until the login marker changes, as before.
                let timed_out =
                    matches!(&failure, ProviderFetchFailure::Terminal { display } if display == AGY_TIMED_OUT);
                release.next = AgyLatch::Failed {
                    marker,
                    retry_at: timed_out.then(|| now + chrono::Duration::seconds(AGY_RETRY_SECS)),
                };
            }
            Err(failure)
        }
    }
}

/// Whether the `agy` route may spawn the CLI. `Failed` carries the login
/// marker that was current when a spawned attempt failed; a different marker
/// (the Keychain item was rewritten by a re-login) re-arms the route.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AgyLatch {
    Idle,
    InFlight,
    /// `retry_at` is set only for a timeout: the route re-arms at that time
    /// for the same login. Nil keeps it latched until the marker changes.
    Failed {
        marker: String,
        retry_at: Option<DateTime<Utc>>,
    },
}

const AGY_TIMED_OUT: &str = "Antigravity CLI usage timed out.";
const AGY_NOT_FOUND: &str = "Antigravity CLI was not found.";
const AGY_NOT_SIGNED_IN: &str = "Antigravity CLI is not signed in.";
const AGY_TIMED_OUT_RETRYING: &str =
    "Antigravity CLI usage timed out. Retrying automatically.";
/// Cooldown after a timed-out `agy /usage`, three tray cycles.
const AGY_RETRY_SECS: i64 = 15 * 60;

#[cfg(target_os = "macos")]
static AGY_LATCH: std::sync::Mutex<AgyLatch> = std::sync::Mutex::new(AgyLatch::Idle);

/// A failed `agy` run, with whether any CLI process was actually attempted.
/// Only an attempted run may latch the route off; "no CLI installed" must not.
#[derive(Debug)]
struct AgyRunError {
    failure: ProviderFetchFailure,
    spawned: bool,
}

/// Writes `next` into the latch when dropped, so `InFlight` cannot outlive the
/// attempt that set it, whether that attempt returns, panics or is cancelled.
struct AgyLatchRelease<'a> {
    latch: &'a std::sync::Mutex<AgyLatch>,
    next: AgyLatch,
}

impl Drop for AgyLatchRelease<'_> {
    fn drop(&mut self) {
        let next = std::mem::replace(&mut self.next, AgyLatch::Idle);
        *lock_agy_latch(self.latch) = next;
    }
}

/// The latch holds no invariant a panic could break halfway, so a poisoned
/// lock is recovered rather than propagated; propagating would panic inside
/// `AgyLatchRelease::drop` during an unwind.
fn lock_agy_latch(latch: &std::sync::Mutex<AgyLatch>) -> std::sync::MutexGuard<'_, AgyLatch> {
    latch
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The Keychain lookup that answers "is `agy` signed in" — the login keychain
/// generic-password item `gemini` / `antigravity`. Measured on macOS: exit 0
/// while signed in, exit 44 after `agy`'s `/logout`, with no access prompt.
///
/// Attributes only. Neither `-w` nor `-g` is passed, so the secret is never
/// requested; a test pins this exact slice so neither can be added silently.
const AGY_KEYCHAIN_QUERY: &[&str] = &["find-generic-password", "-s", "gemini", "-a", "antigravity"];

/// The value of the `"mdat"<timedate>=` attribute line (modification date) in
/// `security find-generic-password` output, trimmed. A re-login rewrites the
/// item, which is what lets it re-arm a latched-off route.
fn parse_keychain_mdat(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        let value = line
            .trim_start()
            .strip_prefix("\"mdat\"<timedate>=")?
            .trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

/// `Some(marker)` when `agy`'s Keychain login item exists, `None` otherwise
/// (exit 44, any other exit, spawn failure or timeout — all read as "do not
/// spawn `agy`"). The marker is the item's modification date, or the fixed
/// `"present"` when that cannot be parsed, so the latch still applies; with the
/// sentinel only an app restart re-arms a latched route.
///
/// stdout is parsed and dropped here, never logged.
#[cfg(target_os = "macos")]
async fn agy_login_marker() -> Option<String> {
    let future = tokio::process::Command::new("/usr/bin/security")
        .args(AGY_KEYCHAIN_QUERY)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(std::time::Duration::from_secs(3), future)
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let marker = parse_keychain_mdat(&String::from_utf8_lossy(&output.stdout));
    Some(marker.unwrap_or_else(|| "present".to_string()))
}

#[cfg(target_os = "macos")]
async fn fetch_agy_cli(now: DateTime<Utc>) -> Result<Fetched, ProviderFetchFailure> {
    let endpoint_resolves = oauth_endpoint_resolves().await;
    // The Keychain is not consulted when the gate has already closed.
    let marker = if endpoint_resolves {
        agy_login_marker().await
    } else {
        None
    };
    fetch_agy_cli_gated(
        now,
        endpoint_resolves,
        marker,
        &AGY_LATCH,
        run_agy_cli_candidates,
    )
    .await
}

/// `spawned` is false only when there was no candidate to run; any call into
/// `fetch_agy_cli_from` counts as an attempt.
#[cfg(target_os = "macos")]
async fn run_agy_cli_candidates(now: DateTime<Utc>) -> Result<Fetched, AgyRunError> {
    let candidates = agy_cli_artifact_candidates(true).await;
    let mut last_failure = None;
    for executable in candidates {
        match fetch_agy_cli_from(&executable, now).await {
            Ok(fetched) => return Ok(fetched),
            Err(failure) => last_failure = Some(failure),
        }
    }
    Err(match last_failure {
        Some(failure) => AgyRunError {
            failure,
            spawned: true,
        },
        None => AgyRunError {
            failure: ProviderFetchFailure::terminal(AGY_NOT_FOUND),
            spawned: false,
        },
    })
}

#[cfg(not(target_os = "macos"))]
async fn fetch_agy_cli(_now: DateTime<Utc>) -> Result<Fetched, ProviderFetchFailure> {
    Err(ProviderFetchFailure::terminal(
        "Antigravity CLI fallback is only supported on macOS.",
    ))
}

#[cfg(target_os = "macos")]
async fn fetch_agy_cli_from(
    executable: &Path,
    now: DateTime<Utc>,
) -> Result<Fetched, ProviderFetchFailure> {
    let future = tokio::process::Command::new(executable)
        .args([
            "--print",
            "/usage",
            "--output-format",
            "json",
            "--print-timeout",
            "30s",
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(std::time::Duration::from_secs(35), future)
        .await
        .map_err(|_| ProviderFetchFailure::terminal(AGY_TIMED_OUT))?
        .map_err(|_| ProviderFetchFailure::terminal("Antigravity CLI usage failed."))?;
    if !output.status.success() {
        return Err(ProviderFetchFailure::terminal(
            "Antigravity CLI usage failed.",
        ));
    }
    parse_agy_usage(&output.stdout, now)
        .map_err(|_| ProviderFetchFailure::terminal("Antigravity CLI usage could not be decoded."))
}

// ── Local IDE API ───────────────────────────────────────────────────────────

struct ProcInfo {
    pid: i32,
    csrf_token: String,
    extension_port: Option<u16>,
    extension_csrf: Option<String>,
}

async fn fetch_local_ide(now: DateTime<Utc>) -> Result<Fetched, String> {
    let processes = discover_local_ide()?;

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true) // loopback language_server uses a self-signed cert
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|e| format!("build Antigravity local client: {e}"))?;
    let body = json!({
        "metadata": {
            "ideName": "antigravity",
            "extensionName": "antigravity",
            "ideVersion": "unknown",
            "locale": "en",
        }
    });

    let mut last_err = "Antigravity local IDE API not reachable".to_string();
    for (port, csrf) in local_api_candidates(processes) {
        let url = format!("https://127.0.0.1:{port}{LANG_SERVICE}");
        let resp = client
            .post(&url)
            .header("X-Codeium-Csrf-Token", &csrf)
            .header("Connect-Protocol-Version", "1")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                last_err = format!("local request failed: {e}");
                continue;
            }
        };
        if !resp.status().is_success() {
            last_err = format!("local API returned {}", resp.status().as_u16());
            continue;
        }
        let Ok(text) = resp.text().await else {
            continue;
        };
        match parse_user_status(&text, now) {
            Ok(mut fetched) if !fetched.windows.is_empty() => {
                fetched.account_scope = resolve_local_account_scope(fetched.identity.as_ref());
                fetched.history_scope = resolve_local_history_scope(fetched.identity.as_ref());
                fetched.cache_binding = fetched
                    .account_scope
                    .as_ref()
                    .ok()
                    .cloned()
                    .map(ProviderCacheBinding::primary);
                return Ok(fetched);
            }
            Ok(_) => last_err = "local API returned no model quotas".to_string(),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

fn local_api_candidates(processes: Vec<(ProcInfo, Vec<u16>)>) -> Vec<(u16, String)> {
    let mut candidates = Vec::new();
    for (proc, ports) in processes {
        // language-server ports use the language-server CSRF; the extension
        // server (if advertised) carries its own token.
        candidates.extend(
            ports
                .into_iter()
                .map(|port| (port, proc.csrf_token.clone())),
        );
        if let Some(port) = proc.extension_port {
            if let Some(csrf) = proc.extension_csrf.as_ref() {
                candidates.push((port, csrf.clone()));
            }
            candidates.push((port, proc.csrf_token.clone()));
        }
    }
    candidates
}

fn discover_local_ide() -> Result<Vec<(ProcInfo, Vec<u16>)>, String> {
    let proc = detect_process()?;
    let ports = listening_ports(proc.pid)?;
    Ok(vec![(proc, ports)])
}

fn detect_process() -> Result<ProcInfo, String> {
    let output = Command::new("/bin/ps")
        .args(["-ax", "-o", "pid=,command="])
        .output()
        .map_err(|e| format!("run ps: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    let mut saw_antigravity = false;
    for line in stdout.lines() {
        let line = line.trim_start();
        let Some((pid_str, cmd)) = line.split_once(' ') else {
            continue;
        };
        let Ok(pid) = pid_str.trim().parse::<i32>() else {
            continue;
        };
        let lower = cmd.to_lowercase();
        if !is_language_server(&lower) || !is_antigravity(&lower) {
            continue;
        }
        saw_antigravity = true;
        let Some(csrf) = extract_flag(cmd, "--csrf_token") else {
            continue;
        };
        return Ok(ProcInfo {
            pid,
            csrf_token: csrf,
            extension_port: extract_flag(cmd, "--extension_server_port")
                .and_then(|s| s.parse().ok()),
            extension_csrf: extract_flag(cmd, "--extension_server_csrf_token"),
        });
    }
    if saw_antigravity {
        Err("Antigravity is running but no CSRF token was found".to_string())
    } else {
        Err("Antigravity is not running".to_string())
    }
}

fn is_language_server(lower_cmd: &str) -> bool {
    lower_cmd.contains("language_server")
}

fn is_antigravity(lower_cmd: &str) -> bool {
    (lower_cmd.contains("--app_data_dir") && lower_cmd.contains("antigravity"))
        || lower_cmd.contains("/antigravity/")
}

/// Value of `flag` in a command line, accepting either `flag value` or `flag=value`.
fn extract_flag(cmd: &str, flag: &str) -> Option<String> {
    let idx = cmd.find(flag)?;
    let rest = &cmd[idx + flag.len()..];
    let rest = rest.trim_start_matches(['=', ' ']);
    let value: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
    (!value.is_empty()).then_some(value)
}

fn listening_ports(pid: i32) -> Result<Vec<u16>, String> {
    let lsof = ["/usr/sbin/lsof", "/usr/bin/lsof"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .ok_or_else(|| "lsof not available".to_string())?;
    let output = Command::new(lsof)
        .args(["-nP", "-iTCP", "-sTCP:LISTEN", "-a", "-p", &pid.to_string()])
        .output()
        .map_err(|e| format!("run lsof: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let ports: BTreeSet<u16> = stdout.lines().filter_map(parse_listen_port).collect();
    if ports.is_empty() {
        Err("no listening ports for Antigravity".to_string())
    } else {
        Ok(ports.into_iter().collect())
    }
}

/// Pull the port out of an `lsof` LISTEN line, e.g. `... TCP 127.0.0.1:54321 (LISTEN)`.
fn parse_listen_port(line: &str) -> Option<u16> {
    let idx = line.find("(LISTEN)")?;
    let before = line[..idx].trim_end();
    let colon = before.rfind(':')?;
    before[colon + 1..].trim().parse().ok()
}

#[derive(Debug, Deserialize)]
struct UserStatusResponse {
    #[serde(rename = "userStatus")]
    user_status: Option<UserStatus>,
}

#[derive(Debug, Deserialize)]
struct UserStatus {
    email: Option<String>,
    #[serde(rename = "planStatus")]
    plan_status: Option<PlanStatus>,
    #[serde(rename = "cascadeModelConfigData")]
    cascade_model_config_data: Option<ModelConfigData>,
    #[serde(rename = "userTier")]
    user_tier: Option<NamedTier>,
}

#[derive(Debug, Deserialize)]
struct PlanStatus {
    #[serde(rename = "planInfo")]
    plan_info: Option<LocalPlanInfo>,
}

#[derive(Debug, Deserialize)]
struct LocalPlanInfo {
    #[serde(rename = "planDisplayName")]
    plan_display_name: Option<String>,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    #[serde(rename = "productName")]
    product_name: Option<String>,
    #[serde(rename = "planName")]
    plan_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NamedTier {
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ModelConfigData {
    #[serde(rename = "clientModelConfigs")]
    client_model_configs: Option<Vec<Box<RawValue>>>,
}

#[derive(Debug, Deserialize)]
struct AvailableModelsResponse {
    #[serde(default)]
    models: BTreeMap<String, Box<RawValue>>,
}

#[derive(Debug, Deserialize)]
struct QuotaBucketsResponse {
    #[serde(default)]
    buckets: Vec<Box<RawValue>>,
}

#[derive(Debug, Deserialize)]
struct AgyUsageResponse {
    status: Option<String>,
    command: Option<AgyUsageCommand>,
}

#[derive(Debug, Deserialize)]
struct AgyUsageCommand {
    name: Option<String>,
    data: Option<AgyUsageData>,
}

#[derive(Debug, Deserialize)]
struct AgyUsageData {
    #[serde(default)]
    groups: Vec<AgyUsageGroup>,
}

#[derive(Debug, Deserialize)]
struct AgyUsageGroup {
    name: Option<String>,
    #[serde(default)]
    buckets: Vec<AgyUsageBucket>,
}

#[derive(Debug, Deserialize)]
struct AgyUsageBucket {
    id: Option<String>,
    name: Option<String>,
    /// The bucket's window, as agy `/usage` and `retrieveUserQuotaSummary`
    /// both state it: "weekly" or "5h" (measured 2026-10-02/03).
    window: Option<String>,
    #[serde(rename = "remaining_fraction")]
    remaining_fraction: Option<f64>,
    #[serde(rename = "reset_time")]
    reset_time: Option<String>,
}

#[derive(Debug)]
struct ModelCandidate {
    model_id: Option<String>,
    fraction: f64,
    reset: Option<DateTime<Utc>>,
    source_index: usize,
    label: String,
}

fn valid_remaining_fraction(fraction: f64) -> bool {
    fraction.is_finite() && (0.0..=1.0).contains(&fraction)
}

/// The window length a grouped Antigravity bucket declares in its `window`
/// field. Without it the engine has to learn the duration over several resets,
/// and until then the window card cannot draw a curve ("no recorded quota
/// history"); observed on the maintainer's Mac 2026-10-03. Unknown values stay
/// unknown and are learned as before.
fn agy_window_duration(window: Option<&str>) -> Option<DurationEvidence> {
    match window?.trim() {
        "weekly" => Some(DurationEvidence::contract(7 * 86_400)),
        "5h" => Some(DurationEvidence::contract(5 * 3_600)),
        _ => None,
    }
}

/// A grouped bucket (agy `/usage` or `retrieveUserQuotaSummary`): card id and
/// window key `agy.<bucketId>.v1`, with the declared window as a contract
/// duration once the bucket is in use and its cycle has started by this
/// Mac's clock.
///
/// An unused bucket (remaining fraction 1) has a rolling reset: Google
/// reports it as its own now plus the window, on every poll (two unused 5h
/// buckets measured at the same `18:30:15Z` reset). As a contract that
/// fails either way:
/// - this Mac's clock behind Google's: the cycle start `reset - window` lies
///   in the future, `valid_evidence` rejects it, and a rejected contract is
///   `InvalidEvidence` with no fallback, so the card read "invalid duration
///   evidence" on every poll;
/// - in step or ahead: it is accepted, and every poll records a 0% sample
///   under a reset minutes later than the last, each its own cycle in
///   durable history.
///
/// So an unused bucket gets no contract and stays learning until it is
/// used, when its reset stops rolling. The cycle-start check stays, so a
/// declared window longer than the real one cannot become a contract that
/// `valid_evidence` would reject. Usage, not the clock, is the test, because
/// the clock check alone flips with the direction of the skew.
fn agy_bucket_window(
    label: String,
    fraction: f64,
    reset: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    card_id: String,
    window: Option<&str>,
) -> Option<UsageWindow> {
    let in_use = fraction < 1.0;
    let contract = agy_window_duration(window).filter(|evidence| {
        in_use
            && reset.is_some_and(|reset| {
            reset
                .timestamp()
                .checked_sub(evidence.duration_seconds)
                .is_some_and(|cycle_start| cycle_start <= now.timestamp())
        })
    });
    UsageWindow::try_from_provider_fraction(label, fraction, reset, now)
        .map(|usage| usage.with_identity(card_id.clone(), Some(card_id), None, contract))
}

fn quota_window(
    label: String,
    fraction: f64,
    reset: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    card_id: String,
    window_key: Option<String>,
) -> Option<UsageWindow> {
    UsageWindow::try_from_provider_fraction(label, fraction, reset, now)
        .map(|window| window.with_identity(card_id, window_key, None, None))
}

pub(crate) fn parse_agy_usage(body: &[u8], now: DateTime<Utc>) -> Result<Fetched, String> {
    let response: AgyUsageResponse =
        serde_json::from_slice(body).map_err(|e| format!("decode agy usage: {e}"))?;
    if response.status.as_deref() != Some("SUCCESS") {
        return Err("agy usage command was not successful".to_string());
    }
    let command = response
        .command
        .ok_or_else(|| "agy usage response missing command".to_string())?;
    if command.name.as_deref() != Some("usage") {
        return Err("agy usage response missing usage command".to_string());
    }
    let data = command
        .data
        .ok_or_else(|| "agy usage response missing data".to_string())?;

    let mut windows = Vec::new();
    for group in data.groups {
        let group_name = group
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or("Antigravity");
        for bucket in group.buckets {
            let Some(id) = bucket
                .id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
            else {
                continue;
            };
            let Some(fraction) = bucket.remaining_fraction else {
                continue;
            };
            let reset = bucket.reset_time.as_deref().and_then(parse_datetime);
            let label = bucket
                .name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(|name| format!("{group_name} · {name}"))
                .unwrap_or_else(|| format!("{group_name} · Limit"));
            let card_id = format!("agy.{id}.v1");
            if let Some(window) = agy_bucket_window(
                label,
                fraction,
                reset,
                now,
                card_id,
                bucket.window.as_deref(),
            ) {
                windows.push(window);
            }
        }
    }
    if windows.is_empty() {
        return Err("agy usage response contained no valid quota windows".to_string());
    }

    Ok(Fetched {
        agy_login_marker: None,
        bound_account_key: None,
        source: "agy".to_string(),
        identity: None,
        account_scope: Err(AccountScopeError::NoTrustedEvidence),
        history_scope: Err(AccountScopeError::NoTrustedEvidence),
        cache_binding: None,
        windows,
    })
}

fn parse_user_status(body: &str, now: DateTime<Utc>) -> Result<Fetched, String> {
    let response: UserStatusResponse =
        serde_json::from_str(body).map_err(|e| format!("decode GetUserStatus: {e}"))?;
    let status = response
        .user_status
        .ok_or_else(|| "GetUserStatus missing userStatus".to_string())?;

    let configs = status
        .cascade_model_config_data
        .and_then(|d| d.client_model_configs)
        .unwrap_or_default();
    let mut selected: BTreeMap<String, ModelCandidate> = BTreeMap::new();
    let mut missing_model = Vec::new();
    for (index, config) in configs.into_iter().enumerate() {
        let Ok(config) = serde_json::from_str::<Value>(config.get()) else {
            continue;
        };
        let Some(quota) = config.get("quotaInfo") else {
            continue;
        };
        let Some(fraction) = quota.get("remainingFraction").and_then(Value::as_f64) else {
            continue;
        };
        let reset = quota
            .get("resetTime")
            .and_then(Value::as_str)
            .and_then(parse_datetime);
        let model_id = config
            .pointer("/modelOrAlias/model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(str::to_string);
        let label = config
            .get("label")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .or_else(|| model_id.clone())
            .unwrap_or_else(|| "Model".to_string());
        let candidate = ModelCandidate {
            model_id: model_id.clone(),
            fraction,
            reset,
            source_index: index,
            label,
        };
        let Some(model_id) = model_id else {
            missing_model.push(candidate);
            continue;
        };
        match selected.get(&model_id) {
            Some(current)
                if !binding_candidate_is_better(
                    candidate.fraction,
                    candidate.reset,
                    candidate.source_index,
                    current.fraction,
                    current.reset,
                    current.source_index,
                    now,
                ) => {}
            _ => {
                selected.insert(model_id, candidate);
            }
        }
    }
    let mut candidates: Vec<ModelCandidate> = selected.into_values().collect();
    candidates.extend(missing_model);
    candidates.sort_by_key(|candidate| candidate.source_index);
    let windows: Vec<UsageWindow> = candidates
        .into_iter()
        .filter_map(|candidate| {
            let (card_id, window_key) = match candidate.model_id {
                Some(model_id) => {
                    let key = format!("model.{model_id}.v1");
                    (key.clone(), Some(key))
                }
                None => (
                    format!("row.cli.config.{}.v1", candidate.source_index),
                    None,
                ),
            };
            quota_window(
                candidate.label,
                candidate.fraction,
                candidate.reset,
                now,
                card_id,
                window_key,
            )
        })
        .collect();

    let plan = status
        .user_tier
        .and_then(|t| t.name)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            status
                .plan_status
                .and_then(|p| p.plan_info)
                .and_then(local_plan_name)
        });

    let email = status.email.filter(|value| !value.trim().is_empty());
    Ok(Fetched {
        agy_login_marker: None,
        bound_account_key: None,
        source: "cli".to_string(),
        identity: Some(AgentIdentity { email, plan }),
        // Parsing remains pure and hermetic. fetch_local_ide resolves this only
        // after the authenticated loopback response has been accepted.
        account_scope: Err(AccountScopeError::NoTrustedEvidence),
        history_scope: Err(AccountScopeError::NoTrustedEvidence),
        cache_binding: None,
        windows,
    })
}

fn resolve_local_account_scope(
    identity: Option<&AgentIdentity>,
) -> Result<AccountScope, AccountScopeError> {
    let email = identity
        .and_then(|identity| identity.email.as_deref())
        .ok_or(AccountScopeError::NoTrustedEvidence)?;
    agent_account_scope::resolve_authoritative("antigravity", AuthoritativeIdKind::Email, email)
}

/// The local IDE route is one of the two routes with an authoritative owner ID
/// today: `GetUserStatus` returns the authenticated account's email. The history
/// scope must consume it, so two accounts keep two series.
fn resolve_local_history_scope(
    identity: Option<&AgentIdentity>,
) -> Result<HistoryScope, AccountScopeError> {
    resolve_local_history_scope_with(identity, agent_account_scope::resolve_history_scope)
}

fn resolve_local_history_scope_with<R>(
    identity: Option<&AgentIdentity>,
    resolve: R,
) -> Result<HistoryScope, AccountScopeError>
where
    R: FnOnce(&str, Option<(AuthoritativeIdKind, &str)>) -> Result<HistoryScope, AccountScopeError>,
{
    resolve(
        "antigravity",
        identity
            .and_then(|identity| identity.email.as_deref())
            .map(str::trim)
            .filter(|email| !email.is_empty())
            .map(|email| (AuthoritativeIdKind::Email, email)),
    )
}

fn local_plan_name(info: LocalPlanInfo) -> Option<String> {
    [
        info.plan_display_name,
        info.display_name,
        info.product_name,
        info.plan_name,
    ]
    .into_iter()
    .flatten()
    .map(|s| s.trim().to_string())
    .find(|s| !s.is_empty())
}

// ── OAuth remote (Google Code Assist) ─────────────────────────────────────────

struct RemoteContext {
    client: reqwest::Client,
    access_token: String,
    project: Option<String>,
    plan: Option<String>,
    account_scope: AccountScope,
    cache_binding: Option<ProviderCacheBinding>,
}

impl RemoteContext {
    /// `history_scope` is the caller's: the primary route passes the
    /// per-installation constant (`primary_remote_history_scope`), a captured
    /// account passes the scope of its own key.
    fn finish(
        self,
        windows: Vec<UsageWindow>,
        history_scope: Result<HistoryScope, AccountScopeError>,
    ) -> Fetched {
        Fetched {
            agy_login_marker: None,
            bound_account_key: None,
            source: "oauth".to_string(),
            // google_accounts.active is unrelated local state, not authenticated
            // by the credential that fetched these quotas.
            identity: Some(remote_identity(self.plan)),
            account_scope: Ok(self.account_scope),
            history_scope,
            cache_binding: self.cache_binding,
            windows,
        }
    }
}

enum PrimaryQuotaAttempt {
    Success(Vec<UsageWindow>),
    Forbidden,
    SchemaContradiction(ProviderFetchFailure),
    Transient(ProviderFetchFailure),
    Terminal(ProviderFetchFailure),
}

async fn fetch_oauth_primary(now: DateTime<Utc>) -> PrimaryAttempt<RemoteContext> {
    let context = match prepare_remote_context(now).await {
        Ok(context) => context,
        Err(failure) => return PrimaryAttempt::FinalFailure(failure),
    };
    match fetch_available_models(&context, now).await {
        PrimaryQuotaAttempt::Success(windows) => {
            PrimaryAttempt::Success(context.finish(windows, primary_remote_history_scope()))
        }
        PrimaryQuotaAttempt::Forbidden => PrimaryAttempt::Forbidden(context),
        PrimaryQuotaAttempt::SchemaContradiction(failure) => {
            PrimaryAttempt::SchemaContradiction { context, failure }
        }
        PrimaryQuotaAttempt::Transient(failure) => PrimaryAttempt::Transient { context, failure },
        PrimaryQuotaAttempt::Terminal(failure) => PrimaryAttempt::FinalFailure(failure),
    }
}

async fn fetch_oauth_secondary(
    context: RemoteContext,
    now: DateTime<Utc>,
) -> Result<Fetched, ProviderFetchFailure> {
    let windows = fetch_user_quota(&context, now).await?;
    Ok(context.finish(windows, primary_remote_history_scope()))
}

/// The primary remote route's history scope, resolved at the same point as
/// before `finish` took it as a parameter: after the quota windows arrived.
/// Remote OAuth carries no authoritative owner ID today: the stored Google
/// `id_token` is not read yet (HISTID-B).
fn primary_remote_history_scope() -> Result<HistoryScope, AccountScopeError> {
    agent_account_scope::resolve_history_scope("antigravity", None)
}

async fn prepare_remote_context(now: DateTime<Utc>) -> Result<RemoteContext, ProviderFetchFailure> {
    let creds_path = gemini_home()
        .map(|home| home.join("oauth_creds.json"))
        .ok_or_else(|| {
            ProviderFetchFailure::terminal("Antigravity credential location could not be resolved.")
        })?;
    let creds = remote_credentials_or_unconfigured(&creds_path)?;
    let verified = if remote_credentials_need_refresh(&creds, now) {
        refresh_access_token(&creds_path, now).await.map(
            |(_, access_token, account_scope, cache_binding)| {
                (access_token, account_scope, cache_binding)
            },
        )
    } else {
        remote_access_token(&creds)
            .map_err(|_| {
                ProviderFetchFailure::terminal("Antigravity credentials have no access token.")
            })
            .and_then(|access_token| {
                resolve_remote_account_scope(&creds_path, &creds)
                    .map(|account_scope| {
                        let cache_binding = ProviderCacheBinding::primary(account_scope.clone());
                        (access_token, account_scope, Some(cache_binding))
                    })
                    .map_err(|_| {
                        ProviderFetchFailure::terminal(
                            "Antigravity account identity could not be verified.",
                        )
                    })
            })
    };

    request_after_verified_binding(
        verified,
        |(access_token, account_scope, cache_binding)| async move {
            let client = provider_http_client_builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .map_err(|_| {
                    ProviderFetchFailure::terminal(
                        "Antigravity usage client could not be created.",
                    )
                })?;
            let code_assist_body = code_assist_post(
                &client,
                "loadCodeAssist",
                &json!({
                    "metadata": { "ideType": "ANTIGRAVITY", "platform": "PLATFORM_UNSPECIFIED", "pluginType": "GEMINI" }
                }),
                &access_token,
                cache_binding.clone(),
                false,
            )
            .await
            .map_err(|failure| match failure {
                CodeAssistPostFailure::Forbidden => ProviderFetchFailure::terminal(
                    "Antigravity loadCodeAssist permission was denied.",
                ),
                CodeAssistPostFailure::Failure(failure) => failure,
            })?;
            let code_assist: Value = serde_json::from_str(&code_assist_body).map_err(|_| {
                ProviderFetchFailure::terminal(
                    "Antigravity loadCodeAssist response could not be decoded.",
                )
            })?;

            Ok(RemoteContext {
                client,
                access_token,
                project: project_id(&code_assist),
                plan: resolve_remote_plan(&code_assist),
                account_scope,
                cache_binding,
            })
        },
    )
    .await
}

fn remote_identity(plan: Option<String>) -> AgentIdentity {
    AgentIdentity { email: None, plan }
}

/// Why the shared Google credential could not be loaded.
///
/// The distinction is behaviour, not wording. Only a genuinely absent file means
/// "nothing is configured", and only that verdict may take the card out of tab
/// navigation (#345). A file that exists but cannot be read or parsed belongs to
/// a configured account with a broken credential: it has to stay visible and say
/// so, or a permission problem and a corrupt JSON both present as "you never set
/// this up" while the card silently leaves the tab bar.
#[derive(Debug, PartialEq, Eq)]
enum RemoteCredentialError {
    Absent,
    Unreadable,
}

/// A credential that exists but cannot be used. Distinct from
/// `ANTIGRAVITY_UNCONFIGURED_ERROR` so `required_card_source` leaves it at
/// `oauth`, which keeps the card and its tab.
const ANTIGRAVITY_UNREADABLE_ERROR: &str =
    "Antigravity credentials could not be read. Re-login in Antigravity.";

/// The credential step of `prepare_remote_context`, split out so the pairing of
/// "no credential at all" with `ANTIGRAVITY_UNCONFIGURED_ERROR` is reachable
/// from a test without a Gemini home, a running IDE or the network. That pairing
/// is what keeps an Antigravity card that has never been set up out of tab
/// navigation, so it is behaviour, not a message (#345).
fn remote_credentials_or_unconfigured(path: &Path) -> Result<Value, ProviderFetchFailure> {
    load_remote_credentials(path).map_err(|error| match error {
        RemoteCredentialError::Absent => {
            ProviderFetchFailure::terminal(ANTIGRAVITY_UNCONFIGURED_ERROR)
        }
        RemoteCredentialError::Unreadable => {
            ProviderFetchFailure::terminal(ANTIGRAVITY_UNREADABLE_ERROR)
        }
    })
}

fn load_remote_credentials(path: &Path) -> Result<Value, RemoteCredentialError> {
    let raw = std::fs::read_to_string(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            RemoteCredentialError::Absent
        } else {
            RemoteCredentialError::Unreadable
        }
    })?;
    serde_json::from_str(&raw).map_err(|_| RemoteCredentialError::Unreadable)
}

fn remote_access_token(creds: &Value) -> Result<String, String> {
    creds
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "Antigravity creds have no access token".to_string())
}

fn remote_refresh_marker(creds: &Value) -> Option<&[u8]> {
    creds
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::as_bytes)
}

fn remote_credentials_need_refresh(creds: &Value, now: DateTime<Utc>) -> bool {
    let expiry_ms = creds.get("expiry_date").and_then(Value::as_f64);
    let now_ms = now.timestamp_millis() as f64;
    expiry_ms.is_none_or(|expiry| expiry <= now_ms + (REFRESH_SAFETY_SECS * 1000) as f64)
}

fn remote_scope_location(path: &Path) -> Result<String, AccountScopeError> {
    agent_account_scope::canonical_file_location(path, Some("refresh_token"))
}

fn resolve_remote_account_scope(
    path: &Path,
    creds: &Value,
) -> Result<AccountScope, AccountScopeError> {
    let marker = remote_refresh_marker(creds).ok_or(AccountScopeError::NoTrustedEvidence)?;
    agent_account_scope::resolve_credential(
        "antigravity",
        "google-oauth-creds",
        &remote_scope_location(path)?,
        marker,
    )
}

async fn refresh_access_token(
    creds_path: &Path,
    now: DateTime<Utc>,
) -> Result<(Value, String, AccountScope, Option<ProviderCacheBinding>), ProviderFetchFailure> {
    let refresh = agent_account_scope::begin_refresh("antigravity").map_err(|_| {
        ProviderFetchFailure::terminal("Antigravity credential refresh lock is unavailable.")
    })?;
    refresh_access_token_with(
        creds_path,
        now,
        &refresh,
        request_access_token,
        |creds| write_creds_atomic(creds_path, creds),
        |_| Ok(()),
    )
    .await
}

async fn request_access_token(
    refresh_token: String,
    attempt_binding: ProviderCacheBinding,
) -> Result<Value, ProviderFetchFailure> {
    let client = resolve_oauth_client().await.ok_or_else(|| {
        ProviderFetchFailure::terminal(
            "Antigravity OAuth client was not found. Install Antigravity.app or configure its OAuth client.",
        )
    })?;
    let http = provider_http_client_builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|_| {
            ProviderFetchFailure::terminal("Antigravity refresh client could not be created.")
        })?;
    let form = format!(
        "client_id={}&client_secret={}&refresh_token={}&grant_type=refresh_token",
        percent_encode(&client.0),
        percent_encode(&client.1),
        percent_encode(&refresh_token),
    );
    let response = http
        .post(GOOGLE_TOKEN_URL)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(form)
        .send()
        .await
        .map_err(|error| {
            ProviderFetchFailure::from_send_error(
                "Antigravity token refresh failed. Retrying automatically.",
                Some(attempt_binding.clone()),
                &error,
            )
        })?;
    let status = response.status().as_u16();
    let body = read_response_body(status, false, || async {
        response.text().await.map_err(|error| {
            TransportErrorFacts::from_reqwest(&error, TransportPhase::ResponseBody)
        })
    })
    .await
    .map_err(|failure| match failure {
        ResponseReadFailure::Transient(diagnostic) => ProviderFetchFailure::transient(
            "Antigravity token refresh failed. Retrying automatically.",
            Some(attempt_binding),
            diagnostic,
        ),
        ResponseReadFailure::Terminal(_) => ProviderFetchFailure::terminal(
            "Antigravity token refresh was rejected. Re-login in Antigravity.",
        ),
    })?;
    serde_json::from_str(&body).map_err(|_| {
        ProviderFetchFailure::terminal("Antigravity token refresh response could not be decoded.")
    })
}

async fn refresh_access_token_with<R, Request, RequestFuture, Save, Checkpoint>(
    creds_path: &Path,
    now: DateTime<Utc>,
    refresh: &R,
    request: Request,
    save: Save,
    mut checkpoint: Checkpoint,
) -> Result<(Value, String, AccountScope, Option<ProviderCacheBinding>), ProviderFetchFailure>
where
    R: RefreshScopeTransaction + ?Sized,
    Request: FnOnce(String, ProviderCacheBinding) -> RequestFuture,
    RequestFuture: std::future::Future<Output = Result<Value, ProviderFetchFailure>>,
    Save: FnOnce(&Value) -> std::io::Result<()>,
    Checkpoint: FnMut(RefreshCheckpoint) -> Result<(), ProviderFetchFailure>,
{
    let creds = load_remote_credentials(creds_path).map_err(|_| {
        ProviderFetchFailure::terminal("Antigravity credentials could not be reloaded.")
    })?;
    checkpoint(RefreshCheckpoint::Reloaded)?;
    let location = remote_scope_location(creds_path).map_err(|_| {
        ProviderFetchFailure::terminal("Antigravity auth location could not be verified.")
    })?;
    let old_marker = remote_refresh_marker(&creds)
        .ok_or_else(|| {
            ProviderFetchFailure::terminal("Antigravity credential has no trusted refresh marker.")
        })?
        .to_vec();
    let pre_scope = refresh
        .resolve_current("google-oauth-creds", &location, &old_marker)
        .map_err(|_| {
            ProviderFetchFailure::terminal("Antigravity account identity could not be verified.")
        })?;
    let pre_binding = ProviderCacheBinding::primary(pre_scope.clone());
    if !remote_credentials_need_refresh(&creds, now) {
        let access_token = remote_access_token(&creds).map_err(|_| {
            ProviderFetchFailure::terminal("Antigravity credentials have no access token.")
        })?;
        return Ok((creds, access_token, pre_scope, Some(pre_binding)));
    }

    let refresh_token = std::str::from_utf8(&old_marker)
        .map_err(|_| ProviderFetchFailure::terminal("Antigravity refresh credential is invalid."))?
        .to_string();
    let json = request(refresh_token, pre_binding).await?;
    checkpoint(RefreshCheckpoint::NetworkReturned)?;
    let access_token = json
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            ProviderFetchFailure::terminal(
                "Antigravity token refresh response had no access token.",
            )
        })?
        .to_string();

    // The provider refresh lock serializes Syrtis writers, but the credential
    // file has no cross-process compare-and-swap. Re-reading closes the network
    // wait race; an external writer can still race this check and atomic rename.
    let mut current_creds = load_remote_credentials(creds_path).map_err(|_| {
        ProviderFetchFailure::terminal(
            "Antigravity credentials changed during refresh; refusing stale write-back.",
        )
    })?;
    let current_marker = remote_refresh_marker(&current_creds).ok_or_else(|| {
        ProviderFetchFailure::terminal(
            "Antigravity credentials changed during refresh; refusing stale write-back.",
        )
    })?;
    if current_marker != old_marker.as_slice() {
        return Err(ProviderFetchFailure::terminal(
            "Antigravity credentials changed during refresh; refusing stale write-back.",
        ));
    }

    let obj = current_creds.as_object_mut().ok_or_else(|| {
        ProviderFetchFailure::terminal(
            "Antigravity credentials changed during refresh; refusing stale write-back.",
        )
    })?;
    obj.insert("access_token".into(), Value::String(access_token.clone()));
    if let Some(expires_in) = json.get("expires_in").and_then(Value::as_f64) {
        let expiry = now.timestamp_millis() as f64 + expires_in * 1000.0;
        obj.insert("expiry_date".into(), json!(expiry));
    }
    if let Some(id_token) = json.get("id_token").and_then(Value::as_str) {
        obj.insert("id_token".into(), Value::String(id_token.to_string()));
    }
    if let Some(replacement) = json
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
    {
        obj.insert(
            "refresh_token".into(),
            Value::String(replacement.to_string()),
        );
    }
    let new_marker = remote_refresh_marker(&current_creds)
        .ok_or_else(|| {
            ProviderFetchFailure::terminal(
                "Antigravity refreshed credential has no trusted marker.",
            )
        })?
        .to_vec();
    let account_scope = refresh
        .transfer("google-oauth-creds", &location, &old_marker, &new_marker)
        .map_err(|_| {
            ProviderFetchFailure::terminal("Antigravity credential lineage could not be preserved.")
        })?;
    checkpoint(RefreshCheckpoint::MetadataHandled)?;
    let persisted = save(&current_creds).is_ok();
    checkpoint(RefreshCheckpoint::CredentialsPersisted)?;
    let cache_binding = if persisted {
        Some(ProviderCacheBinding::primary(
            refresh
                .resolve_current("google-oauth-creds", &location, &new_marker)
                .map_err(|_| {
                    ProviderFetchFailure::terminal(
                        "Antigravity account identity could not be verified after refresh.",
                    )
                })?,
        ))
    } else {
        None
    };
    Ok((current_creds, access_token, account_scope, cache_binding))
}

fn write_creds_atomic(path: &Path, creds: &Value) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    let data = serde_json::to_vec_pretty(creds).map_err(std::io::Error::other)?;
    let directory = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "credential path has no parent",
        )
    })?;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let tmp = directory.join(format!(
        ".oauth_creds.json.tokenbar.{}.{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let staged = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(&data)?;
        file.sync_all()
    })();
    if let Err(error) = staged {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    if let Err(error) = tokscale_core::fs_atomic::replace_file(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    #[cfg(unix)]
    {
        let _ = std::fs::File::open(directory).and_then(|dir| dir.sync_all());
    }
    Ok(())
}

enum CodeAssistPostFailure {
    Forbidden,
    Failure(ProviderFetchFailure),
}

async fn code_assist_post(
    client: &reqwest::Client,
    method: &'static str,
    body: &Value,
    access_token: &str,
    attempt_binding: Option<ProviderCacheBinding>,
    forbidden_is_route_miss: bool,
) -> Result<String, CodeAssistPostFailure> {
    let response = client
        .post(format!("{CODE_ASSIST_BASE}:{method}"))
        .bearer_auth(access_token)
        .header(reqwest::header::USER_AGENT, "antigravity")
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(body)
        .send()
        .await
        .map_err(|error| {
            CodeAssistPostFailure::Failure(ProviderFetchFailure::from_send_error(
                format!("Antigravity {method} request failed. Retrying automatically."),
                attempt_binding.clone(),
                &error,
            ))
        })?;
    let status = response.status().as_u16();
    if status == 403 && forbidden_is_route_miss {
        return Err(CodeAssistPostFailure::Forbidden);
    }
    read_response_body(status, false, || async {
        response.text().await.map_err(|error| {
            TransportErrorFacts::from_reqwest(&error, TransportPhase::ResponseBody)
        })
    })
    .await
    .map_err(|failure| {
        CodeAssistPostFailure::Failure(match failure {
            ResponseReadFailure::Transient(diagnostic) => ProviderFetchFailure::transient(
                format!("Antigravity {method} request failed. Retrying automatically."),
                attempt_binding,
                diagnostic,
            ),
            ResponseReadFailure::Terminal(401) => {
                ProviderFetchFailure::terminal(ANTIGRAVITY_AUTH_EXPIRED)
            }
            ResponseReadFailure::Terminal(403) => ProviderFetchFailure::terminal(format!(
                "Antigravity {method} permission was denied."
            )),
            ResponseReadFailure::Terminal(status) => ProviderFetchFailure::terminal(format!(
                "Antigravity {method} rejected the request (status {status})."
            )),
        })
    })
}

async fn fetch_available_models(
    context: &RemoteContext,
    now: DateTime<Utc>,
) -> PrimaryQuotaAttempt {
    let body = match context.project.as_deref() {
        Some(project) => json!({ "project": project }),
        None => json!({}),
    };
    let response = match code_assist_post(
        &context.client,
        "fetchAvailableModels",
        &body,
        &context.access_token,
        context.cache_binding.clone(),
        true,
    )
    .await
    {
        Ok(response) => response,
        Err(CodeAssistPostFailure::Forbidden) => return PrimaryQuotaAttempt::Forbidden,
        Err(CodeAssistPostFailure::Failure(failure @ ProviderFetchFailure::Transient { .. })) => {
            return PrimaryQuotaAttempt::Transient(failure);
        }
        Err(CodeAssistPostFailure::Failure(failure)) => {
            return PrimaryQuotaAttempt::Terminal(failure);
        }
    };
    match models_from_available(&response, now) {
        Ok(windows) if !windows.is_empty() => PrimaryQuotaAttempt::Success(windows),
        Ok(_) | Err(_) => PrimaryQuotaAttempt::SchemaContradiction(ProviderFetchFailure::terminal(
            "Antigravity fetchAvailableModels returned no usable quota windows.",
        )),
    }
}

async fn fetch_user_quota(
    context: &RemoteContext,
    now: DateTime<Utc>,
) -> Result<Vec<UsageWindow>, ProviderFetchFailure> {
    let body = match context.project.as_deref() {
        Some(project) => json!({ "project": project }),
        None => json!({}),
    };
    let response = code_assist_post(
        &context.client,
        "retrieveUserQuota",
        &body,
        &context.access_token,
        context.cache_binding.clone(),
        false,
    )
    .await
    .map_err(|failure| match failure {
        CodeAssistPostFailure::Forbidden => {
            ProviderFetchFailure::terminal("Antigravity retrieveUserQuota permission was denied.")
        }
        CodeAssistPostFailure::Failure(failure) => failure,
    })?;
    let windows = buckets_from_quota(&response, now).map_err(|_| {
        ProviderFetchFailure::terminal(
            "Antigravity retrieveUserQuota response could not be decoded.",
        )
    })?;
    if windows.is_empty() {
        return Err(ProviderFetchFailure::terminal(
            "Antigravity retrieveUserQuota returned no usable quota windows.",
        ));
    }
    Ok(windows)
}

fn project_id(code_assist: &Value) -> Option<String> {
    match code_assist.get("cloudaicompanionProject") {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Some(Value::Object(obj)) => obj
            .get("value")
            .or_else(|| obj.get("id"))
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        _ => None,
    }
}

fn resolve_remote_plan(code_assist: &Value) -> Option<String> {
    // A paying Google AI subscriber was labelled "Free" from `currentTier`
    // (observed 2026-10-05); the subscription is named in `paidTier` (CodexBar
    // `GeminiStatusProbe.resolveAccountPlan` treats it as authoritative).
    if let Some(paid) = code_assist
        .pointer("/paidTier/name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(paid.to_string());
    }
    if let Some(plan_type) = code_assist
        .pointer("/planInfo/planType")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(clean_plan(plan_type));
    }
    match code_assist
        .pointer("/currentTier/id")
        .and_then(Value::as_str)
        .map(str::trim)
    {
        Some("standard-tier") => Some("Paid".to_string()),
        Some("free-tier") => Some("Free".to_string()),
        Some("legacy-tier") => Some("Legacy".to_string()),
        _ => code_assist
            .pointer("/currentTier/name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    }
}

fn models_from_available(body: &str, now: DateTime<Utc>) -> Result<Vec<UsageWindow>, String> {
    let response: AvailableModelsResponse = serde_json::from_str(body)
        .map_err(|e| format!("decode Antigravity fetchAvailableModels: {e}"))?;
    let mut selected: BTreeMap<String, ModelCandidate> = BTreeMap::new();
    let mut missing_model = Vec::new();
    for (source_index, (raw_id, model)) in response.models.into_iter().enumerate() {
        let Ok(model) = serde_json::from_str::<Value>(model.get()) else {
            continue;
        };
        let Some(quota) = model.get("quotaInfo") else {
            continue;
        };
        let Some(fraction) = quota.get("remainingFraction").and_then(Value::as_f64) else {
            continue;
        };
        let reset = quota
            .get("resetTime")
            .and_then(Value::as_str)
            .and_then(parse_datetime);
        let model_id = raw_id.trim().to_string();
        let model_id = (!model_id.is_empty()).then_some(model_id);
        let label = model
            .get("displayName")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                model
                    .get("label")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
            })
            .unwrap_or(raw_id.as_str())
            .to_string();
        let candidate = ModelCandidate {
            model_id: model_id.clone(),
            fraction,
            reset,
            source_index,
            label,
        };
        let Some(model_id) = model_id else {
            missing_model.push(candidate);
            continue;
        };
        match selected.get(&model_id) {
            Some(current)
                if !binding_candidate_is_better(
                    candidate.fraction,
                    candidate.reset,
                    candidate.source_index,
                    current.fraction,
                    current.reset,
                    current.source_index,
                    now,
                ) => {}
            _ => {
                selected.insert(model_id, candidate);
            }
        }
    }

    let mut candidates: Vec<ModelCandidate> = selected.into_values().collect();
    candidates.extend(missing_model);
    candidates.sort_by_key(|candidate| candidate.source_index);
    Ok(candidates
        .into_iter()
        .filter_map(|candidate| {
            let (card_id, window_key) = match candidate.model_id {
                Some(model_id) => {
                    let key = format!("model.{model_id}.v1");
                    (key.clone(), Some(key))
                }
                None => (format!("row.models.{}.v1", candidate.source_index), None),
            };
            quota_window(
                candidate.label,
                candidate.fraction,
                candidate.reset,
                now,
                card_id,
                window_key,
            )
        })
        .collect())
}

#[derive(Debug)]
struct QuotaBucketCandidate {
    model_id: Option<String>,
    fraction: f64,
    reset: Option<DateTime<Utc>>,
    source_index: usize,
}

fn buckets_from_quota(body: &str, now: DateTime<Utc>) -> Result<Vec<UsageWindow>, String> {
    let response: QuotaBucketsResponse = serde_json::from_str(body)
        .map_err(|e| format!("decode Antigravity retrieveUserQuota: {e}"))?;
    let mut selected: BTreeMap<String, QuotaBucketCandidate> = BTreeMap::new();
    let mut missing = Vec::new();
    for (source_index, bucket) in response.buckets.into_iter().enumerate() {
        let Ok(bucket) = serde_json::from_str::<Value>(bucket.get()) else {
            continue;
        };
        let Some(fraction) = bucket.get("remainingFraction").and_then(Value::as_f64) else {
            continue;
        };
        let reset = bucket
            .get("resetTime")
            .and_then(Value::as_str)
            .and_then(parse_datetime);
        let model_id = bucket
            .get("modelId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(str::to_string);
        let candidate = QuotaBucketCandidate {
            model_id: model_id.clone(),
            fraction,
            reset,
            source_index,
        };
        let Some(model_id) = model_id else {
            missing.push(candidate);
            continue;
        };
        match selected.get(&model_id) {
            Some(current) if !bucket_candidate_is_better(&candidate, current, now) => {}
            _ => {
                selected.insert(model_id, candidate);
            }
        }
    }

    let mut chosen: Vec<QuotaBucketCandidate> = selected.into_values().collect();
    chosen.extend(missing);
    chosen.sort_by_key(|candidate| candidate.source_index);
    Ok(chosen
        .into_iter()
        .filter_map(|candidate| {
            let label = candidate
                .model_id
                .clone()
                .unwrap_or_else(|| "Model".to_string());
            let (card_id, window_key) = match candidate.model_id {
                Some(model_id) => {
                    let key = format!("model.{model_id}.v1");
                    (key.clone(), Some(key))
                }
                None => (
                    format!("row.quota.bucket.{}.v1", candidate.source_index),
                    None,
                ),
            };
            quota_window(
                label,
                candidate.fraction,
                candidate.reset,
                now,
                card_id,
                window_key,
            )
        })
        .collect())
}

fn bucket_candidate_is_better(
    candidate: &QuotaBucketCandidate,
    current: &QuotaBucketCandidate,
    now: DateTime<Utc>,
) -> bool {
    binding_candidate_is_better(
        candidate.fraction,
        candidate.reset,
        candidate.source_index,
        current.fraction,
        current.reset,
        current.source_index,
        now,
    )
}

fn binding_candidate_is_better(
    candidate_fraction: f64,
    candidate_reset: Option<DateTime<Utc>>,
    candidate_index: usize,
    current_fraction: f64,
    current_reset: Option<DateTime<Utc>>,
    current_index: usize,
    now: DateTime<Utc>,
) -> bool {
    match (
        valid_remaining_fraction(candidate_fraction),
        valid_remaining_fraction(current_fraction),
    ) {
        (true, false) => return true,
        (false, true) => return false,
        _ => {}
    }
    match candidate_fraction.total_cmp(&current_fraction) {
        std::cmp::Ordering::Less => return true,
        std::cmp::Ordering::Greater => return false,
        std::cmp::Ordering::Equal => {}
    }
    let candidate_reset = candidate_reset.filter(|reset| *reset > now);
    let current_reset = current_reset.filter(|reset| *reset > now);
    match (candidate_reset, current_reset) {
        (Some(candidate), Some(current)) if candidate != current => return candidate < current,
        (Some(_), None) => return true,
        (None, Some(_)) => return false,
        _ => {}
    }
    candidate_index < current_index
}

// ── OAuth client discovery (scan Antigravity.app and the agy CLI) ─────────────

async fn resolve_oauth_client() -> Option<(String, String)> {
    if let (Ok(id), Ok(secret)) = (
        std::env::var("ANTIGRAVITY_OAUTH_CLIENT_ID"),
        std::env::var("ANTIGRAVITY_OAUTH_CLIENT_SECRET"),
    ) {
        let (id, secret) = (id.trim().to_string(), secret.trim().to_string());
        if !id.is_empty() && !secret.is_empty() {
            return Some((id, secret));
        }
    }
    static CACHE: tokio::sync::OnceCell<Option<(String, String)>> =
        tokio::sync::OnceCell::const_new();
    CACHE
        .get_or_init(|| async { discover_client_from_app().await })
        .await
        .clone()
}

async fn discover_client_from_app() -> Option<(String, String)> {
    // The IDE is the canonical installation. Only consult `agy` when its
    // artifacts do not provide a usable client, so the CLI remains optional.
    if let Some(client) = discover_client_from_artifacts(client_artifact_candidates()) {
        return Some(client);
    }
    discover_client_from_artifacts(agy_cli_artifact_candidates(false).await)
}

fn discover_client_from_artifacts<I>(paths: I) -> Option<(String, String)>
where
    I: IntoIterator<Item = PathBuf>,
{
    for path in paths {
        let Ok(data) = std::fs::read(&path) else {
            continue;
        };
        let ids = scan_client_ids(&data);
        let secrets = scan_client_secrets(&data);
        if let Some(client) = preferred_client(&ids, &secrets) {
            return Some(client);
        }
    }
    None
}

fn client_artifact_candidates() -> Vec<PathBuf> {
    let relative = [
        "Contents/Resources/bin/language_server",
        "Contents/Resources/bin/language_server_macos",
        "Contents/Resources/app/extensions/antigravity/bin/language_server_macos_arm",
        "Contents/Resources/app/extensions/antigravity/bin/language_server_macos_x64",
        "Contents/Resources/app/extensions/antigravity/bin/language_server_macos",
        "Contents/Resources/app/out/main.js",
    ];
    let mut roots = vec![PathBuf::from("/Applications/Antigravity.app")];
    if let Some(home) = crate::user_home_dir() {
        roots.push(home.join("Applications/Antigravity.app"));
    }
    roots
        .iter()
        .flat_map(|root| relative.iter().map(move |r| root.join(r)))
        .collect()
}

/// `throttle_login_shell` is true only for the quota poll. Without `agy` an
/// empty result would start a login shell on every quota refresh (#353), so
/// that caller is rate-limited by `AGY_LOGIN_SHELL_COOLDOWN`. OAuth client
/// discovery must not be: `resolve_oauth_client` stores its first answer for
/// the life of the process, so a cooldown-suppressed empty list would become a
/// permanent `None`.
#[cfg(target_os = "macos")]
async fn agy_cli_artifact_candidates(throttle_login_shell: bool) -> Vec<PathBuf> {
    static CACHE: tokio::sync::OnceCell<Vec<PathBuf>> = tokio::sync::OnceCell::const_new();
    static LAST_SHELL_DISCOVERY: std::sync::Mutex<Option<std::time::Instant>> =
        std::sync::Mutex::new(None);
    if let Some(cached) = CACHE.get() {
        return cached.clone();
    }

    // PATH is the normal resolution rule. Only start a login shell when the
    // GUI process did not inherit a usable PATH entry. Do not cache an empty
    // result: the CLI may be installed after Syrtis has started.
    let candidates =
        if let Some(path) = executable_from_path(std::env::var_os("PATH").as_deref(), "agy") {
            vec![path]
        } else if !throttle_login_shell
            || claim_agy_login_shell_discovery(&LAST_SHELL_DISCOVERY, std::time::Instant::now())
        {
            agy_cli_artifact_candidates_from(None, discover_agy_from_login_shell().await)
        } else {
            Vec::new()
        };
    cache_non_empty_agy_candidates(&CACHE, candidates)
}

/// How long one polled login-shell discovery suppresses the next. It bounds
/// how late an `agy` installed after launch, and reachable only through the
/// login shell, is found by the poll. A shell that timed out also holds the
/// slot: a slow shell is the costly case this limits. The monotonic clock
/// pauses across sleep, which only lengthens the wait.
#[cfg(any(target_os = "macos", test))]
const AGY_LOGIN_SHELL_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Whether a polled login-shell discovery may start now. Claims the slot
/// before the shell runs, so overlapping callers start one shell, not one each.
#[cfg(any(target_os = "macos", test))]
fn claim_agy_login_shell_discovery(
    last: &std::sync::Mutex<Option<std::time::Instant>>,
    now: std::time::Instant,
) -> bool {
    let mut last = last
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if last.is_some_and(|at| now.saturating_duration_since(at) < AGY_LOGIN_SHELL_COOLDOWN) {
        return false;
    }
    *last = Some(now);
    true
}

#[cfg(not(target_os = "macos"))]
async fn agy_cli_artifact_candidates(_throttle_login_shell: bool) -> Vec<PathBuf> {
    Vec::new()
}

fn agy_cli_artifact_candidates_from(
    path_env: Option<&OsStr>,
    shell_path: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = executable_from_path(path_env, "agy") {
        candidates.push(path);
    }
    if let Some(path) = shell_path.filter(|path| is_executable(path)) {
        if !candidates.contains(&path) {
            candidates.push(path);
        }
    }
    candidates
}

fn cache_non_empty_agy_candidates(
    cache: &tokio::sync::OnceCell<Vec<PathBuf>>,
    candidates: Vec<PathBuf>,
) -> Vec<PathBuf> {
    if let Some(cached) = cache.get() {
        return cached.clone();
    }
    if !candidates.is_empty() {
        let _ = cache.set(candidates.clone());
    }
    cache.get().cloned().unwrap_or(candidates)
}

fn executable_from_path(path_env: Option<&OsStr>, name: &str) -> Option<PathBuf> {
    let path_env = path_env?;
    std::env::split_paths(path_env)
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable(candidate))
}

#[cfg(target_os = "macos")]
async fn discover_agy_from_login_shell() -> Option<PathBuf> {
    let script = "printf '\\0__TB_AGY_S__\\0'; command -v -- agy; printf '\\0__TB_AGY_E__\\0'";
    let future = tokio::process::Command::new(crate::agent_usage::detect_login_shell())
        .args(["-l", "-i", "-c", script])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(std::time::Duration::from_secs(5), future)
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let start_marker = "\0__TB_AGY_S__\0";
    let end_marker = "\0__TB_AGY_E__\0";
    let start = stdout.find(start_marker)? + start_marker.len();
    let rest = &stdout[start..];
    let end = rest.find(end_marker)?;
    rest[..end]
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .find(|candidate| candidate.is_absolute() && is_executable(candidate))
}

#[cfg(not(target_os = "macos"))]
async fn discover_agy_from_login_shell() -> Option<PathBuf> {
    None
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn scan_client_ids(data: &[u8]) -> Vec<String> {
    let suffix = b".apps.googleusercontent.com";
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while let Some(pos) = find_sub(&data[i..], suffix) {
        let end = i + pos + suffix.len();
        let mut start = i + pos;
        while start > 0 && is_token_byte(data[start - 1]) {
            start -= 1;
        }
        // The walk-back is greedy over `-` and `_` as well as alphanumerics, and
        // in a packed binary the bytes in front of a client id belong to whichever
        // string was laid down next to it. A Google client id is `<digits>-<token>`
        // with a single hyphen — the token and the `.apps.googleusercontent.com`
        // suffix carry none — so the id's delimiter is the LAST hyphen in the
        // segment. Re-anchor there and keep only the digit run immediately before
        // it. Anchoring on the first hyphen instead would keep a neighbour's own
        // `…letters<digits>-` tail in the head, and because `valid_client_id` only
        // checks the digits before the first hyphen it would accept the fabricated
        // id (e.g. `123-beta456-real.apps…` instead of `456-real.apps…`).
        if let Some(dash) = data[start..end].iter().rposition(|b| *b == b'-') {
            let mut head = start + dash;
            while head > start && data[head - 1].is_ascii_digit() {
                head -= 1;
            }
            start = head;
        }
        if let Ok(candidate) = std::str::from_utf8(&data[start..end]) {
            if valid_client_id(candidate) && !out.contains(&candidate.to_string()) {
                out.push(candidate.to_string());
            }
        }
        i = i + pos + suffix.len();
    }
    out
}

fn valid_client_id(s: &str) -> bool {
    s.ends_with(".apps.googleusercontent.com")
        && s.split_once('-')
            .is_some_and(|(head, _)| !head.is_empty() && head.bytes().all(|b| b.is_ascii_digit()))
}

fn scan_client_secrets(data: &[u8]) -> Vec<String> {
    let prefix = b"GOCSPX-";
    let total = prefix.len() + 28;
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while let Some(pos) = find_sub(&data[i..], prefix) {
        let abs = i + pos;
        if abs + total <= data.len() {
            let candidate = &data[abs..abs + total];
            if candidate[prefix.len()..].iter().all(|b| is_token_byte(*b)) {
                if let Ok(s) = std::str::from_utf8(candidate) {
                    if !out.contains(&s.to_string()) {
                        out.push(s.to_string());
                    }
                }
            }
        }
        i = abs + prefix.len();
    }
    out
}

/// codexbar's pairing heuristic for the (possibly multiple) ids/secrets baked
/// into the language_server binary.
fn preferred_client(ids: &[String], secrets: &[String]) -> Option<(String, String)> {
    if ids.is_empty() || secrets.is_empty() {
        return None;
    }
    if secrets.len() == 1 && ids.len() > 1 {
        return Some((ids[ids.len() - 1].clone(), secrets[0].clone()));
    }
    let secret = if secrets.len() == ids.len() && secrets.len() > 1 {
        secrets[secrets.len() - 1].clone()
    } else {
        secrets[0].clone()
    };
    Some((ids[0].clone(), secret))
}

// ── shared ────────────────────────────────────────────────────────────────────

fn gemini_home() -> Option<PathBuf> {
    gemini_home_from(
        std::env::var("GEMINI_CLI_HOME"),
        crate::user_home_dir().as_deref(),
    )
}

fn gemini_home_from(
    gemini_cli_home: Result<String, std::env::VarError>,
    user_home: Option<&Path>,
) -> Option<PathBuf> {
    let root = match gemini_cli_home {
        Ok(root) if !root.trim().is_empty() => root,
        Ok(_) | Err(_) => format!("{}/.gemini", user_home?.to_string_lossy()),
    };
    Some(PathBuf::from(root))
}

// ── Captured accounts (extra Google accounts copied from agy's login) ─────────
//
// A captured account is a second Google login the user signed `agy` into once
// and asked Syrtis to keep. Capture copies that login's refresh token, plus the
// OAuth client that issued it, into a login-keychain item Syrtis owns; every
// later poll refreshes from that item and calls the same Code Assist quota
// methods as the primary remote route.
//
// It deliberately does not reuse the primary's file-bound refresh path
// (`refresh_access_token_with`): that path exists to share a credential with an
// external writer, and this item has exactly one writer, Syrtis. So there is no
// refresh lock, no lineage binding and no compare-and-swap.
//
// What crosses each boundary:
// - agy's own item (`gemini` / `antigravity`) is read through
//   `/usr/bin/security -w` when the user presses Capture, or once per login
//   change while automatic capture is on (`auto_capture_with`), and never
//   written. The poll path reads only its attributes (`login_marker_with`).
// - Syrtis's item (`CAPTURED_SERVICE`, account = key) is written through a
//   `/usr/bin/security -i` child that receives the command on stdin, so the
//   secret is never on an argv; reads use `-w` and print only to our pipe.
// - The raw Google `sub` never leaves `capture_with` / `auto_capture_with`.
//   Everything downstream —
//   the keychain account, the registry, the FFI `accountKey`, the account and
//   history scopes — uses `captured_key(sub)`.
// - Every error leaving this section is a fixed code or a literal string: no
//   token, sub, key, email, `security` stderr, serde text or Google
//   `error_description`.

/// Keychain service of the items Syrtis writes for captured accounts.
const CAPTURED_SERVICE: &str = "com.nyanako.tokenbar.antigravity-account";
const SECURITY_TOOL: &str = "/usr/bin/security";
/// Domain separator for `captured_key`, so the key cannot collide with a
/// SHA-256 of the bare `sub` computed anywhere else.
const CAPTURED_KEY_DOMAIN: &[u8] = b"antigravity-account\0";
/// An access token is reused until this long before its expiry.
const CAPTURED_TOKEN_MARGIN_SECS: i64 = 5 * 60;
/// `security`'s exit status for errSecItemNotFound, measured on macOS.
const SECURITY_ITEM_NOT_FOUND: i32 = 44;
/// The agy read may raise a Keychain consent dialog the user has to answer.
const AGY_ITEM_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const CAPTURED_ITEM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const CAPTURED_FALLBACK_LABEL: &str = "Antigravity account";

/// agy's login item, read with `-w` (the secret): when the user presses
/// Capture, or once per login change while automatic capture is on. Never on
/// a quota poll, which reads only `AGY_KEYCHAIN_QUERY`'s attributes.
const AGY_ITEM_READ: &[&str] = &[
    "find-generic-password",
    "-s",
    "gemini",
    "-a",
    "antigravity",
    "-w",
];

const CAPTURED_REFRESH_RETRY: &str = "Antigravity token refresh failed. Retrying automatically.";
pub(crate) const CAPTURED_ITEM_MISSING: &str =
    "Antigravity account credential was not found. Capture the account again.";
const CAPTURED_ITEM_UNREADABLE: &str =
    "Antigravity account credential could not be read. Capture the account again.";
const CAPTURED_REFRESH_REJECTED: &str =
    "Antigravity account sign-in was rejected. Capture the account again.";
const CAPTURED_AUTH_EXPIRED: &str =
    "Antigravity account sign-in expired. Capture the account again.";
const CAPTURED_IDENTITY_UNVERIFIED: &str = "Antigravity account identity could not be verified.";
const CAPTURED_CLIENT_UNAVAILABLE: &str = "Antigravity usage client could not be created.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapturedAccount {
    pub key: String,
    pub label: String,
}

// ── registry ──

static CAPTURED_ACCOUNTS: std::sync::LazyLock<std::sync::RwLock<Vec<CapturedAccount>>> =
    std::sync::LazyLock::new(|| std::sync::RwLock::new(Vec::new()));

/// Registered captured accounts, in the order Swift listed them. Empty by
/// default, so a process that never calls the setter fetches the one
/// Antigravity card it always has. Holds no secret.
pub(crate) fn captured_accounts() -> Vec<CapturedAccount> {
    CAPTURED_ACCOUNTS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Replace the registry from `[{"key","label"}]`. Full-replace (`[]` clears).
/// A rejected entry is reported by index and a fixed reason, never echoed.
pub(crate) fn set_captured_accounts_from_json(raw: &str) -> Result<Value, String> {
    let input: Value =
        serde_json::from_str(raw).map_err(|_| "invalid_accounts_json".to_string())?;
    let entries = input
        .as_array()
        .ok_or_else(|| "invalid_accounts_json".to_string())?;
    let mut registered: Vec<CapturedAccount> = Vec::new();
    let mut rejected: Vec<Value> = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let key = entry.get("key").and_then(Value::as_str);
        let label = entry.get("label").and_then(Value::as_str);
        let reason = match (key, label) {
            (Some(key), Some(_)) if !valid_captured_key(key) => "invalid key",
            (Some(key), Some(_)) if registered.iter().any(|a| a.key == key) => "duplicate key",
            (Some(key), Some(label)) => {
                registered.push(CapturedAccount {
                    key: key.to_string(),
                    label: label.to_string(),
                });
                continue;
            }
            _ => "invalid entry",
        };
        rejected.push(json!({ "index": index, "reason": reason }));
    }
    let registered_count = registered.len();
    *CAPTURED_ACCOUNTS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = registered;
    Ok(json!({ "registeredCount": registered_count, "rejected": rejected }))
}

#[cfg(test)]
pub(crate) static CAPTURED_ACCOUNTS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

// ── key, value and command validation ──

/// `hex(SHA-256("antigravity-account\0" + sub))`, 64 lowercase hex.
pub(crate) fn captured_key(sub: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(CAPTURED_KEY_DOMAIN);
    hasher.update(sub.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `^[0-9a-f]{64}$`. Checked before any `security` process is started, since
/// the key is interpolated into the `security -i` command language.
fn valid_captured_key(key: &str) -> bool {
    key.len() == 64 && key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `^[A-Za-z0-9+/=]+$`: standard base64, which has no separator, quote or
/// newline that could end the `-w` argument inside the `security -i` line.
fn valid_keychain_value(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
}

/// One `/usr/bin/security` invocation. Only the builders below create one for
/// a captured item, and each validates its inputs first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SecurityCall {
    argv: Vec<String>,
    stdin: Option<Vec<u8>>,
}

fn security_argv(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| arg.to_string()).collect()
}

/// `security -i` with the add command on stdin: the value never reaches argv,
/// where any same-user process could read it from the process table.
fn write_item_call(key: &str, value: &str) -> Option<SecurityCall> {
    if !valid_captured_key(key) || !valid_keychain_value(value) {
        return None;
    }
    Some(SecurityCall {
        argv: security_argv(&["-i"]),
        stdin: Some(
            format!("add-generic-password -U -s {CAPTURED_SERVICE} -a {key} -w {value}\n")
                .into_bytes(),
        ),
    })
}

fn read_item_call(key: &str) -> Option<SecurityCall> {
    valid_captured_key(key).then(|| SecurityCall {
        argv: security_argv(&[
            "find-generic-password",
            "-s",
            CAPTURED_SERVICE,
            "-a",
            key,
            "-w",
        ]),
        stdin: None,
    })
}

fn delete_item_call(key: &str) -> Option<SecurityCall> {
    valid_captured_key(key).then(|| SecurityCall {
        argv: security_argv(&["delete-generic-password", "-s", CAPTURED_SERVICE, "-a", key]),
        stdin: None,
    })
}

// ── stored credential and agy's login ──

/// The value of a captured item, base64 JSON. The client is pinned at
/// capture so a poll never rescans application binaries.
#[derive(Clone, PartialEq, Eq)]
struct StoredCredential {
    refresh_token: String,
    client: OAuthClient,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct OAuthClient {
    id: String,
    secret: String,
}

fn encode_stored(credential: &StoredCredential) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(
        json!({
            "refresh_token": credential.refresh_token,
            "client_id": credential.client.id,
            "client_secret": credential.client.secret,
        })
        .to_string(),
    )
}

fn non_empty_str<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

fn decode_stored(stdout: &[u8]) -> Option<StoredCredential> {
    use base64::Engine as _;
    let text = std::str::from_utf8(stdout).ok()?.trim();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(text)
        .ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    Some(StoredCredential {
        refresh_token: non_empty_str(&value, "refresh_token")?.to_string(),
        client: OAuthClient {
            id: non_empty_str(&value, "client_id")?.to_string(),
            secret: non_empty_str(&value, "client_secret")?.to_string(),
        },
    })
}

/// The claims of a JWT, decoded without verification. Callers use them only
/// as a label, a client selector, or a value compared with another JWT's.
fn jwt_claims(token: &str) -> Option<Value> {
    use base64::Engine as _;
    let payload = token.split('.').nth(1)?.trim_end_matches('=');
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

struct AgyLogin {
    refresh_token: String,
    sub: String,
    aud: String,
    email: Option<String>,
}

/// agy's go-keyring item: `go-keyring-base64:` + base64 JSON
/// `{token:{refresh_token,…}, id_token, …}`.
fn parse_agy_login(stdout: &[u8]) -> Result<AgyLogin, CaptureError> {
    use base64::Engine as _;
    let text = std::str::from_utf8(stdout)
        .map_err(|_| CaptureError::AgyLoginUnreadable)?
        .trim();
    let encoded = text
        .strip_prefix("go-keyring-base64:")
        .ok_or(CaptureError::AgyLoginUnreadable)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| CaptureError::AgyLoginUnreadable)?;
    let login: Value =
        serde_json::from_slice(&bytes).map_err(|_| CaptureError::AgyLoginUnreadable)?;
    let refresh_token = login
        .get("token")
        .and_then(|token| non_empty_str(token, "refresh_token"))
        .ok_or(CaptureError::AgyLoginUnreadable)?
        .to_string();
    let claims = non_empty_str(&login, "id_token")
        .and_then(jwt_claims)
        .ok_or(CaptureError::AgyLoginMissingIdentity)?;
    let sub = non_empty_str(&claims, "sub").ok_or(CaptureError::AgyLoginMissingIdentity)?;
    let aud = non_empty_str(&claims, "aud").ok_or(CaptureError::AgyLoginMissingIdentity)?;
    Ok(AgyLogin {
        refresh_token,
        sub: sub.to_string(),
        aud: aud.to_string(),
        email: non_empty_str(&claims, "email").map(str::to_string),
    })
}

/// Every client with id `client_id` found in the artifacts, one per secret of
/// each artifact that contains that id. The id is the token's own `aud`, not
/// `preferred_client`'s positional pick: S1 measured agy's issuing client
/// differing from that pick, and a refresh token only refreshes with the
/// client that issued it.
fn clients_for_aud<I>(client_id: &str, artifacts: I) -> Vec<OAuthClient>
where
    I: IntoIterator<Item = Vec<u8>>,
{
    let mut clients: Vec<OAuthClient> = Vec::new();
    for data in artifacts {
        if !scan_client_ids(&data).iter().any(|id| id == client_id) {
            continue;
        }
        for secret in scan_client_secrets(&data) {
            let client = OAuthClient {
                id: client_id.to_string(),
                secret,
            };
            if !clients.contains(&client) {
                clients.push(client);
            }
        }
    }
    clients
}

// ── token endpoint ──

enum TokenRejection {
    /// `invalid_client` / `unauthorized_client`: this secret is not the
    /// issuing client's; capture tries the next candidate.
    WrongClient,
    Other,
}

/// Only the OAuth `error` code is read from a rejection; `error_description`
/// is never looked at, so it cannot reach a card or the FFI.
fn token_response(status: u16, body: &str) -> Result<Value, TokenRejection> {
    let json: Option<Value> = serde_json::from_str(body).ok();
    if (200..=299).contains(&status) {
        return json
            .filter(|json| non_empty_str(json, "access_token").is_some())
            .ok_or(TokenRejection::Other);
    }
    match json.as_ref().and_then(|json| non_empty_str(json, "error")) {
        Some("invalid_client" | "unauthorized_client") => Err(TokenRejection::WrongClient),
        _ => Err(TokenRejection::Other),
    }
}

// ── I/O seam ──

/// What `security` returned: the exit code (None when killed by a signal)
/// and stdout. stderr is never captured.
pub(crate) struct SecurityExit {
    code: Option<i32>,
    stdout: Vec<u8>,
}

/// Everything the captured path does outside this process. Production is
/// `SystemCapturedIo`; tests substitute every method, so no test starts a
/// `security` process, touches the login keychain, scans an installed
/// binary, or reaches the network.
pub(crate) trait CapturedIo {
    async fn security(
        &self,
        call: SecurityCall,
        timeout: std::time::Duration,
    ) -> Option<SecurityExit>;
    /// The bytes of each Antigravity.app / agy artifact that may embed a
    /// client. Called when the user presses Capture, or by an automatic
    /// capture after a login change (`throttle_login_shell` true: the
    /// login-shell lookup for `agy` is rate-limited like the quota poll's).
    async fn client_artifacts(&self, throttle_login_shell: bool) -> Vec<Vec<u8>>;
    /// POST a refresh-token grant; `Ok((status, body))` for any HTTP answer
    /// except 429 / 5xx, which are transient failures like every other
    /// transport failure.
    async fn token_post(
        &self,
        client: &OAuthClient,
        refresh_token: &str,
        binding: Option<ProviderCacheBinding>,
    ) -> Result<(u16, String), ProviderFetchFailure>;
    fn scopes(
        &self,
        key: &str,
    ) -> (
        Result<AccountScope, AccountScopeError>,
        Result<HistoryScope, AccountScopeError>,
    );
    async fn quota(
        &self,
        access_token: String,
        account_scope: AccountScope,
        history_scope: Result<HistoryScope, AccountScopeError>,
        now: DateTime<Utc>,
    ) -> Result<Fetched, ProviderFetchFailure>;
}

/// A captured account's scopes: `OpaqueId` = its key, under the provider
/// "antigravity". The key differs per `sub`, and the primary's account scope
/// is credential-derived and its history scope the per-installation constant,
/// so neither can coincide with a captured account's.
fn captured_scopes<Auth, History>(
    key: &str,
    resolve_authoritative: Auth,
    resolve_history: History,
) -> (
    Result<AccountScope, AccountScopeError>,
    Result<HistoryScope, AccountScopeError>,
)
where
    Auth: FnOnce(&str, AuthoritativeIdKind, &str) -> Result<AccountScope, AccountScopeError>,
    History: FnOnce(
        &str,
        Option<(AuthoritativeIdKind, &str)>,
    ) -> Result<HistoryScope, AccountScopeError>,
{
    (
        resolve_authoritative("antigravity", AuthoritativeIdKind::OpaqueId, key),
        resolve_history("antigravity", Some((AuthoritativeIdKind::OpaqueId, key))),
    )
}

/// `retrieveUserQuotaSummary`: the allowance groups agy's `/usage` prints,
/// e.g. "Gemini Models" and "Claude and GPT models", each with a weekly and a
/// five-hour bucket. Shape measured on 2026-10-02:
/// `groups[].{displayName, buckets[].{bucketId, displayName, remainingFraction,
/// resetTime, window}}`. Labels and card ids follow the agy route
/// (`parse_agy_usage`), so a captured card reads like the primary's. `None`
/// on any failure or an empty answer: the caller falls back to the catalog.
async fn fetch_quota_summary(context: &RemoteContext, now: DateTime<Utc>) -> Option<Vec<UsageWindow>> {
    let body = match context.project.as_deref() {
        Some(project) => json!({ "project": project }),
        None => json!({}),
    };
    let response = code_assist_post(
        &context.client,
        "retrieveUserQuotaSummary",
        &body,
        &context.access_token,
        context.cache_binding.clone(),
        true,
    )
    .await
    .ok()?;
    let windows = windows_from_quota_summary(&response, now);
    (!windows.is_empty()).then_some(windows)
}

fn windows_from_quota_summary(body: &str, now: DateTime<Utc>) -> Vec<UsageWindow> {
    let Ok(response) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let text = |value: &Value, key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let mut windows = Vec::new();
    for group in response.get("groups").and_then(Value::as_array).into_iter().flatten() {
        let group_name = text(group, "displayName").unwrap_or_else(|| "Antigravity".to_string());
        for bucket in group.get("buckets").and_then(Value::as_array).into_iter().flatten() {
            let Some(id) = text(bucket, "bucketId") else { continue };
            let Some(fraction) = bucket.get("remainingFraction").and_then(Value::as_f64) else {
                continue;
            };
            let reset = bucket
                .get("resetTime")
                .and_then(Value::as_str)
                .and_then(parse_datetime);
            let name = text(bucket, "displayName").unwrap_or_else(|| "Limit".to_string());
            let card_id = format!("agy.{id}.v1");
            if let Some(window) = agy_bucket_window(
                format!("{group_name} · {name}"),
                fraction,
                reset,
                now,
                card_id,
                bucket.get("window").and_then(Value::as_str),
            ) {
                windows.push(window);
            }
        }
    }
    windows
}

/// Both the token and the Code Assist requests of a captured account go
/// through this client. No redirects: the default policy would resend a POST
/// body carrying the refresh token or the bearer to wherever a 307/308 points.
fn captured_http_client() -> Result<reqwest::Client, ProviderFetchFailure> {
    provider_http_client_builder()
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| ProviderFetchFailure::terminal(CAPTURED_CLIENT_UNAVAILABLE))
}

pub(crate) struct SystemCapturedIo;

impl CapturedIo for SystemCapturedIo {
    async fn security(
        &self,
        call: SecurityCall,
        timeout: std::time::Duration,
    ) -> Option<SecurityExit> {
        use tokio::io::AsyncWriteExt as _;
        let mut command = tokio::process::Command::new(SECURITY_TOOL);
        command
            .args(&call.argv)
            .stdin(if call.stdin.is_some() {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let run = async move {
            let mut child = command.spawn().ok()?;
            if let Some(input) = call.stdin {
                let mut stdin = child.stdin.take()?;
                stdin.write_all(&input).await.ok()?;
                // Closing stdin is what ends `security -i`'s read loop.
                drop(stdin);
            }
            let output = child.wait_with_output().await.ok()?;
            Some(SecurityExit {
                code: output.status.code(),
                stdout: output.stdout,
            })
        };
        // A timeout drops `run`, and `kill_on_drop` kills the child with it.
        tokio::time::timeout(timeout, run).await.ok().flatten()
    }

    async fn client_artifacts(&self, throttle_login_shell: bool) -> Vec<Vec<u8>> {
        let mut paths = client_artifact_candidates();
        paths.extend(agy_cli_artifact_candidates(throttle_login_shell).await);
        paths
            .into_iter()
            .filter_map(|path| std::fs::read(path).ok())
            .collect()
    }

    async fn token_post(
        &self,
        client: &OAuthClient,
        refresh_token: &str,
        binding: Option<ProviderCacheBinding>,
    ) -> Result<(u16, String), ProviderFetchFailure> {
        let http = captured_http_client()?;
        let form = format!(
            "client_id={}&client_secret={}&refresh_token={}&grant_type=refresh_token",
            percent_encode(&client.id),
            percent_encode(&client.secret),
            percent_encode(refresh_token),
        );
        let response = http
            .post(GOOGLE_TOKEN_URL)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(form)
            .send()
            .await
            .map_err(|error| {
                ProviderFetchFailure::from_send_error(
                    CAPTURED_REFRESH_RETRY,
                    binding.clone(),
                    &error,
                )
            })?;
        let status = response.status().as_u16();
        if status == 429 {
            return Err(ProviderFetchFailure::transient(
                CAPTURED_REFRESH_RETRY,
                binding,
                SafeTransportDiagnostic::rate_limited(status),
            ));
        }
        if (500..=599).contains(&status) {
            return Err(ProviderFetchFailure::transient(
                CAPTURED_REFRESH_RETRY,
                binding,
                SafeTransportDiagnostic::server_error(status),
            ));
        }
        let body = response.text().await.map_err(|error| {
            ProviderFetchFailure::transient(
                CAPTURED_REFRESH_RETRY,
                binding,
                SafeTransportDiagnostic::from_facts(TransportErrorFacts::from_reqwest(
                    &error,
                    TransportPhase::ResponseBody,
                )),
            )
        })?;
        Ok((status, body))
    }

    fn scopes(
        &self,
        key: &str,
    ) -> (
        Result<AccountScope, AccountScopeError>,
        Result<HistoryScope, AccountScopeError>,
    ) {
        captured_scopes(
            key,
            agent_account_scope::resolve_authoritative,
            agent_account_scope::resolve_history_scope,
        )
    }

    /// loadCodeAssist, then fetchAvailableModels with retrieveUserQuota as the
    /// fallback, with the same precedence as the primary remote route
    /// (`fetch_with` with no local route).
    async fn quota(
        &self,
        access_token: String,
        account_scope: AccountScope,
        history_scope: Result<HistoryScope, AccountScopeError>,
        now: DateTime<Utc>,
    ) -> Result<Fetched, ProviderFetchFailure> {
        let cache_binding = ProviderCacheBinding::primary(account_scope.clone());
        let client = captured_http_client()?;
        let code_assist_body = code_assist_post(
            &client,
            "loadCodeAssist",
            &json!({
                "metadata": { "ideType": "ANTIGRAVITY", "platform": "PLATFORM_UNSPECIFIED", "pluginType": "GEMINI" }
            }),
            &access_token,
            Some(cache_binding.clone()),
            false,
        )
        .await
        .map_err(|failure| match failure {
            CodeAssistPostFailure::Forbidden => ProviderFetchFailure::terminal(
                "Antigravity loadCodeAssist permission was denied.",
            ),
            CodeAssistPostFailure::Failure(failure) => failure,
        })?;
        let code_assist: Value = serde_json::from_str(&code_assist_body).map_err(|_| {
            ProviderFetchFailure::terminal(
                "Antigravity loadCodeAssist response could not be decoded.",
            )
        })?;
        let context = RemoteContext {
            client,
            access_token,
            project: project_id(&code_assist),
            plan: resolve_remote_plan(&code_assist),
            account_scope,
            cache_binding: Some(cache_binding),
        };
        // The same grouped allowances agy's `/usage` prints (and the primary
        // card shows on the agy route); the per-model catalog is the fallback.
        if let Some(windows) = fetch_quota_summary(&context, now).await {
            return Ok(context.finish(windows, history_scope));
        }
        let secondary_history = history_scope.clone();
        fetch_with(
            || async { LocalAttempt::RouteMiss },
            || async move {
                match fetch_available_models(&context, now).await {
                    PrimaryQuotaAttempt::Success(windows) => {
                        PrimaryAttempt::Success(context.finish(windows, history_scope))
                    }
                    PrimaryQuotaAttempt::Forbidden => PrimaryAttempt::Forbidden(context),
                    PrimaryQuotaAttempt::SchemaContradiction(failure) => {
                        PrimaryAttempt::SchemaContradiction { context, failure }
                    }
                    PrimaryQuotaAttempt::Transient(failure) => {
                        PrimaryAttempt::Transient { context, failure }
                    }
                    PrimaryQuotaAttempt::Terminal(failure) => PrimaryAttempt::FinalFailure(failure),
                }
            },
            |context: RemoteContext| async move {
                let windows = fetch_user_quota(&context, now).await?;
                Ok(context.finish(windows, secondary_history))
            },
        )
        .await
    }
}

// ── access tokens ──

pub(crate) struct CachedToken {
    access_token: String,
    expires_at: DateTime<Utc>,
}

/// Access tokens per key, in memory only. Never persisted.
pub(crate) type CapturedTokenCache = std::sync::Mutex<HashMap<String, CachedToken>>;

static CAPTURED_TOKENS: std::sync::LazyLock<CapturedTokenCache> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

fn lock_tokens(
    cache: &CapturedTokenCache,
) -> std::sync::MutexGuard<'_, HashMap<String, CachedToken>> {
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A cached access token while it has more than five minutes left; otherwise
/// read the item and refresh once. The item is written only when Google
/// returns a refresh token different from the stored one.
///
/// ponytail: no cross-process lock. Two Syrtis processes refreshing one
/// captured account both succeed, because Google issues a new access token
/// without rotating the refresh token (S1: U1 and U2 both unrotated). A lock is
/// added only if rotation is ever observed, which is a stop condition.
async fn captured_access_token<I: CapturedIo>(
    io: &I,
    cache: &CapturedTokenCache,
    key: &str,
    binding: &ProviderCacheBinding,
    now: DateTime<Utc>,
) -> Result<String, ProviderFetchFailure> {
    if let Some(cached) = lock_tokens(cache).get(key).filter(|cached| {
        cached.expires_at - chrono::Duration::seconds(CAPTURED_TOKEN_MARGIN_SECS) > now
    }) {
        return Ok(cached.access_token.clone());
    }
    let call = read_item_call(key)
        .ok_or_else(|| ProviderFetchFailure::terminal(CAPTURED_ITEM_UNREADABLE))?;
    let stored = match io.security(call, CAPTURED_ITEM_TIMEOUT).await {
        Some(SecurityExit {
            code: Some(0),
            stdout,
        }) => decode_stored(&stdout),
        Some(SecurityExit {
            code: Some(SECURITY_ITEM_NOT_FOUND),
            ..
        }) => return Err(ProviderFetchFailure::terminal(CAPTURED_ITEM_MISSING)),
        _ => None,
    }
    .ok_or_else(|| ProviderFetchFailure::terminal(CAPTURED_ITEM_UNREADABLE))?;

    let (status, body) = io
        .token_post(&stored.client, &stored.refresh_token, Some(binding.clone()))
        .await?;
    let json = token_response(status, &body)
        .map_err(|_| ProviderFetchFailure::terminal(CAPTURED_REFRESH_REJECTED))?;
    let access_token = non_empty_str(&json, "access_token")
        .ok_or_else(|| ProviderFetchFailure::terminal(CAPTURED_REFRESH_REJECTED))?
        .to_string();
    if let Some(expires_in) = json.get("expires_in").and_then(Value::as_i64) {
        lock_tokens(cache).insert(
            key.to_string(),
            CachedToken {
                access_token: access_token.clone(),
                expires_at: now + chrono::Duration::seconds(expires_in),
            },
        );
    }
    if let Some(rotated) = non_empty_str(&json, "refresh_token") {
        if rotated != stored.refresh_token {
            let updated = StoredCredential {
                refresh_token: rotated.to_string(),
                client: stored.client.clone(),
            };
            // A failed write keeps the token that was just used; the next cold
            // refresh then tries the old refresh token and reports rejection.
            if let Some(call) = write_item_call(key, &encode_stored(&updated)) {
                let _ = io.security(call, CAPTURED_ITEM_TIMEOUT).await;
            }
        }
    }
    Ok(access_token)
}

// ── capture, fetch, remove ──

/// Fixed capture/remove outcomes. `code()` is the whole FFI error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptureError {
    AgyNotSignedIn,
    AgyLoginUnreadable,
    AgyLoginMissingIdentity,
    OAuthClientNotFound,
    OAuthClientRejected,
    RefreshRejected,
    RefreshUnreachable,
    AccountMismatch,
    InvalidCredentialFormat,
    KeychainWriteFailed,
    InvalidKey,
    KeychainDeleteFailed,
    /// Automatic capture only: agy's item read ended other than exit 0 or 44
    /// (a timeout, a cancelled Keychain dialog, any other failure).
    Paused,
    /// Automatic capture only: agy has no login item (exit 44).
    NotSignedIn,
}

impl CaptureError {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::AgyNotSignedIn => "agy_not_signed_in",
            Self::AgyLoginUnreadable => "agy_login_unreadable",
            Self::AgyLoginMissingIdentity => "agy_login_missing_identity",
            Self::OAuthClientNotFound => "oauth_client_not_found",
            Self::OAuthClientRejected => "oauth_client_rejected",
            Self::RefreshRejected => "refresh_rejected",
            Self::RefreshUnreachable => "refresh_unreachable",
            Self::AccountMismatch => "account_mismatch",
            Self::InvalidCredentialFormat => "invalid_credential_format",
            Self::KeychainWriteFailed => "keychain_write_failed",
            Self::InvalidKey => "invalid_key",
            Self::KeychainDeleteFailed => "keychain_delete_failed",
            Self::Paused => "paused",
            Self::NotSignedIn => "not_signed_in",
        }
    }
}

/// Copy agy's current login into a Syrtis-owned item and return its key and
/// label. Nothing is written unless the token refreshed with its own issuing
/// client and any `id_token` in that response names the same `sub`.
async fn capture_with<I: CapturedIo>(io: &I) -> Result<CapturedAccount, CaptureError> {
    let agy_item = io
        .security(
            SecurityCall {
                argv: security_argv(AGY_ITEM_READ),
                stdin: None,
            },
            AGY_ITEM_READ_TIMEOUT,
        )
        .await
        .filter(|exit| exit.code == Some(0))
        .ok_or(CaptureError::AgyNotSignedIn)?;
    let login = parse_agy_login(&agy_item.stdout)?;
    drop(agy_item);

    let (client, json) = refresh_with_issuing_client(io, &login, false).await?;

    // The stored id_token is local and untrusted; Google's answer is not.
    if let Some(id_token) = json.get("id_token") {
        if response_sub(Some(id_token)).as_deref() != Some(login.sub.as_str()) {
            return Err(CaptureError::AccountMismatch);
        }
    }

    let key = captured_key(&login.sub);
    write_captured(io, &key, &login, client, &json).await?;
    Ok(CapturedAccount {
        key,
        label: login
            .email
            .unwrap_or_else(|| CAPTURED_FALLBACK_LABEL.to_string()),
    })
}

/// One refresh of `login`'s token with the client named by its `aud`,
/// trying that client's candidate secrets; `invalid_client` /
/// `unauthorized_client` falls through to the next, any other answer stops.
async fn refresh_with_issuing_client<I: CapturedIo>(
    io: &I,
    login: &AgyLogin,
    throttle_login_shell: bool,
) -> Result<(OAuthClient, Value), CaptureError> {
    let clients = clients_for_aud(&login.aud, io.client_artifacts(throttle_login_shell).await);
    if clients.is_empty() {
        return Err(CaptureError::OAuthClientNotFound);
    }
    for client in clients {
        let (status, body) = io
            .token_post(&client, &login.refresh_token, None)
            .await
            .map_err(|_| CaptureError::RefreshUnreachable)?;
        match token_response(status, &body) {
            Ok(json) => return Ok((client, json)),
            Err(TokenRejection::WrongClient) => continue,
            Err(TokenRejection::Other) => return Err(CaptureError::RefreshRejected),
        }
    }
    Err(CaptureError::OAuthClientRejected)
}

/// The `sub` of a token response's `id_token`, when it is a decodable JWT.
fn response_sub(id_token: Option<&Value>) -> Option<String> {
    id_token
        .and_then(Value::as_str)
        .and_then(jwt_claims)
        .and_then(|claims| non_empty_str(&claims, "sub").map(str::to_string))
}

/// Write `{refresh_token, client}` to `key`'s item. The refresh token is the
/// response's when Google rotated it, otherwise the login's.
async fn write_captured<I: CapturedIo>(
    io: &I,
    key: &str,
    login: &AgyLogin,
    client: OAuthClient,
    json: &Value,
) -> Result<(), CaptureError> {
    let credential = StoredCredential {
        refresh_token: non_empty_str(json, "refresh_token")
            .unwrap_or(&login.refresh_token)
            .to_string(),
        client,
    };
    let call = write_item_call(key, &encode_stored(&credential))
        .ok_or(CaptureError::InvalidCredentialFormat)?;
    match io.security(call, CAPTURED_ITEM_TIMEOUT).await {
        Some(SecurityExit { code: Some(0), .. }) => Ok(()),
        _ => Err(CaptureError::KeychainWriteFailed),
    }
}

/// What one automatic capture did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AutoCaptured {
    /// The login was verified at Google and written to its own item.
    Captured(CapturedAccount),
    /// Its own item already holds this refresh token: no scan, no request,
    /// no write.
    Unchanged(CapturedAccount),
    /// The user removed this account; nothing was requested or written.
    SkippedRemoved,
}

/// The automatic counterpart of `capture_with`, run once per agy login change
/// while automatic capture is on. Stricter than the manual path, because
/// nobody pressed a button:
/// 1. agy's item: exit 0 continues, 44 is `NotSignedIn`, anything else
///    (timeout, a cancelled Keychain dialog) is `Paused`, which stops further
///    automatic attempts until the user acts;
/// 2. a key in `removed_keys` is skipped before any request;
/// 3. a refresh token its own item already holds is `Unchanged`, with no
///    client scan, no request and no write;
/// 4. the client scan uses the throttled login-shell lookup;
/// 5. the refresh response MUST carry an `id_token` whose `sub` equals the
///    stored one, else `AccountMismatch` and no write.
async fn auto_capture_with<I: CapturedIo>(
    io: &I,
    removed_keys: &[String],
) -> Result<AutoCaptured, CaptureError> {
    let agy_item = match io
        .security(
            SecurityCall {
                argv: security_argv(AGY_ITEM_READ),
                stdin: None,
            },
            AGY_ITEM_READ_TIMEOUT,
        )
        .await
    {
        Some(SecurityExit {
            code: Some(0),
            stdout,
        }) => stdout,
        Some(SecurityExit {
            code: Some(SECURITY_ITEM_NOT_FOUND),
            ..
        }) => return Err(CaptureError::NotSignedIn),
        _ => return Err(CaptureError::Paused),
    };
    let login = parse_agy_login(&agy_item)?;
    drop(agy_item);

    let key = captured_key(&login.sub);
    if removed_keys.contains(&key) {
        return Ok(AutoCaptured::SkippedRemoved);
    }

    let stored_label = login.email.clone();
    let own = read_item_call(&key).ok_or(CaptureError::InvalidCredentialFormat)?;
    let unchanged = match io.security(own, CAPTURED_ITEM_TIMEOUT).await {
        Some(SecurityExit {
            code: Some(0),
            stdout,
        }) => decode_stored(&stdout)
            .is_some_and(|stored| stored.refresh_token == login.refresh_token),
        _ => false,
    };
    if unchanged {
        return Ok(AutoCaptured::Unchanged(CapturedAccount {
            key,
            label: stored_label.unwrap_or_else(|| CAPTURED_FALLBACK_LABEL.to_string()),
        }));
    }

    let (client, json) = refresh_with_issuing_client(io, &login, true).await?;
    let claims = json
        .get("id_token")
        .and_then(Value::as_str)
        .and_then(jwt_claims);
    if claims.as_ref().and_then(|claims| non_empty_str(claims, "sub")) != Some(login.sub.as_str()) {
        return Err(CaptureError::AccountMismatch);
    }
    let label = claims
        .as_ref()
        .and_then(|claims| non_empty_str(claims, "email"))
        .map(str::to_string)
        .or(stored_label)
        .unwrap_or_else(|| CAPTURED_FALLBACK_LABEL.to_string());

    write_captured(io, &key, &login, client, &json).await?;
    Ok(AutoCaptured::Captured(CapturedAccount { key, label }))
}

/// How long the attributes-only marker query may take. No dialog can appear
/// for it (no secret is requested), so a slow answer is a failure.
const AGY_MARKER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// agy's login marker for the automatic-capture trigger: the item's
/// modification date, `"present"` when it cannot be parsed, or `"absent"`
/// (exit 44). Runs only the pinned attributes-only `AGY_KEYCHAIN_QUERY`, so no
/// secret is requested; stdout is parsed for the `mdat` line and dropped.
/// Any other outcome is `None`, and the caller does nothing that poll.
async fn login_marker_with<I: CapturedIo>(io: &I) -> Option<String> {
    let exit = io
        .security(
            SecurityCall {
                argv: security_argv(AGY_KEYCHAIN_QUERY),
                stdin: None,
            },
            AGY_MARKER_TIMEOUT,
        )
        .await?;
    match exit.code {
        Some(0) => Some(
            parse_keychain_mdat(&String::from_utf8_lossy(&exit.stdout))
                .unwrap_or_else(|| "present".to_string()),
        ),
        Some(SECURITY_ITEM_NOT_FOUND) => Some("absent".to_string()),
        _ => None,
    }
}

/// Delete one captured item and its cached access token. Never revokes: the
/// refresh token stays valid at Google until the user revokes it there.
async fn remove_with<I: CapturedIo>(
    io: &I,
    cache: &CapturedTokenCache,
    key: &str,
) -> Result<(), CaptureError> {
    let call = delete_item_call(key).ok_or(CaptureError::InvalidKey)?;
    lock_tokens(cache).remove(key);
    match io.security(call, CAPTURED_ITEM_TIMEOUT).await {
        Some(SecurityExit {
            code: Some(0 | SECURITY_ITEM_NOT_FOUND),
            ..
        }) => Ok(()),
        _ => Err(CaptureError::KeychainDeleteFailed),
    }
}

/// One captured account's quota. Every failure is a per-account
/// `ProviderFetchFailure`; none is provider-wide.
pub(crate) async fn fetch_captured_with<I: CapturedIo>(
    io: &I,
    cache: &CapturedTokenCache,
    key: &str,
    label: &str,
    now: DateTime<Utc>,
) -> Result<Fetched, ProviderFetchFailure> {
    if !valid_captured_key(key) {
        return Err(ProviderFetchFailure::terminal(CAPTURED_ITEM_UNREADABLE));
    }
    let (account_scope, history_scope) = io.scopes(key);
    let account_scope =
        account_scope.map_err(|_| ProviderFetchFailure::terminal(CAPTURED_IDENTITY_UNVERIFIED))?;
    let binding = ProviderCacheBinding::primary(account_scope.clone());
    let access_token = captured_access_token(io, cache, key, &binding, now).await?;
    let mut fetched = match io
        .quota(access_token, account_scope, history_scope, now)
        .await
    {
        Ok(fetched) => fetched,
        Err(failure) => {
            // A rejected token is not reused for the rest of its lifetime.
            if matches!(failure, ProviderFetchFailure::Terminal { .. }) {
                lock_tokens(cache).remove(key);
            }
            // The quota calls are shared with the primary, whose 401 text
            // tells the user to sign in to Antigravity again. That is wrong
            // advice here: Antigravity is signed in to a different account.
            return Err(match failure {
                ProviderFetchFailure::Terminal { ref display }
                    if display == ANTIGRAVITY_AUTH_EXPIRED =>
                {
                    ProviderFetchFailure::terminal(CAPTURED_AUTH_EXPIRED)
                }
                failure => failure,
            });
        }
    };
    fetched.identity = Some(AgentIdentity {
        email: Some(label.to_string()),
        plan: fetched.identity.and_then(|identity| identity.plan),
    });
    Ok(fetched)
}

pub(crate) async fn capture() -> Result<CapturedAccount, CaptureError> {
    capture_with(&SystemCapturedIo).await
}

pub(crate) async fn auto_capture(removed_keys: &[String]) -> Result<AutoCaptured, CaptureError> {
    auto_capture_with(&SystemCapturedIo, removed_keys).await
}

pub(crate) async fn login_marker() -> Option<String> {
    login_marker_with(&SystemCapturedIo).await
}

pub(crate) async fn remove(key: &str) -> Result<(), CaptureError> {
    remove_with(&SystemCapturedIo, &CAPTURED_TOKENS, key).await
}

pub(crate) async fn fetch_captured(
    key: &str,
    label: &str,
    now: DateTime<Utc>,
) -> Result<Fetched, ProviderFetchFailure> {
    fetch_captured_with(&SystemCapturedIo, &CAPTURED_TOKENS, key, label, now).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_account_scope::test_support::TestRefreshScope;
    use crate::agent_usage::SafeTransportDiagnostic;

    fn mark_executable(path: &Path) {
        #[cfg(unix)]
        {
            let mut permissions = std::fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(path, permissions).unwrap();
        }
        #[cfg(not(unix))]
        let _ = path;
    }

    /// An unused bucket's reset rolls with Google's clock (reset = its now +
    /// window). Its declared window must not become a contract at any clock
    /// skew: behind, the contract is `InvalidEvidence` on every poll; in step
    /// or ahead, every poll would open a fresh single-sample cycle. A bucket in
    /// use (fraction < 1, reset fixed) keeps its contract.
    #[test]
    fn an_unused_rolling_bucket_gets_no_contract_at_any_clock() {
        let google_now = DateTime::parse_from_rfc3339("2026-10-04T13:30:15Z")
            .unwrap()
            .with_timezone(&Utc);
        let at = |now| -> Vec<Option<i64>> {
            parse_agy_usage(AGY_ROLLING_5H_USAGE, now)
                .unwrap()
                .windows
                .iter()
                .map(|w| w.duration_seconds_for_test())
                .collect()
        };
        // [weekly in use, 5h unused and rolling, 5h in use]
        for offset in [-1, 0, 1] {
            assert_eq!(
                at(google_now + chrono::Duration::seconds(offset)),
                [Some(7 * 86_400), None, Some(5 * 3_600)],
                "clock offset {offset}s"
            );
        }
        // A bucket in use, read before its cycle start by this clock (a
        // declared window longer than the real one): no contract, or
        // `valid_evidence` would reject it as InvalidEvidence.
        let early = DateTime::parse_from_rfc3339("2026-10-04T10:59:59Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(at(early)[2], None, "3p-5h before 11:00 cycle start");
        // The defect: that contract with the clock 1 s behind.
        let reset = DateTime::parse_from_rfc3339("2026-10-04T18:30:15Z")
            .unwrap()
            .timestamp();
        assert_eq!(
            crate::agent_quota_duration::resolve_duration(
                google_now.timestamp() - 1,
                Some(reset),
                None,
                Some(DurationEvidence::contract(5 * 3_600)),
                None,
            ),
            crate::agent_quota_duration::DurationResolution::Unavailable(
                crate::agent_quota_duration::DurationUnavailableReason::InvalidEvidence
            )
        );
    }

    /// W7c (agy 1.2.16, 188, 2026-10-04): after a 5h reset passes, the bucket
    /// reads fraction 1 with a FIXED reset chained to the old reset + 5h for
    /// about 30 min, then rolls again. Its cycle has started by this clock
    /// (reset - 5h = the old reset, in the past), so only the in-use test keeps
    /// the declared window from becoming a contract here.
    #[test]
    fn a_chained_unused_bucket_after_a_reset_gets_no_contract() {
        let now = DateTime::parse_from_rfc3339("2026-10-04T21:40:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let chained = DateTime::parse_from_rfc3339("2026-10-05T02:32:24Z")
            .unwrap()
            .with_timezone(&Utc);
        let window = |fraction| {
            agy_bucket_window(
                "Gemini Models · Five Hour Limit Remaining".to_string(),
                fraction,
                Some(chained),
                now,
                "agy.gemini-5h.v1".to_string(),
                Some("5h"),
            )
            .unwrap()
            .duration_seconds_for_test()
        };
        assert_eq!(window(1.0), None, "chained phase, fraction 1");
        // Control: the same reset once the bucket is used keeps its contract.
        assert_eq!(window(0.99), Some(5 * 3_600));
    }

    #[test]
    fn gemini_home_uses_nonempty_configured_root_unchanged() {
        let configured = " /tmp/gemini-cli-home ";
        assert_eq!(
            gemini_home_from(Ok(configured.to_string()), None),
            Some(PathBuf::from(configured))
        );
    }

    #[test]
    fn gemini_home_falls_back_on_environment_errors() {
        let home = PathBuf::from("resolved-home");
        let fallback = Some(PathBuf::from("resolved-home/.gemini"));
        for error in [
            std::env::VarError::NotPresent,
            std::env::VarError::NotUnicode(std::ffi::OsString::new()),
        ] {
            assert_eq!(gemini_home_from(Err(error), Some(&home)), fallback);
        }
    }

    #[test]
    fn gemini_home_falls_back_for_trim_empty_root() {
        let home = PathBuf::from("resolved-home");
        assert_eq!(
            gemini_home_from(Ok(" \t\n ".to_string()), Some(&home)),
            Some(PathBuf::from("resolved-home/.gemini"))
        );
    }

    #[test]
    fn extracts_flags_both_forms() {
        let cmd = "/x/language_server --app_data_dir /Users/me/.gemini/antigravity --csrf_token=ABC123 --extension_server_port 4567";
        assert_eq!(extract_flag(cmd, "--csrf_token").as_deref(), Some("ABC123"));
        assert_eq!(
            extract_flag(cmd, "--extension_server_port").as_deref(),
            Some("4567")
        );
        assert!(is_language_server(&cmd.to_lowercase()));
        assert!(is_antigravity(&cmd.to_lowercase()));
    }

    #[test]
    fn parses_lsof_listen_port() {
        let line = "language_ 123 nanako 30u IPv4 0x0 0t0 TCP 127.0.0.1:54321 (LISTEN)";
        assert_eq!(parse_listen_port(line), Some(54321));
        assert_eq!(parse_listen_port("... (ESTABLISHED)"), None);
    }

    #[test]
    fn scans_and_pairs_oauth_client_from_bytes() {
        let blob = b"junk\x00123-abcDEF_g.apps.googleusercontent.com\x00\x00GOCSPX-abcdefghijklmnopqrstuvwxyz12\x00tail";
        let ids = scan_client_ids(blob);
        let secrets = scan_client_secrets(blob);
        assert_eq!(
            ids,
            vec!["123-abcDEF_g.apps.googleusercontent.com".to_string()]
        );
        assert_eq!(secrets.len(), 1);
        let client = preferred_client(&ids, &secrets).unwrap();
        assert_eq!(client.0, "123-abcDEF_g.apps.googleusercontent.com");
        assert!(client.1.starts_with("GOCSPX-"));
    }

    #[test]
    fn discovers_oauth_client_from_agy_on_path_without_installation_assumptions() {
        let root = tempfile::tempdir().unwrap();
        let agy = root.path().join("agy");
        let client_id = ["884354919052", "-abc.apps.googleusercontent.com"].concat();
        let client_secret = ["GOCSPX-", "abcdefghijklmnopqrstuvwxyz12"].concat();
        std::fs::write(&agy, format!("agy cli\0{client_id}\0{client_secret}\0")).unwrap();
        mark_executable(&agy);

        let path_env = root.path().as_os_str();
        let candidates = agy_cli_artifact_candidates_from(Some(path_env), None);
        assert_eq!(candidates, vec![agy.clone()]);

        let client = discover_client_from_artifacts(candidates).unwrap();
        assert_eq!(client.0, client_id);
        assert_eq!(client.1, client_secret);
    }

    #[test]
    fn prefers_ide_artifact_before_agy_fallback() {
        let root = tempfile::tempdir().unwrap();
        let ide = root.path().join("language_server");
        let agy = root.path().join("agy");
        let ide_client_id = ["111111111111", "-ide.apps.googleusercontent.com"].concat();
        let agy_client_id = ["222222222222", "-agy.apps.googleusercontent.com"].concat();
        let ide_client_secret = ["GOCSPX-", "abcdefghijklmnopqrstuvwxyz12"].concat();
        let agy_client_secret = ["GOCSPX-", "zyxwvutsrqponmlkjihgfedcba21"].concat();
        std::fs::write(&ide, format!("ide {ide_client_id}\0{ide_client_secret}\0")).unwrap();
        std::fs::write(&agy, format!("agy {agy_client_id}\0{agy_client_secret}\0")).unwrap();

        let client = discover_client_from_artifacts(vec![ide, agy]).unwrap();
        assert_eq!(client.0, ide_client_id);
        assert_eq!(client.1, ide_client_secret);
    }

    #[test]
    fn parses_agy_usage_json_into_quota_windows() {
        let now = DateTime::parse_from_rfc3339("2026-08-24T06:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let body = br#"{
            "status": "SUCCESS",
            "command": {
                "name": "usage",
                "data": {
                    "groups": [
                        {
                            "name": "Gemini Models",
                            "buckets": [
                                {"id": "gemini-weekly", "name": "Weekly Limit Remaining", "remaining_fraction": 0.8837259411811829, "reset_time": "2026-08-28T06:58:51Z"},
                                {"id": "gemini-5h", "name": "Five Hour Limit Remaining", "remaining_fraction": 1, "reset_time": "2026-08-24T18:14:23Z"}
                            ]
                        },
                        {
                            "name": "Claude and GPT models",
                            "buckets": [
                                {"id": "3p-weekly", "name": "Weekly Limit Remaining", "remaining_fraction": 1, "reset_time": "2026-08-31T13:14:23Z"},
                                {"id": "3p-5h", "name": "Five Hour Limit Remaining", "remaining_fraction": 1, "reset_time": "2026-08-24T18:14:23Z"}
                            ]
                        }
                    ]
                }
            }
        }"#;

        let fetched = parse_agy_usage(body, now).unwrap();
        assert_eq!(fetched.source, "agy");
        assert_eq!(fetched.windows.len(), 4);
        assert_eq!(
            fetched.windows[0].label_for_test(),
            "Gemini Models · Weekly Limit Remaining"
        );
        assert!((fetched.windows[0].remaining_for_test() - 88.37259411811829).abs() < f64::EPSILON);
        assert_eq!(
            fetched.windows[0].pace_window_key_for_test(),
            Some("agy.gemini-weekly.v1")
        );
        assert_eq!(
            fetched.windows[2].label_for_test(),
            "Claude and GPT models · Weekly Limit Remaining"
        );
        let wire = serde_json::to_value(&fetched.windows[1]).unwrap();
        assert_eq!(wire["cardId"], "agy.gemini-5h.v1");
        let third_wire = serde_json::to_value(&fetched.windows[2]).unwrap();
        assert_eq!(third_wire["cardId"], "agy.3p-weekly.v1");
        // Control: an older agy without the `window` field leaves the duration
        // to be learned, as before.
        assert_eq!(fetched.windows[0].duration_seconds_for_test(), None);
    }

    #[test]
    fn agy_usage_window_field_sets_the_duration() {
        let now = DateTime::parse_from_rfc3339("2026-10-03T06:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        // agy 1.2.16 `/usage` shape, measured 2026-10-03.
        let body = br#"{"status":"SUCCESS","command":{"name":"usage","data":{"groups":[
            {"name":"Gemini Models","buckets":[
              {"id":"gemini-weekly","name":"Weekly Limit Remaining","window":"weekly","remaining_fraction":0.8456,"reset_time":"2026-10-08T18:46:28Z"},
              {"id":"gemini-5h","name":"Five Hour Limit Remaining","window":"5h","remaining_fraction":0.9389,"reset_time":"2026-10-03T07:43:34Z"},
              {"id":"odd","name":"Odd Limit Remaining","window":"monthly","remaining_fraction":1,"reset_time":"2026-10-30T00:00:00Z"}
            ]}]}}}"#;
        let fetched = parse_agy_usage(body, now).unwrap();
        let durations: Vec<Option<i64>> =
            fetched.windows.iter().map(|w| w.duration_seconds_for_test()).collect();
        assert_eq!(durations, [Some(7 * 86_400), Some(5 * 3_600), None]);
    }

    #[test]
    fn rejects_agy_usage_without_valid_windows() {
        let now = Utc::now();
        let body = br#"{"status":"SUCCESS","command":{"name":"usage","data":{"groups":[{"buckets":[{"id":"bad","remaining_fraction":2.0}]}]}}}"#;
        assert!(parse_agy_usage(body, now).is_err());
    }

    #[test]
    fn rejects_agy_chat_response_without_usage_command() {
        let now = Utc::now();
        let body = br#"{
            "status": "SUCCESS",
            "response": "A model-generated answer",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }"#;
        assert!(parse_agy_usage(body, now).is_err());
    }

    #[test]
    fn login_shell_agy_candidate_is_used_when_path_is_missing() {
        let root = tempfile::tempdir().unwrap();
        let agy = root.path().join("agy");
        std::fs::write(&agy, b"agy cli").unwrap();
        mark_executable(&agy);

        assert_eq!(
            agy_cli_artifact_candidates_from(None, Some(agy.clone())),
            vec![agy]
        );
    }

    #[test]
    fn scans_client_id_glued_to_a_neighbouring_string() {
        // The fixture above separates the id with NUL, which is not a token byte,
        // so the walk-back stops on its own. A real language_server packs strings
        // with no separator: the neighbour's tail is token bytes and gets absorbed
        // into the head, which must be all digits. Observed on Antigravity 1.x —
        // both ids in the shipped binary were rejected this way, which took the
        // whole OAuth route down whenever the IDE was not running.
        let blob = b"someNeighbourKey123-abcDEF_g.apps.googleusercontent.com\x00tail";
        assert_eq!(
            scan_client_ids(blob),
            vec!["123-abcDEF_g.apps.googleusercontent.com".to_string()]
        );

        // A neighbour whose own tail is `…<letters><digits>-` puts a second hyphen
        // in the segment. The id's delimiter is the last one (its token carries
        // none), so anchoring there recovers the real id; anchoring on the first
        // hyphen would keep the neighbour's `123-beta` and `valid_client_id` — which
        // only checks the digits before the first hyphen — would accept it.
        let two_hyphens = b"label123-beta456-real.apps.googleusercontent.com\x00tail";
        assert_eq!(
            scan_client_ids(two_hyphens),
            vec!["456-real.apps.googleusercontent.com".to_string()]
        );

        // ponytail: a neighbour whose tail is digits glued straight onto the id's
        // project number (no intervening hyphen) is indistinguishable from the head,
        // so the longest digit run wins and those digits are kept. Nothing in the
        // byte stream marks that boundary; the only stronger fix would be reading
        // Mach-O string sections instead of scanning bytes, which is a lot of
        // machinery for a case Google's 12-digit ids make rare.
        let glued_digits = b"prefix99000123-abcDEF_g.apps.googleusercontent.com\x00tail";
        assert_eq!(
            scan_client_ids(glued_digits),
            vec!["99000123-abcDEF_g.apps.googleusercontent.com".to_string()]
        );
    }

    #[test]
    fn skips_non_executable_path_candidate_before_shell_fallback() {
        let root = tempfile::tempdir().unwrap();
        let path_dir = root.path().join("path");
        std::fs::create_dir_all(&path_dir).unwrap();
        let path_candidate = path_dir.join("agy");
        let shell_candidate = root.path().join("shell-agy");
        std::fs::write(&path_candidate, b"not executable").unwrap();
        std::fs::write(&shell_candidate, b"agy cli").unwrap();
        mark_executable(&shell_candidate);

        assert_eq!(
            agy_cli_artifact_candidates_from(
                Some(path_candidate.parent().unwrap().as_os_str()),
                Some(shell_candidate.clone()),
            ),
            vec![shell_candidate]
        );
    }

    /// An **absent** credential file must report the marker verbatim: the
    /// snapshot's `source` is decided by comparing against it
    /// (`agent_usage::required_card_source`), so a message edited here and not
    /// there silently restores the phantom tab this pairing removes.
    ///
    /// Absent, not unreadable — the two are now different verdicts.
    /// `RemoteCredentialError::Unreadable` deliberately does NOT reach this
    /// marker, and `malformed_remote_credentials_are_unreadable_not_absent`
    /// below is the assertion that keeps it out. This wording predated that
    /// split and described the behaviour the split removed.
    #[test]
    fn absent_remote_credentials_report_the_unconfigured_marker() {
        let missing = std::env::temp_dir()
            .join("tokenbar-antigravity-unconfigured-probe")
            .join("oauth_creds.json");
        assert!(!missing.exists(), "the probe path must not exist");
        let failure = remote_credentials_or_unconfigured(&missing).unwrap_err();
        assert!(
            matches!(
                failure,
                ProviderFetchFailure::Terminal { ref display }
                    if display == ANTIGRAVITY_UNCONFIGURED_ERROR
            ),
            "absent credentials must carry the unconfigured marker, got {failure:?}"
        );
    }

    #[test]
    fn empty_agy_candidate_cache_does_not_block_later_discovery() {
        let cache = tokio::sync::OnceCell::const_new();
        let candidate = PathBuf::from("/tmp/agy");

        assert!(cache_non_empty_agy_candidates(&cache, Vec::new()).is_empty());
        assert_eq!(
            cache_non_empty_agy_candidates(&cache, vec![candidate.clone()]),
            vec![candidate.clone()]
        );
        assert_eq!(
            cache_non_empty_agy_candidates(&cache, Vec::new()),
            vec![candidate]
        );
    }

    #[test]
    fn agy_login_shell_discovery_waits_out_the_cooldown() {
        let last = std::sync::Mutex::new(None);
        let start = std::time::Instant::now();

        assert!(claim_agy_login_shell_discovery(&last, start));
        assert!(!claim_agy_login_shell_discovery(&last, start));
        assert!(!claim_agy_login_shell_discovery(
            &last,
            start + AGY_LOGIN_SHELL_COOLDOWN - std::time::Duration::from_secs(1),
        ));
        assert!(claim_agy_login_shell_discovery(
            &last,
            start + AGY_LOGIN_SHELL_COOLDOWN,
        ));
    }

    #[test]
    fn agy_fallback_only_runs_for_terminal_failures() {
        let transient = ProviderFetchFailure::transient(
            "temporary",
            None,
            SafeTransportDiagnostic::from_facts(TransportErrorFacts::synthetic(
                true,
                false,
                TransportPhase::Request,
                None,
            )),
        );
        assert!(!should_try_agy_fallback(&transient));
        assert!(should_try_agy_fallback(&ProviderFetchFailure::terminal(
            "terminal"
        )));
    }

    #[test]
    fn prefers_last_id_when_single_secret() {
        let ids = vec![
            "1-a.apps.googleusercontent.com".into(),
            "2-b.apps.googleusercontent.com".into(),
        ];
        let secrets = vec!["GOCSPX-only".into()];
        assert_eq!(
            preferred_client(&ids, &secrets).unwrap().0,
            "2-b.apps.googleusercontent.com"
        );
    }

    #[test]
    fn parses_local_user_status_quotas() {
        let now = Utc::now();
        let body = r#"{
            "userStatus": {
                "email": "me@gmail.com",
                "userTier": { "name": "Pro" },
                "cascadeModelConfigData": {
                    "clientModelConfigs": [
                        { "label": "Gemini 3 Pro", "modelOrAlias": {"model":"gemini-3-pro"},
                          "quotaInfo": { "remainingFraction": 0.42, "resetTime": "2026-06-09T00:00:00Z" } },
                        { "label": "No Quota", "modelOrAlias": {"model":"x"} }
                    ]
                }
            }
        }"#;
        let fetched = parse_user_status(body, now).unwrap();
        assert_eq!(fetched.source, "cli");
        assert_eq!(
            fetched.identity.as_ref().unwrap().email.as_deref(),
            Some("me@gmail.com")
        );
        assert_eq!(
            fetched.identity.as_ref().unwrap().plan.as_deref(),
            Some("Pro")
        );
        assert_eq!(fetched.windows.len(), 1);
        assert_eq!(fetched.windows[0].label_for_test(), "Gemini 3 Pro");
        assert!((fetched.windows[0].remaining_for_test() - 42.0).abs() < 0.01);
    }

    #[test]
    fn quota_summary_maps_like_agy_usage() {
        // The capture instant, not an arbitrary midnight: the unused 3p-weekly
        // bucket's reset rolls as Google's now plus seven days, so this body
        // was produced at 2026-10-09T07:58:14Z - 7 d. A declared window is a
        // contract only for a bucket in use whose cycle has started
        // (`agy_bucket_window`); at midnight the 5h cycle had not.
        let now = DateTime::parse_from_rfc3339("2026-10-02T07:58:14Z")
            .unwrap()
            .with_timezone(&Utc);
        // Shape measured from retrieveUserQuotaSummary on 2026-10-02.
        let body = json!({
            "description": "…",
            "groups": [
                { "displayName": "Gemini Models", "description": "…", "buckets": [
                    { "bucketId": "gemini-weekly", "displayName": "Weekly Limit Remaining",
                      "remainingFraction": 0.968718, "resetTime": "2026-10-08T18:46:28Z", "window": "weekly" },
                    { "bucketId": "gemini-5h", "displayName": "Five Hour Limit Remaining",
                      "remainingFraction": 0.8799, "resetTime": "2026-10-02T11:20:43Z", "window": "5h" }
                ]},
                { "displayName": "Claude and GPT models", "buckets": [
                    { "bucketId": "3p-weekly", "displayName": "Weekly Limit Remaining",
                      "remainingFraction": 1, "resetTime": "2026-10-09T07:58:14Z", "window": "weekly" },
                    { "displayName": "No id is skipped", "remainingFraction": 1 },
                    { "bucketId": "3p-5h", "displayName": "Five Hour Limit Remaining", "window": "5h" }
                ]}
            ]
        });
        let windows = windows_from_quota_summary(&body.to_string(), now);
        let rows: Vec<(&str, Option<&str>)> = windows
            .iter()
            .map(|w| (w.label_for_test(), w.pace_window_key_for_test()))
            .collect();
        assert_eq!(
            rows,
            [
                ("Gemini Models · Weekly Limit Remaining", Some("agy.gemini-weekly.v1")),
                ("Gemini Models · Five Hour Limit Remaining", Some("agy.gemini-5h.v1")),
                ("Claude and GPT models · Weekly Limit Remaining", Some("agy.3p-weekly.v1")),
            ]
        );
        assert!((windows[1].remaining_for_test() - 87.99).abs() < 0.01);
        // The declared window becomes the duration of a bucket in use, so its
        // card can draw a curve without first learning the length over
        // several resets. The unused 3p-weekly bucket (fraction 1, rolling
        // reset) gets none.
        let durations: Vec<Option<i64>> = windows.iter().map(|w| w.duration_seconds_for_test()).collect();
        assert_eq!(durations, [Some(7 * 86_400), Some(5 * 3_600), None]);
        assert!(windows_from_quota_summary("not json", now).is_empty());
        assert!(windows_from_quota_summary("{}", now).is_empty());
    }

    #[test]
    fn maps_available_models_and_quota_buckets() {
        let now = Utc::now();
        let models = json!({
            "models": {
                "gemini-3-pro": { "displayName": "Gemini 3 Pro", "quotaInfo": { "remainingFraction": 0.5 } }
            }
        });
        let w = models_from_available(&models.to_string(), now).unwrap();
        assert_eq!(w.len(), 1);

        let quota = json!({
            "buckets": [
                { "modelId": "claude", "remainingFraction": 0.8 },
                { "modelId": "claude", "remainingFraction": 0.3 }
            ]
        });
        let b = buckets_from_quota(&quota.to_string(), now).unwrap();
        assert_eq!(b.len(), 1);
        assert!((b[0].remaining_for_test() - 30.0).abs() < 0.01); // lowest kept
    }

    #[test]
    fn stage4_antigravity_identity_and_duplicate_rules_are_deterministic() {
        let now = DateTime::parse_from_rfc3339("2026-07-10T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let models = json!({
            "models": {
                "model-A": {
                    "displayName": "Trimmed loser",
                    "quotaInfo": { "remainingFraction": 0.5, "resetTime": "2026-07-12T00:00:00Z" }
                },
                "  model-A  ": {
                    "displayName": "Trimmed winner",
                    "quotaInfo": { "remainingFraction": 0.2, "resetTime": "2026-07-13T00:00:00Z" }
                },
                "model-B": {
                    "displayName": "Shared label",
                    "quotaInfo": { "remainingFraction": 0.4, "resetTime": "2026-07-12T00:00:00Z" }
                },
                "Model-Byte-Case": {
                    "displayName": "Shared label",
                    "quotaInfo": { "remainingFraction": 0.3, "resetTime": "2026-07-13T00:00:00Z" }
                }
            }
        });
        let windows = models_from_available(&models.to_string(), now).unwrap();
        assert_eq!(windows.len(), 3);
        let model_a = windows
            .iter()
            .find(|window| window.pace_window_key_for_test() == Some("model.model-A.v1"))
            .unwrap();
        assert_eq!(model_a.label_for_test(), "Trimmed winner");
        assert!((model_a.remaining_for_test() - 20.0).abs() < 0.01);
        assert_eq!(
            windows
                .iter()
                .filter(|window| window.label_for_test() == "Shared label")
                .count(),
            2,
            "display labels never merge distinct model IDs"
        );
        assert!(windows.iter().any(|window| {
            window.pace_window_key_for_test() == Some("model.Model-Byte-Case.v1")
        }));

        let cli = r#"{
            "userStatus": {
                "cascadeModelConfigData": {
                    "clientModelConfigs": [
                        {
                            "label": "CLI loser",
                            "modelOrAlias": { "model": " Model-X " },
                            "quotaInfo": { "remainingFraction": 0.6, "resetTime": "2026-07-12T00:00:00Z" }
                        },
                        {
                            "label": "CLI winner",
                            "modelOrAlias": { "model": "Model-X" },
                            "quotaInfo": { "remainingFraction": 0.2, "resetTime": "2026-07-13T00:00:00Z" }
                        },
                        { "label": "Config only", "quotaInfo": { "remainingFraction": 0.7 } }
                    ]
                }
            }
        }"#;
        let fetched = parse_user_status(cli, now).unwrap();
        assert_eq!(fetched.windows.len(), 2);
        assert_eq!(
            fetched.windows[0].pace_window_key_for_test(),
            Some("model.Model-X.v1")
        );
        assert_eq!(fetched.windows[0].label_for_test(), "CLI winner");
        let missing_wire = serde_json::to_value(&fetched.windows[1]).unwrap();
        assert_eq!(missing_wire["cardId"], "row.cli.config.2.v1");
        assert_eq!(missing_wire["paceStatus"]["reason"], "windowIdentity");

        let missing_remote = models_from_available(
            &json!({
                "models": {
                    "   ": {
                        "displayName": "Remote model",
                        "quotaInfo": { "remainingFraction": 0.7 }
                    }
                }
            })
            .to_string(),
            now,
        )
        .unwrap();
        let wire = serde_json::to_value(&missing_remote[0]).unwrap();
        assert_eq!(wire["cardId"], "row.models.0.v1");
        assert_eq!(wire["paceStatus"]["reason"], "windowIdentity");

        let missing_bucket = buckets_from_quota(
            &json!({
                "buckets": [
                    { "modelId": "   ", "remainingFraction": 0.7 }
                ]
            })
            .to_string(),
            now,
        )
        .unwrap();
        let wire = serde_json::to_value(&missing_bucket[0]).unwrap();
        assert_eq!(wire["cardId"], "row.quota.bucket.0.v1");
        assert_eq!(wire["paceStatus"]["reason"], "windowIdentity");

        let duplicate = json!({
            "buckets": [
                {
                    "modelId": "same-model",
                    "remainingFraction": 0.25,
                    "resetTime": "2026-07-09T00:00:00Z"
                },
                {
                    "modelId": "same-model",
                    "remainingFraction": 0.25,
                    "resetTime": "2026-07-12T00:00:00Z"
                },
                {
                    "modelId": "same-model",
                    "remainingFraction": 0.25,
                    "resetTime": "2026-07-11T00:00:00Z"
                }
            ]
        });
        let selected = buckets_from_quota(&duplicate.to_string(), now).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(
            selected[0].resets_at_for_test(),
            Some("2026-07-11T00:00:00.000Z"),
            "future reset beats past reset, then earliest future reset wins"
        );
        let same_reset = parse_datetime("2026-07-11T00:00:00Z");
        assert!(!binding_candidate_is_better(
            0.25, same_reset, 1, 0.25, same_reset, 0, now
        ));
    }

    #[test]
    fn stage4_antigravity_rejects_invalid_fractions_at_every_source() {
        assert!(!valid_remaining_fraction(f64::NAN));
        assert!(!valid_remaining_fraction(f64::INFINITY));
        assert!(!valid_remaining_fraction(-0.01));
        assert!(!valid_remaining_fraction(1.01));
        assert!(valid_remaining_fraction(0.0));
        assert!(valid_remaining_fraction(1.0));

        let now = DateTime::parse_from_rfc3339("2026-07-10T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let assert_rows = |windows: Vec<UsageWindow>| {
            assert_eq!(windows.len(), 1);
            assert_eq!(
                windows[0].pace_window_key_for_test(),
                Some("model.valid.v1")
            );
            for key in ["model.negative.v1", "model.over.v1"] {
                assert!(
                    windows
                        .iter()
                        .all(|window| window.pace_window_key_for_test() != Some(key)),
                    "invalid quota row must not be published: {key}"
                );
            }
        };

        let cli = r#"{
            "userStatus": {
                "cascadeModelConfigData": {
                    "clientModelConfigs": [
                        {
                            "modelOrAlias": { "model": "valid" },
                            "quotaInfo": { "remainingFraction": 0.5 }
                        },
                        {
                            "modelOrAlias": { "model": "negative" },
                            "quotaInfo": { "remainingFraction": -0.1 }
                        },
                        {
                            "modelOrAlias": { "model": "over" },
                            "quotaInfo": { "remainingFraction": 1.1 }
                        },
                        {
                            "modelOrAlias": { "model": "missing" },
                            "quotaInfo": {}
                        }
                    ]
                }
            }
        }"#;
        assert_rows(parse_user_status(cli, now).unwrap().windows);

        let assert_overflowing_duplicate = |windows: Vec<UsageWindow>| {
            assert_eq!(windows.len(), 1);
            assert_eq!(
                windows[0].pace_window_key_for_test(),
                Some("model.same-model.v1")
            );
            assert!((windows[0].remaining_for_test() - 50.0).abs() < 0.01);
        };
        let overflowing_cli = r#"{
            "userStatus": {
                "cascadeModelConfigData": {
                    "clientModelConfigs": [
                        {
                            "modelOrAlias": { "model": "same-model" },
                            "quotaInfo": { "remainingFraction": 1e400 }
                        },
                        {
                            "modelOrAlias": { "model": "same-model" },
                            "quotaInfo": { "remainingFraction": 0.5 }
                        }
                    ]
                }
            }
        }"#;
        assert_overflowing_duplicate(parse_user_status(overflowing_cli, now).unwrap().windows);

        assert_rows(
            models_from_available(
                &json!({
                    "models": {
                        "valid": { "quotaInfo": { "remainingFraction": 0.5 } },
                        "negative": { "quotaInfo": { "remainingFraction": -0.1 } },
                        "over": { "quotaInfo": { "remainingFraction": 1.1 } },
                        "missing": { "quotaInfo": {} }
                    }
                })
                .to_string(),
                now,
            )
            .unwrap(),
        );

        let overflowing_models = r#"{
            "models": {
                "same-model": {
                    "quotaInfo": { "remainingFraction": 1e400 }
                },
                " same-model ": {
                    "quotaInfo": { "remainingFraction": 0.5 }
                }
            }
        }"#;
        assert_overflowing_duplicate(models_from_available(overflowing_models, now).unwrap());

        assert_rows(
            buckets_from_quota(
                &json!({
                    "buckets": [
                        { "modelId": "valid", "remainingFraction": 0.5 },
                        { "modelId": "negative", "remainingFraction": -0.1 },
                        { "modelId": "over", "remainingFraction": 1.1 },
                        { "modelId": "missing" }
                    ]
                })
                .to_string(),
                now,
            )
            .unwrap(),
        );

        let overflowing_buckets = r#"{
            "buckets": [
                { "modelId": "same-model", "remainingFraction": 1e400 },
                { "modelId": "same-model", "remainingFraction": 0.5 }
            ]
        }"#;
        assert_overflowing_duplicate(buckets_from_quota(overflowing_buckets, now).unwrap());

        let malformed_cli_row = r#"{
            "userStatus": {
                "cascadeModelConfigData": {
                    "clientModelConfigs": [
                        {
                            "label": 1e400,
                            "modelOrAlias": { "model": "same-model" },
                            "quotaInfo": { "remainingFraction": 0.4 }
                        },
                        {
                            "modelOrAlias": { "model": "same-model" },
                            "quotaInfo": { "remainingFraction": 0.5 }
                        }
                    ]
                }
            }
        }"#;
        assert_overflowing_duplicate(parse_user_status(malformed_cli_row, now).unwrap().windows);

        let malformed_model_row = r#"{
            "models": {
                "same-model": {
                    "displayName": 1e400,
                    "quotaInfo": { "remainingFraction": 0.4 }
                },
                " same-model ": {
                    "quotaInfo": { "remainingFraction": 0.5 }
                }
            }
        }"#;
        assert_overflowing_duplicate(models_from_available(malformed_model_row, now).unwrap());

        let malformed_bucket_fields = r#"{
            "buckets": [
                { "modelId": 42, "remainingFraction": 0.4 },
                { "modelId": "valid", "remainingFraction": 0.5 }
            ]
        }"#;
        let malformed_bucket_windows = buckets_from_quota(malformed_bucket_fields, now).unwrap();
        assert_eq!(malformed_bucket_windows.len(), 2);
        assert!(malformed_bucket_windows
            .iter()
            .any(|window| window.pace_window_key_for_test() == Some("model.valid.v1")));
        let malformed_bucket_wire = serde_json::to_value(&malformed_bucket_windows[0]).unwrap();
        assert_eq!(
            malformed_bucket_wire["paceStatus"]["reason"],
            "windowIdentity"
        );

        assert!(quota_window(
            "Non-finite".to_string(),
            f64::NAN,
            None,
            now,
            "model.non-finite.v1".to_string(),
            Some("model.non-finite.v1".to_string()),
        )
        .is_none());
        assert!(!binding_candidate_is_better(
            -0.1, None, 1, 0.5, None, 0, now
        ));
    }

    #[test]
    fn remote_scope_and_presentation_ignore_unbound_active_email() {
        let stale_active_email = "stale-other-account@example.com";
        let credentials = json!({
            "access_token": "short-lived-access",
            "refresh_token": "bound-google-refresh"
        });
        assert_eq!(
            remote_refresh_marker(&credentials),
            Some(b"bound-google-refresh".as_slice())
        );
        assert_ne!(
            remote_refresh_marker(&credentials),
            Some(stale_active_email.as_bytes())
        );
        let identity = remote_identity(Some("Paid".to_string()));
        assert_eq!(identity.email, None);
        assert_eq!(identity.plan.as_deref(), Some("Paid"));

        let access_only = json!({ "access_token": "access-is-not-the-frozen-marker" });
        assert_eq!(remote_refresh_marker(&access_only), None);
    }

    #[test]
    fn local_route_fails_closed_without_authenticated_email() {
        let fetched = parse_user_status(
            r#"{"userStatus":{"cascadeModelConfigData":{"clientModelConfigs":[]}}}"#,
            Utc::now(),
        )
        .unwrap();
        assert_eq!(
            fetched.account_scope,
            Err(AccountScopeError::NoTrustedEvidence)
        );
        assert_eq!(
            fetched.history_scope,
            Err(AccountScopeError::NoTrustedEvidence)
        );
    }

    /// A2b: the local IDE route resolves authoritatively today by the
    /// authenticated `GetUserStatus` email. Its history scope must consume the
    /// same email, or two accounts on one installation would share one series.
    #[test]
    fn histid_a_local_history_scope_consumes_the_authenticated_email() {
        let scope = TestRefreshScope::new("antigravity", "histid-antigravity");
        let resolve = |provider: &str, authoritative: Option<(AuthoritativeIdKind, &str)>| {
            scope.resolve_history(provider, authoritative)
        };
        let identity = |email: Option<&str>| AgentIdentity {
            email: email.map(str::to_string),
            plan: None,
        };

        let one =
            resolve_local_history_scope_with(Some(&identity(Some("one@example.invalid"))), resolve)
                .expect("authoritative history scope");
        let expected = scope
            .resolve_authoritative(
                "antigravity",
                AuthoritativeIdKind::Email,
                "one@example.invalid",
            )
            .unwrap();
        assert_eq!(one.as_str(), expected.as_str());

        let two =
            resolve_local_history_scope_with(Some(&identity(Some("two@example.invalid"))), resolve)
                .expect("second authoritative history scope");
        assert_ne!(
            crate::agent_quota_history::SeriesKey::new("antigravity", &one, "model.v1"),
            crate::agent_quota_history::SeriesKey::new("antigravity", &two, "model.v1")
        );

        let constant = scope.resolve_history("antigravity", None).unwrap();
        for absent in [None, Some(identity(None)), Some(identity(Some("  ")))] {
            let fallback = resolve_local_history_scope_with(absent.as_ref(), resolve)
                .expect("email-less local route must fall back to the constant, not error");
            assert_eq!(fallback.as_str(), constant.as_str());
            assert_ne!(fallback.as_str(), one.as_str());
            assert_ne!(fallback.as_str(), two.as_str());
        }
        scope.cleanup();
    }

    #[test]
    fn resolves_remote_plan_from_tier() {
        assert_eq!(
            resolve_remote_plan(&json!({"currentTier":{"id":"free-tier"}})).as_deref(),
            Some("Free")
        );
        assert_eq!(
            resolve_remote_plan(&json!({"planInfo":{"planType":"standard"}})).as_deref(),
            Some("Standard")
        );
        assert_eq!(
            resolve_remote_plan(&json!({
                "currentTier": {"id": "free-tier"},
                "paidTier": {"id": "g1-pro-tier", "name": "Google AI Pro"}
            }))
            .as_deref(),
            Some("Google AI Pro")
        );
    }

    /// The post-fetch marker reader for a poll with no bound account: plan E
    /// must not read agy's marker then.
    pub(super) async fn unread_marker() -> Option<String> {
        panic!("no bound account: the marker is not read")
    }

    fn orchestration_fetched(source: &str) -> Fetched {
        Fetched {
            agy_login_marker: None,
            bound_account_key: None,
            source: source.to_string(),
            identity: None,
            account_scope: Err(AccountScopeError::NoTrustedEvidence),
            history_scope: Err(AccountScopeError::NoTrustedEvidence),
            cache_binding: None,
            windows: Vec::new(),
        }
    }

    /// A credential that exists but cannot be parsed belongs to a configured
    /// account, so it must NOT reach the absence marker. Before the split it
    /// did: every `load_remote_credentials` failure became
    /// `ANTIGRAVITY_UNCONFIGURED_ERROR`, so a corrupt `oauth_creds.json` took
    /// the card out of tab navigation and claimed the user had never logged in.
    #[test]
    fn malformed_remote_credentials_are_unreadable_not_absent() {
        let dir = std::env::temp_dir().join("tokenbar-antigravity-malformed-probe");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("oauth_creds.json");
        std::fs::write(&path, b"{ this is not json").unwrap();

        assert_eq!(
            load_remote_credentials(&path),
            Err(RemoteCredentialError::Unreadable)
        );
        let failure = remote_credentials_or_unconfigured(&path).unwrap_err();
        assert!(
            matches!(
                failure,
                ProviderFetchFailure::Terminal { ref display }
                    if display == ANTIGRAVITY_UNREADABLE_ERROR
            ),
            "a corrupt credential must not read as an absent one, got {failure:?}"
        );
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            load_remote_credentials(&path),
            Err(RemoteCredentialError::Absent),
            "control: the same path with the file gone IS absent, so the case above \
             is the parse and not the path"
        );
    }

    /// The reported machine's state, which this one cannot enter: no running
    /// IDE, no Google credential, and an `agy` that cannot answer either. The
    /// verdict has to be the ORIGINAL marker, because that is the string
    /// `required_card_source` keys the `unconfigured` source on — if the CLI's
    /// own "not found" replaced it, the card would report `oauth`, stop being a
    /// setup placeholder, and the tab #345 removes would come back.
    #[tokio::test]
    async fn nothing_configured_survives_the_cli_route_as_the_unconfigured_marker() {
        let agy_runs = std::cell::Cell::new(0);
        let failure = with_agy_fallback(
            Err(ProviderFetchFailure::terminal(
                ANTIGRAVITY_UNCONFIGURED_ERROR,
            )),
            None,
            unread_marker,
            || async {
                agy_runs.set(agy_runs.get() + 1);
                Err(ProviderFetchFailure::terminal(
                    "Antigravity CLI was not found.",
                ))
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            agy_runs.get(),
            1,
            "the CLI route is still tried before the verdict is taken"
        );
        assert!(
            matches!(
                failure,
                ProviderFetchFailure::Terminal { ref display }
                    if display == ANTIGRAVITY_UNCONFIGURED_ERROR
            ),
            "the CLI's failure must not replace the marker, got {failure:?}"
        );

        // Control: this machine's state. A CLI that answers makes the card
        // configured, so the tab stays — the assertion above is about absence,
        // not about the fallback being dead.
        let fetched = with_agy_fallback(
            Err(ProviderFetchFailure::terminal(
                ANTIGRAVITY_UNCONFIGURED_ERROR,
            )),
            None,
            unread_marker,
            || async { Ok(orchestration_fetched("cli")) },
        )
        .await
        .unwrap();
        assert_eq!(fetched.source, "cli");
    }

    fn orchestration_transient(display: &str) -> ProviderFetchFailure {
        ProviderFetchFailure::transient(
            display,
            None,
            crate::agent_usage::SafeTransportDiagnostic::from_facts(
                TransportErrorFacts::synthetic(true, false, TransportPhase::Request, None),
            ),
        )
    }

    #[tokio::test]
    async fn orchestration_local_success_and_primary_terminal_do_not_call_later_routes() {
        let primary_calls = std::cell::Cell::new(0);
        let secondary_calls = std::cell::Cell::new(0);
        let local = fetch_with(
            || async { LocalAttempt::Success(orchestration_fetched("cli")) },
            || async {
                primary_calls.set(primary_calls.get() + 1);
                PrimaryAttempt::<()>::FinalFailure(ProviderFetchFailure::terminal("unexpected"))
            },
            |_: ()| async {
                secondary_calls.set(secondary_calls.get() + 1);
                Ok(orchestration_fetched("secondary"))
            },
        )
        .await
        .unwrap();
        assert_eq!(local.source, "cli");
        assert_eq!(primary_calls.get(), 0);
        assert_eq!(secondary_calls.get(), 0);

        let failure = fetch_with(
            || async { LocalAttempt::RouteMiss },
            || async {
                PrimaryAttempt::<()>::FinalFailure(ProviderFetchFailure::terminal("primary 401"))
            },
            |_: ()| async {
                secondary_calls.set(secondary_calls.get() + 1);
                Ok(orchestration_fetched("secondary"))
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(
            failure,
            ProviderFetchFailure::Terminal { ref display } if display == "primary 401"
        ));
        assert_eq!(secondary_calls.get(), 0);
    }

    #[tokio::test]
    async fn orchestration_primary_forbidden_uses_secondary_result() {
        let success = fetch_with(
            || async { LocalAttempt::RouteMiss },
            || async { PrimaryAttempt::Forbidden(()) },
            |()| async { Ok(orchestration_fetched("secondary")) },
        )
        .await
        .unwrap();
        assert_eq!(success.source, "secondary");

        let transient = fetch_with(
            || async { LocalAttempt::RouteMiss },
            || async { PrimaryAttempt::Forbidden(()) },
            |()| async { Err(orchestration_transient("secondary transient")) },
        )
        .await
        .unwrap_err();
        assert!(matches!(
            transient,
            ProviderFetchFailure::Transient { ref display, .. }
                if display == "secondary transient"
        ));

        let terminal = fetch_with(
            || async { LocalAttempt::RouteMiss },
            || async { PrimaryAttempt::Forbidden(()) },
            |()| async { Err(ProviderFetchFailure::terminal("secondary terminal")) },
        )
        .await
        .unwrap_err();
        assert!(matches!(
            terminal,
            ProviderFetchFailure::Terminal { ref display }
                if display == "secondary terminal"
        ));
    }

    #[tokio::test]
    async fn orchestration_schema_and_transient_precedence_is_fail_closed() {
        let schema_recovered = fetch_with(
            || async { LocalAttempt::RouteMiss },
            || async {
                PrimaryAttempt::SchemaContradiction {
                    context: (),
                    failure: ProviderFetchFailure::terminal("primary schema"),
                }
            },
            |()| async { Ok(orchestration_fetched("secondary")) },
        )
        .await
        .unwrap();
        assert_eq!(schema_recovered.source, "secondary");

        let schema_stays_terminal = fetch_with(
            || async { LocalAttempt::RouteMiss },
            || async {
                PrimaryAttempt::SchemaContradiction {
                    context: (),
                    failure: ProviderFetchFailure::terminal("primary schema"),
                }
            },
            |()| async { Err(orchestration_transient("secondary transient")) },
        )
        .await
        .unwrap_err();
        assert!(matches!(
            schema_stays_terminal,
            ProviderFetchFailure::Terminal { ref display } if display == "primary schema"
        ));

        let transient_recovered = fetch_with(
            || async { LocalAttempt::RouteMiss },
            || async {
                PrimaryAttempt::Transient {
                    context: (),
                    failure: orchestration_transient("primary transient"),
                }
            },
            |()| async { Ok(orchestration_fetched("secondary")) },
        )
        .await
        .unwrap();
        assert_eq!(transient_recovered.source, "secondary");

        let transient_then_terminal = fetch_with(
            || async { LocalAttempt::RouteMiss },
            || async {
                PrimaryAttempt::Transient {
                    context: (),
                    failure: orchestration_transient("primary transient"),
                }
            },
            |()| async { Err(ProviderFetchFailure::terminal("secondary terminal")) },
        )
        .await
        .unwrap_err();
        assert!(matches!(
            transient_then_terminal,
            ProviderFetchFailure::Transient { ref display, .. }
                if display == "primary transient"
        ));

        let both_transient = fetch_with(
            || async { LocalAttempt::RouteMiss },
            || async {
                PrimaryAttempt::Transient {
                    context: (),
                    failure: orchestration_transient("primary transient"),
                }
            },
            |()| async { Err(orchestration_transient("secondary transient")) },
        )
        .await
        .unwrap_err();
        assert!(matches!(
            both_transient,
            ProviderFetchFailure::Transient { ref display, .. }
                if display == "secondary transient"
        ));
    }

    fn checkpoint_at(
        target: Option<RefreshCheckpoint>,
    ) -> impl FnMut(RefreshCheckpoint) -> Result<(), ProviderFetchFailure> {
        move |checkpoint| {
            if Some(checkpoint) == target {
                Err(ProviderFetchFailure::terminal("injected crash"))
            } else {
                Ok(())
            }
        }
    }

    async fn test_refresh_response(
        refresh_token: String,
        _attempt_binding: ProviderCacheBinding,
    ) -> Result<Value, ProviderFetchFailure> {
        assert_eq!(refresh_token, "antigravity-old-refresh");
        Ok(json!({
            "access_token": "antigravity-new-access",
            "refresh_token": "antigravity-new-refresh",
            "expires_in": 3600
        }))
    }

    fn setup_refresh(tag: &str) -> (TestRefreshScope, PathBuf, AccountScope, Vec<u8>, String) {
        let scope = TestRefreshScope::new("antigravity", tag);
        let path = scope.root().join("antigravity/oauth_creds.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&json!({
                "access_token": "antigravity-old-access",
                "refresh_token": "antigravity-old-refresh",
                "expiry_date": 0
            }))
            .unwrap(),
        )
        .unwrap();
        let location = remote_scope_location(&path).unwrap();
        let old_scope = scope
            .resolve_current("google-oauth-creds", &location, b"antigravity-old-refresh")
            .unwrap();
        let metadata = scope.metadata_bytes();
        (scope, path, old_scope, metadata, location)
    }

    async fn run_refresh(
        scope: &TestRefreshScope,
        path: &Path,
        crash: Option<RefreshCheckpoint>,
    ) -> Result<(Value, String, AccountScope, Option<ProviderCacheBinding>), ProviderFetchFailure>
    {
        let now = DateTime::parse_from_rfc3339("2026-07-17T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        refresh_access_token_with(
            path,
            now,
            scope,
            test_refresh_response,
            |creds| write_creds_atomic(path, creds),
            checkpoint_at(crash),
        )
        .await
    }

    fn stored_refresh_token(path: &Path) -> String {
        let credentials = load_remote_credentials(path).unwrap();
        std::str::from_utf8(remote_refresh_marker(&credentials).unwrap())
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn refresh_rejects_concurrent_account_switch_without_touching_b() {
        const B_BYTES: &[u8] = br#"{
  "access_token": "account-b-access",
  "refresh_token": "account-b-refresh",
  "id_token": "account-b-id",
  "expiry_date": 4102444800000,
  "sibling": {"writer": "b", "revision": 2}
}
"#;
        let (scope, path, _, before, _) = setup_refresh("antigravity-target-switch");
        let request_path = path.clone();
        let now = DateTime::parse_from_rfc3339("2026-07-17T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let failure = refresh_access_token_with(
            &path,
            now,
            &scope,
            move |refresh_token, _attempt_binding| async move {
                assert_eq!(refresh_token, "antigravity-old-refresh");
                std::fs::write(&request_path, B_BYTES).unwrap();
                Ok(json!({
                    "access_token": "antigravity-new-access",
                    "refresh_token": "antigravity-new-refresh"
                }))
            },
            |_| -> std::io::Result<()> {
                panic!("target mismatch must not reach credential persistence")
            },
            checkpoint_at(None),
        )
        .await
        .unwrap_err();

        assert!(matches!(failure, ProviderFetchFailure::Terminal { .. }));
        let stored_bytes = std::fs::read(&path).unwrap();
        assert_eq!(stored_bytes, B_BYTES);
        assert!(!String::from_utf8_lossy(&stored_bytes).contains("antigravity-new"));
        let stored = load_remote_credentials(&path).unwrap();
        assert_eq!(stored["access_token"], "account-b-access");
        assert_eq!(stored["refresh_token"], "account-b-refresh");
        assert_eq!(stored["id_token"], "account-b-id");
        assert_eq!(stored["sibling"]["writer"], "b");
        assert_eq!(scope.metadata_bytes(), before);
        scope.cleanup();
    }

    #[tokio::test]
    async fn refresh_patches_unchanged_target_and_preserves_current_root_siblings() {
        let (scope, path, old_scope, _, location) = setup_refresh("antigravity-target-unchanged");
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&json!({
                "access_token": "antigravity-old-access",
                "refresh_token": "antigravity-old-refresh",
                "id_token": "antigravity-old-id",
                "expiry_date": 0,
                "stale_only": "must-not-return"
            }))
            .unwrap(),
        )
        .unwrap();
        let current = json!({
            "access_token": "concurrent-access",
            "refresh_token": "antigravity-old-refresh",
            "id_token": "concurrent-id",
            "expiry_date": 0,
            "token_type": "current-writer",
            "sibling": {"writer": "antigravity-cli", "revision": 2},
            "unrelated": [1, 2, 3]
        });
        let current_bytes = serde_json::to_vec_pretty(&current).unwrap();
        let request_path = path.clone();
        let now = DateTime::parse_from_rfc3339("2026-07-17T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let (refreshed, access_token, scope_outcome, cache_binding) = refresh_access_token_with(
            &path,
            now,
            &scope,
            move |refresh_token, _attempt_binding| async move {
                assert_eq!(refresh_token, "antigravity-old-refresh");
                std::fs::write(&request_path, current_bytes).unwrap();
                Ok(json!({
                    "access_token": "antigravity-new-access",
                    "refresh_token": "antigravity-new-refresh",
                    "id_token": "antigravity-new-id",
                    "expires_in": 3600,
                    "token_type": "provider-response"
                }))
            },
            |credentials| write_creds_atomic(&path, credentials),
            checkpoint_at(None),
        )
        .await
        .unwrap();

        assert_eq!(access_token, "antigravity-new-access");
        assert_eq!(scope_outcome, old_scope);
        assert_eq!(
            cache_binding,
            Some(ProviderCacheBinding::primary(old_scope.clone()))
        );
        let stored = load_remote_credentials(&path).unwrap();
        assert_eq!(refreshed, stored);
        assert_eq!(stored["access_token"], "antigravity-new-access");
        assert_eq!(stored["refresh_token"], "antigravity-new-refresh");
        assert_eq!(stored["id_token"], "antigravity-new-id");
        assert_eq!(
            stored["expiry_date"].as_f64(),
            Some(now.timestamp_millis() as f64 + 3_600_000.0)
        );
        assert_eq!(stored["token_type"], "current-writer");
        assert_eq!(stored["sibling"]["writer"], "antigravity-cli");
        assert_eq!(stored["sibling"]["revision"], 2);
        assert_eq!(stored["unrelated"], json!([1, 2, 3]));
        assert!(stored.get("stale_only").is_none());
        assert_eq!(
            scope
                .resolve_current("google-oauth-creds", &location, b"antigravity-new-refresh",)
                .unwrap(),
            old_scope
        );
        scope.cleanup();
    }

    #[tokio::test]
    async fn refresh_rejects_concurrent_logout_removal_and_malformed_root_without_restoring_a() {
        const LOGGED_OUT_BYTES: &[u8] = br#"{
  "sibling": {"writer": "logout", "revision": 2}
}
"#;
        const MALFORMED_BYTES: &[u8] = b"{not-json";
        let cases: [(&str, Option<&[u8]>); 3] = [
            ("logout", Some(LOGGED_OUT_BYTES)),
            ("removed", None),
            ("malformed", Some(MALFORMED_BYTES)),
        ];

        for (case, current_bytes) in cases {
            let (scope, path, _, before, _) = setup_refresh(&format!("antigravity-target-{case}"));
            let request_path = path.clone();
            let now = DateTime::parse_from_rfc3339("2026-07-17T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc);

            let failure = refresh_access_token_with(
                &path,
                now,
                &scope,
                move |refresh_token, _attempt_binding| async move {
                    assert_eq!(refresh_token, "antigravity-old-refresh");
                    if let Some(bytes) = current_bytes {
                        std::fs::write(&request_path, bytes).unwrap();
                    } else {
                        std::fs::remove_file(&request_path).unwrap();
                    }
                    Ok(json!({
                        "access_token": "antigravity-new-access",
                        "refresh_token": "antigravity-new-refresh"
                    }))
                },
                |_| -> std::io::Result<()> {
                    panic!("missing or malformed target must not reach credential persistence")
                },
                checkpoint_at(None),
            )
            .await
            .unwrap_err();

            assert!(matches!(failure, ProviderFetchFailure::Terminal { .. }));
            assert_eq!(scope.metadata_bytes(), before);
            if let Some(expected) = current_bytes {
                let stored = std::fs::read(&path).unwrap();
                assert_eq!(stored, expected);
                assert!(!String::from_utf8_lossy(&stored).contains("antigravity-new"));
            } else {
                assert!(!path.exists());
            }
            scope.cleanup();
        }
    }

    #[tokio::test]
    async fn refresh_crash_boundaries_and_scope_gate_use_production_sequence() {
        for boundary in [
            RefreshCheckpoint::Reloaded,
            RefreshCheckpoint::NetworkReturned,
            RefreshCheckpoint::MetadataHandled,
            RefreshCheckpoint::CredentialsPersisted,
        ] {
            let (scope, path, old_scope, before, location) = setup_refresh("antigravity-crash");
            let failure = run_refresh(&scope, &path, Some(boundary))
                .await
                .unwrap_err();
            assert!(matches!(
                failure,
                ProviderFetchFailure::Terminal { ref display } if display == "injected crash"
            ));
            assert_eq!(
                stored_refresh_token(&path),
                if boundary == RefreshCheckpoint::CredentialsPersisted {
                    "antigravity-new-refresh"
                } else {
                    "antigravity-old-refresh"
                }
            );
            if matches!(
                boundary,
                RefreshCheckpoint::Reloaded | RefreshCheckpoint::NetworkReturned
            ) {
                assert_eq!(scope.metadata_bytes(), before);
            } else {
                assert_ne!(scope.metadata_bytes(), before);
                assert_eq!(
                    scope
                        .resolve_current(
                            "google-oauth-creds",
                            &location,
                            b"antigravity-old-refresh",
                        )
                        .unwrap(),
                    old_scope
                );
                assert_eq!(
                    scope
                        .resolve_current(
                            "google-oauth-creds",
                            &location,
                            b"antigravity-new-refresh",
                        )
                        .unwrap(),
                    old_scope
                );
            }
            scope.cleanup();
        }

        let (scope, path, old_scope, before, location) = setup_refresh("antigravity-metadata-fail");
        scope.fail_metadata_save();
        let failure = run_refresh(&scope, &path, None).await.unwrap_err();
        assert!(matches!(failure, ProviderFetchFailure::Terminal { .. }));
        assert_eq!(scope.metadata_bytes(), before);
        assert_eq!(stored_refresh_token(&path), "antigravity-old-refresh");
        assert_eq!(
            scope
                .resolve_current("google-oauth-creds", &location, b"antigravity-old-refresh")
                .unwrap(),
            old_scope
        );
        scope.cleanup();

        let (scope, path, old_scope, _, location) = setup_refresh("antigravity-save-fail");
        let now = DateTime::parse_from_rfc3339("2026-07-17T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let (refreshed, access_token, scope_outcome, cache_binding) = refresh_access_token_with(
            &path,
            now,
            &scope,
            test_refresh_response,
            |_| Err(std::io::Error::other("injected save failure")),
            checkpoint_at(None),
        )
        .await
        .unwrap();
        assert_eq!(access_token, "antigravity-new-access");
        assert_eq!(remote_access_token(&refreshed).unwrap(), access_token);
        assert_eq!(scope_outcome, old_scope);
        assert_eq!(cache_binding, None);
        assert_eq!(stored_refresh_token(&path), "antigravity-old-refresh");
        assert_eq!(
            scope
                .resolve_current("google-oauth-creds", &location, b"antigravity-new-refresh")
                .unwrap(),
            old_scope
        );
        scope.cleanup();

        let (scope, path, old_scope, _, location) = setup_refresh("antigravity-success");
        let (_, _, scope_outcome, cache_binding) = run_refresh(&scope, &path, None).await.unwrap();
        assert_eq!(scope_outcome, old_scope);
        assert_eq!(
            cache_binding,
            Some(ProviderCacheBinding::primary(old_scope.clone()))
        );
        assert_eq!(
            scope
                .resolve_current("google-oauth-creds", &location, b"antigravity-new-refresh")
                .unwrap(),
            old_scope
        );
        scope.cleanup();
    }

    #[tokio::test]
    async fn refresh_transient_uses_lock_reloaded_binding_not_outer_binding() {
        let (scope, path, inner_scope, _, location) = setup_refresh("antigravity-lock-binding");
        let outer_scope = scope
            .resolve_current("google-oauth-creds", &location, b"outer-refresh-a")
            .unwrap();
        assert_ne!(outer_scope, inner_scope);
        let expected = ProviderCacheBinding::primary(inner_scope);
        let request_expected = expected.clone();
        let now = DateTime::parse_from_rfc3339("2026-07-17T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let failure = refresh_access_token_with(
            &path,
            now,
            &scope,
            move |refresh_token, attempt_binding| async move {
                assert_eq!(refresh_token, "antigravity-old-refresh");
                assert_eq!(attempt_binding, request_expected);
                Err(ProviderFetchFailure::transient(
                    "Antigravity token refresh failed. Retrying automatically.",
                    Some(attempt_binding),
                    crate::agent_usage::SafeTransportDiagnostic::from_facts(
                        TransportErrorFacts::synthetic(true, false, TransportPhase::Request, None),
                    ),
                ))
            },
            |credentials| write_creds_atomic(&path, credentials),
            checkpoint_at(None),
        )
        .await
        .unwrap_err();

        match failure {
            ProviderFetchFailure::Transient {
                attempt_binding, ..
            } => assert_eq!(attempt_binding, Some(expected)),
            ProviderFetchFailure::Terminal { .. } => panic!("timeout must remain transient"),
        }
        scope.cleanup();
    }

    /// #329: a quota poll must not be able to spawn the CLI when the CLI's own
    /// token endpoint cannot be resolved, because `agy --print` answers an
    /// unresolvable endpoint by opening a browser tab for interactive OAuth.
    ///
    /// The runner is a counter rather than a real invocation, so this asserts
    /// the child is never spawned instead of inferring it from the error text —
    /// an error message can be produced by a path that also ran the CLI.
    #[tokio::test]
    async fn an_unresolvable_token_endpoint_does_not_spawn_the_cli() {
        let now = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let runs = std::cell::Cell::new(0);
        let latch = std::sync::Mutex::new(AgyLatch::Idle);

        let blocked = fetch_agy_cli_gated(now, false, marker("m1"), &latch, |now| {
            runs.set(runs.get() + 1);
            async move { Ok(unreachable_probe_fetched(now)) }
        })
        .await;
        assert_eq!(runs.get(), 0, "an unreachable endpoint must not spawn agy");
        assert!(matches!(blocked, Err(ProviderFetchFailure::Terminal { .. })));

        // Control. Without it, `runs == 0` above would also hold if the gate
        // rejected every call for an unrelated reason, or if the runner were
        // never wired in at all.
        let allowed = fetch_agy_cli_gated(now, true, marker("m1"), &latch, |now| {
            runs.set(runs.get() + 1);
            async move { Ok(unreachable_probe_fetched(now)) }
        })
        .await;
        assert_eq!(runs.get(), 1, "a resolvable endpoint must still spawn agy");
        assert_eq!(
            allowed.unwrap().agy_login_marker.as_deref(),
            Some("m1"),
            "an agy card carries the login marker it was fetched under"
        );
    }

    fn marker(value: &str) -> Option<String> {
        Some(value.to_string())
    }

    /// agy `/usage` with a weekly bucket in use, an unused 5h bucket whose
    /// reset is Google's now (13:30:15Z) plus five hours, and a 5h bucket in
    /// use with a fixed reset.
    const AGY_ROLLING_5H_USAGE: &[u8] = br#"{"status":"SUCCESS","command":{"name":"usage","data":{"groups":[
        {"name":"Gemini Models","buckets":[
          {"id":"gemini-weekly","name":"Weekly Limit Remaining","window":"weekly","remaining_fraction":0.8,"reset_time":"2026-10-08T18:46:28Z"},
          {"id":"gemini-5h","name":"Five Hour Limit Remaining","window":"5h","remaining_fraction":1,"reset_time":"2026-10-04T18:30:15Z"}
        ]},
        {"name":"Claude and GPT models","buckets":[
          {"id":"3p-5h","name":"Five Hour Limit Remaining","window":"5h","remaining_fraction":0.6,"reset_time":"2026-10-04T16:00:00Z"}
        ]}]}}}"#;

    fn agy_now() -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap()
    }

    /// Runs the gate once with a counting runner that answers `outcome`.
    async fn run_gate(
        latch: &std::sync::Mutex<AgyLatch>,
        marker: Option<String>,
        runs: &std::cell::Cell<usize>,
        outcome: Result<(), bool>,
    ) -> Result<Fetched, ProviderFetchFailure> {
        fetch_agy_cli_gated(agy_now(), true, marker, latch, |now| {
            runs.set(runs.get() + 1);
            async move {
                match outcome {
                    Ok(()) => Ok(unreachable_probe_fetched(now)),
                    Err(spawned) => Err(AgyRunError {
                        failure: ProviderFetchFailure::terminal("Antigravity CLI usage failed."),
                        spawned,
                    }),
                }
            }
        })
        .await
    }

    fn latch_state(latch: &std::sync::Mutex<AgyLatch>) -> AgyLatch {
        latch.lock().unwrap().clone()
    }

    /// Signed out, `agy --print /usage` opens a browser for OAuth (measured on
    /// macOS). No Keychain login item must mean no spawn and no latch write.
    #[tokio::test]
    async fn a_missing_login_marker_does_not_spawn_the_cli() {
        let latch = std::sync::Mutex::new(AgyLatch::Idle);
        let runs = std::cell::Cell::new(0);

        let blocked = run_gate(&latch, None, &runs, Ok(())).await;
        assert_eq!(runs.get(), 0, "a signed-out agy must not be spawned");
        assert!(matches!(blocked, Err(ProviderFetchFailure::Terminal { .. })));
        assert_eq!(latch_state(&latch), AgyLatch::Idle);

        // Control: the same gate with a marker does run.
        assert!(run_gate(&latch, marker("m1"), &runs, Ok(())).await.is_ok());
        assert_eq!(runs.get(), 1);
        assert_eq!(latch_state(&latch), AgyLatch::Idle);
    }

    #[tokio::test]
    async fn a_spawned_failure_latches_until_the_marker_changes() {
        let latch = std::sync::Mutex::new(AgyLatch::Idle);
        let runs = std::cell::Cell::new(0);

        assert!(run_gate(&latch, marker("m1"), &runs, Err(true))
            .await
            .is_err());
        assert_eq!(runs.get(), 1);
        assert_eq!(latch_state(&latch), AgyLatch::Failed { marker: "m1".to_string(), retry_at: None });

        let paused = run_gate(&latch, marker("m1"), &runs, Ok(())).await;
        assert_eq!(
            runs.get(),
            1,
            "the same login must not respawn a failed agy"
        );
        assert!(matches!(paused, Err(ProviderFetchFailure::Terminal { .. })));
        assert_eq!(latch_state(&latch), AgyLatch::Failed { marker: "m1".to_string(), retry_at: None });

        // A rewritten Keychain item (re-login) re-arms the route.
        assert!(run_gate(&latch, marker("m2"), &runs, Ok(())).await.is_ok());
        assert_eq!(runs.get(), 2, "a new login must spawn agy again");
        assert_eq!(latch_state(&latch), AgyLatch::Idle);
    }

    /// A timed-out run is transient: it re-arms after the cooldown for the same
    /// login, while another failure of a started run stays latched (it may be
    /// agy asking for re-authentication, which can open a browser).
    #[tokio::test]
    async fn a_timed_out_run_retries_after_the_cooldown() {
        let latch = std::sync::Mutex::new(AgyLatch::Idle);
        let runs = std::cell::Cell::new(0);
        let t0 = agy_now();
        let (latch, runs) = (&latch, &runs);
        let gate = |at: DateTime<Utc>, display: Option<&'static str>| {
            fetch_agy_cli_gated(at, true, marker("m1"), latch, move |now| {
                runs.set(runs.get() + 1);
                async move {
                    match display {
                        None => Ok(unreachable_probe_fetched(now)),
                        Some(text) => Err(AgyRunError {
                            failure: ProviderFetchFailure::terminal(text),
                            spawned: true,
                        }),
                    }
                }
            })
        };
        assert!(gate(t0, Some(AGY_TIMED_OUT)).await.is_err());
        assert_eq!(runs.get(), 1);
        let early = gate(t0 + chrono::Duration::seconds(AGY_RETRY_SECS - 1), None).await;
        assert_eq!(runs.get(), 1, "inside the cooldown the same login is not respawned");
        assert!(matches!(early, Err(ProviderFetchFailure::Terminal { display }) if display == AGY_TIMED_OUT_RETRYING));
        assert!(gate(t0 + chrono::Duration::seconds(AGY_RETRY_SECS), None).await.is_ok());
        assert_eq!(runs.get(), 2, "after the cooldown the timed-out route runs again");
        assert_eq!(latch_state(latch), AgyLatch::Idle);

        // Control: a non-timeout failure stays latched past the cooldown.
        assert!(gate(t0, Some("Antigravity CLI usage failed.")).await.is_err());
        assert!(gate(t0 + chrono::Duration::hours(6), None).await.is_err());
        assert_eq!(runs.get(), 3, "a non-timeout failure waits for a new login");
    }

    /// With no other route configured, the agy failure is what the card shows;
    /// agy missing or signed out keeps the primary's "not logged in".
    #[tokio::test]
    async fn the_agy_failure_replaces_not_logged_in_when_agy_actually_ran() {
        let unconfigured = || Err(ProviderFetchFailure::terminal(ANTIGRAVITY_UNCONFIGURED_ERROR));
        let shown = |r: Result<Fetched, ProviderFetchFailure>| match r {
            Err(ProviderFetchFailure::Terminal { display }) => display,
            _ => String::new(),
        };
        let timed = with_agy_fallback(unconfigured(), None, unread_marker, || async {
            Err(ProviderFetchFailure::terminal(AGY_TIMED_OUT_RETRYING))
        })
        .await;
        assert_eq!(shown(timed), AGY_TIMED_OUT_RETRYING);
        for quiet in [AGY_NOT_FOUND, AGY_NOT_SIGNED_IN] {
            let kept = with_agy_fallback(unconfigured(), None, unread_marker, move || async move {
                Err(ProviderFetchFailure::terminal(quiet))
            })
            .await;
            assert_eq!(shown(kept), ANTIGRAVITY_UNCONFIGURED_ERROR);
        }
        // Control: another primary failure keeps its own message.
        let other = with_agy_fallback(
            Err(ProviderFetchFailure::terminal("Antigravity loadCodeAssist permission was denied.")),
            None,
            unread_marker,
            || async { Err(ProviderFetchFailure::terminal(AGY_TIMED_OUT_RETRYING)) },
        )
        .await;
        assert_eq!(shown(other), "Antigravity loadCodeAssist permission was denied.");
    }

    #[tokio::test]
    async fn a_failure_without_a_spawn_does_not_latch() {
        let latch = std::sync::Mutex::new(AgyLatch::Idle);
        let runs = std::cell::Cell::new(0);

        assert!(run_gate(&latch, marker("m1"), &runs, Err(false))
            .await
            .is_err());
        assert_eq!(runs.get(), 1);
        assert_eq!(latch_state(&latch), AgyLatch::Idle);

        assert!(run_gate(&latch, marker("m1"), &runs, Ok(())).await.is_ok());
        assert_eq!(runs.get(), 2, "a CLI that was never found must not latch");
    }

    #[tokio::test]
    async fn an_in_flight_attempt_blocks_an_overlapping_poll() {
        let latch = std::sync::Mutex::new(AgyLatch::InFlight);
        let runs = std::cell::Cell::new(0);

        let blocked = run_gate(&latch, marker("m1"), &runs, Ok(())).await;
        assert_eq!(
            runs.get(),
            0,
            "an overlapping poll must not spawn a second agy"
        );
        assert!(matches!(blocked, Err(ProviderFetchFailure::Terminal { .. })));
        assert_eq!(latch_state(&latch), AgyLatch::InFlight);
    }

    /// A poll cancelled mid-run (the future dropped) must not leave the route
    /// stuck in `InFlight` for the rest of the process.
    #[tokio::test]
    async fn a_cancelled_attempt_releases_the_latch() {
        let latch = std::sync::Mutex::new(AgyLatch::Idle);
        let runs = std::cell::Cell::new(0);

        let cancelled = tokio::time::timeout(
            std::time::Duration::from_millis(10),
            fetch_agy_cli_gated(agy_now(), true, marker("m1"), &latch, |_| {
                runs.set(runs.get() + 1);
                std::future::pending::<Result<Fetched, AgyRunError>>()
            }),
        )
        .await;
        assert!(cancelled.is_err(), "the runner must still be pending");
        assert_eq!(runs.get(), 1, "the runner must have started");
        assert_eq!(latch_state(&latch), AgyLatch::Idle);

        assert!(run_gate(&latch, marker("m1"), &runs, Ok(())).await.is_ok());
        assert_eq!(runs.get(), 2);
    }

    // Pins that neither `-w` nor `-g` (which would request the secret) is added.
    #[test]
    fn keychain_query_requests_attributes_only() {
        assert_eq!(
            AGY_KEYCHAIN_QUERY,
            &["find-generic-password", "-s", "gemini", "-a", "antigravity"]
        );
    }

    #[test]
    fn keychain_mdat_is_parsed_from_attribute_output() {
        let with_mdat = "keychain: \"/Users/x/Library/Keychains/login.keychain-db\"\n\
            attributes:\n    \"acct\"<blob>=\"antigravity\"\n    \
            \"mdat\"<timedate>=0x32303236303932333137343035365A00  \"20260923174056Z\\000\"\n    \
            \"svce\"<blob>=\"gemini\"\n";
        assert_eq!(
            parse_keychain_mdat(with_mdat).as_deref(),
            Some("0x32303236303932333137343035365A00  \"20260923174056Z\\000\"")
        );

        let without_mdat = "attributes:\n    \"acct\"<blob>=\"antigravity\"\n";
        assert_eq!(parse_keychain_mdat(without_mdat), None);
    }

    fn unreachable_probe_fetched(now: DateTime<Utc>) -> Fetched {
        Fetched {
            agy_login_marker: None,
            bound_account_key: None,
            source: "agy".to_string(),
            identity: None,
            account_scope: Err(AccountScopeError::NoTrustedEvidence),
            history_scope: Err(AccountScopeError::NoTrustedEvidence),
            cache_binding: None,
            windows: vec![UsageWindow::from_provider_used_percent(
                "Probe".to_string(),
                10.0,
                None,
                now,
            )],
        }
    }
}

/// Fakes for every captured-account boundary: `security`, the artifacts, the
/// token endpoint, the scope store and the quota calls. Shared with
/// `agent_usage`'s tests, which compose captured accounts into cards.
#[cfg(test)]
pub(crate) mod captured_test_support {
    use super::*;
    use crate::agent_account_scope::test_support::TestRefreshScope;
    use base64::Engine as _;
    use std::cell::{Cell, RefCell};

    pub(crate) const AUD: &str = "111111111111-agy.apps.googleusercontent.com";
    pub(crate) const OTHER: &str = "222222222222-other.apps.googleusercontent.com";

    pub(crate) fn secret(fill: char) -> String {
        format!("GOCSPX-{}", fill.to_string().repeat(28))
    }

    pub(crate) fn jwt(claims: Value) -> String {
        format!(
            "e30.{}.sig",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string())
        )
    }

    pub(crate) fn agy_item(refresh_token: &str, claims: Option<Value>) -> Vec<u8> {
        let mut login = json!({
            "token": {
                "access_token": "ya29.stored",
                "token_type": "Bearer",
                "refresh_token": refresh_token,
                "expiry": "2026-10-02T00:00:00Z",
            },
            "auth_method": "oauth",
        });
        if let Some(claims) = claims {
            login["id_token"] = Value::String(jwt(claims));
        }
        format!(
            "go-keyring-base64:{}\n",
            base64::engine::general_purpose::STANDARD.encode(login.to_string())
        )
        .into_bytes()
    }

    pub(crate) fn stored_value(
        refresh_token: &str,
        client_id: &str,
        client_secret: &str,
    ) -> String {
        encode_stored(&StoredCredential {
            refresh_token: refresh_token.to_string(),
            client: OAuthClient {
                id: client_id.to_string(),
                secret: client_secret.to_string(),
            },
        })
    }

    /// `(refresh_token, client_id, client_secret)` of a stored value.
    pub(crate) fn decode_value(value: &str) -> (String, String, String) {
        let stored = decode_stored(value.as_bytes()).expect("a decodable stored value");
        (stored.refresh_token, stored.client.id, stored.client.secret)
    }

    pub(crate) fn token_ok(access_token: &str, extra: Value) -> (u16, String) {
        let mut body =
            json!({ "access_token": access_token, "expires_in": 3599, "token_type": "Bearer" });
        if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
            body.extend(extra.clone());
        }
        (200, body.to_string())
    }

    type TokenFn = Box<dyn Fn(&OAuthClient, &str) -> Result<(u16, String), ProviderFetchFailure>>;

    pub(crate) struct FakeIo {
        pub agy_item: Option<Vec<u8>>,
        pub artifacts: Vec<Vec<u8>>,
        /// The fake keychain: account (key) → value.
        pub items: RefCell<BTreeMap<String, String>>,
        pub write_status: i32,
        pub token: TokenFn,
        pub security_calls: RefCell<Vec<SecurityCall>>,
        /// `(client_id, client_secret, refresh_token)` per token request.
        pub token_calls: RefCell<Vec<(String, String, String)>>,
        pub artifact_calls: Cell<usize>,
        /// `throttle_login_shell` of each artifact scan.
        pub artifact_throttles: RefCell<Vec<bool>>,
        /// When set, agy's `-w` read ends with this exit code instead of
        /// following `agy_item`; `Some(None)` is a timeout or a signal.
        pub agy_read_exit: Option<Option<i32>>,
        /// stdout of the attributes-only marker query while `agy_item` is set.
        pub agy_attributes: String,
        pub quota_calls: Cell<usize>,
        /// When set, `quota` fails terminally with this display text.
        pub quota_terminal: Option<&'static str>,
        pub scope: TestRefreshScope,
    }

    impl FakeIo {
        pub(crate) fn new(tag: &str) -> Self {
            Self {
                agy_item: None,
                artifacts: Vec::new(),
                items: RefCell::new(BTreeMap::new()),
                write_status: 0,
                token: Box::new(|_, _| Ok(token_ok("ya29.fresh", json!({})))),
                security_calls: RefCell::new(Vec::new()),
                token_calls: RefCell::new(Vec::new()),
                artifact_calls: Cell::new(0),
                artifact_throttles: RefCell::new(Vec::new()),
                agy_read_exit: None,
                agy_attributes: String::new(),
                quota_calls: Cell::new(0),
                quota_terminal: None,
                scope: TestRefreshScope::new("antigravity", tag),
            }
        }

        pub(crate) fn writes(&self) -> Vec<SecurityCall> {
            self.security_calls
                .borrow()
                .iter()
                .filter(|call| call.argv == ["-i"])
                .cloned()
                .collect()
        }

        pub(crate) fn reads_of_captured_items(&self) -> usize {
            self.security_calls
                .borrow()
                .iter()
                .filter(|call| {
                    call.argv.first().map(String::as_str) == Some("find-generic-password")
                        && call.argv.get(2).map(String::as_str) == Some(CAPTURED_SERVICE)
                })
                .count()
        }

        pub(crate) fn network_calls(&self) -> usize {
            self.token_calls.borrow().len() + self.quota_calls.get()
        }
    }

    impl Drop for FakeIo {
        fn drop(&mut self) {
            self.scope.cleanup();
        }
    }

    fn exit(code: i32, stdout: Vec<u8>) -> Option<SecurityExit> {
        Some(SecurityExit {
            code: Some(code),
            stdout,
        })
    }

    impl CapturedIo for FakeIo {
        async fn security(
            &self,
            call: SecurityCall,
            _timeout: std::time::Duration,
        ) -> Option<SecurityExit> {
            self.security_calls.borrow_mut().push(call.clone());
            let argv: Vec<&str> = call.argv.iter().map(String::as_str).collect();
            match argv.as_slice() {
                ["-i"] => {
                    let line = String::from_utf8(call.stdin.expect("stdin")).unwrap();
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    assert_eq!(
                        parts[..5],
                        ["add-generic-password", "-U", "-s", CAPTURED_SERVICE, "-a"]
                    );
                    assert_eq!(parts[6], "-w");
                    assert_eq!(parts.len(), 8);
                    if self.write_status == 0 {
                        self.items
                            .borrow_mut()
                            .insert(parts[5].to_string(), parts[7].to_string());
                    }
                    exit(self.write_status, Vec::new())
                }
                ["find-generic-password", "-s", "gemini", "-a", "antigravity", "-w"] => {
                    if let Some(code) = self.agy_read_exit {
                        return code.and_then(|code| exit(code, Vec::new()));
                    }
                    match &self.agy_item {
                        Some(item) => exit(0, item.clone()),
                        None => exit(SECURITY_ITEM_NOT_FOUND, Vec::new()),
                    }
                }
                ["find-generic-password", "-s", "gemini", "-a", "antigravity"] => {
                    match &self.agy_item {
                        Some(_) => exit(0, self.agy_attributes.clone().into_bytes()),
                        None => exit(SECURITY_ITEM_NOT_FOUND, Vec::new()),
                    }
                }
                ["find-generic-password", "-s", service, "-a", key, "-w"]
                    if *service == CAPTURED_SERVICE =>
                {
                    match self.items.borrow().get(*key) {
                        Some(value) => exit(0, format!("{value}\n").into_bytes()),
                        None => exit(SECURITY_ITEM_NOT_FOUND, Vec::new()),
                    }
                }
                ["delete-generic-password", "-s", service, "-a", key]
                    if *service == CAPTURED_SERVICE =>
                {
                    match self.items.borrow_mut().remove(*key) {
                        Some(_) => exit(0, Vec::new()),
                        None => exit(SECURITY_ITEM_NOT_FOUND, Vec::new()),
                    }
                }
                other => panic!("unexpected security call {other:?}"),
            }
        }

        async fn client_artifacts(&self, throttle_login_shell: bool) -> Vec<Vec<u8>> {
            self.artifact_calls.set(self.artifact_calls.get() + 1);
            self.artifact_throttles.borrow_mut().push(throttle_login_shell);
            self.artifacts.clone()
        }

        async fn token_post(
            &self,
            client: &OAuthClient,
            refresh_token: &str,
            _binding: Option<ProviderCacheBinding>,
        ) -> Result<(u16, String), ProviderFetchFailure> {
            self.token_calls.borrow_mut().push((
                client.id.clone(),
                client.secret.clone(),
                refresh_token.to_string(),
            ));
            (self.token)(client, refresh_token)
        }

        fn scopes(
            &self,
            key: &str,
        ) -> (
            Result<AccountScope, AccountScopeError>,
            Result<HistoryScope, AccountScopeError>,
        ) {
            captured_scopes(
                key,
                |provider, kind, id| self.scope.resolve_authoritative(provider, kind, id),
                |provider, authoritative| self.scope.resolve_history(provider, authoritative),
            )
        }

        async fn quota(
            &self,
            _access_token: String,
            account_scope: AccountScope,
            history_scope: Result<HistoryScope, AccountScopeError>,
            now: DateTime<Utc>,
        ) -> Result<Fetched, ProviderFetchFailure> {
            self.quota_calls.set(self.quota_calls.get() + 1);
            if let Some(display) = self.quota_terminal {
                return Err(ProviderFetchFailure::terminal(display));
            }
            let window = quota_window(
                "Gemini".to_string(),
                0.5,
                Some(now + chrono::Duration::hours(1)),
                now,
                "agy.test.v1".to_string(),
                Some("agy.test.v1".to_string()),
            )
            .expect("a valid window");
            Ok(Fetched {
                agy_login_marker: None,
                bound_account_key: None,
                source: "oauth".to_string(),
                identity: Some(remote_identity(Some("Paid".to_string()))),
                account_scope: Ok(account_scope.clone()),
                history_scope,
                cache_binding: Some(ProviderCacheBinding::primary(account_scope)),
                windows: vec![window],
            })
        }
    }

    pub(crate) fn new_token_cache() -> CapturedTokenCache {
        std::sync::Mutex::new(HashMap::new())
    }

    pub(crate) fn cached_keys(cache: &CapturedTokenCache) -> Vec<String> {
        lock_tokens(cache).keys().cloned().collect()
    }
}

#[cfg(test)]
mod captured_account_tests {
    use super::captured_test_support::*;
    use super::*;

    fn claims(sub: &str, aud: &str, email: &str) -> Value {
        json!({ "sub": sub, "aud": aud, "email": email })
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-02T06:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    /// One artifact naming the token's client and another, with ONE secret:
    /// `preferred_client` pairs that secret with the LAST id (OTHER), so a
    /// positional pick would refresh with the wrong client.
    fn artifacts_where_positional_pick_differs() -> Vec<Vec<u8>> {
        vec![
            format!("ide\0{AUD}\0{OTHER}\0{}\0", secret('a')).into_bytes(),
            format!("agy\0{OTHER}\0{}\0", secret('b')).into_bytes(),
        ]
    }

    #[test]
    fn parses_go_keyring_agy_login() {
        let login = parse_agy_login(&agy_item(
            "1//rt-a",
            Some(claims("sub-a", AUD, "a@example.com")),
        ))
        .unwrap_or_else(|_| panic!("parse"));
        assert_eq!(login.refresh_token, "1//rt-a");
        assert_eq!(login.sub, "sub-a");
        assert_eq!(login.aud, AUD);
        assert_eq!(login.email.as_deref(), Some("a@example.com"));

        assert_eq!(
            parse_agy_login(b"{\"token\":{}}").err(),
            Some(CaptureError::AgyLoginUnreadable),
            "without the go-keyring prefix"
        );
        assert_eq!(
            parse_agy_login(&agy_item("", Some(claims("s", AUD, "e")))).err(),
            Some(CaptureError::AgyLoginUnreadable),
            "without a refresh token"
        );
    }

    #[tokio::test]
    async fn capture_without_sub_or_aud_fails_without_write() {
        for claims in [
            Some(json!({ "aud": AUD, "email": "a@example.com" })),
            Some(json!({ "sub": "sub-a", "email": "a@example.com" })),
            None,
        ] {
            let mut io = FakeIo::new("capture-missing-identity");
            io.agy_item = Some(agy_item("1//rt-a", claims));
            io.artifacts = artifacts_where_positional_pick_differs();
            assert_eq!(
                capture_with(&io).await.err(),
                Some(CaptureError::AgyLoginMissingIdentity)
            );
            assert_eq!(
                io.security_calls.borrow().len(),
                1,
                "only agy's item was read"
            );
            assert!(io.writes().is_empty());
            assert!(io.items.borrow().is_empty());
            assert_eq!(io.network_calls(), 0);
        }
    }

    #[tokio::test]
    async fn capture_refreshes_with_the_client_named_by_aud() {
        let ids = scan_client_ids(&artifacts_where_positional_pick_differs()[0]);
        let secrets = scan_client_secrets(&artifacts_where_positional_pick_differs()[0]);
        assert_eq!(
            preferred_client(&ids, &secrets)
                .map(|client| client.0)
                .as_deref(),
            Some(OTHER),
            "the fixture must make the positional pick differ from aud"
        );

        let mut io = FakeIo::new("capture-by-aud");
        io.agy_item = Some(agy_item(
            "1//rt-a",
            Some(claims("sub-a", AUD, "a@example.com")),
        ));
        io.artifacts = artifacts_where_positional_pick_differs();
        let issuing = secret('a');
        io.token = Box::new(move |client, _| {
            if client.id == AUD && client.secret == issuing {
                Ok(token_ok(
                    "ya29.a",
                    json!({ "id_token": jwt(json!({ "sub": "sub-a" })) }),
                ))
            } else {
                Ok((401, r#"{"error":"invalid_client"}"#.to_string()))
            }
        });
        let account = capture_with(&io).await.unwrap();
        assert_eq!(account.key, captured_key("sub-a"));
        assert_eq!(account.label, "a@example.com");
        assert_eq!(
            *io.token_calls.borrow(),
            vec![(AUD.to_string(), secret('a'), "1//rt-a".to_string())]
        );
        let items = io.items.borrow();
        assert_eq!(items.len(), 1);
        assert_eq!(
            decode_value(&items[&account.key]),
            ("1//rt-a".to_string(), AUD.to_string(), secret('a'))
        );
    }

    #[tokio::test]
    async fn capture_tries_the_next_secret_only_for_a_wrong_client() {
        let artifact = format!("ide\0{AUD}\0{}\0{}\0", secret('a'), secret('b')).into_bytes();

        let mut io = FakeIo::new("capture-fallthrough");
        io.agy_item = Some(agy_item(
            "1//rt-a",
            Some(claims("sub-a", AUD, "a@example.com")),
        ));
        io.artifacts = vec![artifact.clone()];
        let second = secret('b');
        io.token = Box::new(move |client, _| {
            if client.secret == second {
                Ok(token_ok("ya29.a", json!({})))
            } else {
                Ok((401, r#"{"error":"unauthorized_client"}"#.to_string()))
            }
        });
        let account = capture_with(&io).await.unwrap();
        assert_eq!(io.token_calls.borrow().len(), 2);
        assert_eq!(
            decode_value(&io.items.borrow()[&account.key]).2,
            secret('b')
        );

        let mut io = FakeIo::new("capture-stops");
        io.agy_item = Some(agy_item(
            "1//rt-a",
            Some(claims("sub-a", AUD, "a@example.com")),
        ));
        io.artifacts = vec![artifact];
        io.token = Box::new(|_, _| Ok((400, r#"{"error":"invalid_grant"}"#.to_string())));
        assert_eq!(
            capture_with(&io).await.err(),
            Some(CaptureError::RefreshRejected)
        );
        assert_eq!(io.token_calls.borrow().len(), 1, "any other error stops");
        assert!(io.writes().is_empty());
    }

    #[tokio::test]
    async fn capture_rejects_a_refresh_for_another_account() {
        for id_token in [
            Value::String(jwt(json!({ "sub": "sub-b" }))),
            Value::String("not-a-jwt".to_string()),
            Value::Null,
        ] {
            let mut io = FakeIo::new("capture-mismatch");
            io.agy_item = Some(agy_item(
                "1//rt-a",
                Some(claims("sub-a", AUD, "a@example.com")),
            ));
            io.artifacts = artifacts_where_positional_pick_differs();
            let id_token = id_token.clone();
            io.token = Box::new(move |_, _| {
                Ok(token_ok("ya29.a", json!({ "id_token": id_token.clone() })))
            });
            assert_eq!(
                capture_with(&io).await.err(),
                Some(CaptureError::AccountMismatch)
            );
            assert!(io.writes().is_empty());
        }
    }

    #[test]
    fn keychain_write_keeps_the_secret_off_argv() {
        let key = captured_key("sub-a");
        let value = stored_value("1//rt-a", AUD, &secret('a'));
        let call = write_item_call(&key, &value).unwrap();
        assert_eq!(call.argv, vec!["-i".to_string()]);
        assert!(call.argv.iter().all(|arg| !arg.contains(&value)));
        let stdin = String::from_utf8(call.stdin.unwrap()).unwrap();
        assert_eq!(
            stdin,
            format!("add-generic-password -U -s {CAPTURED_SERVICE} -a {key} -w {value}\n")
        );
        assert_eq!(stdin.matches('\n').count(), 1, "exactly one command line");
    }

    #[tokio::test]
    async fn no_security_argv_carries_a_secret_during_capture() {
        let mut io = FakeIo::new("capture-argv");
        io.agy_item = Some(agy_item(
            "1//rt-a",
            Some(claims("sub-a", AUD, "a@example.com")),
        ));
        io.artifacts = artifacts_where_positional_pick_differs();
        let account = capture_with(&io).await.unwrap();
        let value = io.items.borrow()[&account.key].clone();
        for call in io.security_calls.borrow().iter() {
            for arg in &call.argv {
                assert!(
                    !arg.contains("1//rt-a") && !arg.contains(&value) && !arg.contains("GOCSPX-")
                );
                assert!(!arg.contains("sub-a"), "the raw sub never reaches argv");
            }
        }
    }

    #[test]
    fn malformed_key_or_value_builds_no_command() {
        let key = captured_key("sub-a");
        let value = stored_value("1//rt-a", AUD, &secret('a'));
        let bad_keys = [
            String::new(),
            format!("{} ", &key[..63]),
            format!("{}\"", &key[..63]),
            format!("{}\n", &key[..63]),
            key.to_uppercase(),
            key[..63].to_string(),
            format!("{key}0"),
            format!("{} -w x", &key[..58]),
        ];
        for bad in &bad_keys {
            assert!(write_item_call(bad, &value).is_none(), "{bad:?}");
            assert!(read_item_call(bad).is_none(), "{bad:?}");
            assert!(delete_item_call(bad).is_none(), "{bad:?}");
        }
        for bad in ["", "a b", "a\"b", "a\nb", "a'b", "a-b_c"] {
            assert!(write_item_call(&key, bad).is_none(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn malformed_key_starts_no_process() {
        let io = FakeIo::new("malformed-key");
        let cache = new_token_cache();
        for bad in ["a b", "a\"b", "a\nb", "ZZ"] {
            assert_eq!(
                remove_with(&io, &cache, bad).await.err(),
                Some(CaptureError::InvalidKey)
            );
            assert!(fetch_captured_with(&io, &cache, bad, "label", now())
                .await
                .is_err());
        }
        assert!(io.security_calls.borrow().is_empty());
        assert_eq!(io.network_calls(), 0);
    }

    #[tokio::test]
    async fn polls_within_a_token_lifetime_refresh_once_and_never_write() {
        let mut io = FakeIo::new("polls");
        let key = captured_key("sub-a");
        io.items
            .borrow_mut()
            .insert(key.clone(), stored_value("1//rt-a", AUD, &secret('a')));
        // Google echoing the same refresh token is not a rotation.
        io.token = Box::new(|_, _| Ok(token_ok("ya29.a", json!({ "refresh_token": "1//rt-a" }))));
        let cache = new_token_cache();
        for minutes in [0, 10, 20, 30, 40, 54] {
            let fetched = fetch_captured_with(
                &io,
                &cache,
                &key,
                "a@example.com",
                now() + chrono::Duration::minutes(minutes),
            )
            .await
            .unwrap();
            assert_eq!(
                fetched.identity.as_ref().and_then(|id| id.email.as_deref()),
                Some("a@example.com")
            );
        }
        assert_eq!(io.token_calls.borrow().len(), 1);
        assert_eq!(io.reads_of_captured_items(), 1);
        assert!(io.writes().is_empty());
        assert_eq!(io.quota_calls.get(), 6);

        // Inside the five-minute margin the token is refreshed again.
        fetch_captured_with(
            &io,
            &cache,
            &key,
            "a",
            now() + chrono::Duration::minutes(56),
        )
        .await
        .unwrap();
        assert_eq!(io.token_calls.borrow().len(), 2);
        assert!(io.writes().is_empty());
    }

    #[tokio::test]
    async fn a_rotated_refresh_token_is_written_once_to_its_own_item() {
        let mut io = FakeIo::new("rotation");
        let key_a = captured_key("sub-a");
        let key_b = captured_key("sub-b");
        let stored_b = stored_value("1//rt-b", AUD, &secret('b'));
        io.items
            .borrow_mut()
            .insert(key_a.clone(), stored_value("1//rt-a", AUD, &secret('a')));
        io.items
            .borrow_mut()
            .insert(key_b.clone(), stored_b.clone());
        io.token = Box::new(|_, refresh_token| {
            if refresh_token == "1//rt-a" {
                Ok(token_ok("ya29.a", json!({ "refresh_token": "1//rt-a2" })))
            } else {
                Ok(token_ok("ya29.other", json!({})))
            }
        });
        let cache = new_token_cache();
        fetch_captured_with(&io, &cache, &key_a, "a", now())
            .await
            .unwrap();
        fetch_captured_with(&io, &cache, &key_b, "b", now())
            .await
            .unwrap();

        let writes = io.writes();
        assert_eq!(writes.len(), 1);
        let line = String::from_utf8(writes[0].stdin.clone().unwrap()).unwrap();
        assert!(line.contains(&format!(" -a {key_a} ")));
        let items = io.items.borrow();
        assert_eq!(
            decode_value(&items[&key_a]),
            ("1//rt-a2".to_string(), AUD.to_string(), secret('a'))
        );
        assert_eq!(
            items[&key_b], stored_b,
            "the other account's item is untouched"
        );
    }

    #[test]
    fn captured_accounts_get_their_own_scopes() {
        let io = FakeIo::new("scopes");
        let key_a = captured_key("sub-a");
        let key_b = captured_key("sub-b");
        assert_ne!(key_a, key_b);
        assert!(valid_captured_key(&key_a) && !key_a.contains("sub"));

        let (account_a, history_a) = io.scopes(&key_a);
        let (account_b, history_b) = io.scopes(&key_b);
        let (account_a, history_a) = (account_a.unwrap(), history_a.unwrap());
        let (account_b, history_b) = (account_b.unwrap(), history_b.unwrap());
        assert_ne!(account_a, account_b);
        assert_ne!(history_a, history_b);

        let primary_history = io.scope.resolve_history("antigravity", None).unwrap();
        let primary_account = io
            .scope
            .resolve_current(
                "google-oauth-creds",
                "/x/.gemini/oauth_creds.json\0refresh_token",
                b"1//rt-a",
            )
            .unwrap();
        for history in [&history_a, &history_b] {
            assert_ne!(*history, primary_history);
        }
        for account in [&account_a, &account_b] {
            assert_ne!(*account, primary_account);
        }

        // Stable across polls: the same key resolves to the same scopes.
        let (again_account, again_history) = io.scopes(&key_a);
        assert_eq!(again_account.unwrap(), account_a);
        assert_eq!(again_history.unwrap(), history_a);
    }

    #[tokio::test]
    async fn remove_makes_no_network_call_and_clears_the_token_cache() {
        let io = FakeIo::new("remove");
        let key = captured_key("sub-a");
        io.items
            .borrow_mut()
            .insert(key.clone(), stored_value("1//rt-a", AUD, &secret('a')));
        let cache = new_token_cache();
        fetch_captured_with(&io, &cache, &key, "a", now())
            .await
            .unwrap();
        assert_eq!(cached_keys(&cache), vec![key.clone()]);
        let network_before = io.network_calls();

        remove_with(&io, &cache, &key).await.unwrap();
        assert_eq!(
            io.network_calls(),
            network_before,
            "remove makes no network call"
        );
        assert_eq!(io.artifact_calls.get(), 0);
        assert!(io.items.borrow().is_empty());
        assert!(cached_keys(&cache).is_empty());
        // Already gone counts as removed.
        remove_with(&io, &cache, &key).await.unwrap();

        // A later poll no longer has a token for the removed account.
        let failure = fetch_captured_with(&io, &cache, &key, "a", now())
            .await
            .unwrap_err();
        assert!(matches!(
            failure,
            ProviderFetchFailure::Terminal { ref display } if display == CAPTURED_ITEM_MISSING
        ));
    }

    /// A Code Assist 401 on a captured account asks for a new capture, not
    /// for an Antigravity re-login, which would sign in the wrong account.
    /// Other terminal failures pass through unchanged.
    #[tokio::test]
    async fn a_captured_401_asks_for_a_new_capture() {
        let key = captured_key("sub-a");
        for (from, to) in [
            (ANTIGRAVITY_AUTH_EXPIRED, CAPTURED_AUTH_EXPIRED),
            ("some other failure", "some other failure"),
        ] {
            let mut io = FakeIo::new("captured-401");
            io.items
                .borrow_mut()
                .insert(key.clone(), stored_value("1//rt-a", AUD, &secret('a')));
            io.quota_terminal = Some(from);
            let failure = fetch_captured_with(&io, &new_token_cache(), &key, "a", now())
                .await
                .unwrap_err();
            assert!(
                matches!(failure, ProviderFetchFailure::Terminal { ref display } if display == to),
                "{from} -> {to}"
            );
        }
    }

    /// Every capture failure is a fixed code: seeded with sentinel token, sub,
    /// email and error_description values, none of them reaches the FFI JSON.
    #[tokio::test]
    async fn capture_failures_carry_no_secret_or_identity() {
        const SENTINELS: [&str; 5] = [
            "SENTINELTOKEN",
            "SENTINELSUB",
            "SENTINELMAIL",
            "SENTINELDESC",
            "SENTINELOTHERSUB",
        ];
        let login = || {
            agy_item(
                "1//SENTINELTOKEN",
                Some(claims("SENTINELSUB", AUD, "SENTINELMAIL@example.com")),
            )
        };
        let mut cases: Vec<FakeIo> = Vec::new();

        cases.push(FakeIo::new("sentinel-absent"));
        let mut io = FakeIo::new("sentinel-unreadable");
        io.agy_item = Some(b"go-keyring-base64:SENTINELTOKEN!!".to_vec());
        cases.push(io);
        let mut io = FakeIo::new("sentinel-identity");
        io.agy_item = Some(agy_item(
            "1//SENTINELTOKEN",
            Some(json!({ "sub": "SENTINELSUB", "email": "SENTINELMAIL@example.com" })),
        ));
        cases.push(io);
        let mut io = FakeIo::new("sentinel-client");
        io.agy_item = Some(login());
        cases.push(io);
        let mut io = FakeIo::new("sentinel-rejected");
        io.agy_item = Some(login());
        io.artifacts = artifacts_where_positional_pick_differs();
        io.token = Box::new(|_, _| {
            Ok((
                400,
                r#"{"error":"invalid_grant","error_description":"SENTINELDESC 1//SENTINELTOKEN"}"#
                    .to_string(),
            ))
        });
        cases.push(io);
        let mut io = FakeIo::new("sentinel-wrong-client");
        io.agy_item = Some(login());
        io.artifacts = artifacts_where_positional_pick_differs();
        io.token = Box::new(|_, _| {
            Ok((
                401,
                r#"{"error":"invalid_client","error_description":"SENTINELDESC"}"#.to_string(),
            ))
        });
        cases.push(io);
        let mut io = FakeIo::new("sentinel-transient");
        io.agy_item = Some(login());
        io.artifacts = artifacts_where_positional_pick_differs();
        io.token = Box::new(|_, _| {
            Err(ProviderFetchFailure::transient(
                "SENTINELDESC",
                None,
                SafeTransportDiagnostic::server_error(503),
            ))
        });
        cases.push(io);
        let mut io = FakeIo::new("sentinel-mismatch");
        io.agy_item = Some(login());
        io.artifacts = artifacts_where_positional_pick_differs();
        io.token = Box::new(|_, _| {
            Ok(token_ok(
                "ya29.SENTINELTOKEN",
                json!({ "id_token": jwt(json!({ "sub": "SENTINELOTHERSUB" })) }),
            ))
        });
        cases.push(io);
        let mut io = FakeIo::new("sentinel-write");
        io.agy_item = Some(login());
        io.artifacts = artifacts_where_positional_pick_differs();
        io.write_status = 1;
        cases.push(io);

        let mut codes = Vec::new();
        for io in &cases {
            let error = capture_with(io).await.expect_err("every case fails");
            let ffi = json!({ "ok": false, "err": error.code() }).to_string();
            let debug = format!("{error:?}");
            for sentinel in SENTINELS {
                assert!(
                    !ffi.contains(sentinel) && !debug.contains(sentinel),
                    "{ffi}"
                );
            }
            codes.push(error.code());
        }
        assert_eq!(
            codes,
            vec![
                "agy_not_signed_in",
                "agy_login_unreadable",
                "agy_login_missing_identity",
                "oauth_client_not_found",
                "refresh_rejected",
                "oauth_client_rejected",
                "refresh_unreachable",
                "account_mismatch",
                "keychain_write_failed",
            ]
        );
    }

    #[test]
    fn registry_accepts_only_hex_keys_and_reports_by_index() {
        let _guard = CAPTURED_ACCOUNTS_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let key_a = captured_key("sub-a");
        let key_b = captured_key("sub-b");
        let raw = json!([
            { "key": key_a, "label": "a@example.com" },
            { "key": "sub-raw value", "label": "x" },
            { "key": key_a, "label": "dup" },
            { "label": "no key" },
            "not an object",
            { "key": key_b, "label": "b@example.com" },
        ])
        .to_string();
        let result = set_captured_accounts_from_json(&raw).unwrap();
        assert_eq!(result["registeredCount"], 2);
        assert_eq!(
            result["rejected"],
            json!([
                { "index": 1, "reason": "invalid key" },
                { "index": 2, "reason": "duplicate key" },
                { "index": 3, "reason": "invalid entry" },
                { "index": 4, "reason": "invalid entry" },
            ])
        );
        assert!(
            !result.to_string().contains("sub-raw"),
            "a rejected key is never echoed"
        );
        assert_eq!(
            captured_accounts(),
            vec![
                CapturedAccount {
                    key: key_a.clone(),
                    label: "a@example.com".to_string()
                },
                CapturedAccount {
                    key: key_b,
                    label: "b@example.com".to_string()
                },
            ]
        );

        assert_eq!(
            set_captured_accounts_from_json("{not json SENTINEL").unwrap_err(),
            "invalid_accounts_json"
        );
        assert_eq!(
            captured_accounts().len(),
            2,
            "malformed JSON changes nothing"
        );
        set_captured_accounts_from_json("[]").unwrap();
        assert!(captured_accounts().is_empty());
    }

    // ── automatic capture (S4) ──

    fn auto_io(tag: &str) -> FakeIo {
        let mut io = FakeIo::new(tag);
        io.agy_item = Some(agy_item(
            "1//rt-a",
            Some(claims("sub-a", AUD, "stored@example.com")),
        ));
        io.artifacts = artifacts_where_positional_pick_differs();
        io.token = Box::new(|_, _| {
            Ok(token_ok(
                "ya29.a",
                json!({ "id_token": jwt(json!({ "sub": "sub-a", "email": "fresh@example.com" })) }),
            ))
        });
        io
    }

    #[tokio::test]
    async fn auto_capture_writes_once_with_the_throttled_scan() {
        let io = auto_io("auto-captured");
        let key = captured_key("sub-a");
        assert_eq!(
            auto_capture_with(&io, &[]).await,
            Ok(AutoCaptured::Captured(CapturedAccount {
                key: key.clone(),
                label: "fresh@example.com".to_string(),
            })),
            "the label is Google's answer, not the stored id_token"
        );
        assert_eq!(io.writes().len(), 1);
        assert_eq!(io.token_calls.borrow().len(), 1);
        assert_eq!(*io.artifact_throttles.borrow(), vec![true]);
        assert_eq!(decode_value(&io.items.borrow()[&key]).0, "1//rt-a");

        // The manual path keeps the unthrottled lookup.
        let manual = auto_io("manual-unthrottled");
        capture_with(&manual).await.unwrap();
        assert_eq!(*manual.artifact_throttles.borrow(), vec![false]);
    }

    #[tokio::test]
    async fn auto_capture_skips_a_removed_key_before_any_request() {
        let key = captured_key("sub-a");
        let other = captured_key("sub-b");
        let io = auto_io("auto-removed");
        assert_eq!(
            auto_capture_with(&io, &[other.clone(), key.clone()]).await,
            Ok(AutoCaptured::SkippedRemoved)
        );
        assert_eq!(io.network_calls(), 0);
        assert_eq!(io.artifact_calls.get(), 0);
        assert!(io.writes().is_empty());
        assert_eq!(io.reads_of_captured_items(), 0);

        // Control: another account's removal does not skip this one.
        let io = auto_io("auto-removed-control");
        assert!(matches!(
            auto_capture_with(&io, &[other]).await,
            Ok(AutoCaptured::Captured(_))
        ));
        assert_eq!(io.writes().len(), 1);
    }

    #[tokio::test]
    async fn auto_capture_of_a_stored_token_does_nothing() {
        let key = captured_key("sub-a");
        let io = auto_io("auto-unchanged");
        io.items
            .borrow_mut()
            .insert(key.clone(), stored_value("1//rt-a", AUD, &secret('a')));
        assert_eq!(
            auto_capture_with(&io, &[]).await,
            Ok(AutoCaptured::Unchanged(CapturedAccount {
                key: key.clone(),
                label: "stored@example.com".to_string(),
            }))
        );
        assert_eq!(io.artifact_calls.get(), 0, "no client scan");
        assert_eq!(io.network_calls(), 0, "no request");
        assert!(io.writes().is_empty(), "no write");

        // Control: a different stored token is a new login and is captured.
        let io = auto_io("auto-unchanged-control");
        io.items
            .borrow_mut()
            .insert(key.clone(), stored_value("1//rt-old", AUD, &secret('a')));
        assert!(matches!(
            auto_capture_with(&io, &[]).await,
            Ok(AutoCaptured::Captured(_))
        ));
        assert_eq!(io.writes().len(), 1);
        assert_eq!(decode_value(&io.items.borrow()[&key]).0, "1//rt-a");
    }

    #[tokio::test]
    async fn auto_capture_requires_googles_id_token_for_the_same_sub() {
        for (tag, extra) in [
            ("auto-no-id-token", json!({})),
            ("auto-other-sub", json!({ "id_token": jwt(json!({ "sub": "sub-b" })) })),
            ("auto-bad-id-token", json!({ "id_token": "not-a-jwt" })),
        ] {
            let mut io = auto_io(tag);
            let extra = extra.clone();
            io.token = Box::new(move |_, _| Ok(token_ok("ya29.a", extra.clone())));
            assert_eq!(
                auto_capture_with(&io, &[]).await,
                Err(CaptureError::AccountMismatch),
                "{tag}"
            );
            assert!(io.writes().is_empty(), "{tag}");
            assert!(io.items.borrow().is_empty(), "{tag}");
        }
        // The manual path still accepts a response without an id_token.
        let mut io = auto_io("manual-no-id-token");
        io.token = Box::new(|_, _| Ok(token_ok("ya29.a", json!({}))));
        assert!(capture_with(&io).await.is_ok());
    }

    #[tokio::test]
    async fn auto_capture_pauses_on_any_agy_read_but_success_or_not_found() {
        for code in [Some(1), Some(51), Some(128), None] {
            let mut io = auto_io("auto-paused");
            io.agy_read_exit = Some(code);
            assert_eq!(
                auto_capture_with(&io, &[]).await,
                Err(CaptureError::Paused),
                "{code:?}"
            );
            assert_eq!(io.network_calls(), 0);
            assert!(io.writes().is_empty());
        }
        let mut io = auto_io("auto-not-signed-in");
        io.agy_read_exit = Some(Some(SECURITY_ITEM_NOT_FOUND));
        assert_eq!(
            auto_capture_with(&io, &[]).await,
            Err(CaptureError::NotSignedIn)
        );
        assert_eq!(io.security_calls.borrow().len(), 1);
        assert_eq!(io.network_calls(), 0);
        assert_eq!(
            [CaptureError::Paused.code(), CaptureError::NotSignedIn.code()],
            ["paused", "not_signed_in"]
        );
    }

    #[tokio::test]
    async fn login_marker_runs_the_attributes_only_query() {
        let mut io = auto_io("marker");
        io.agy_attributes = "attributes:\n    \"acct\"<blob>=\"antigravity\"\n    \
            \"mdat\"<timedate>=0x3230  \"20261002101010Z\\000\"\n\
            password: \"go-keyring-base64:1//rt-a\"\n"
            .to_string();
        let marker = login_marker_with(&io).await.unwrap();
        assert_eq!(marker, "0x3230  \"20261002101010Z\\000\"");
        assert!(!marker.contains("1//rt-a") && !marker.contains("go-keyring"));
        let calls = io.security_calls.borrow().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].argv, security_argv(AGY_KEYCHAIN_QUERY));
        assert!(!calls[0].argv.iter().any(|arg| arg == "-w" || arg == "-g"));
        assert_eq!(io.network_calls(), 0);

        io.agy_attributes = "attributes:\n".to_string();
        assert_eq!(login_marker_with(&io).await.as_deref(), Some("present"));
        io.agy_item = None;
        assert_eq!(login_marker_with(&io).await.as_deref(), Some("absent"));
    }

    /// The maintainer's real round trip (S2 merge gate). It touches the login
    /// keychain and the network, so it never runs in CI:
    /// `cargo test -p tb_core_ffi --lib real_capture_fetch_remove_round_trip -- --ignored --nocapture`
    /// with Syrtis quit and agy signed into an account not captured in
    /// production. Prints only the key prefix, the window count and pass/fail.
    #[tokio::test]
    #[ignore = "reads agy's login and writes the login keychain; run by the maintainer"]
    async fn real_capture_fetch_remove_round_trip() {
        let account = match capture().await {
            Ok(account) => {
                println!("capture: pass (key {})", &account.key[..8]);
                account
            }
            Err(error) => panic!("capture: fail ({})", error.code()),
        };
        let fetched = fetch_captured(&account.key, &account.label, Utc::now()).await;
        match &fetched {
            Ok(fetched) => println!("fetch: pass ({} windows)", fetched.windows.len()),
            Err(_) => println!("fetch: fail"),
        }
        let removed = remove(&account.key).await;
        println!(
            "remove: {}",
            match removed {
                Ok(()) => "pass",
                Err(error) => error.code(),
            }
        );
        assert!(fetched.is_ok_and(|fetched| !fetched.windows.is_empty()));
        assert!(removed.is_ok());
    }
}

/// Plan E (agy primary takes the bound captured account's OAuth result).
/// Every seam is injected: the marker reader, the captured result and the agy
/// runner, so no test reads the Keychain, reaches Google or spawns agy.
#[cfg(test)]
mod plan_e_tests {
    use super::captured_test_support::FakeIo;
    use super::tests::unread_marker;
    use super::*;
    use std::cell::{Cell, RefCell};

    /// The real shape (`keychain_mdat_is_parsed_from_attribute_output`).
    const MARKER: &str = r#"0x32303236303932333137343035365A00  "20260923174056Z\000""#;
    const OTHER_MARKER: &str = r#"0x32303236303932333137343035375A00  "20260923174057Z\000""#;

    fn account(sub: &str) -> CapturedAccount {
        CapturedAccount {
            key: captured_key(sub),
            label: format!("{sub}@example.com"),
        }
    }

    fn binding_for(account: &CapturedAccount, marker: &str) -> AntigravityBinding {
        AntigravityBinding {
            key: account.key.clone(),
            marker: marker.to_string(),
        }
    }

    async fn no_read() -> Option<String> {
        panic!("the marker must not be read for plan E here")
    }

    /// A live marker reader returning `answers` in order and counting reads.
    struct Reader {
        answers: RefCell<Vec<Option<String>>>,
        reads: Cell<u32>,
    }

    impl Reader {
        fn new(answers: &[Option<&str>]) -> Self {
            Self {
                answers: RefCell::new(answers.iter().rev().map(|a| a.map(str::to_string)).collect()),
                reads: Cell::new(0),
            }
        }

        fn read(&self) -> Option<String> {
            self.reads.set(self.reads.get() + 1);
            self.answers.borrow_mut().pop().expect("an unexpected marker read")
        }
    }

    /// The agy runner: counts runs and answers like an agy-route success.
    struct Agy {
        runs: Cell<u32>,
    }

    impl Agy {
        fn new() -> Self {
            Self { runs: Cell::new(0) }
        }

        async fn run(&self) -> Result<Fetched, ProviderFetchFailure> {
            self.runs.set(self.runs.get() + 1);
            let mut fetched = parse_agy_usage(
                br#"{"status":"SUCCESS","command":{"name":"usage","data":{"groups":[{"name":"G","buckets":[{"id":"b","name":"L","remaining_fraction":0.5}]}]}}}"#,
                Utc::now(),
            )
            .expect("an agy usage body");
            fetched.agy_login_marker = Some(MARKER.to_string());
            Ok(fetched)
        }
    }

    /// The captured account's own result, as `fetch_captured_with` returns it:
    /// `oauth`, the key's scopes, a cache binding and one window.
    async fn captured_ok(io: &FakeIo, key: &str) -> Fetched {
        let (account_scope, history_scope) = io.scopes(key);
        io.quota("ya29.test".to_string(), account_scope.expect("a test scope"), history_scope, Utc::now())
            .await
            .expect("the fake quota answers")
    }

    fn terminal() -> Result<Fetched, ProviderFetchFailure> {
        Err(ProviderFetchFailure::terminal("primary terminal"))
    }

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        CAPTURED_ACCOUNTS_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// T1, T10 (engine side), T16, S7: binding holds, pre == bound == post,
    /// captured Ok. The primary is the captured result relabelled, agy is not
    /// run, and the marker is read exactly twice (pre and post).
    #[tokio::test]
    async fn t1_a_bound_account_replaces_the_agy_run_while_the_login_holds() {
        let io = FakeIo::new("plan-e-t1");
        let a = account("sub-a");
        let reader = Reader::new(&[Some(MARKER), Some(MARKER)]);
        let (bound, marker_pre) = bound_account(
            Some(&binding_for(&a, MARKER)),
            std::slice::from_ref(&a),
            || async { reader.read() },
        )
        .await
        .expect("conditions 1-3 hold");
        assert_eq!(bound, a);
        let captured = captured_ok(&io, &a.key).await;
        assert!(captured.cache_binding.is_some() && captured.history_scope.is_ok());
        let offer = BoundOAuth::from_raw(a.key.clone(), marker_pre, &Ok(captured))
            .expect("condition 4 holds");

        let agy = Agy::new();
        let primary = with_agy_fallback(terminal(), Some(offer), || async { reader.read() }, || agy.run())
            .await
            .expect("the substitution is a card");

        assert_eq!(agy.runs.get(), 0, "agy is not run");
        assert_eq!(reader.reads.get(), 2, "one pre and one post read");
        assert_eq!(primary.source, "oauth");
        assert_eq!(primary.agy_login_marker.as_deref(), Some(MARKER));
        assert_eq!(primary.bound_account_key.as_deref(), Some(a.key.as_str()));
        assert!(primary.cache_binding.is_none(), "T16: no last-good binding");
        assert!(
            matches!(primary.account_scope, Err(AccountScopeError::NoTrustedEvidence)),
            "S7: account scope as the agy route sets it"
        );
        assert!(
            matches!(primary.history_scope, Err(AccountScopeError::NoTrustedEvidence)),
            "S7: no history scope, so history is recorded once, by the captured card"
        );
        assert_eq!(primary.windows.len(), 1);
    }

    /// T2, T10: agy switched accounts between the pre and the post read.
    #[tokio::test]
    async fn t2_a_marker_change_during_the_fetch_takes_the_agy_route() {
        let io = FakeIo::new("plan-e-t2");
        let a = account("sub-a");
        let reader = Reader::new(&[Some(MARKER), Some(OTHER_MARKER)]);
        let (_, marker_pre) = bound_account(
            Some(&binding_for(&a, MARKER)),
            std::slice::from_ref(&a),
            || async { reader.read() },
        )
        .await
        .expect("conditions 1-3 hold");
        let offer = BoundOAuth::from_raw(a.key.clone(), marker_pre, &Ok(captured_ok(&io, &a.key).await));
        assert!(offer.is_some());

        let agy = Agy::new();
        let primary = with_agy_fallback(terminal(), offer, || async { reader.read() }, || agy.run())
            .await
            .expect("the agy route answers");
        assert_eq!(agy.runs.get(), 1, "today's path: agy runs once");
        assert_eq!(primary.source, "agy");
        assert_eq!(primary.bound_account_key, None);
        assert_eq!(reader.reads.get(), 2);
    }

    /// An unreadable live marker at the decision point never matches.
    #[tokio::test]
    async fn an_unreadable_post_marker_fails_closed() {
        let io = FakeIo::new("plan-e-post-unreadable");
        let a = account("sub-a");
        let offer = BoundOAuth::from_raw(a.key.clone(), MARKER.to_string(), &Ok(captured_ok(&io, &a.key).await));
        let agy = Agy::new();
        let primary = with_agy_fallback(terminal(), offer, || async { None }, || agy.run())
            .await
            .unwrap();
        assert_eq!(agy.runs.get(), 1);
        assert_eq!(primary.source, "agy");
    }

    /// T3: the live marker before the fetch is not the bound one (agy is on
    /// another login, signed out, or unreadable): no offer.
    #[tokio::test]
    async fn t3_a_pre_fetch_marker_other_than_the_bound_one_is_not_bound() {
        let a = account("sub-a");
        let binding = binding_for(&a, MARKER);
        for live in [Some(OTHER_MARKER), Some("absent"), Some("present"), None] {
            let reader = Reader::new(&[live]);
            assert_eq!(
                bound_account(Some(&binding), std::slice::from_ref(&a), || async { reader.read() }).await,
                None,
                "{live:?}"
            );
            assert_eq!(reader.reads.get(), 1);
        }
        // Control: the same binding with the bound marker live does bind.
        let reader = Reader::new(&[Some(MARKER)]);
        assert!(bound_account(Some(&binding), std::slice::from_ref(&a), || async { reader.read() })
            .await
            .is_some());
    }

    /// T4: the bound key is not a registered captured account; the marker is
    /// not read.
    #[tokio::test]
    async fn t4_a_bound_key_that_is_not_registered_is_ignored_without_a_marker_read() {
        let a = account("sub-a");
        let b = account("sub-b");
        assert_eq!(bound_account(Some(&binding_for(&a, MARKER)), &[b], no_read).await, None);
        assert_eq!(bound_account(Some(&binding_for(&a, MARKER)), &[], no_read).await, None);
    }

    /// T5: the captured fetch failed, or answered without a window.
    #[tokio::test]
    async fn t5_a_failed_or_empty_captured_result_is_not_offered() {
        let io = FakeIo::new("plan-e-t5");
        let a = account("sub-a");
        let key = || a.key.clone();
        assert!(BoundOAuth::from_raw(
            key(),
            MARKER.to_string(),
            &Err(ProviderFetchFailure::terminal("captured failed"))
        )
        .is_none());
        let mut empty = captured_ok(&io, &a.key).await;
        empty.windows.clear();
        assert!(BoundOAuth::from_raw(key(), MARKER.to_string(), &Ok(empty)).is_none());
        // Control: the same result with its window is offered.
        assert!(BoundOAuth::from_raw(key(), MARKER.to_string(), &Ok(captured_ok(&io, &a.key).await)).is_some());
    }

    /// T6: an earlier route (local IDE, `oauth_creds.json`) succeeded: it wins,
    /// with neither the substitution nor agy, and no post read.
    #[tokio::test]
    async fn t6_an_earlier_route_that_succeeds_wins() {
        let io = FakeIo::new("plan-e-t6");
        let a = account("sub-a");
        let offer = BoundOAuth::from_raw(a.key.clone(), MARKER.to_string(), &Ok(captured_ok(&io, &a.key).await));
        let mut earlier = captured_ok(&io, &account("sub-local").key).await;
        earlier.source = "cli".to_string();
        let agy = Agy::new();
        let primary = with_agy_fallback(Ok(earlier), offer, no_read, || agy.run()).await.unwrap();
        assert_eq!(primary.source, "cli");
        assert_eq!(primary.agy_login_marker, None);
        assert_eq!(primary.bound_account_key, None);
        assert_eq!(agy.runs.get(), 0);
    }

    /// T6b / T14: the earlier routes ended in a Transient failure. It comes
    /// back unchanged (so that route's own last-good still serves), with no
    /// substitution, no agy run and no marker read.
    #[tokio::test]
    async fn t6b_t14_a_transient_primary_failure_is_not_substituted() {
        let io = FakeIo::new("plan-e-t6b");
        let a = account("sub-a");
        let offer = BoundOAuth::from_raw(a.key.clone(), MARKER.to_string(), &Ok(captured_ok(&io, &a.key).await));
        let agy = Agy::new();
        let transient = ProviderFetchFailure::transient(
            "primary transient",
            None,
            SafeTransportDiagnostic::from_facts(TransportErrorFacts::synthetic(
                true,
                false,
                TransportPhase::Request,
                None,
            )),
        );
        let result = with_agy_fallback(Err(transient), offer, no_read, || agy.run()).await;
        match result {
            Err(ProviderFetchFailure::Transient { display, .. }) => assert_eq!(display, "primary transient"),
            other => panic!("expected the transient failure unchanged, got {other:?}"),
        }
        assert_eq!(agy.runs.get(), 0);
    }

    /// T7 (regression guard): no binding, no marker read, today's route.
    #[tokio::test]
    async fn t7_no_binding_reads_nothing_and_runs_agy() {
        assert_eq!(bound_account(None, &[account("sub-a")], no_read).await, None);
        let agy = Agy::new();
        let primary = with_agy_fallback(terminal(), None, unread_marker, || agy.run()).await.unwrap();
        assert_eq!((agy.runs.get(), primary.source.as_str()), (1, "agy"));
    }

    /// T13-mac (S1): the marker parser is anchored at both ends, and a binding
    /// whose marker names no single login write never binds, even forced past
    /// the setter, and without a marker read; nor does a malformed key.
    #[tokio::test]
    async fn t13_only_a_parsed_mdat_marker_binds() {
        // The two real shapes in this file: the full hex, and the truncated
        // hex `login_marker_runs_the_attributes_only_query` records.
        assert_eq!(
            mdat_marker_time(MARKER),
            NaiveDateTime::parse_from_str("2026-09-23 17:40:56", "%Y-%m-%d %H:%M:%S").ok()
        );
        assert!(mdat_marker_time(r#"0x3230  "20261002101010Z\000""#).is_some());
        let rejected = [
            "present".to_string(),
            "absent".to_string(),
            String::new(),
            format!("{MARKER}x"),
            format!("{MARKER} "),
            format!("x{MARKER}"),
            format!(" {MARKER}"),
            format!("present{MARKER}"),
            r#"0x  "20260923174056Z\000""#.to_string(),
            r#"0xZZ  "20260923174056Z\000""#.to_string(),
            r#"0x32 "20260923174056Z\000""#.to_string(),
            r#"0x32  "2026092317405Z\000""#.to_string(),
            r#"0x32  "202609231740567Z\000""#.to_string(),
            r#"0x32  "20261323174056Z\000""#.to_string(),
            r#"0x32  "20260923174056Z""#.to_string(),
            r#"0x32  "2026092317405aZ\000""#.to_string(),
        ];
        let a = account("sub-a");
        for marker in &rejected {
            assert_eq!(mdat_marker_time(marker), None, "{marker:?}");
            assert_eq!(
                bound_account(Some(&binding_for(&a, marker)), std::slice::from_ref(&a), no_read).await,
                None,
                "{marker:?}"
            );
        }
        let bad_key = AntigravityBinding {
            key: a.key.to_uppercase(),
            marker: MARKER.to_string(),
        };
        let registered = CapturedAccount {
            key: bad_key.key.clone(),
            label: String::new(),
        };
        assert_eq!(bound_account(Some(&bad_key), &[registered], no_read).await, None);
    }

    /// T11, T13 (setter): every bad input clears the stored binding first and
    /// answers one fixed code, never echoing the input (S6, canary); NULL and
    /// `{"key":null}` clear with `{"bound":false}`.
    #[test]
    fn t11_t13_the_binding_setter_clears_on_every_bad_input() {
        let _guard = lock();
        let a = account("sub-a");
        let valid = serde_json::to_vec(&json!({ "key": a.key, "marker": MARKER })).unwrap();
        let set = set_antigravity_binding;
        let with_marker = |marker: &str| serde_json::to_vec(&json!({ "key": a.key, "marker": marker })).unwrap();

        assert_eq!(set(Some(&valid)).unwrap(), json!({ "bound": true }));
        assert!(antigravity_binding() == Some(binding_for(&a, MARKER)));

        let cases: Vec<(Vec<u8>, &str)> = vec![
            (b"{not json CANARY".to_vec(), "invalid_binding_json"),
            (br#"{"key":"CANARY","marker":"#.to_vec(), "invalid_binding_json"),
            (vec![b'"', 0xff, 0xfe, b'"'], "invalid_binding_json"),
            (b"null".to_vec(), "invalid_binding_json"),
            (b"[]".to_vec(), "invalid_binding_json"),
            (b"\"CANARY\"".to_vec(), "invalid_binding_json"),
            (b"{}".to_vec(), "invalid_binding_json"),
            (serde_json::to_vec(&json!({ "key": 7, "marker": MARKER })).unwrap(), "invalid_binding_json"),
            (serde_json::to_vec(&json!({ "key": a.key, "marker": 7 })).unwrap(), "invalid_binding_json"),
            (serde_json::to_vec(&json!({ "key": null, "marker": MARKER })).unwrap(), "invalid_key"),
            (serde_json::to_vec(&json!({ "key": "CANARY", "marker": MARKER })).unwrap(), "invalid_key"),
            (serde_json::to_vec(&json!({ "key": a.key })).unwrap(), "invalid_marker"),
            (with_marker(""), "invalid_marker"),
            (with_marker("absent"), "invalid_marker"),
            (with_marker("present"), "invalid_marker"),
            (with_marker(&format!("{MARKER}CANARY")), "invalid_marker"),
            (with_marker(&format!("CANARY{MARKER}")), "invalid_marker"),
        ];
        for (raw, code) in cases {
            set(Some(&valid)).unwrap();
            let shown = String::from_utf8_lossy(&raw).into_owned();
            let answer = set(Some(&raw));
            assert_eq!(answer, Err(code.to_string()), "{shown}");
            assert!(!answer.unwrap_err().contains("CANARY"), "{shown}");
            assert!(antigravity_binding().is_none(), "cleared: {shown}");
        }

        for clear in [None, Some(&br#"{"key":null}"#[..])] {
            set(Some(&valid)).unwrap();
            assert_eq!(set(clear).unwrap(), json!({ "bound": false }));
            assert!(antigravity_binding().is_none());
        }
    }

    /// T11: the setter's "cleared" is what the fetch reads: after a refused
    /// input the next fetch finds no binding and reads no marker.
    // The guard serializes tests that share the process-wide binding; the
    // test runtime is single-threaded, so holding it across an await blocks
    // nothing but the other binding tests.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn t11_a_refused_binding_leaves_the_next_fetch_unbound() {
        let _guard = lock();
        let a = account("sub-a");
        let valid = serde_json::to_vec(&json!({ "key": a.key, "marker": MARKER })).unwrap();
        set_antigravity_binding(Some(&valid)).unwrap();
        let reader = Reader::new(&[Some(MARKER)]);
        assert!(bound_account(antigravity_binding().as_ref(), std::slice::from_ref(&a), || async {
            reader.read()
        })
        .await
        .is_some());
        let refused = serde_json::to_vec(&json!({ "key": a.key, "marker": "present" })).unwrap();
        assert!(set_antigravity_binding(Some(&refused)).is_err());
        assert_eq!(bound_account(antigravity_binding().as_ref(), &[a], no_read).await, None);
        set_antigravity_binding(None).unwrap();
    }

    /// S3a: the binding is read once, at the start of the fetch. A setter call
    /// between the pre read and the decision (another poll binding another
    /// account) does not change the key the substituted primary carries.
    // The guard serializes tests that share the process-wide binding; the
    // test runtime is single-threaded, so holding it across an await blocks
    // nothing but the other binding tests.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn s3a_a_binding_set_mid_fetch_does_not_change_the_stamped_key() {
        let _guard = lock();
        let io = FakeIo::new("plan-e-s3a");
        let (a, b) = (account("sub-a"), account("sub-b"));
        let set_b = || {
            let raw = serde_json::to_vec(&json!({ "key": b.key, "marker": MARKER })).unwrap();
            set_antigravity_binding(Some(&raw)).unwrap();
        };
        let raw_a = serde_json::to_vec(&json!({ "key": a.key, "marker": MARKER })).unwrap();
        set_antigravity_binding(Some(&raw_a)).unwrap();

        let binding = antigravity_binding();
        let accounts = [a.clone(), b.clone()];
        let (bound, marker_pre) = bound_account(binding.as_ref(), &accounts, || async {
            set_b();
            Some(MARKER.to_string())
        })
        .await
        .expect("bound to a");
        assert_eq!(bound, a);
        let offer = BoundOAuth::from_raw(bound.key.clone(), marker_pre, &Ok(captured_ok(&io, &a.key).await));
        let agy = Agy::new();
        let primary = with_agy_fallback(
            terminal(),
            offer,
            || async {
                set_b();
                Some(MARKER.to_string())
            },
            || agy.run(),
        )
        .await
        .unwrap();
        assert!(antigravity_binding().is_some_and(|now| now.key == b.key), "the global moved to b");
        assert_eq!(primary.bound_account_key.as_deref(), Some(a.key.as_str()));
        assert_eq!(agy.runs.get(), 0);
        set_antigravity_binding(None).unwrap();
    }
}
