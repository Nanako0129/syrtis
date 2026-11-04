//! Process-wide registry of the macOS Keychain reads the user has agreed to.
//! Written by `tb_set_keychain_consent`, read by the provider adapters that
//! would otherwise put a system authorization dialog in front of the user with
//! no warning.
//!
//! Reading a Keychain item makes macOS — not Syrtis — ask the question, and
//! by then it is too late to explain what is being read or why. An adapter
//! therefore asks this registry first and declines to reach the Keychain at
//! all until the answer is yes. The app owns the explanation and the "Allow"
//! button; this registry is only how that answer crosses the FFI boundary.
//!
//! Two consumers, each gated on its own id. `grok-bot`:
//! `agent_grokbot::decode_desktop_secret`, which checks it immediately before
//! its `decrypt` call — at the decrypt and not at "a desktop login exists",
//! because a `plaintext:v1:` secret needs no key and there is nothing to ask
//! about. `cursor`: `cursor_sync::SealedRound::open`, checked only for a
//! sealed (`cursor-safe-storage:`) token, before the cached key is used or the
//! Keychain is read; a plaintext Cursor token never consults it. A payload
//! that does not grant `cursor` also drops the Cursor key the sync keeps in
//! memory (`cursor_sync::drop_sealed_key`).
//!
//! **Deliberately two-valued.** "Never asked" and "declined" are the same
//! thing to an adapter — in both cases it must not touch the Keychain — so
//! only granted ids are stored. The third state lives in Swift, where it picks
//! the copy (a full explanation the first time, a one-line row with a way back
//! afterwards).
//!
//! A `RwLock` static rather than an env var, for the same reason
//! `extra_scan_paths` is one: the process is resident and `std::env::set_var`
//! is unsafe once the scan pool's threads are running, so an env-var-backed
//! answer could only take effect after a restart. An "Allow" button that needs
//! a relaunch is not an Allow button.
//!
//! The registry starts empty every launch — it is in-memory, not persisted —
//! so Swift owns re-applying the user's stored answer at startup. The empty
//! default is the correct behaviour for a process that never calls the setter:
//! no Grok Bot or Cursor Keychain contact, no dialog for either.
//!
//! **This is not a process-wide no-Keychain guarantee.** `agent_usage` reads
//! `Claude Code-credentials` and `tokenbar-claude-oauth-token` through
//! `/usr/bin/security` without consulting anything here, so a protected Claude
//! item can still raise a dialog. That client is out of scope on purpose — it
//! has shipped for a long time, its users have already answered the ACL, and
//! putting the flagship card behind a consent gate is a separate product
//! decision. Because this registry is client-keyed, adding it later is a UI
//! change rather than an ABI change.

use std::collections::BTreeSet;
use std::sync::{LazyLock, RwLock};

/// Public client ids this consumer wires Keychain consent for. An id outside
/// this list is refused rather than stored, so a typo in the payload cannot
/// register a grant that nothing will ever read — and cannot be mistaken for
/// one that is being honoured.
const CONSENTABLE_CLIENTS: &[&str] = &["grok-bot", "cursor"];

static KEYCHAIN_CONSENT: LazyLock<RwLock<BTreeSet<String>>> =
    LazyLock::new(|| RwLock::new(BTreeSet::new()));

/// Whether the user has agreed to let Syrtis read this client's Keychain
/// item. `false` by default, which is what makes "we never asked" and "the
/// user said no" behave identically without either being stored.
pub(crate) fn allowed(client_id: &str) -> bool {
    KEYCHAIN_CONSENT
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains(client_id)
}

/// Replace the whole registry from `{"<public-client-id>": true|false}`.
/// Full-replace, not merge: `{}` clears every grant, and an id mapped to
/// `false` is simply absent afterwards. Success data is
/// `{"grantedCount":N,"rejected":[{"client","reason"}]}`.
///
/// A `false` entry is not an error and is not reported as rejected — Swift
/// sends the user's stored answer verbatim, and "no" is a legitimate answer.
/// Only an unrecognised client id is rejected.
pub(crate) fn set_from_json(raw: &str) -> Result<serde_json::Value, String> {
    let input: std::collections::BTreeMap<String, bool> =
        serde_json::from_str(raw).map_err(|e| format!("invalid Keychain consent JSON: {}", e))?;

    let mut granted: BTreeSet<String> = BTreeSet::new();
    let mut rejected: Vec<serde_json::Value> = Vec::new();
    for (client_id, allowed) in input {
        if !CONSENTABLE_CLIENTS.contains(&client_id.as_str()) {
            rejected.push(serde_json::json!({
                "client": client_id,
                "reason": "client does not read the Keychain",
            }));
            continue;
        }
        if allowed {
            granted.insert(client_id);
        }
    }

    let granted_count = granted.len();
    let cursor_granted = granted.contains("cursor");
    *KEYCHAIN_CONSENT
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = granted;
    // After the write lock is released (a round holds the key cache while it
    // reads consent): no grant, no key in memory (U3, Design 4).
    if !cursor_granted {
        crate::cursor_sync::drop_sealed_key();
    }

    Ok(serde_json::json!({
        "grantedCount": granted_count,
        "rejected": rejected,
    }))
}

/// One process-wide mutex for every test that reads or writes the static, so
/// parallel `cargo test` threads do not observe each other's registry.
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) fn reset_for_test() {
    *KEYCHAIN_CONSENT
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = BTreeSet::new();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_and_clears() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset_for_test();

        assert!(
            !allowed("grok-bot"),
            "an untouched registry must deny, or a process that never calls the \
             setter would raise the dialog this registry exists to prevent"
        );

        let result = set_from_json(r#"{"grok-bot":true}"#).unwrap();
        assert_eq!(result["grantedCount"], 1);
        assert!(allowed("grok-bot"));

        // `false` is an answer, not an error: it clears the grant and is not
        // reported as a rejection.
        let result = set_from_json(r#"{"grok-bot":false}"#).unwrap();
        assert_eq!(result["grantedCount"], 0);
        assert!(result["rejected"].as_array().unwrap().is_empty());
        assert!(!allowed("grok-bot"));

        // Full-replace, including back to nothing.
        set_from_json(r#"{"grok-bot":true}"#).unwrap();
        set_from_json("{}").unwrap();
        assert!(!allowed("grok-bot"));
        reset_for_test();
    }

    #[test]
    fn an_unwired_client_id_is_refused_rather_than_stored() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset_for_test();

        let result = set_from_json(r#"{"grok-bot":true,"claude":true}"#).unwrap();
        assert_eq!(result["grantedCount"], 1);
        assert_eq!(result["rejected"][0]["client"], "claude");
        assert!(allowed("grok-bot"));
        assert!(
            !allowed("claude"),
            "storing a grant nothing reads would report consent for a dialog \
             that still appears unannounced"
        );
        reset_for_test();
    }

    #[test]
    fn malformed_json_leaves_the_registry_untouched() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset_for_test();
        set_from_json(r#"{"grok-bot":true}"#).unwrap();
        let error = set_from_json("{not json").unwrap_err();
        assert!(error.contains("invalid Keychain consent JSON"), "{error}");
        assert!(allowed("grok-bot"));
        reset_for_test();
    }
}
