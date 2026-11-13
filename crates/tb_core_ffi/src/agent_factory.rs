//! Factory (Droid) subscription quota from `/api/billing/limits`.
//!
//! Factory's documented Analytics API (`/api/v1/analytics/cost/me/query`) is
//! Enterprise-only and reports consumption without an allowance, so it cannot
//! draw a quota card. The Droid CLI itself reads `/api/billing/limits` at
//! startup; that endpoint accepts a Factory API key and reports, per tier
//! (`standard`, `core`), a five-hour, weekly and monthly window as a
//! used-percent with a `windowEnd`. Each present window maps to one
//! `UsageWindow`.
//!
//! The API key is not discovered from Droid's own login store: Droid keeps its
//! session encrypted under a Keychain key Syrtis would have to ask the user to
//! release. Instead the user pastes a Factory API key into Settings → Accounts
//! → Factory, which stores it in the login Keychain under [`KEYCHAIN_SERVICE`]
//! through `/usr/bin/security` (`FactoryAPIKey.swift`); reading it back
//! through the same tool raises no authorization dialog. With no such item the
//! provider is absent and no card is published.
//!
//! Window rules follow `agent_opencode_go.rs` and `provider-quota-pace.md`: an
//! out-of-range or non-finite percent and an already-expired `windowEnd` drop
//! only that window; a tier or window that fails to decode never poisons its
//! siblings; a response with no usable window is terminal. The five-hour and
//! weekly windows carry a contract duration; the monthly window follows the
//! billing cycle, whose length varies, so pace learns it.

use crate::agent_account_scope::{self, AccountScope, AccountScopeError};
use crate::agent_quota_duration::DurationEvidence;
use crate::agent_usage::{
    parse_datetime, provider_http_client_builder, read_response_body,
    request_after_verified_binding, AgentIdentity, ProviderCacheBinding, ProviderFetchFailure,
    ResponseReadFailure, TransportErrorFacts, TransportPhase, UsageWindow,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer};
use serde_json::{value::RawValue, Value};

const LIMITS_URL: &str = "https://api.factory.ai/api/billing/limits";

/// Login-Keychain service holding the Factory API key. Written by the Settings
/// field (`FactoryAPIKey.keychainService` in Swift, the same literal).
pub(crate) const KEYCHAIN_SERVICE: &str = "Syrtis Factory API Key";

/// `security(1)` exit status for "The specified item could not be found".
const SECURITY_ITEM_NOT_FOUND: i32 = 44;

const HOUR: i64 = 3_600;
const DAY: i64 = 24 * HOUR;

pub(crate) struct FactoryCredential {
    request_token: String,
}

pub(crate) enum FactoryCredentialLoad {
    Absent,
    Terminal(String),
    Present(FactoryCredential),
}

pub(crate) struct FactoryData {
    pub identity: Option<AgentIdentity>,
    pub account_scope: Result<AccountScope, AccountScopeError>,
    pub cache_binding: ProviderCacheBinding,
    pub windows: Vec<UsageWindow>,
}

/// Read the API key from the login Keychain. A missing item is `Absent` (the
/// user has not set Factory up); any other `security` failure, such as a locked
/// Keychain, is terminal so the card says why instead of silently vanishing.
#[cfg(target_os = "macos")]
pub(crate) async fn factory_credential() -> FactoryCredentialLoad {
    let output = tokio::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .await;
    let output = match output {
        Ok(output) => output,
        Err(_) => {
            return FactoryCredentialLoad::Terminal(
                "Factory API key could not be read from the Keychain.".to_string(),
            )
        }
    };
    if output.status.code() == Some(SECURITY_ITEM_NOT_FOUND) {
        return FactoryCredentialLoad::Absent;
    }
    if !output.status.success() {
        return FactoryCredentialLoad::Terminal(
            "Factory API key could not be read from the Keychain.".to_string(),
        );
    }
    let key = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if key.is_empty() {
        return FactoryCredentialLoad::Absent;
    }
    FactoryCredentialLoad::Present(FactoryCredential { request_token: key })
}

#[cfg(not(target_os = "macos"))]
pub(crate) async fn factory_credential() -> FactoryCredentialLoad {
    FactoryCredentialLoad::Absent
}

#[derive(Debug, Deserialize)]
struct LimitsResponse {
    #[serde(default, rename = "hasActiveSubscription")]
    has_active_subscription: Option<bool>,
    #[serde(default)]
    limits: Option<Tiers>,
}

/// Each tier is kept raw so a malformed tier drops alone.
#[derive(Debug, Default, Deserialize)]
struct Tiers {
    #[serde(default)]
    standard: Option<Box<RawValue>>,
    #[serde(default)]
    core: Option<Box<RawValue>>,
}

/// Each window is kept raw so a malformed window drops alone.
#[derive(Debug, Default, Deserialize)]
struct TierWindows {
    #[serde(default, rename = "fiveHour")]
    five_hour: Option<Box<RawValue>>,
    #[serde(default)]
    weekly: Option<Box<RawValue>>,
    #[serde(default)]
    monthly: Option<Box<RawValue>>,
}

#[derive(Debug, Deserialize)]
struct Window {
    #[serde(default, rename = "usedPercent")]
    used_percent: Option<f64>,
    // `windowEnd` is `null` for a window with no activity yet; any non-string
    // value is treated the same way rather than failing the row.
    #[serde(
        default,
        rename = "windowEnd",
        deserialize_with = "deserialize_optional_string"
    )]
    window_end: Option<String>,
}

fn deserialize_optional_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(
        Option::<Value>::deserialize(deserializer)?.and_then(|value| match value {
            Value::String(value) => Some(value),
            _ => None,
        }),
    )
}

/// Reads the clock after the response arrives (see `agent_opencode_go::fetch`):
/// the expired-window test compares against the present.
pub(crate) async fn fetch(
    credential: FactoryCredential,
) -> Result<FactoryData, ProviderFetchFailure> {
    let verified = agent_account_scope::resolve_credential(
        "factory",
        "syrtis-keychain",
        KEYCHAIN_SERVICE,
        credential.request_token.as_bytes(),
    )
    .map(|account_scope| {
        let cache_binding = ProviderCacheBinding::primary(account_scope.clone());
        (account_scope, cache_binding)
    })
    .map_err(|_| ProviderFetchFailure::terminal("Factory account identity could not be verified."));
    let (account_scope, cache_binding, response) =
        request_after_verified_binding(verified, |(account_scope, cache_binding)| async move {
            let client = provider_http_client_builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .map_err(|_| {
                    ProviderFetchFailure::terminal("Factory usage client could not be created.")
                })?;
            let response = client
                .get(LIMITS_URL)
                .header(
                    reqwest::header::AUTHORIZATION,
                    format!("Bearer {}", credential.request_token),
                )
                .header(reqwest::header::ACCEPT, "application/json")
                .send()
                .await
                .map_err(|error| {
                    ProviderFetchFailure::from_send_error(
                        "Factory usage request failed. Retrying automatically.",
                        Some(cache_binding.clone()),
                        &error,
                    )
                })?;
            Ok((account_scope, cache_binding, response))
        })
        .await?;
    let status = response.status().as_u16();
    let body = read_response_body(status, false, || async {
        response.text().await.map_err(|error| {
            TransportErrorFacts::from_reqwest(&error, TransportPhase::ResponseBody)
        })
    })
    .await
    .map_err(|failure| match failure {
        ResponseReadFailure::Transient(diagnostic) => ProviderFetchFailure::transient(
            "Factory usage request failed. Retrying automatically.",
            Some(cache_binding.clone()),
            diagnostic,
        ),
        ResponseReadFailure::Terminal(401 | 403) => {
            ProviderFetchFailure::terminal("Factory API key is invalid or lacks access.")
        }
        ResponseReadFailure::Terminal(status) => ProviderFetchFailure::terminal(format!(
            "Factory billing API rejected the request (status {status})."
        )),
    })?;
    let windows = decode_limits_response(&body, Utc::now())?;
    Ok(FactoryData {
        identity: None,
        account_scope: Ok(account_scope),
        cache_binding,
        windows,
    })
}

pub(crate) fn decode_limits_response(
    body: &str,
    now: DateTime<Utc>,
) -> Result<Vec<UsageWindow>, ProviderFetchFailure> {
    let response: LimitsResponse = serde_json::from_str(body).map_err(|_| {
        ProviderFetchFailure::terminal("Factory billing response could not be decoded.")
    })?;
    let windows = response
        .limits
        .as_ref()
        .map(|tiers| map_tiers(tiers, now))
        .unwrap_or_default();
    if windows.is_empty() {
        if response.has_active_subscription == Some(false) {
            return Err(ProviderFetchFailure::terminal(
                "This Factory account has no active subscription.",
            ));
        }
        // A 200 with no usable window is more likely a malformed payload than
        // real zero usage; do not present a healthy 0% card for it.
        return Err(ProviderFetchFailure::terminal(
            "Factory billing API returned no usable quota windows.",
        ));
    }
    Ok(windows)
}

/// Standard before Core, and within a tier the shortest window first.
fn map_tiers(tiers: &Tiers, now: DateTime<Utc>) -> Vec<UsageWindow> {
    let mut windows = Vec::new();
    for (raw, tier_label, tier_key) in [
        (&tiers.standard, "Standard", "standard"),
        (&tiers.core, "Core", "core"),
    ] {
        let Some(tier) = raw
            .as_deref()
            .and_then(|raw| serde_json::from_str::<TierWindows>(raw.get()).ok())
        else {
            continue;
        };
        for (raw, label, key, contract_seconds) in [
            (&tier.five_hour, "5h", "five_hour", Some(5 * HOUR)),
            (&tier.weekly, "Weekly", "weekly", Some(7 * DAY)),
            (&tier.monthly, "Monthly", "monthly", None),
        ] {
            let card_id = format!("{tier_key}.{key}.v1");
            let label = format!("{tier_label} {label}");
            if let Some(window) = map_window(raw.as_deref(), label, card_id, contract_seconds, now)
            {
                windows.push(window);
            }
        }
    }
    windows
}

fn map_window(
    raw: Option<&RawValue>,
    label: String,
    card_id: String,
    contract_seconds: Option<i64>,
    now: DateTime<Utc>,
) -> Option<UsageWindow> {
    let window: Window = serde_json::from_str(raw?.get()).ok()?;
    let percent = window.used_percent?;
    let resets_at = window.window_end.as_deref().and_then(parse_datetime);
    // An expired window end is invalid evidence (provider-quota-pace.md):
    // drop it rather than emit a card dated to a cycle that already ended.
    if resets_at.is_some_and(|reset| reset <= now) {
        return None;
    }
    UsageWindow::try_from_provider_used_percent(label, percent, resets_at, now).map(|window| {
        window.with_identity(
            card_id.clone(),
            Some(card_id),
            None,
            contract_seconds.map(DurationEvidence::contract),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        // 2026-11-13T05:00:00Z, shortly before the windows in `LIVE_SHAPE` end.
        Utc.timestamp_opt(1_794_546_000, 0).single().unwrap()
    }

    /// The response shape the live endpoint returned on 2026-11-13, with every
    /// value replaced by an illustrative one.
    const LIVE_SHAPE: &str = r#"{
        "usesTokenRateLimitsBilling": true,
        "hasActiveSubscription": true,
        "limits": {
            "standard": {
                "fiveHour": {"usedPercent": 30, "windowEnd": "2026-11-13T08:00:00.000Z", "secondsRemaining": 10800},
                "weekly":   {"usedPercent": 15, "windowEnd": "2026-11-17T00:00:00.000Z", "secondsRemaining": 327600},
                "monthly":  {"usedPercent": 20, "windowEnd": "2026-12-01T00:00:00.000Z", "secondsRemaining": 1537200}
            },
            "core": {
                "fiveHour": {"usedPercent": 0, "windowEnd": null, "secondsRemaining": null},
                "weekly":   {"usedPercent": 45, "windowEnd": "2026-11-15T00:00:00.000Z", "secondsRemaining": 154800},
                "monthly":  {"usedPercent": 60, "windowEnd": "2026-11-25T00:00:00.000Z", "secondsRemaining": 1018800}
            }
        },
        "overagePreference": "fixture",
        "canManageOverage": true,
        "extraUsageBalanceCents": 0,
        "extraUsageAllowed": true
    }"#;

    #[test]
    fn maps_both_tiers_standard_first_and_shortest_window_first() {
        let windows = decode_limits_response(LIVE_SHAPE, now()).unwrap();
        let labels: Vec<_> = windows.iter().map(|w| w.label_for_test()).collect();
        assert_eq!(
            labels,
            [
                "Standard 5h",
                "Standard Weekly",
                "Standard Monthly",
                "Core 5h",
                "Core Weekly",
                "Core Monthly"
            ]
        );
        let keys: Vec<_> = windows
            .iter()
            .map(|w| w.pace_window_key_for_test())
            .collect();
        assert_eq!(
            keys,
            [
                Some("standard.five_hour.v1"),
                Some("standard.weekly.v1"),
                Some("standard.monthly.v1"),
                Some("core.five_hour.v1"),
                Some("core.weekly.v1"),
                Some("core.monthly.v1"),
            ]
        );
        // Used 30 -> remaining 70.
        assert!((windows[0].remaining_for_test() - 70.0).abs() < 0.01);
        assert_eq!(
            windows[0].resets_at_for_test(),
            Some("2026-11-13T08:00:00.000Z")
        );
    }

    #[test]
    fn five_hour_and_weekly_carry_contract_durations_monthly_learns() {
        let windows = decode_limits_response(LIVE_SHAPE, now()).unwrap();
        assert_eq!(windows[0].window_minutes_for_test(), Some(5 * 60));
        assert_eq!(windows[1].window_minutes_for_test(), Some(7 * 24 * 60));
        assert_eq!(windows[2].window_minutes_for_test(), None);
    }

    #[test]
    fn an_idle_window_with_a_null_end_is_kept_without_a_reset() {
        let windows = decode_limits_response(LIVE_SHAPE, now()).unwrap();
        let core_five_hour = &windows[3];
        assert_eq!(core_five_hour.label_for_test(), "Core 5h");
        assert_eq!(core_five_hour.resets_at_for_test(), None);
        assert!((core_five_hour.remaining_for_test() - 100.0).abs() < 0.01);
    }

    #[test]
    fn invalid_or_expired_windows_drop_without_poisoning_siblings() {
        let body = r#"{
            "hasActiveSubscription": true,
            "limits": {
                "standard": {
                    "fiveHour": {"usedPercent": 140, "windowEnd": "2026-11-13T07:00:00Z"},
                    "weekly":   {"usedPercent": "oops", "windowEnd": "2026-11-16T00:00:00Z"},
                    "monthly":  {"usedPercent": 30, "windowEnd": "2026-11-01T00:00:00Z"}
                },
                "core": {
                    "weekly":   {"usedPercent": 40, "windowEnd": {"unexpected": true}},
                    "monthly":  {"usedPercent": 1e400}
                }
            }
        }"#;
        let windows = decode_limits_response(body, now()).unwrap();
        let labels: Vec<_> = windows.iter().map(|w| w.label_for_test()).collect();
        assert_eq!(
            labels,
            ["Core Weekly"],
            "out-of-range, wrong-typed, expired and non-finite drop"
        );
        assert_eq!(windows[0].resets_at_for_test(), None);
        assert!(!serde_json::to_string(&windows[0])
            .unwrap()
            .contains("unexpected"));
    }

    #[test]
    fn a_malformed_tier_does_not_drop_the_other_tier() {
        let body = r#"{
            "limits": {
                "standard": "not an object",
                "core": {"weekly": {"usedPercent": 45, "windowEnd": "2026-11-15T00:00:00.000Z"}}
            }
        }"#;
        let windows = decode_limits_response(body, now()).unwrap();
        let labels: Vec<_> = windows.iter().map(|w| w.label_for_test()).collect();
        assert_eq!(labels, ["Core Weekly"]);
    }

    #[test]
    fn keychain_service_is_the_name_the_settings_field_writes() {
        // `FactoryAPIKey.keychainService` in Swift writes this item; SelfTest
        // pins the same literal on that side.
        assert_eq!(KEYCHAIN_SERVICE, "Syrtis Factory API Key");
    }

    #[test]
    fn no_usable_window_is_terminal() {
        for body in [
            r#"{}"#,
            r#"{"limits": {}}"#,
            r#"{"hasActiveSubscription": false}"#,
            r#"{"limits": {"standard": {"weekly": {"usedPercent": null}}}}"#,
            r#"not json"#,
        ] {
            assert!(
                matches!(
                    decode_limits_response(body, now()),
                    Err(ProviderFetchFailure::Terminal { .. })
                ),
                "{body}"
            );
        }
    }
}
