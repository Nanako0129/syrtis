//! Opaque account identity for provider quota history.
//!
//! The installation key lives in an owner-only binary file beside the authenticated
//! metadata. Raw provider identifiers and credential markers are reduced to
//! domain-separated HMACs before metadata is persisted. Provider adapters only get
//! an opaque scope or a typed failure; no raw identity crosses into history.

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use fs2::FileExt as _;
use hmac::{Hmac, Mac as _};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

const INSTALLATION_KEY_FILE: &str = "quota-account-scope-installation-key-v1.bin";
const METADATA_FILE: &str = "quota-account-scope-v1.json";
const METADATA_LOCK_FILE: &str = "quota-account-scope-v1.lock";
const V3_HISTORY_FILE: &str = "quota-pace-history-v3.json";
const METADATA_SCHEMA_VERSION: u32 = 1;
const INSTALLATION_KEY_BYTES: usize = 32;
const LINEAGE_ID_BYTES: usize = 16;
const DIGEST_BYTES: usize = 32;

static ACCOUNT_SCOPE_PROCESS_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static CODEX_REFRESH_LOCK: Mutex<()> = Mutex::new(());
static CLAUDE_REFRESH_LOCK: Mutex<()> = Mutex::new(());
static GROK_REFRESH_LOCK: Mutex<()> = Mutex::new(());
static ANTIGRAVITY_REFRESH_LOCK: Mutex<()> = Mutex::new(());
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct AccountScope(String);

impl AccountScope {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// Distinct scopes for tests that only need identity, not a real lineage.
    #[cfg(test)]
    pub(crate) fn for_test(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl std::fmt::Debug for AccountScope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("AccountScope(<opaque>)")
    }
}

/// Identity for the durable quota-pace history store, deliberately *not* an
/// `AccountScope`.
///
/// An `AccountScope` is derived from the live credential when no authoritative
/// owner ID exists, so a sibling application rotating the shared refresh token
/// mints a new one. That is correct for a cache binding, whose whole job is to
/// refuse another account's data, and destructive for a series that has to span
/// weeks. There is deliberately no conversion in either direction: passing an
/// `AccountScope` to a durable-history writer must not compile.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct HistoryScope(String);

impl HistoryScope {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// Distinct scopes for tests that only need identity, not a real derivation.
    #[cfg(test)]
    pub(crate) fn for_test(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl std::fmt::Debug for HistoryScope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("HistoryScope(<opaque>)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthoritativeIdKind {
    Email,
    OpaqueId,
}

impl AuthoritativeIdKind {
    fn domain_value(self) -> &'static str {
        match self {
            Self::Email => "email",
            Self::OpaqueId => "opaque-id",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccountScopeError {
    NoTrustedEvidence,
    InvalidEvidence,
    /// Returned by `SystemBackend::random_bytes` on a target neither secure
    /// storage path covers, so it is unreachable on macOS and Windows — the
    /// only two the app ships for. Matching it in `Display` does not count as
    /// constructing it, which is what the warning is about. Cfg-gating the
    /// variant would force the same cfg onto that arm for no gain.
    #[allow(dead_code)]
    UnsupportedPlatform,
    InstallationKeyRead,
    InvalidInstallationKey,
    InstallationKeyWrite,
    OrphanedArtifacts,
    RandomUnavailable,
    StorageUnavailable,
    MetadataLock,
    MetadataRead,
    MetadataCorrupt,
    MetadataConflict,
    MetadataWrite,
    QuarantineFailed,
}

impl std::fmt::Display for AccountScopeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::NoTrustedEvidence => "no trusted account evidence",
            Self::InvalidEvidence => "invalid account evidence",
            Self::UnsupportedPlatform => "secure account scope is unavailable on this platform",
            Self::InstallationKeyRead => "installation key could not be read",
            Self::InvalidInstallationKey => "installation key failed validation",
            Self::InstallationKeyWrite => "installation key could not be saved",
            Self::OrphanedArtifacts => "account-scope artifacts were orphaned after key loss",
            Self::RandomUnavailable => "secure randomness is unavailable",
            Self::StorageUnavailable => "account-scope storage is unavailable",
            Self::MetadataLock => "account-scope metadata lock failed",
            Self::MetadataRead => "account-scope metadata could not be read",
            Self::MetadataCorrupt => "account-scope metadata failed authentication",
            Self::MetadataConflict => "account-scope metadata contains conflicting bindings",
            Self::MetadataWrite => "account-scope metadata could not be saved",
            Self::QuarantineFailed => "account-scope metadata could not be quarantined",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for AccountScopeError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FsOperation {
    CreateDirectory,
    ReadInstallationKey,
    ValidateInstallationKey,
    InspectArtifacts,
    InspectOrphanedMetadata,
    OpenMetadataLock,
    AcquireMetadataLock,
    ReadMetadata,
    QuarantineMetadata,
    CreateTemp,
    WriteTemp,
    SyncTemp,
    ReplaceFile,
    SyncDirectory,
    OpenRefreshLock,
    AcquireRefreshLock,
}

trait Backend {
    fn random_bytes(&self, length: usize) -> Result<Vec<u8>, AccountScopeError>;
    fn storage_dir(&self) -> Result<PathBuf, AccountScopeError>;
    fn now_seconds(&self) -> i64;
    fn before_fs(&self, _operation: FsOperation) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
struct SystemBackend;

impl Backend for SystemBackend {
    #[cfg(target_os = "macos")]
    fn random_bytes(&self, length: usize) -> Result<Vec<u8>, AccountScopeError> {
        let mut bytes = vec![0_u8; length];
        security_framework::random::SecRandom::default()
            .copy_bytes(&mut bytes)
            .map_err(|_| AccountScopeError::RandomUnavailable)?;
        Ok(bytes)
    }

    #[cfg(not(target_os = "macos"))]
    fn random_bytes(&self, _length: usize) -> Result<Vec<u8>, AccountScopeError> {
        Err(AccountScopeError::UnsupportedPlatform)
    }

    fn storage_dir(&self) -> Result<PathBuf, AccountScopeError> {
        dirs::data_dir()
            .map(|path| path.join("com.nyanako.tokenbar"))
            .ok_or(AccountScopeError::StorageUnavailable)
    }

    fn now_seconds(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
            .unwrap_or(0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MetadataEnvelope {
    schema_version: u32,
    payload_bytes_base64: String,
    payload_mac: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MetadataPayload {
    bindings: Vec<Binding>,
    current_fingerprint_by_slot: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Binding {
    provider: String,
    slot_digest: String,
    credential_fingerprint: String,
    random_lineage_id: String,
}

pub(crate) fn resolve_authoritative(
    provider: &str,
    kind: AuthoritativeIdKind,
    identifier: &str,
) -> Result<AccountScope, AccountScopeError> {
    resolve_authoritative_with(
        &SystemBackend,
        &ACCOUNT_SCOPE_PROCESS_LOCK,
        provider,
        kind,
        identifier,
    )
}

/// History identity for one provider: the authoritative owner ID when the caller
/// has one, otherwise a per-installation, per-provider constant.
///
/// The constant reads only the installation key and never loads metadata, so a
/// credential rotation cannot move it. The authoritative branch delegates to
/// `resolve_authoritative` unchanged, which keeps its metadata load, its MAC
/// verification and its quarantine contract.
pub(crate) fn resolve_history_scope(
    provider: &str,
    authoritative: Option<(AuthoritativeIdKind, &str)>,
) -> Result<HistoryScope, AccountScopeError> {
    resolve_history_scope_with(
        &SystemBackend,
        &ACCOUNT_SCOPE_PROCESS_LOCK,
        provider,
        authoritative,
    )
}

pub(crate) fn resolve_credential(
    provider: &str,
    semantic_source: &str,
    canonical_location: &str,
    raw_marker: &[u8],
) -> Result<AccountScope, AccountScopeError> {
    resolve_credential_with(
        &SystemBackend,
        &ACCOUNT_SCOPE_PROCESS_LOCK,
        provider,
        semantic_source,
        canonical_location,
        raw_marker,
    )
}

pub(crate) fn canonical_file_location(
    path: &Path,
    record: Option<&str>,
) -> Result<String, AccountScopeError> {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let path = canonical
        .to_str()
        .ok_or(AccountScopeError::InvalidEvidence)?;
    let mut location = path.to_string();
    if let Some(record) = record.filter(|value| !value.is_empty()) {
        location.push('\0');
        location.push_str(record);
    }
    Ok(location)
}

fn resolve_authoritative_with<B: Backend>(
    backend: &B,
    process_lock: &Mutex<()>,
    provider: &str,
    kind: AuthoritativeIdKind,
    identifier: &str,
) -> Result<AccountScope, AccountScopeError> {
    let provider = validate_text(provider)?;
    let normalized = match kind {
        AuthoritativeIdKind::Email => identifier.trim().to_ascii_lowercase(),
        AuthoritativeIdKind::OpaqueId => identifier.trim().to_string(),
    };
    if normalized.is_empty() {
        return Err(AccountScopeError::NoTrustedEvidence);
    }
    let key = ensure_installation_key(backend, process_lock)?;
    let directory = ensure_storage_dir(backend)?;
    with_metadata_lock(backend, process_lock, &directory, || {
        load_metadata(backend, &directory, &key)?;
        Ok(())
    })?;
    scope_from_authoritative(&key, provider, kind, normalized.as_bytes())
}

fn resolve_history_scope_with<B: Backend>(
    backend: &B,
    process_lock: &Mutex<()>,
    provider: &str,
    authoritative: Option<(AuthoritativeIdKind, &str)>,
) -> Result<HistoryScope, AccountScopeError> {
    if let Some((kind, identifier)) = authoritative {
        return resolve_authoritative_with(backend, process_lock, provider, kind, identifier)
            .map(|scope| HistoryScope(scope.0));
    }
    let provider = validate_text(provider)?;
    let key = ensure_installation_key(backend, process_lock)?;
    scope_from_history_constant(&key, provider)
}

fn resolve_credential_with<B: Backend>(
    backend: &B,
    process_lock: &Mutex<()>,
    provider: &str,
    semantic_source: &str,
    canonical_location: &str,
    raw_marker: &[u8],
) -> Result<AccountScope, AccountScopeError> {
    validate_credential_evidence(provider, semantic_source, canonical_location, raw_marker)?;
    let key = ensure_installation_key(backend, process_lock)?;
    bind_current_credential(
        backend,
        process_lock,
        &key,
        provider,
        semantic_source,
        canonical_location,
        raw_marker,
    )
}

fn validate_credential_evidence<'a>(
    provider: &'a str,
    semantic_source: &str,
    canonical_location: &str,
    raw_marker: &[u8],
) -> Result<&'a str, AccountScopeError> {
    let provider = validate_text(provider)?;
    validate_text(semantic_source)?;
    validate_text(canonical_location)?;
    if raw_marker.is_empty() {
        return Err(AccountScopeError::NoTrustedEvidence);
    }
    Ok(provider)
}

fn validate_text(value: &str) -> Result<&str, AccountScopeError> {
    if value.is_empty() || value.len() > u32::MAX as usize {
        Err(AccountScopeError::InvalidEvidence)
    } else {
        Ok(value)
    }
}

fn ensure_installation_key<B: Backend>(
    backend: &B,
    process_lock: &Mutex<()>,
) -> Result<[u8; INSTALLATION_KEY_BYTES], AccountScopeError> {
    let directory = ensure_storage_dir(backend)?;
    with_metadata_lock(backend, process_lock, &directory, || {
        ensure_installation_key_locked(backend, &directory)
    })
}

fn ensure_installation_key_locked<B: Backend>(
    backend: &B,
    directory: &Path,
) -> Result<[u8; INSTALLATION_KEY_BYTES], AccountScopeError> {
    let key_path = directory.join(INSTALLATION_KEY_FILE);
    if let Some(key) = read_installation_key(backend, &key_path)? {
        return Ok(key);
    }

    backend
        .before_fs(FsOperation::InspectArtifacts)
        .map_err(|_| AccountScopeError::StorageUnavailable)?;
    let metadata_path = directory.join(METADATA_FILE);
    let history_path = directory.join(V3_HISTORY_FILE);
    let metadata_exists = regular_artifact_exists(backend, &metadata_path)
        .map_err(|_| AccountScopeError::StorageUnavailable)?;
    let history_exists = regular_artifact_exists(backend, &history_path)
        .map_err(|_| AccountScopeError::StorageUnavailable)?;
    let orphaned_metadata_exists = orphaned_metadata_artifact_exists(backend, directory)
        .map_err(|_| AccountScopeError::StorageUnavailable)?;
    let had_artifacts = metadata_exists || history_exists || orphaned_metadata_exists;

    let generated = installation_key_from_bytes(&backend.random_bytes(INSTALLATION_KEY_BYTES)?)?;
    if metadata_exists {
        quarantine_metadata(backend, &metadata_path, "orphaned")?;
    }
    save_atomic(backend, directory, &key_path, &generated)
        .map_err(|_| AccountScopeError::InstallationKeyWrite)?;
    let winner =
        read_installation_key(backend, &key_path)?.ok_or(AccountScopeError::InstallationKeyRead)?;

    if had_artifacts {
        Err(AccountScopeError::OrphanedArtifacts)
    } else {
        Ok(winner)
    }
}

fn installation_key_from_bytes(
    bytes: &[u8],
) -> Result<[u8; INSTALLATION_KEY_BYTES], AccountScopeError> {
    bytes
        .try_into()
        .map_err(|_| AccountScopeError::InvalidInstallationKey)
}

fn read_installation_key<B: Backend>(
    backend: &B,
    path: &Path,
) -> Result<Option<[u8; INSTALLATION_KEY_BYTES]>, AccountScopeError> {
    backend
        .before_fs(FsOperation::ReadInstallationKey)
        .map_err(|_| AccountScopeError::InstallationKeyRead)?;

    let mut file = {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_file() => {}
            Ok(_) => return Err(AccountScopeError::InvalidInstallationKey),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(AccountScopeError::InstallationKeyRead),
        }
        OpenOptions::new()
            .read(true)
            .open(path)
            .map_err(|_| AccountScopeError::InstallationKeyRead)?
    };

    backend
        .before_fs(FsOperation::ValidateInstallationKey)
        .map_err(|_| AccountScopeError::InstallationKeyRead)?;
    verify_installation_key_file(backend, path, &file)?;

    let mut key = [0_u8; INSTALLATION_KEY_BYTES];
    match file.read_exact(&mut key) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
            return Err(AccountScopeError::InvalidInstallationKey)
        }
        Err(_) => return Err(AccountScopeError::InstallationKeyRead),
    }
    let mut trailing = [0_u8; 1];
    if file
        .read(&mut trailing)
        .map_err(|_| AccountScopeError::InstallationKeyRead)?
        != 0
    {
        return Err(AccountScopeError::InvalidInstallationKey);
    }
    verify_installation_key_file(backend, path, &file)?;
    Ok(Some(key))
}

fn verify_installation_key_file<B: Backend>(
    backend: &B,
    path: &Path,
    file: &File,
) -> Result<(), AccountScopeError> {
    verify_open_regular_file(backend, path, file)
        .map_err(|_| AccountScopeError::InvalidInstallationKey)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if file
            .metadata()
            .map_err(|_| AccountScopeError::InstallationKeyRead)?
            .permissions()
            .mode()
            & 0o7777
            != 0o600
        {
            return Err(AccountScopeError::InvalidInstallationKey);
        }
    }
    Ok(())
}

fn bind_current_credential<B: Backend>(
    backend: &B,
    process_lock: &Mutex<()>,
    key: &[u8; INSTALLATION_KEY_BYTES],
    provider: &str,
    semantic_source: &str,
    canonical_location: &str,
    raw_marker: &[u8],
) -> Result<AccountScope, AccountScopeError> {
    let fingerprint = credential_fingerprint(key, provider, raw_marker)?;
    let slot = slot_digest(key, provider, semantic_source, canonical_location)?;
    let directory = ensure_storage_dir(backend)?;
    with_metadata_lock(backend, process_lock, &directory, || {
        let mut payload = load_metadata(backend, &directory, key)?;
        let lineage = match lineage_for_fingerprint(&payload, provider, &fingerprint)? {
            Some(lineage) => lineage,
            None => encode_lineage_id(&backend.random_bytes(LINEAGE_ID_BYTES)?)?,
        };
        add_binding(&mut payload, provider, &slot, &fingerprint, &lineage)?;
        payload
            .current_fingerprint_by_slot
            .insert(slot, fingerprint);
        validate_payload(&payload)?;
        save_metadata(backend, &directory, key, &payload)?;
        scope_from_lineage(key, provider, &lineage)
    })
}

fn transfer_credential_with<B: Backend>(
    backend: &B,
    process_lock: &Mutex<()>,
    key: &[u8; INSTALLATION_KEY_BYTES],
    provider: &str,
    semantic_source: &str,
    canonical_location: &str,
    old_marker: &[u8],
    new_marker: &[u8],
) -> Result<AccountScope, AccountScopeError> {
    validate_credential_evidence(provider, semantic_source, canonical_location, old_marker)?;
    if new_marker.is_empty() {
        return Err(AccountScopeError::NoTrustedEvidence);
    }
    let old_fingerprint = credential_fingerprint(key, provider, old_marker)?;
    let new_fingerprint = credential_fingerprint(key, provider, new_marker)?;
    let slot = slot_digest(key, provider, semantic_source, canonical_location)?;
    let directory = ensure_storage_dir(backend)?;
    with_metadata_lock(backend, process_lock, &directory, || {
        let mut payload = load_metadata(backend, &directory, key)?;
        let old_lineage = lineage_for_fingerprint(&payload, provider, &old_fingerprint)?;
        let new_lineage = lineage_for_fingerprint(&payload, provider, &new_fingerprint)?;
        let lineage = match (old_lineage, new_lineage) {
            (Some(old), Some(new)) if old != new => {
                return Err(AccountScopeError::MetadataConflict)
            }
            (Some(lineage), _) | (_, Some(lineage)) => lineage,
            (None, None) => encode_lineage_id(&backend.random_bytes(LINEAGE_ID_BYTES)?)?,
        };
        add_binding(&mut payload, provider, &slot, &old_fingerprint, &lineage)?;
        add_binding(&mut payload, provider, &slot, &new_fingerprint, &lineage)?;
        payload
            .current_fingerprint_by_slot
            .insert(slot, new_fingerprint);
        validate_payload(&payload)?;
        save_metadata(backend, &directory, key, &payload)?;
        scope_from_lineage(key, provider, &lineage)
    })
}

fn add_binding(
    payload: &mut MetadataPayload,
    provider: &str,
    slot_digest: &str,
    credential_fingerprint: &str,
    lineage: &str,
) -> Result<(), AccountScopeError> {
    for binding in &payload.bindings {
        if binding.provider == provider
            && binding.credential_fingerprint == credential_fingerprint
            && binding.random_lineage_id != lineage
        {
            return Err(AccountScopeError::MetadataConflict);
        }
        if binding.provider == provider
            && binding.slot_digest == slot_digest
            && binding.credential_fingerprint == credential_fingerprint
        {
            return if binding.random_lineage_id == lineage {
                Ok(())
            } else {
                Err(AccountScopeError::MetadataConflict)
            };
        }
    }
    payload.bindings.push(Binding {
        provider: provider.to_string(),
        slot_digest: slot_digest.to_string(),
        credential_fingerprint: credential_fingerprint.to_string(),
        random_lineage_id: lineage.to_string(),
    });
    Ok(())
}

fn lineage_for_fingerprint(
    payload: &MetadataPayload,
    provider: &str,
    fingerprint: &str,
) -> Result<Option<String>, AccountScopeError> {
    let mut lineage: Option<&str> = None;
    for binding in payload.bindings.iter().filter(|binding| {
        binding.provider == provider && binding.credential_fingerprint == fingerprint
    }) {
        match lineage {
            None => lineage = Some(&binding.random_lineage_id),
            Some(existing) if existing == binding.random_lineage_id => {}
            Some(_) => return Err(AccountScopeError::MetadataConflict),
        }
    }
    Ok(lineage.map(str::to_string))
}

fn load_metadata<B: Backend>(
    backend: &B,
    directory: &Path,
    key: &[u8; INSTALLATION_KEY_BYTES],
) -> Result<MetadataPayload, AccountScopeError> {
    backend
        .before_fs(FsOperation::ReadMetadata)
        .map_err(|_| AccountScopeError::MetadataRead)?;
    let path = directory.join(METADATA_FILE);
    let Some(bytes) =
        read_owner_only(backend, &path).map_err(|_| AccountScopeError::MetadataRead)?
    else {
        return Ok(MetadataPayload::default());
    };
    match decode_metadata(key, &bytes) {
        Ok(payload) => Ok(payload),
        Err(AccountScopeError::MetadataConflict) => Err(AccountScopeError::MetadataConflict),
        Err(_) => {
            quarantine_metadata(backend, &path, "corrupt")?;
            Err(AccountScopeError::MetadataCorrupt)
        }
    }
}

fn decode_metadata(
    key: &[u8; INSTALLATION_KEY_BYTES],
    bytes: &[u8],
) -> Result<MetadataPayload, AccountScopeError> {
    let envelope: MetadataEnvelope =
        serde_json::from_slice(bytes).map_err(|_| AccountScopeError::MetadataCorrupt)?;
    if envelope.schema_version != METADATA_SCHEMA_VERSION {
        return Err(AccountScopeError::MetadataCorrupt);
    }
    let payload_bytes = STANDARD
        .decode(envelope.payload_bytes_base64.as_bytes())
        .map_err(|_| AccountScopeError::MetadataCorrupt)?;
    let stored_mac = URL_SAFE_NO_PAD
        .decode(envelope.payload_mac.as_bytes())
        .map_err(|_| AccountScopeError::MetadataCorrupt)?;
    if stored_mac.len() != DIGEST_BYTES {
        return Err(AccountScopeError::MetadataCorrupt);
    }
    let metadata_key = metadata_mac_key(key)?;
    let encoded = encode_fields(&[payload_bytes.as_slice()])?;
    let mut mac = HmacSha256::new_from_slice(&metadata_key)
        .map_err(|_| AccountScopeError::MetadataCorrupt)?;
    mac.update(&encoded);
    mac.verify_slice(&stored_mac)
        .map_err(|_| AccountScopeError::MetadataCorrupt)?;
    let payload: MetadataPayload =
        serde_json::from_slice(&payload_bytes).map_err(|_| AccountScopeError::MetadataCorrupt)?;
    validate_payload(&payload)?;
    Ok(payload)
}

fn save_metadata<B: Backend>(
    backend: &B,
    directory: &Path,
    key: &[u8; INSTALLATION_KEY_BYTES],
    payload: &MetadataPayload,
) -> Result<(), AccountScopeError> {
    validate_payload(payload)?;
    let mut payload = payload.clone();
    payload.bindings.sort_by(|left, right| {
        left.provider
            .cmp(&right.provider)
            .then(left.slot_digest.cmp(&right.slot_digest))
            .then(
                left.credential_fingerprint
                    .cmp(&right.credential_fingerprint),
            )
            .then(left.random_lineage_id.cmp(&right.random_lineage_id))
    });
    let payload_bytes =
        serde_json::to_vec(&payload).map_err(|_| AccountScopeError::MetadataWrite)?;
    let metadata_key = metadata_mac_key(key)?;
    let payload_mac = hmac_digest(&metadata_key, &[payload_bytes.as_slice()])?;
    let envelope = MetadataEnvelope {
        schema_version: METADATA_SCHEMA_VERSION,
        payload_bytes_base64: STANDARD.encode(&payload_bytes),
        payload_mac: encode_digest(&payload_mac),
    };
    let bytes =
        serde_json::to_vec_pretty(&envelope).map_err(|_| AccountScopeError::MetadataWrite)?;
    save_atomic(backend, directory, &directory.join(METADATA_FILE), &bytes)
        .map_err(|_| AccountScopeError::MetadataWrite)
}

fn validate_payload(payload: &MetadataPayload) -> Result<(), AccountScopeError> {
    let mut exact = BTreeSet::new();
    let mut fingerprint_lineages: BTreeMap<(&str, &str), &str> = BTreeMap::new();
    let mut slot_providers: BTreeMap<&str, &str> = BTreeMap::new();
    for binding in &payload.bindings {
        validate_text(&binding.provider).map_err(|_| AccountScopeError::MetadataConflict)?;
        validate_digest_text(&binding.slot_digest)?;
        validate_digest_text(&binding.credential_fingerprint)?;
        validate_lineage_text(&binding.random_lineage_id)?;
        if !exact.insert((
            binding.provider.as_str(),
            binding.slot_digest.as_str(),
            binding.credential_fingerprint.as_str(),
        )) {
            return Err(AccountScopeError::MetadataConflict);
        }
        match fingerprint_lineages.insert(
            (
                binding.provider.as_str(),
                binding.credential_fingerprint.as_str(),
            ),
            binding.random_lineage_id.as_str(),
        ) {
            Some(existing) if existing != binding.random_lineage_id => {
                return Err(AccountScopeError::MetadataConflict)
            }
            _ => {}
        }
        match slot_providers.insert(binding.slot_digest.as_str(), binding.provider.as_str()) {
            Some(existing) if existing != binding.provider => {
                return Err(AccountScopeError::MetadataConflict)
            }
            _ => {}
        }
    }

    for (slot, fingerprint) in &payload.current_fingerprint_by_slot {
        validate_digest_text(slot)?;
        validate_digest_text(fingerprint)?;
        let matches = payload
            .bindings
            .iter()
            .filter(|binding| {
                binding.slot_digest == *slot && binding.credential_fingerprint == *fingerprint
            })
            .count();
        if matches != 1 {
            return Err(AccountScopeError::MetadataConflict);
        }
    }
    Ok(())
}

fn validate_digest_text(value: &str) -> Result<(), AccountScopeError> {
    let decoded = URL_SAFE_NO_PAD
        .decode(value.as_bytes())
        .map_err(|_| AccountScopeError::MetadataConflict)?;
    if decoded.len() != DIGEST_BYTES || URL_SAFE_NO_PAD.encode(decoded) != value {
        return Err(AccountScopeError::MetadataConflict);
    }
    Ok(())
}

fn validate_lineage_text(value: &str) -> Result<(), AccountScopeError> {
    let decoded = URL_SAFE_NO_PAD
        .decode(value.as_bytes())
        .map_err(|_| AccountScopeError::MetadataConflict)?;
    if decoded.len() != LINEAGE_ID_BYTES || URL_SAFE_NO_PAD.encode(decoded) != value {
        return Err(AccountScopeError::MetadataConflict);
    }
    Ok(())
}

fn quarantine_metadata<B: Backend>(
    backend: &B,
    path: &Path,
    reason: &str,
) -> Result<PathBuf, AccountScopeError> {
    quarantine_metadata_with(
        backend,
        path,
        reason,
        |source, candidate| fs::hard_link(source, candidate),
        |source| fs::remove_file(source),
    )
}

fn quarantine_metadata_with<B, L, U>(
    backend: &B,
    path: &Path,
    reason: &str,
    mut link: L,
    unlink: U,
) -> Result<PathBuf, AccountScopeError>
where
    B: Backend,
    L: FnMut(&Path, &Path) -> io::Result<()>,
    U: Fn(&Path) -> io::Result<()>,
{
    backend
        .before_fs(FsOperation::QuarantineMetadata)
        .map_err(|_| AccountScopeError::QuarantineFailed)?;

    let source = open_existing_owner_only(backend, path)
        .map_err(|_| AccountScopeError::QuarantineFailed)?
        .ok_or(AccountScopeError::QuarantineFailed)?;
    let directory = path.parent().ok_or(AccountScopeError::QuarantineFailed)?;
    let now = backend.now_seconds();
    for suffix in 0..=u32::MAX {
        let name = if suffix == 0 {
            format!("quota-account-scope-v1.{reason}-{now}.json")
        } else {
            format!("quota-account-scope-v1.{reason}-{now}.{suffix}.json")
        };
        let candidate = directory.join(name);
        if verify_open_regular_file(backend, path, &source).is_err() {
            return Err(AccountScopeError::QuarantineFailed);
        }
        match link(path, &candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(AccountScopeError::QuarantineFailed),
        }
        if verify_open_regular_file(backend, path, &source).is_err()
            || verify_open_regular_file(backend, &candidate, &source).is_err()
        {
            rollback_quarantine_link(backend, &candidate, &source);
            return Err(AccountScopeError::QuarantineFailed);
        }
        if unlink(path).is_err() {
            rollback_quarantine_link(backend, &candidate, &source);
            return Err(AccountScopeError::QuarantineFailed);
        }
        sync_directory(backend, directory).map_err(|_| AccountScopeError::QuarantineFailed)?;
        return Ok(candidate);
    }
    Err(AccountScopeError::QuarantineFailed)
}

fn save_atomic<B: Backend>(
    backend: &B,
    directory: &Path,
    path: &Path,
    bytes: &[u8],
) -> io::Result<()> {
    let target_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing target filename"))?
        .to_string_lossy();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = directory.join(format!(
        ".{target_name}.tmp-{}-{counter}",
        std::process::id()
    ));
    let staged = (|| -> io::Result<()> {
        backend.before_fs(FsOperation::CreateTemp)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = secure_open_regular_file(backend, &temp, options.open(&temp)?)?;
        backend.before_fs(FsOperation::WriteTemp)?;
        file.write_all(bytes)?;
        file.flush()?;
        backend.before_fs(FsOperation::SyncTemp)?;
        file.sync_all()?;
        drop(file);
        backend.before_fs(FsOperation::ReplaceFile)?;
        tokscale_core::fs_atomic::replace_file(&temp, path)?;
        sync_directory(backend, directory)
    })();
    if staged.is_err() {
        let _ = fs::remove_file(&temp);
    }
    staged
}

fn ensure_storage_dir<B: Backend>(backend: &B) -> Result<PathBuf, AccountScopeError> {
    let directory = backend.storage_dir()?;
    backend
        .before_fs(FsOperation::CreateDirectory)
        .map_err(|_| AccountScopeError::StorageUnavailable)?;
    ensure_real_directory(backend, &directory)
        .map_err(|_| AccountScopeError::StorageUnavailable)?;
    Ok(directory)
}

fn with_metadata_lock<B: Backend, T>(
    backend: &B,
    process_lock: &Mutex<()>,
    directory: &Path,
    body: impl FnOnce() -> Result<T, AccountScopeError>,
) -> Result<T, AccountScopeError> {
    let _process_guard = process_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let lock_path = directory.join(METADATA_LOCK_FILE);
    backend
        .before_fs(FsOperation::OpenMetadataLock)
        .map_err(|_| AccountScopeError::MetadataLock)?;
    let lock_file = {
        let lock_file =
            open_owner_only(backend, &lock_path).map_err(|_| AccountScopeError::MetadataLock)?;
        backend
            .before_fs(FsOperation::AcquireMetadataLock)
            .map_err(|_| AccountScopeError::MetadataLock)?;
        lock_file
            .lock_exclusive()
            .map_err(|_| AccountScopeError::MetadataLock)?;
        lock_file
    };
    let result = body();
    let unlock = fs2::FileExt::unlock(&lock_file).map_err(|_| AccountScopeError::MetadataLock);
    match (result, unlock) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(value), Ok(())) => Ok(value),
    }
}

fn ensure_real_directory<B: Backend>(_backend: &B, directory: &Path) -> io::Result<()> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "account-scope storage is not a real directory",
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(directory)?;
        }
        Err(error) => return Err(error),
    }

    let path_metadata = fs::symlink_metadata(directory)?;
    if !path_metadata.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "account-scope storage is not a real directory",
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let file = File::open(directory)?;
        verify_open_directory(directory, &file)?;
        file.set_permissions(fs::Permissions::from_mode(0o700))?;
        verify_open_directory(directory, &file)?;
    }
    Ok(())
}

fn open_owner_only<B: Backend>(backend: &B, path: &Path) -> io::Result<File> {
    let mut create = OpenOptions::new();
    create.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        create.mode(0o600);
    }
    match create.open(path) {
        Ok(file) => secure_open_regular_file(backend, path, file),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            require_regular_file_path(backend, path)?;
            let file = OpenOptions::new().read(true).write(true).open(path)?;
            secure_open_regular_file(backend, path, file)
        }
        Err(error) => Err(error),
    }
}

fn open_existing_owner_only<B: Backend>(backend: &B, path: &Path) -> io::Result<Option<File>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "account-scope artifact is not a regular file",
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    let file = OpenOptions::new().read(true).open(path)?;
    secure_open_regular_file(backend, path, file).map(Some)
}

fn read_owner_only<B: Backend>(backend: &B, path: &Path) -> io::Result<Option<Vec<u8>>> {
    let Some(mut file) = open_existing_owner_only(backend, path)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    verify_open_regular_file(backend, path, &file)?;
    Ok(Some(bytes))
}

fn require_regular_file_path<B: Backend>(_backend: &B, path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_file() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "account-scope artifact is not a regular file",
        ))
    }
}

fn regular_artifact_exists<B: Backend>(_backend: &B, path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "account-scope artifact is not a regular file",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn orphaned_metadata_artifact_exists<B: Backend>(
    backend: &B,
    directory: &Path,
) -> io::Result<bool> {
    backend.before_fs(FsOperation::InspectOrphanedMetadata)?;
    let mut found = false;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !is_orphaned_metadata_name(name) {
            continue;
        }
        require_regular_file_path(backend, &entry.path())?;
        found = true;
    }
    Ok(found)
}

fn is_orphaned_metadata_name(name: &str) -> bool {
    let Some(stem) = name
        .strip_prefix("quota-account-scope-v1.orphaned-")
        .and_then(|name| name.strip_suffix(".json"))
    else {
        return false;
    };
    let mut parts = stem.split('.');
    let Some(timestamp) = parts
        .next()
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value >= 0)
    else {
        return false;
    };
    let Some(suffix) = parts.next() else {
        return timestamp.to_string() == stem;
    };
    let Some(suffix) = suffix.parse::<u32>().ok().filter(|value| *value > 0) else {
        return false;
    };
    parts.next().is_none() && format!("{timestamp}.{suffix}") == stem
}

fn secure_open_regular_file<B: Backend>(backend: &B, path: &Path, file: File) -> io::Result<File> {
    verify_open_regular_file(backend, path, &file)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    verify_open_regular_file(backend, path, &file)?;
    Ok(file)
}

fn verify_open_regular_file<B: Backend>(_backend: &B, path: &Path, file: &File) -> io::Result<()> {
    let file_metadata = file.metadata()?;
    let path_metadata = fs::symlink_metadata(path)?;
    if !file_metadata.file_type().is_file() || !path_metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "account-scope artifact is not a regular file",
        ));
    }
    #[cfg(unix)]
    if !same_file(&file_metadata, &path_metadata) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "account-scope artifact changed while opening",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn verify_open_directory(path: &Path, file: &File) -> io::Result<()> {
    let file_metadata = file.metadata()?;
    let path_metadata = fs::symlink_metadata(path)?;
    if !file_metadata.file_type().is_dir()
        || !path_metadata.file_type().is_dir()
        || !same_file(&file_metadata, &path_metadata)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "account-scope storage changed while opening",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    left.dev() == right.dev() && left.ino() == right.ino()
}

fn rollback_quarantine_link<B: Backend>(backend: &B, path: &Path, source: &File) {
    if verify_open_regular_file(backend, path, source).is_ok() {
        let _ = fs::remove_file(path);
    }
}

fn open_refresh_lock_file<B: Backend>(
    backend: &B,
    directory: &Path,
    provider: &str,
) -> Result<File, AccountScopeError> {
    validate_text(provider)?;
    backend
        .before_fs(FsOperation::OpenRefreshLock)
        .map_err(|_| AccountScopeError::MetadataLock)?;
    let path = directory.join(format!("quota-auth-refresh-{provider}.lock"));
    let file = open_owner_only(backend, &path).map_err(|_| AccountScopeError::MetadataLock)?;
    backend
        .before_fs(FsOperation::AcquireRefreshLock)
        .map_err(|_| AccountScopeError::MetadataLock)?;
    file.lock_exclusive()
        .map_err(|_| AccountScopeError::MetadataLock)?;
    Ok(file)
}

fn sync_directory<B: Backend>(backend: &B, directory: &Path) -> io::Result<()> {
    backend.before_fs(FsOperation::SyncDirectory)?;
    File::open(directory)?.sync_all()
}

fn scope_from_authoritative(
    key: &[u8; INSTALLATION_KEY_BYTES],
    provider: &str,
    kind: AuthoritativeIdKind,
    normalized_identifier: &[u8],
) -> Result<AccountScope, AccountScopeError> {
    let digest = hmac_digest(
        key,
        &[
            b"scope-id-v1",
            provider.as_bytes(),
            kind.domain_value().as_bytes(),
            normalized_identifier,
        ],
    )?;
    Ok(AccountScope(encode_digest(&digest)))
}

/// The history identity for a provider with no authoritative owner ID. Its only
/// inputs are the installation key and the provider name, so nothing a sibling
/// application does to the credential can move it.
fn scope_from_history_constant(
    key: &[u8; INSTALLATION_KEY_BYTES],
    provider: &str,
) -> Result<HistoryScope, AccountScopeError> {
    let digest = hmac_digest(key, &[b"scope-history-v1", provider.as_bytes()])?;
    Ok(HistoryScope(encode_digest(&digest)))
}

fn credential_fingerprint(
    key: &[u8; INSTALLATION_KEY_BYTES],
    provider: &str,
    marker: &[u8],
) -> Result<String, AccountScopeError> {
    hmac_digest(key, &[b"credential-v1", provider.as_bytes(), marker])
        .map(|digest| encode_digest(&digest))
}

fn slot_digest(
    key: &[u8; INSTALLATION_KEY_BYTES],
    provider: &str,
    semantic_source: &str,
    canonical_location: &str,
) -> Result<String, AccountScopeError> {
    hmac_digest(
        key,
        &[
            b"slot-v1",
            provider.as_bytes(),
            semantic_source.as_bytes(),
            canonical_location.as_bytes(),
        ],
    )
    .map(|digest| encode_digest(&digest))
}

fn scope_from_lineage(
    key: &[u8; INSTALLATION_KEY_BYTES],
    provider: &str,
    encoded_lineage: &str,
) -> Result<AccountScope, AccountScopeError> {
    let lineage = URL_SAFE_NO_PAD
        .decode(encoded_lineage.as_bytes())
        .map_err(|_| AccountScopeError::MetadataConflict)?;
    if lineage.len() != LINEAGE_ID_BYTES {
        return Err(AccountScopeError::MetadataConflict);
    }
    let digest = hmac_digest(
        key,
        &[b"scope-lineage-v1", provider.as_bytes(), lineage.as_slice()],
    )?;
    Ok(AccountScope(encode_digest(&digest)))
}

fn metadata_mac_key(
    key: &[u8; INSTALLATION_KEY_BYTES],
) -> Result<[u8; DIGEST_BYTES], AccountScopeError> {
    hmac_digest(key, &[b"metadata-key-v1"])
}

fn encode_lineage_id(bytes: &[u8]) -> Result<String, AccountScopeError> {
    if bytes.len() != LINEAGE_ID_BYTES {
        return Err(AccountScopeError::RandomUnavailable);
    }
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn encode_digest(bytes: &[u8; DIGEST_BYTES]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// A file-name-safe name for `value` under this installation's key: lowercase
/// hex of an HMAC domain-separated by `domain` (e.g. `cursor-sync-file`), so a
/// file name never carries the raw account id it is derived from.
pub(crate) fn installation_digest_hex(
    domain: &str,
    value: &[u8],
) -> Result<String, AccountScopeError> {
    let key = ensure_installation_key(&SystemBackend, &ACCOUNT_SCOPE_PROCESS_LOCK)?;
    keyed_digest_hex(&key, domain, value)
}

/// `installation_digest_hex` with the key supplied, so tests never touch the
/// real installation key.
pub(crate) fn keyed_digest_hex(
    key: &[u8],
    domain: &str,
    value: &[u8],
) -> Result<String, AccountScopeError> {
    let domain = validate_text(domain)?;
    let digest = hmac_digest(key, &[b"file-name-v1", domain.as_bytes(), value])?;
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn hmac_digest(key: &[u8], fields: &[&[u8]]) -> Result<[u8; DIGEST_BYTES], AccountScopeError> {
    let encoded = encode_fields(fields)?;
    let mut mac =
        HmacSha256::new_from_slice(key).map_err(|_| AccountScopeError::InvalidEvidence)?;
    mac.update(&encoded);
    Ok(mac.finalize().into_bytes().into())
}

fn encode_fields(fields: &[&[u8]]) -> Result<Vec<u8>, AccountScopeError> {
    let capacity = fields.iter().try_fold(0_usize, |total, field| {
        let _ = u32::try_from(field.len()).map_err(|_| AccountScopeError::InvalidEvidence)?;
        total
            .checked_add(4)
            .and_then(|value| value.checked_add(field.len()))
            .ok_or(AccountScopeError::InvalidEvidence)
    })?;
    let mut encoded = Vec::with_capacity(capacity);
    for field in fields {
        let length = u32::try_from(field.len()).map_err(|_| AccountScopeError::InvalidEvidence)?;
        encoded.extend_from_slice(&length.to_be_bytes());
        encoded.extend_from_slice(field);
    }
    Ok(encoded)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefreshCheckpoint {
    Reloaded,
    NetworkReturned,
    MetadataHandled,
    CredentialsPersisted,
}

pub(crate) trait RefreshScopeTransaction {
    fn resolve_current(
        &self,
        semantic_source: &str,
        canonical_location: &str,
        marker: &[u8],
    ) -> Result<AccountScope, AccountScopeError>;

    fn transfer(
        &self,
        semantic_source: &str,
        canonical_location: &str,
        old_marker: &[u8],
        new_marker: &[u8],
    ) -> Result<AccountScope, AccountScopeError>;
}

pub(crate) struct RefreshTransaction {
    provider: &'static str,
    key: Result<[u8; INSTALLATION_KEY_BYTES], AccountScopeError>,
    _process_guard: MutexGuard<'static, ()>,
    lock_file: File,
}

pub(crate) fn begin_refresh(
    provider: &'static str,
) -> Result<RefreshTransaction, AccountScopeError> {
    let backend = SystemBackend;
    // Installation-key read or key-loss recovery completes before the provider
    // refresh lock is acquired. The refresh transaction keeps the existing
    // refresh-lock -> metadata-lock ordering below this point.
    let key = ensure_installation_key(&backend, &ACCOUNT_SCOPE_PROCESS_LOCK);
    let directory = ensure_storage_dir(&backend)?;
    let process_guard = refresh_process_lock(provider)?
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let lock_file = open_refresh_lock_file(&backend, &directory, provider)?;
    Ok(RefreshTransaction {
        provider,
        key,
        _process_guard: process_guard,
        lock_file,
    })
}

impl RefreshTransaction {
    pub(crate) fn resolve_current(
        &self,
        semantic_source: &str,
        canonical_location: &str,
        marker: &[u8],
    ) -> Result<AccountScope, AccountScopeError> {
        validate_credential_evidence(self.provider, semantic_source, canonical_location, marker)?;
        let key = self.key.as_ref().map_err(|error| *error)?;
        bind_current_credential(
            &SystemBackend,
            &ACCOUNT_SCOPE_PROCESS_LOCK,
            key,
            self.provider,
            semantic_source,
            canonical_location,
            marker,
        )
    }

    pub(crate) fn transfer(
        &self,
        semantic_source: &str,
        canonical_location: &str,
        old_marker: &[u8],
        new_marker: &[u8],
    ) -> Result<AccountScope, AccountScopeError> {
        let key = self.key.as_ref().map_err(|error| *error)?;
        transfer_credential_with(
            &SystemBackend,
            &ACCOUNT_SCOPE_PROCESS_LOCK,
            key,
            self.provider,
            semantic_source,
            canonical_location,
            old_marker,
            new_marker,
        )
    }
}

impl RefreshScopeTransaction for RefreshTransaction {
    fn resolve_current(
        &self,
        semantic_source: &str,
        canonical_location: &str,
        marker: &[u8],
    ) -> Result<AccountScope, AccountScopeError> {
        RefreshTransaction::resolve_current(self, semantic_source, canonical_location, marker)
    }

    fn transfer(
        &self,
        semantic_source: &str,
        canonical_location: &str,
        old_marker: &[u8],
        new_marker: &[u8],
    ) -> Result<AccountScope, AccountScopeError> {
        RefreshTransaction::transfer(
            self,
            semantic_source,
            canonical_location,
            old_marker,
            new_marker,
        )
    }
}

impl Drop for RefreshTransaction {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.lock_file);
    }
}

fn refresh_process_lock(provider: &str) -> Result<&'static Mutex<()>, AccountScopeError> {
    match provider {
        "codex" => Ok(&CODEX_REFRESH_LOCK),
        "claude" => Ok(&CLAUDE_REFRESH_LOCK),
        "grok" => Ok(&GROK_REFRESH_LOCK),
        "antigravity" => Ok(&ANTIGRAVITY_REFRESH_LOCK),
        _ => Err(AccountScopeError::InvalidEvidence),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Arc;

    #[derive(Clone)]
    pub(super) struct TestBackend {
        pub(super) directory: PathBuf,
        pub(super) state: Arc<Mutex<TestState>>,
    }

    pub(super) struct TestState {
        pub(super) random: VecDeque<Vec<u8>>,
        pub(super) fail_fs_once: Option<FsOperation>,
        pub(super) replace_installation_key_on: Option<(FsOperation, Vec<u8>)>,
        pub(super) events: Vec<FsOperation>,
        pub(super) now: i64,
    }
    impl TestBackend {
        pub(super) fn new(tag: &str) -> Self {
            let directory = std::env::temp_dir().join(format!(
                "tb-account-scope-{tag}-{}-{}",
                std::process::id(),
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&directory);
            Self {
                directory,
                state: Arc::new(Mutex::new(TestState {
                    random: VecDeque::from([
                        vec![0x11; INSTALLATION_KEY_BYTES],
                        vec![0x21; LINEAGE_ID_BYTES],
                        vec![0x22; LINEAGE_ID_BYTES],
                        vec![0x23; LINEAGE_ID_BYTES],
                        vec![0x24; LINEAGE_ID_BYTES],
                    ]),
                    fail_fs_once: None,
                    replace_installation_key_on: None,
                    events: Vec::new(),
                    now: 1_752_710_400,
                })),
            }
        }
        pub(super) fn with_installation_key(self, key: Vec<u8>) -> Self {
            self.write_installation_key(&key);
            self
        }
        pub(super) fn write_installation_key(&self, key: &[u8]) {
            ensure_real_directory(self, &self.directory).unwrap();
            let path = self.directory.join(INSTALLATION_KEY_FILE);
            fs::write(&path, key).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
            }
        }
        pub(super) fn set_random(&self, values: Vec<Vec<u8>>) {
            self.state.lock().unwrap().random = values.into();
        }
        pub(super) fn fail_fs(&self, operation: FsOperation) {
            self.state.lock().unwrap().fail_fs_once = Some(operation);
        }
        pub(super) fn replace_installation_key_on_validate(&self, key: Vec<u8>) {
            self.replace_installation_key_on(FsOperation::ValidateInstallationKey, key);
        }
        pub(super) fn replace_installation_key_on_sync(&self, key: Vec<u8>) {
            self.replace_installation_key_on(FsOperation::SyncDirectory, key);
        }
        fn replace_installation_key_on(&self, operation: FsOperation, key: Vec<u8>) {
            self.state.lock().unwrap().replace_installation_key_on = Some((operation, key));
        }
        pub(super) fn events(&self) -> Vec<FsOperation> {
            self.state.lock().unwrap().events.clone()
        }
        pub(super) fn cleanup(&self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    impl Backend for TestBackend {
        fn random_bytes(&self, length: usize) -> Result<Vec<u8>, AccountScopeError> {
            let mut state = self.state.lock().unwrap();
            let index = state
                .random
                .iter()
                .position(|bytes| bytes.len() == length)
                .ok_or(AccountScopeError::RandomUnavailable)?;
            state
                .random
                .remove(index)
                .ok_or(AccountScopeError::RandomUnavailable)
        }

        fn storage_dir(&self) -> Result<PathBuf, AccountScopeError> {
            Ok(self.directory.clone())
        }

        fn now_seconds(&self) -> i64 {
            self.state.lock().unwrap().now
        }

        fn before_fs(&self, operation: FsOperation) -> io::Result<()> {
            let replacement = {
                let mut state = self.state.lock().unwrap();
                state.events.push(operation);
                if state.fail_fs_once == Some(operation) {
                    state.fail_fs_once = None;
                    return Err(io::Error::other("injected failure"));
                }
                state
                    .replace_installation_key_on
                    .take_if(|(trigger, _)| *trigger == operation)
                    .map(|(_, bytes)| bytes)
            };
            if let Some(bytes) = replacement {
                let replacement_path = self.directory.join(".installation-key-replacement");
                fs::write(&replacement_path, bytes)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    fs::set_permissions(&replacement_path, fs::Permissions::from_mode(0o600))?;
                }
                fs::rename(replacement_path, self.directory.join(INSTALLATION_KEY_FILE))?;
            }
            Ok(())
        }
    }

    pub(crate) struct TestRefreshScope {
        backend: TestBackend,
        process_lock: Mutex<()>,
        provider: &'static str,
    }

    impl TestRefreshScope {
        pub(crate) fn new(provider: &'static str, tag: &str) -> Self {
            Self {
                backend: TestBackend::new(tag)
                    .with_installation_key(vec![0x11; INSTALLATION_KEY_BYTES]),
                process_lock: Mutex::new(()),
                provider,
            }
        }

        pub(crate) fn root(&self) -> &Path {
            &self.backend.directory
        }

        pub(crate) fn metadata_bytes(&self) -> Vec<u8> {
            fs::read(self.backend.directory.join(METADATA_FILE)).unwrap()
        }

        pub(crate) fn fail_metadata_save(&self) {
            self.backend.fail_fs(FsOperation::ReplaceFile);
        }

        pub(crate) fn resolve_history(
            &self,
            provider: &str,
            authoritative: Option<(AuthoritativeIdKind, &str)>,
        ) -> Result<HistoryScope, AccountScopeError> {
            resolve_history_scope_with(&self.backend, &self.process_lock, provider, authoritative)
        }

        pub(crate) fn resolve_authoritative(
            &self,
            provider: &str,
            kind: AuthoritativeIdKind,
            identifier: &str,
        ) -> Result<AccountScope, AccountScopeError> {
            resolve_authoritative_with(
                &self.backend,
                &self.process_lock,
                provider,
                kind,
                identifier,
            )
        }

        pub(crate) fn cleanup(&self) {
            self.backend.cleanup();
        }
    }

    impl RefreshScopeTransaction for TestRefreshScope {
        fn resolve_current(
            &self,
            semantic_source: &str,
            canonical_location: &str,
            marker: &[u8],
        ) -> Result<AccountScope, AccountScopeError> {
            resolve_credential_with(
                &self.backend,
                &self.process_lock,
                self.provider,
                semantic_source,
                canonical_location,
                marker,
            )
        }

        fn transfer(
            &self,
            semantic_source: &str,
            canonical_location: &str,
            old_marker: &[u8],
            new_marker: &[u8],
        ) -> Result<AccountScope, AccountScopeError> {
            let key = ensure_installation_key(&self.backend, &self.process_lock)?;
            transfer_credential_with(
                &self.backend,
                &self.process_lock,
                &key,
                self.provider,
                semantic_source,
                canonical_location,
                old_marker,
                new_marker,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use sha2::{Digest as _, Sha256};
    use std::sync::{Arc, Barrier};
    use std::thread;

    #[derive(PartialEq, Eq)]
    struct ArtifactSnapshot {
        key: Option<Vec<u8>>,
        metadata: Option<Vec<u8>>,
        history: Option<Vec<u8>>,
        key_temps: usize,
        metadata_temps: usize,
    }

    fn resolve_test(
        backend: &TestBackend,
        process_lock: &Mutex<()>,
        marker: &[u8],
    ) -> Result<AccountScope, AccountScopeError> {
        resolve_credential_with(
            backend,
            process_lock,
            "claude",
            "fixture-source",
            "fixture-location",
            marker,
        )
    }

    fn metadata_bytes(backend: &TestBackend) -> Vec<u8> {
        fs::read(backend.directory.join(METADATA_FILE)).unwrap()
    }

    fn installation_key(backend: &TestBackend) -> [u8; INSTALLATION_KEY_BYTES] {
        read_installation_key(backend, &backend.directory.join(INSTALLATION_KEY_FILE))
            .unwrap()
            .unwrap()
    }

    fn installation_key_path(backend: &TestBackend) -> PathBuf {
        backend.directory.join(INSTALLATION_KEY_FILE)
    }

    fn temp_count(directory: &Path, target: &str) -> usize {
        let prefix = format!(".{target}.tmp-");
        fs::read_dir(directory)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
                    .count()
            })
            .unwrap_or(0)
    }

    fn artifact_snapshot(backend: &TestBackend) -> ArtifactSnapshot {
        ArtifactSnapshot {
            key: fs::read(installation_key_path(backend)).ok(),
            metadata: fs::read(backend.directory.join(METADATA_FILE)).ok(),
            history: fs::read(backend.directory.join(V3_HISTORY_FILE)).ok(),
            key_temps: temp_count(&backend.directory, INSTALLATION_KEY_FILE),
            metadata_temps: temp_count(&backend.directory, METADATA_FILE),
        }
    }

    fn authenticated_envelope(
        key: &[u8; INSTALLATION_KEY_BYTES],
        schema_version: u32,
        payload_bytes: &[u8],
    ) -> Vec<u8> {
        let metadata_key = metadata_mac_key(key).unwrap();
        let payload_mac = hmac_digest(&metadata_key, &[payload_bytes]).unwrap();
        serde_json::to_vec_pretty(&MetadataEnvelope {
            schema_version,
            payload_bytes_base64: STANDARD.encode(payload_bytes),
            payload_mac: encode_digest(&payload_mac),
        })
        .unwrap()
    }

    fn assert_no_sensitive(artifacts: &[Vec<u8>], values: &[&str], owner: &str) {
        for (value_index, value) in values.iter().enumerate() {
            let digest = Sha256::digest(value.as_bytes());
            let forbidden = [
                value.as_bytes().to_vec(),
                format!("{digest:x}").into_bytes(),
                STANDARD.encode(&digest).into_bytes(),
                URL_SAFE_NO_PAD.encode(&digest).into_bytes(),
            ];
            for (artifact_index, artifact) in artifacts.iter().enumerate() {
                for (variant_index, bytes) in forbidden.iter().enumerate() {
                    assert!(
                        !artifact.windows(bytes.len()).any(|window| window == bytes),
                        "{owner}: sensitive value {value_index} variant {variant_index} reached artifact {artifact_index}"
                    );
                }
            }
        }
    }

    #[cfg(unix)]
    fn unix_mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// G1c. The installation key lives under `storage_dir()`, and the primary
    /// account's history scope is derived from that key. If `storage_dir()`
    /// ever learned to read an account-selection variable — the obvious way to
    /// "support config dirs" — the key would move the moment such a variable
    /// was set, the derived scope would change, and every existing series would
    /// be orphaned with no corrupt file to point at.
    ///
    /// `$HOME` derivation is deliberate and out of scope here: the isolated
    /// verification environment works by redirecting `HOME`, and that is the
    /// intended behaviour, not a hazard.
    ///
    /// This test sets real environment variables, which is why it holds
    /// `ENV_TEST_LOCK` and restores them. Asserting the path shape alone would
    /// not fail under the mutation, because a variable nobody sets reads as
    /// absent and the mutated code would fall through to the same default.
    #[test]
    fn g1c_storage_dir_ignores_account_selection_variables() {
        static ENV_TEST_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());

        let baseline = SystemBackend.storage_dir().expect("storage dir");

        const VARS: &[&str] = &[
            "CLAUDE_CONFIG_DIR",
            "TOKENBAR_CLAUDE_CONFIG_DIR",
            "TOKCAT_CLAUDE_CONFIG_DIR",
        ];
        let saved: Vec<_> = VARS.iter().map(|k| (*k, std::env::var_os(k))).collect();
        for key in VARS {
            std::env::set_var(key, "/tmp/tokenbar-g1c-should-be-ignored");
        }
        let observed = SystemBackend.storage_dir();
        for (key, value) in saved {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }

        assert_eq!(
            observed.expect("storage dir under account-selection variables"),
            baseline,
            "storage_dir() moved with an account-selection variable, which relocates the \
             installation key and orphans every existing series"
        );
    }

    #[test]
    fn domain_vectors_normalization_and_separation_are_stable() {
        let key: [u8; INSTALLATION_KEY_BYTES] = std::array::from_fn(|index| index as u8);
        assert!(
            encode_fields(&[b"ab", b"c"]).unwrap()
                == vec![0, 0, 0, 2, b'a', b'b', 0, 0, 0, 1, b'c']
                && encode_fields(&[b"ab", b"c"]).unwrap() != encode_fields(&[b"a", b"bc"]).unwrap(),
            "length-prefix contract"
        );
        let account = scope_from_authoritative(
            &key,
            "antigravity",
            AuthoritativeIdKind::Email,
            b"user@example.com",
        )
        .unwrap();
        let claude_history = scope_from_history_constant(&key, "claude").unwrap();
        let codex_history = scope_from_history_constant(&key, "codex").unwrap();
        let lineage = scope_from_lineage(
            &key,
            "claude",
            &URL_SAFE_NO_PAD.encode([0xA5; LINEAGE_ID_BYTES]),
        )
        .unwrap();
        let fingerprint = credential_fingerprint(&key, "claude", b"fixture-token").unwrap();
        let slot = slot_digest(&key, "claude", "environment", "CLAUDE_CODE_OAUTH_TOKEN").unwrap();
        let metadata = encode_digest(&metadata_mac_key(&key).unwrap());
        assert!(
            account.as_str() == "sK_jjcbkOzChAgJHtE1pPpjKU4AEg_MiNut8GaL1woM"
                && claude_history.as_str() == "yPQyLoK4QzpZjG5p_fIxQVkvgRY6mGC9CKn4NMzyqmA"
                && codex_history.as_str() == "aiwiKwI-dRUWa0g2x2M7afRU5AiQYm3jCePREw7w_z4"
                && lineage.as_str() == "QsM_upNybGz6Hljs9K4Qj5uIuBI1HtHpfmPahxb1SEw"
                && fingerprint.as_str() == "JCR4YryCMKNOeEjYQEHYrXfanXoq24YteoyJyoiSPtc"
                && slot.as_str() == "1nTOH8E7TUly1xvVG2sbUI_C0AzksMJ3iOj9vt2PNj8"
                && metadata == "0Lemwp52DQT0sjS4KS28xOdvxWSKXNWAb9Le0wCs6p8",
            "persisted identity known vectors"
        );
        assert!(
            claude_history
                != scope_from_history_constant(&[0xFF; INSTALLATION_KEY_BYTES], "claude").unwrap(),
            "history installation separation"
        );

        let backend = TestBackend::new("domain-normalization");
        let lock = Mutex::new(());
        let resolve = |kind, identifier| {
            resolve_authoritative_with(&backend, &lock, "provider", kind, identifier).unwrap()
        };
        assert!(
            resolve(AuthoritativeIdKind::Email, "  User@Example.COM ")
                == resolve(AuthoritativeIdKind::Email, "user@example.com"),
            "email normalization"
        );
        assert!(
            resolve(AuthoritativeIdKind::OpaqueId, " Account-A ")
                != resolve(AuthoritativeIdKind::OpaqueId, "account-a"),
            "opaque identifier case"
        );
        assert!(
            scope_from_authoritative(
                &[1; INSTALLATION_KEY_BYTES],
                "codex",
                AuthoritativeIdKind::OpaqueId,
                b"acct-123",
            )
            .unwrap()
                != scope_from_authoritative(
                    &[2; INSTALLATION_KEY_BYTES],
                    "codex",
                    AuthoritativeIdKind::OpaqueId,
                    b"acct-123",
                )
                .unwrap(),
            "account installation separation"
        );
        backend.cleanup();
    }

    #[test]
    fn history_scope_routes_are_constant_authoritative_and_metadata_free() {
        let lock = Mutex::new(());
        let invalid = TestBackend::new("history-key-failure").with_installation_key(vec![0x11; 31]);
        let key_error = resolve_history_scope_with(&invalid, &lock, "claude", None)
            == Err(AccountScopeError::InvalidInstallationKey);
        assert!(key_error, "history constant route key failure");
        invalid.cleanup();
        let backend = TestBackend::new("history-routes");
        let constant = resolve_history_scope_with(&backend, &lock, "claude", None).unwrap();
        let metadata_free = !backend.directory.join(METADATA_FILE).exists()
            && !backend.events().contains(&FsOperation::ReadMetadata);
        assert!(metadata_free, "constant history metadata-free route");
        let first = resolve_test(&backend, &lock, b"marker-one").unwrap();
        let second = resolve_test(&backend, &lock, b"marker-two").unwrap();
        assert!(first != second, "account rotation precondition");
        let stable = resolve_history_scope_with(&backend, &lock, "claude", None).unwrap()
            == constant
            && constant.as_str() != first.as_str()
            && constant.as_str() != second.as_str();
        assert!(stable, "history rotation stability");
        let id = (AuthoritativeIdKind::OpaqueId, "acct-123");
        let authoritative = resolve_history_scope_with(&backend, &lock, "codex", Some(id)).unwrap();
        let account = resolve_authoritative_with(&backend, &lock, "codex", id.0, id.1).unwrap();
        assert!(
            authoritative.as_str() == account.as_str(),
            "authoritative history route"
        );
        backend.cleanup();

        let corrupt = TestBackend::new("authoritative-history-corrupt")
            .with_installation_key(vec![0x11; INSTALLATION_KEY_BYTES]);
        let bytes = b"authoritative-history-corrupt-envelope";
        let active = corrupt.directory.join(METADATA_FILE);
        fs::write(&active, bytes).unwrap();
        let quarantine = corrupt.directory.join(format!(
            "quota-account-scope-v1.corrupt-{}.json",
            corrupt.now_seconds()
        ));
        let lock = Mutex::new(());
        let id = Some((AuthoritativeIdKind::OpaqueId, "acct-history"));
        let resolve = || resolve_history_scope_with(&corrupt, &lock, "codex", id);
        assert!(
            resolve() == Err(AccountScopeError::MetadataCorrupt)
                && fs::read(&quarantine).ok().as_deref() == Some(bytes.as_slice())
                && !active.exists()
                && resolve().is_ok(),
            "AUTHORITATIVE-HISTORY-CORRUPTION-ROUTE quarantine/recovery"
        );
        corrupt.cleanup();
    }

    #[test]
    fn credential_lineage_reuse_fragmentation_and_restart_are_stable() {
        let backend = TestBackend::new("lineage-rules");
        let lock = Mutex::new(());
        let first =
            resolve_credential_with(&backend, &lock, "claude", "file", "slot-a", b"marker-a")
                .unwrap();
        let cross_source = resolve_credential_with(
            &backend,
            &lock,
            "claude",
            "credential-store",
            "slot-b",
            b"marker-a",
        )
        .unwrap();
        let replacement =
            resolve_credential_with(&backend, &lock, "claude", "file", "slot-a", b"marker-b")
                .unwrap();
        assert!(first == cross_source, "cross-source lineage reuse");
        assert!(first != replacement, "external rotation fragmentation");
        let restarted = backend.clone();
        assert!(
            resolve_credential_with(
                &restarted,
                &Mutex::new(()),
                "claude",
                "restart-source",
                "restart-slot",
                b"marker-a",
            )
            .unwrap()
                == first,
            "lineage restart reuse"
        );
        backend.cleanup();
    }

    #[test]
    fn refresh_transitions_preserve_lineage_across_crash_and_known_new_paths() {
        let backend = TestBackend::new("refresh-crash");
        let lock = Mutex::new(());
        let old = resolve_test(&backend, &lock, b"old-marker").unwrap();
        let before = metadata_bytes(&backend);
        assert!(
            resolve_test(&backend, &lock, b"old-marker").unwrap() == old
                && metadata_bytes(&backend) == before,
            "pre-save refresh state"
        );
        let transferred = transfer_credential_with(
            &backend,
            &lock,
            &installation_key(&backend),
            "claude",
            "fixture-source",
            "fixture-location",
            b"old-marker",
            b"new-marker",
        )
        .unwrap();
        assert!(transferred == old, "refresh transfer lineage");
        assert!(
            resolve_test(&backend, &lock, b"old-marker").unwrap() == old
                && resolve_test(&backend, &lock, b"new-marker").unwrap() == old,
            "post-metadata crash recovery"
        );
        backend.cleanup();

        let failed = TestBackend::new("refresh-save-failure");
        let lock = Mutex::new(());
        let old = resolve_credential_with(&failed, &lock, "claude", "s", "l", b"old").unwrap();
        let key = installation_key(&failed);
        let before = metadata_bytes(&failed);
        failed.fail_fs(FsOperation::ReplaceFile);
        assert!(
            transfer_credential_with(&failed, &lock, &key, "claude", "s", "l", b"old", b"new")
                == Err(AccountScopeError::MetadataWrite)
                && metadata_bytes(&failed) == before
                && resolve_credential_with(&failed, &lock, "claude", "s", "l", b"old").unwrap()
                    == old
                && resolve_credential_with(&failed, &lock, "claude", "s", "l", b"new").unwrap()
                    != old,
            "transfer metadata-save failure preserves lineage"
        );
        failed.cleanup();

        let known = TestBackend::new("refresh-known-new");
        let lock = Mutex::new(());
        let known_new = resolve_credential_with(
            &known,
            &lock,
            "claude",
            "credential-store",
            "known-slot",
            b"known-new",
        )
        .unwrap();
        let transferred = transfer_credential_with(
            &known,
            &lock,
            &installation_key(&known),
            "claude",
            "file",
            "refresh-slot",
            b"unseen-old",
            b"known-new",
        )
        .unwrap();
        assert!(transferred == known_new, "known-new lineage reuse");
        assert!(
            resolve_credential_with(
                &known,
                &lock,
                "claude",
                "file",
                "refresh-slot",
                b"unseen-old",
            )
            .unwrap()
                == known_new,
            "old fingerprint backfill"
        );
        known.cleanup();
    }

    #[test]
    fn conflicting_refresh_transfers_have_one_persistent_winner() {
        let backend = TestBackend::new("refresh-conflict");
        let setup_lock = Mutex::new(());
        let scope_a =
            resolve_credential_with(&backend, &setup_lock, "claude", "file", "slot-a", b"old-a")
                .unwrap();
        let scope_b =
            resolve_credential_with(&backend, &setup_lock, "claude", "file", "slot-b", b"old-b")
                .unwrap();
        assert!(scope_a != scope_b, "conflict precondition");
        let key = installation_key(&backend);
        let one_backend = backend.clone();
        let two_backend = backend.clone();
        let one = thread::spawn(move || {
            transfer_credential_with(
                &one_backend,
                &Mutex::new(()),
                &key,
                "claude",
                "file",
                "slot-a",
                b"old-a",
                b"shared-new",
            )
        });
        let two = thread::spawn(move || {
            transfer_credential_with(
                &two_backend,
                &Mutex::new(()),
                &key,
                "claude",
                "file",
                "slot-b",
                b"old-b",
                b"shared-new",
            )
        });
        let results = [one.join().unwrap(), two.join().unwrap()];
        assert!(
            results.iter().filter(|result| result.is_ok()).count() == 1,
            "single transfer winner"
        );
        assert!(
            results
                .iter()
                .filter(|result| **result == Err(AccountScopeError::MetadataConflict))
                .count()
                == 1,
            "single transfer conflict"
        );
        let winner = results
            .iter()
            .find_map(|result| result.as_ref().ok())
            .unwrap()
            .clone();
        assert!(
            resolve_credential_with(
                &backend,
                &Mutex::new(()),
                "claude",
                "restart-source",
                "restart-slot",
                b"shared-new",
            )
            .unwrap()
                == winner,
            "persistent transfer winner"
        );
        backend.cleanup();
    }

    #[test]
    fn installation_key_exact_reload_and_random_rejection_are_stable() {
        let backend = TestBackend::new("key-lifecycle");
        let first = ensure_installation_key(&backend, &Mutex::new(())).unwrap();
        assert!(
            first == [0x11; INSTALLATION_KEY_BYTES]
                && fs::read(installation_key_path(&backend)).unwrap() == first,
            "exact generated and persisted key"
        );
        #[cfg(unix)]
        assert!(
            unix_mode(&backend.directory) == 0o700
                && unix_mode(&installation_key_path(&backend)) == 0o600,
            "installation storage modes"
        );
        backend.write_installation_key(&[0x52; INSTALLATION_KEY_BYTES]);
        let second = ensure_installation_key(&backend, &Mutex::new(())).unwrap();
        assert!(second == [0x52; INSTALLATION_KEY_BYTES], "key reload value");
        assert!(first != second, "key reload bypassed process cache");
        backend.cleanup();

        let unavailable = TestBackend::new("random-unavailable");
        unavailable.set_random(Vec::new());
        assert!(
            resolve_test(&unavailable, &Mutex::new(()), b"marker")
                == Err(AccountScopeError::RandomUnavailable)
                && !installation_key_path(&unavailable).exists()
                && !unavailable.directory.join(METADATA_FILE).exists(),
            "random failure type and artifacts"
        );
        unavailable.cleanup();
    }

    #[test]
    fn installation_key_rejections_preserve_existing_artifacts() {
        let read_failure = TestBackend::new("key-read-failure")
            .with_installation_key(vec![0x61; INSTALLATION_KEY_BYTES]);
        fs::write(read_failure.directory.join(METADATA_FILE), b"metadata").unwrap();
        fs::write(read_failure.directory.join(V3_HISTORY_FILE), b"history").unwrap();
        let before = artifact_snapshot(&read_failure);
        read_failure.fail_fs(FsOperation::ReadInstallationKey);
        assert!(
            ensure_installation_key(&read_failure, &Mutex::new(()))
                == Err(AccountScopeError::InstallationKeyRead)
                && artifact_snapshot(&read_failure) == before,
            "key read failure preservation"
        );
        read_failure.cleanup();

        for (case, length) in [("short", 31), ("trailing", 33)] {
            let backend = TestBackend::new(case).with_installation_key(vec![0x62; length]);
            fs::write(backend.directory.join(METADATA_FILE), b"metadata").unwrap();
            fs::write(backend.directory.join(V3_HISTORY_FILE), b"history").unwrap();
            let before = artifact_snapshot(&backend);
            assert!(
                ensure_installation_key(&backend, &Mutex::new(()))
                    == Err(AccountScopeError::InvalidInstallationKey)
                    && artifact_snapshot(&backend) == before,
                "invalid key length preservation {case}"
            );
            backend.cleanup();
        }
    }

    #[test]
    fn concurrent_installation_key_creation_persists_one_winner() {
        let backend = TestBackend::new("concurrent-key");
        backend.set_random(vec![
            vec![0x31; INSTALLATION_KEY_BYTES],
            vec![0x32; INSTALLATION_KEY_BYTES],
        ]);
        let start = Arc::new(Barrier::new(3));
        let spawn = |backend: TestBackend, start: Arc<Barrier>| {
            thread::spawn(move || {
                start.wait();
                ensure_installation_key(&backend, &Mutex::new(()))
            })
        };
        let one = spawn(backend.clone(), start.clone());
        let two = spawn(backend.clone(), start.clone());
        start.wait();
        let one = one.join().unwrap().unwrap();
        let two = two.join().unwrap().unwrap();
        assert!(one == two, "concurrent key winner");
        assert!(
            fs::read(installation_key_path(&backend)).unwrap() == one,
            "persisted concurrent winner"
        );
        let commits = backend
            .events()
            .iter()
            .filter(|operation| **operation == FsOperation::ReplaceFile)
            .count();
        assert!(commits == 1, "single key commit");
        backend.cleanup();
        let reread = TestBackend::new("persistent-winner-reread");
        reread.set_random(vec![vec![0x41; INSTALLATION_KEY_BYTES]]);
        let replacement = vec![0x42; INSTALLATION_KEY_BYTES];
        reread.replace_installation_key_on_sync(replacement.clone());
        let winner = ensure_installation_key(&reread, &Mutex::new(())).unwrap();
        assert!(
            winner.as_slice() == replacement.as_slice()
                && fs::read(installation_key_path(&reread)).unwrap() == replacement,
            "persistent winner reread"
        );
        reread.cleanup();
    }

    #[test]
    fn atomic_key_and_metadata_commit_boundaries_are_typed_and_recoverable() {
        let precommit = [
            FsOperation::CreateTemp,
            FsOperation::WriteTemp,
            FsOperation::SyncTemp,
            FsOperation::ReplaceFile,
        ];
        for operation in precommit {
            let backend = TestBackend::new("key-precommit");
            backend.fail_fs(operation);
            assert!(
                ensure_installation_key(&backend, &Mutex::new(()))
                    == Err(AccountScopeError::InstallationKeyWrite)
                    && !installation_key_path(&backend).exists()
                    && temp_count(&backend.directory, INSTALLATION_KEY_FILE) == 0,
                "key precommit {operation:?}"
            );
            backend.cleanup();
        }
        let key_postcommit = TestBackend::new("key-postcommit");
        key_postcommit.fail_fs(FsOperation::SyncDirectory);
        assert!(
            ensure_installation_key(&key_postcommit, &Mutex::new(()))
                == Err(AccountScopeError::InstallationKeyWrite),
            "key postcommit type"
        );
        let persisted = fs::read(installation_key_path(&key_postcommit)).unwrap();
        assert!(
            ensure_installation_key(&key_postcommit, &Mutex::new(())).unwrap()
                == persisted.as_slice(),
            "key postcommit recoverability"
        );
        key_postcommit.cleanup();

        for operation in precommit {
            let backend = TestBackend::new("metadata-precommit");
            let lock = Mutex::new(());
            resolve_test(&backend, &lock, b"old-marker").unwrap();
            let before = artifact_snapshot(&backend);
            backend.fail_fs(operation);
            assert!(
                resolve_test(&backend, &lock, b"new-marker")
                    == Err(AccountScopeError::MetadataWrite)
                    && artifact_snapshot(&backend) == before,
                "metadata precommit {operation:?}"
            );
            backend.cleanup();
        }
        let metadata_postcommit = TestBackend::new("metadata-postcommit");
        let lock = Mutex::new(());
        let old = resolve_test(&metadata_postcommit, &lock, b"old-marker").unwrap();
        metadata_postcommit.fail_fs(FsOperation::SyncDirectory);
        assert!(
            resolve_test(&metadata_postcommit, &lock, b"new-marker")
                == Err(AccountScopeError::MetadataWrite),
            "metadata postcommit type"
        );
        decode_metadata(
            &installation_key(&metadata_postcommit),
            &metadata_bytes(&metadata_postcommit),
        )
        .unwrap();
        assert!(
            resolve_test(&metadata_postcommit, &lock, b"new-marker").unwrap() != old
                && temp_count(&metadata_postcommit.directory, METADATA_FILE) == 0,
            "metadata postcommit recoverability"
        );
        metadata_postcommit.cleanup();
    }

    #[test]
    fn key_loss_recovery_preserves_evidence_and_defers_one_poll() {
        let backend = TestBackend::new("key-loss");
        backend.set_random(vec![
            vec![0x31; INSTALLATION_KEY_BYTES],
            vec![0x41; LINEAGE_ID_BYTES],
            vec![0x32; INSTALLATION_KEY_BYTES],
            vec![0x42; LINEAGE_ID_BYTES],
        ]);
        let lock = Mutex::new(());
        let old_scope = resolve_test(&backend, &lock, b"same-marker").unwrap();
        let old_key = installation_key(&backend);
        let old_metadata = metadata_bytes(&backend);
        let history = b"history-evidence";
        fs::write(backend.directory.join(V3_HISTORY_FILE), history).unwrap();
        fs::remove_file(installation_key_path(&backend)).unwrap();
        assert!(
            resolve_test(&backend, &lock, b"same-marker")
                == Err(AccountScopeError::OrphanedArtifacts),
            "key-loss deferral"
        );
        let quarantine = backend
            .directory
            .join("quota-account-scope-v1.orphaned-1752710400.json");
        assert!(
            fs::read(&quarantine).unwrap() == old_metadata,
            "key-loss metadata evidence"
        );
        assert!(
            fs::read(backend.directory.join(V3_HISTORY_FILE)).unwrap() == history,
            "key-loss history preservation"
        );
        let replacement_key = installation_key(&backend);
        assert!(replacement_key != old_key, "key-loss replacement key");
        let replacement_scope = resolve_test(&backend, &lock, b"same-marker").unwrap();
        assert!(replacement_scope != old_scope, "key-loss scope replacement");
        assert!(
            resolve_test(&backend, &lock, b"same-marker").unwrap() == replacement_scope,
            "key-loss stable next poll"
        );
        backend.cleanup();

        let history_only = TestBackend::new("history-only-orphan");
        fs::create_dir_all(&history_only.directory).unwrap();
        fs::write(history_only.directory.join(V3_HISTORY_FILE), history).unwrap();
        assert!(
            resolve_test(&history_only, &Mutex::new(()), b"marker")
                == Err(AccountScopeError::OrphanedArtifacts),
            "history-only deferral"
        );
        assert!(
            fs::read(history_only.directory.join(V3_HISTORY_FILE)).unwrap() == history,
            "history-only preservation"
        );
        assert!(
            resolve_test(&history_only, &Mutex::new(()), b"marker").is_ok(),
            "history-only next poll"
        );
        history_only.cleanup();
    }

    #[test]
    fn orphan_grammar_inspection_and_nonregular_entries_fail_closed() {
        let canonical = "quota-account-scope-v1.orphaned-1752710400.json";
        let is_orphan = is_orphaned_metadata_name;
        for (index, stem, expected) in [
            (0, "0", true),
            (1, "0.1", true),
            (2, "9223372036854775807.4294967295", true),
            (3, "-1", false),
            (4, "+1", false),
            (5, "01", false),
            (6, "9223372036854775808", false),
            (7, "0.1.2", false),
            (8, "0.0", false),
            (9, "0.+1", false),
            (10, "0.01", false),
            (11, "0.4294967296", false),
            (12, "0.-1", false),
        ] {
            let name = format!("quota-account-scope-v1.orphaned-{stem}.json");
            assert!(
                is_orphan(&name) == expected,
                "ORPHAN-CANONICAL-GRAMMAR-MATRIX case {index}: {name}"
            );
        }
        let forged = TestBackend::new("forged-orphan");
        ensure_real_directory(&forged, &forged.directory).unwrap();
        let path = forged
            .directory
            .join("quota-account-scope-v1.orphaned--1.json");
        fs::write(&path, b"forged-evidence").unwrap();
        let preserved = resolve_test(&forged, &Mutex::new(()), b"marker").is_ok()
            && fs::read(&path).unwrap() == b"forged-evidence";
        assert!(preserved, "forged orphan preservation");
        forged.cleanup();
        let inspection = TestBackend::new("orphan-inspection");
        ensure_real_directory(&inspection, &inspection.directory).unwrap();
        let path = inspection.directory.join(canonical);
        fs::write(&path, b"orphan-evidence").unwrap();
        inspection.fail_fs(FsOperation::InspectOrphanedMetadata);
        let failed = resolve_test(&inspection, &Mutex::new(()), b"marker")
            == Err(AccountScopeError::StorageUnavailable)
            && !installation_key_path(&inspection).exists();
        assert!(failed, "orphan inspection failure");
        let retried = resolve_test(&inspection, &Mutex::new(()), b"marker")
            == Err(AccountScopeError::OrphanedArtifacts)
            && fs::read(&path).unwrap() == b"orphan-evidence";
        assert!(retried, "orphan inspection retry");
        inspection.cleanup();
        #[cfg(unix)]
        {
            use std::os::unix::fs::{symlink, PermissionsExt as _};
            for case in ["symlink", "directory"] {
                let backend = TestBackend::new(&format!("orphan-{case}"));
                ensure_real_directory(&backend, &backend.directory).unwrap();
                let path = backend.directory.join(canonical);
                let target = backend.directory.with_extension(format!("{case}-target"));
                if case == "symlink" {
                    fs::write(&target, b"external-evidence").unwrap();
                    fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
                    symlink(&target, &path).unwrap();
                } else {
                    fs::create_dir(&path).unwrap();
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o750)).unwrap();
                }
                let result = resolve_test(&backend, &Mutex::new(()), b"marker");
                let unchanged = if case == "symlink" {
                    path.is_symlink()
                        && fs::read_link(&path).unwrap() == target
                        && fs::read(&target).unwrap() == b"external-evidence"
                        && unix_mode(&target) == 0o640
                } else {
                    !path.is_symlink()
                        && path.is_dir()
                        && unix_mode(&path) == 0o750
                        && fs::read_dir(&path).unwrap().next().is_none()
                };
                let artifacts = artifact_snapshot(&backend);
                let clean = artifacts.key.is_none()
                    && artifacts.metadata.is_none()
                    && artifacts.key_temps + artifacts.metadata_temps == 0
                    && !backend.events().contains(&FsOperation::CreateTemp);
                let safe =
                    result == Err(AccountScopeError::StorageUnavailable) && clean && unchanged;
                assert!(
                    safe,
                    "canonical orphan nonregular fail-closed/no-creation {case}"
                );
                backend.cleanup();
                let _ = fs::remove_file(target);
            }
        }
    }

    #[test]
    fn orphan_key_write_failures_preserve_quarantine_and_defer_again() {
        for operation in [
            FsOperation::CreateTemp,
            FsOperation::WriteTemp,
            FsOperation::SyncTemp,
            FsOperation::ReplaceFile,
        ] {
            let backend = TestBackend::new("orphan-key-write");
            backend.set_random(vec![
                vec![0x11; INSTALLATION_KEY_BYTES],
                vec![0x12; INSTALLATION_KEY_BYTES],
                vec![0x21; LINEAGE_ID_BYTES],
            ]);
            fs::create_dir_all(&backend.directory).unwrap();
            let original = b"orphan-metadata";
            fs::write(backend.directory.join(METADATA_FILE), original).unwrap();
            backend.fail_fs(operation);
            assert!(
                resolve_test(&backend, &Mutex::new(()), b"marker")
                    == Err(AccountScopeError::InstallationKeyWrite),
                "orphan key write type {operation:?}"
            );
            let orphaned = backend
                .directory
                .join("quota-account-scope-v1.orphaned-1752710400.json");
            assert!(
                !installation_key_path(&backend).exists()
                    && !backend.directory.join(METADATA_FILE).exists()
                    && fs::read(&orphaned).unwrap() == original
                    && temp_count(&backend.directory, INSTALLATION_KEY_FILE) == 0,
                "orphan key write artifacts {operation:?}"
            );
            assert!(
                resolve_test(&backend, &Mutex::new(()), b"marker")
                    == Err(AccountScopeError::OrphanedArtifacts),
                "orphan retry deferral {operation:?}"
            );
            assert!(
                resolve_test(&backend, &Mutex::new(()), b"marker").is_ok(),
                "orphan retry recovery {operation:?}"
            );
            backend.cleanup();
        }
    }

    #[test]
    fn metadata_authentication_schema_and_required_fields_quarantine() {
        for case in ["invalid-json", "bad-mac", "wrong-schema", "missing-fields"] {
            let backend =
                TestBackend::new(case).with_installation_key(vec![0x11; INSTALLATION_KEY_BYTES]);
            let key = installation_key(&backend);
            let valid_payload = serde_json::to_vec(&MetadataPayload::default()).unwrap();
            let bytes = match case {
                "invalid-json" => b"not-json".to_vec(),
                "bad-mac" => serde_json::to_vec_pretty(&MetadataEnvelope {
                    schema_version: METADATA_SCHEMA_VERSION,
                    payload_bytes_base64: STANDARD.encode(&valid_payload),
                    payload_mac: encode_digest(&[0xEE; DIGEST_BYTES]),
                })
                .unwrap(),
                "wrong-schema" => {
                    authenticated_envelope(&key, METADATA_SCHEMA_VERSION + 1, &valid_payload)
                }
                "missing-fields" => {
                    authenticated_envelope(&key, METADATA_SCHEMA_VERSION, br#"{"bindings":[]}"#)
                }
                _ => unreachable!(),
            };
            fs::write(backend.directory.join(METADATA_FILE), &bytes).unwrap();
            assert!(
                resolve_test(&backend, &Mutex::new(()), b"marker")
                    == Err(AccountScopeError::MetadataCorrupt),
                "metadata corruption {case}"
            );
            let quarantine = backend.directory.join(format!(
                "quota-account-scope-v1.corrupt-{}.json",
                backend.now_seconds()
            ));
            assert!(
                fs::read(quarantine).ok().as_deref() == Some(bytes.as_slice())
                    && !backend.directory.join(METADATA_FILE).exists(),
                "metadata quarantine {case}"
            );
            assert!(
                resolve_test(&backend, &Mutex::new(()), b"marker").is_ok(),
                "metadata recovery {case}"
            );
            backend.cleanup();
        }

        let backend = TestBackend::new("authoritative-corrupt-route")
            .with_installation_key(vec![0x11; INSTALLATION_KEY_BYTES]);
        let bytes = b"authoritative-corrupt-envelope";
        let active = backend.directory.join(METADATA_FILE);
        fs::write(&active, bytes).unwrap();
        let quarantine = backend.directory.join(format!(
            "quota-account-scope-v1.corrupt-{}.json",
            backend.now_seconds()
        ));
        let lock = Mutex::new(());
        let id = (AuthoritativeIdKind::OpaqueId, "acct-direct");
        let resolve = || resolve_authoritative_with(&backend, &lock, "codex", id.0, id.1);
        assert!(
            resolve() == Err(AccountScopeError::MetadataCorrupt)
                && fs::read(&quarantine).ok().as_deref() == Some(bytes.as_slice())
                && !active.exists()
                && resolve().is_ok(),
            "AUTHORITATIVE-CORRUPTION-QUARANTINE-ROUTE side effects/recovery"
        );
        backend.cleanup();
    }

    #[test]
    fn valid_mac_semantic_conflict_is_preserved_not_quarantined() {
        let backend = TestBackend::new("metadata-conflict");
        let lock = Mutex::new(());
        resolve_test(&backend, &lock, b"old-marker").unwrap();
        let key = installation_key(&backend);
        let mut payload = decode_metadata(&key, &metadata_bytes(&backend)).unwrap();
        let mut duplicate = payload.bindings[0].clone();
        duplicate.random_lineage_id = URL_SAFE_NO_PAD.encode([0xEF; LINEAGE_ID_BYTES]);
        payload.bindings.push(duplicate);
        let payload_bytes = serde_json::to_vec(&payload).unwrap();
        let envelope = authenticated_envelope(&key, METADATA_SCHEMA_VERSION, &payload_bytes);
        fs::write(backend.directory.join(METADATA_FILE), &envelope).unwrap();
        assert!(
            resolve_test(&backend, &lock, b"old-marker")
                == Err(AccountScopeError::MetadataConflict),
            "valid-mac conflict type"
        );
        assert!(
            metadata_bytes(&backend) == envelope,
            "valid-mac conflict preservation"
        );
        assert!(
            !backend
                .directory
                .join(format!(
                    "quota-account-scope-v1.corrupt-{}.json",
                    backend.now_seconds()
                ))
                .exists(),
            "valid-mac conflict quarantine"
        );
        backend.cleanup();
    }

    #[test]
    fn metadata_and_refresh_lock_failures_are_typed_and_preserve_state() {
        let backend = TestBackend::new("metadata-lock");
        let lock = Mutex::new(());
        resolve_test(&backend, &lock, b"old-marker").unwrap();
        let before = artifact_snapshot(&backend);
        backend.fail_fs(FsOperation::AcquireMetadataLock);
        assert!(
            resolve_test(&backend, &lock, b"new-marker") == Err(AccountScopeError::MetadataLock),
            "metadata lock failure type"
        );
        assert!(
            artifact_snapshot(&backend) == before,
            "metadata lock preservation"
        );
        backend.cleanup();

        let refresh = TestBackend::new("refresh-lock");
        let directory = ensure_storage_dir(&refresh).unwrap();
        let file = open_refresh_lock_file(&refresh, &directory, "claude").unwrap();
        #[cfg(unix)]
        assert!(
            unix_mode(&directory.join("quota-auth-refresh-claude.lock")) == 0o600,
            "refresh lock mode"
        );
        fs2::FileExt::unlock(&file).unwrap();
        refresh.fail_fs(FsOperation::AcquireRefreshLock);
        assert!(
            matches!(
                open_refresh_lock_file(&refresh, &directory, "claude"),
                Err(AccountScopeError::MetadataLock)
            ),
            "refresh lock failure type"
        );
        refresh.cleanup();
    }

    #[cfg(unix)]
    #[test]
    fn unix_installation_key_final_object_attacks_fail_closed() {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        for case in ["symlink", "dangling", "nonregular", "mode"] {
            let backend = TestBackend::new(case);
            ensure_real_directory(&backend, &backend.directory).unwrap();
            let key_path = installation_key_path(&backend);
            let external = backend.directory.with_extension("key-target");
            match case {
                "symlink" => {
                    fs::write(&external, [0x71; INSTALLATION_KEY_BYTES]).unwrap();
                    fs::set_permissions(&external, fs::Permissions::from_mode(0o600)).unwrap();
                    symlink(&external, &key_path).unwrap();
                }
                "dangling" => symlink(&external, &key_path).unwrap(),
                "nonregular" => fs::create_dir(&key_path).unwrap(),
                "mode" => {
                    fs::write(&key_path, [0x72; INSTALLATION_KEY_BYTES]).unwrap();
                    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o640)).unwrap();
                }
                _ => unreachable!(),
            }
            fs::write(backend.directory.join(METADATA_FILE), b"metadata").unwrap();
            fs::write(backend.directory.join(V3_HISTORY_FILE), b"history").unwrap();
            assert!(
                ensure_installation_key(&backend, &Mutex::new(()))
                    == Err(AccountScopeError::InvalidInstallationKey),
                "key final object {case}"
            );
            assert!(
                metadata_bytes(&backend) == b"metadata"
                    && fs::read(backend.directory.join(V3_HISTORY_FILE)).unwrap() == b"history",
                "key final object artifacts {case}"
            );
            match case {
                "symlink" => assert!(
                    fs::read(&external).unwrap() == [0x71; INSTALLATION_KEY_BYTES]
                        && fs::symlink_metadata(&key_path)
                            .unwrap()
                            .file_type()
                            .is_symlink(),
                    "key symlink target"
                ),
                "dangling" => assert!(
                    fs::symlink_metadata(&key_path)
                        .unwrap()
                        .file_type()
                        .is_symlink()
                        && !external.exists(),
                    "key dangling symlink"
                ),
                "nonregular" => assert!(key_path.is_dir(), "key nonregular object"),
                "mode" => assert!(
                    fs::read(&key_path).unwrap() == [0x72; INSTALLATION_KEY_BYTES]
                        && unix_mode(&key_path) == 0o640,
                    "key mode preservation"
                ),
                _ => unreachable!(),
            }
            backend.cleanup();
            if external.exists() {
                fs::remove_file(external).unwrap();
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_installation_key_inode_replacement_fails_closed() {
        let backend = TestBackend::new("key-inode-swap")
            .with_installation_key(vec![0x73; INSTALLATION_KEY_BYTES]);
        fs::write(backend.directory.join(METADATA_FILE), b"metadata").unwrap();
        fs::write(backend.directory.join(V3_HISTORY_FILE), b"history").unwrap();
        backend.replace_installation_key_on_validate(vec![0x74; INSTALLATION_KEY_BYTES]);
        assert!(
            ensure_installation_key(&backend, &Mutex::new(()))
                == Err(AccountScopeError::InvalidInstallationKey)
                && metadata_bytes(&backend) == b"metadata"
                && fs::read(backend.directory.join(V3_HISTORY_FILE)).unwrap() == b"history"
                && fs::read(installation_key_path(&backend)).unwrap()
                    == [0x74; INSTALLATION_KEY_BYTES],
            "key inode replacement artifacts"
        );
        backend.cleanup();
    }

    #[cfg(unix)]
    #[test]
    fn unix_metadata_and_lock_symlinks_fail_closed_before_mutation() {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        let missing = TestBackend::new("metadata-symlink-missing-key");
        ensure_real_directory(&missing, &missing.directory).unwrap();
        let path = missing.directory.join(METADATA_FILE);
        let target = missing.directory.with_extension("metadata-no-key-target");
        fs::write(&target, b"external-metadata").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        symlink(&target, &path).unwrap();
        let result = resolve_test(&missing, &Mutex::new(()), b"marker");
        let events = missing.events();
        assert!(
            result == Err(AccountScopeError::StorageUnavailable)
                && fs::read(&target).unwrap() == b"external-metadata"
                && unix_mode(&target) == 0o640
                && path.is_symlink()
                && !installation_key_path(&missing).exists()
                && events.contains(&FsOperation::InspectArtifacts)
                && !events.contains(&FsOperation::QuarantineMetadata)
                && !events.contains(&FsOperation::CreateTemp),
            "missing-key active metadata symlink boundary"
        );
        missing.cleanup();
        fs::remove_file(target).unwrap();

        let metadata = TestBackend::new("metadata-symlink")
            .with_installation_key(vec![0x11; INSTALLATION_KEY_BYTES]);
        let target = metadata.directory.with_extension("metadata-target");
        fs::write(&target, b"external-metadata").unwrap();
        symlink(&target, metadata.directory.join(METADATA_FILE)).unwrap();
        assert!(
            resolve_test(&metadata, &Mutex::new(()), b"marker")
                == Err(AccountScopeError::MetadataRead)
                && fs::read(&target).unwrap() == b"external-metadata"
                && !metadata.events().contains(&FsOperation::QuarantineMetadata),
            "metadata symlink boundary"
        );
        metadata.cleanup();
        fs::remove_file(target).unwrap();

        let lock = TestBackend::new("metadata-lock-symlink")
            .with_installation_key(vec![0x11; INSTALLATION_KEY_BYTES]);
        let target = lock.directory.with_extension("metadata-lock-target");
        fs::write(&target, b"external-lock").unwrap();
        symlink(&target, lock.directory.join(METADATA_LOCK_FILE)).unwrap();
        let result = resolve_test(&lock, &Mutex::new(()), b"marker");
        let events = lock.events();
        assert!(
            result == Err(AccountScopeError::MetadataLock)
                && fs::read(&target).unwrap() == b"external-lock"
                && events.contains(&FsOperation::OpenMetadataLock)
                && !events.contains(&FsOperation::AcquireMetadataLock)
                && !events.contains(&FsOperation::ReadMetadata),
            "metadata lock symlink boundary"
        );
        lock.cleanup();
        fs::remove_file(target).unwrap();

        let refresh = TestBackend::new("refresh-lock-symlink");
        let directory = ensure_storage_dir(&refresh).unwrap();
        let target = refresh.directory.with_extension("refresh-lock-target");
        fs::write(&target, b"external-refresh-lock").unwrap();
        symlink(&target, directory.join("quota-auth-refresh-claude.lock")).unwrap();
        assert!(
            matches!(
                open_refresh_lock_file(&refresh, &directory, "claude"),
                Err(AccountScopeError::MetadataLock)
            ) && fs::read(&target).unwrap() == b"external-refresh-lock"
                && !refresh.events().contains(&FsOperation::AcquireRefreshLock),
            "refresh lock symlink boundary"
        );
        refresh.cleanup();
        fs::remove_file(target).unwrap();

        let restored = TestBackend::new("metadata-mode");
        let lock = Mutex::new(());
        resolve_test(&restored, &lock, b"marker").unwrap();
        let path = restored.directory.join(METADATA_FILE);
        let before = fs::read(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        load_metadata(&restored, &restored.directory, &installation_key(&restored)).unwrap();
        assert!(
            fs::read(&path).unwrap() == before && unix_mode(&path) == 0o600,
            "metadata mode tightening"
        );
        restored.cleanup();
    }

    #[cfg(unix)]
    #[test]
    fn unix_final_directory_symlink_is_rejected_but_ancestor_is_allowed() {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        let final_link = TestBackend::new("final-directory-symlink");
        let target = final_link.directory.with_extension("directory-target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&target, &final_link.directory).unwrap();
        assert!(
            resolve_test(&final_link, &Mutex::new(()), b"marker")
                == Err(AccountScopeError::StorageUnavailable)
                && unix_mode(&target) == 0o755
                && fs::read_dir(&target).unwrap().next().is_none()
                && !final_link.events().contains(&FsOperation::OpenMetadataLock),
            "final directory symlink target"
        );
        fs::remove_file(&final_link.directory).unwrap();
        fs::remove_dir(target).unwrap();

        let mut ancestor = TestBackend::new("ancestor-symlink");
        let seed = ancestor.directory.clone();
        let real_parent = seed.with_extension("real-parent");
        let linked_parent = seed.with_extension("linked-parent");
        ancestor.directory = linked_parent.join("com.nyanako.tokenbar");
        fs::create_dir(&real_parent).unwrap();
        symlink(&real_parent, &linked_parent).unwrap();
        assert!(
            resolve_test(&ancestor, &Mutex::new(()), b"marker").is_ok()
                && fs::symlink_metadata(&ancestor.directory)
                    .unwrap()
                    .file_type()
                    .is_dir(),
            "ancestor symlink admission"
        );
        fs::remove_file(linked_parent).unwrap();
        fs::remove_dir_all(real_parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unix_quarantine_collision_and_identity_bound_rollback_are_safe() {
        use std::os::unix::fs::{symlink, MetadataExt as _, PermissionsExt as _};
        let base_name = "quota-account-scope-v1.corrupt-1752710400";
        let raced = TestBackend::new("quarantine-raced-reservation");
        fs::create_dir_all(&raced.directory).unwrap();
        let source = raced.directory.join(METADATA_FILE);
        let reservation = raced.directory.join(format!("{base_name}.json"));
        let selected = raced.directory.join(format!("{base_name}.1.json"));
        let missing = raced.directory.with_extension("raced-missing-target");
        fs::write(&source, b"source-evidence").unwrap();
        let source_inode = fs::metadata(&source).unwrap().ino();
        let mut first_link = true;
        let quarantined = quarantine_metadata_with(
            &raced,
            &source,
            "corrupt",
            |source, candidate| {
                if std::mem::take(&mut first_link) {
                    symlink(&missing, candidate)?;
                }
                fs::hard_link(source, candidate)
            },
            |source| fs::remove_file(source),
        );
        assert!(
            quarantined.as_deref() == Ok(selected.as_path())
                && reservation.is_symlink()
                && fs::read_link(&reservation).ok().as_deref() == Some(missing.as_path())
                && !missing.exists()
                && fs::read(&selected).ok().as_deref() == Some(b"source-evidence".as_slice())
                && fs::metadata(&selected).map(|metadata| metadata.ino()).ok()
                    == Some(source_inode)
                && !source.exists(),
            "UNIX-QUARANTINE-RACED-ALREADYEXISTS"
        );
        raced.cleanup();

        let collision = TestBackend::new("quarantine-collision");
        fs::create_dir_all(&collision.directory).unwrap();
        let source = collision.directory.join(METADATA_FILE);
        let base = collision.directory.join(format!("{base_name}.json"));
        let first = collision.directory.join(format!("{base_name}.1.json"));
        let second = collision.directory.join(format!("{base_name}.2.json"));
        let missing = collision.directory.with_extension("missing-target");
        fs::write(&source, b"source-evidence").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).unwrap();
        let source_inode = fs::metadata(&source).unwrap().ino();
        symlink(&missing, &base).unwrap();
        fs::write(&first, b"collision-evidence").unwrap();
        let quarantined = quarantine_metadata(&collision, &source, "corrupt");
        let reserved = base.is_symlink()
            && fs::read_link(&base).ok().as_deref() == Some(missing.as_path())
            && !missing.exists();
        assert!(reserved, "quarantine dangling symlink reservation");
        let selected = quarantined.as_deref() == Ok(second.as_path());
        assert!(selected, "quarantine multi-collision selects .2");
        let preserved = fs::read(&first).unwrap() == b"collision-evidence"
            && fs::read(&second).unwrap() == b"source-evidence"
            && fs::metadata(&second).unwrap().ino() == source_inode
            && !source.exists();
        assert!(preserved, "quarantine collision artifacts");
        collision.cleanup();
        for (tag, replace) in [
            ("quarantine-unlink-rollback", false),
            ("quarantine-rollback", true),
        ] {
            let rollback = TestBackend::new(tag);
            fs::create_dir_all(&rollback.directory).unwrap();
            let source = rollback.directory.join(METADATA_FILE);
            let candidate = rollback.directory.join(format!("{base_name}.json"));
            fs::write(&source, b"source-evidence").unwrap();
            fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
            let result = quarantine_metadata_with(
                &rollback,
                &source,
                "corrupt",
                |source, candidate| fs::hard_link(source, candidate),
                |_source| {
                    if replace {
                        fs::remove_file(&candidate)?;
                        fs::write(&candidate, b"replacement-evidence")?;
                        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o600))?;
                    }
                    Err(io::Error::new(io::ErrorKind::PermissionDenied, "injected"))
                },
            );
            let failed = result == Err(AccountScopeError::QuarantineFailed);
            let unsynced = !rollback.events().contains(&FsOperation::SyncDirectory);
            if replace {
                assert!(failed, "quarantine rollback type");
                let preserved = fs::read(&source).unwrap() == b"source-evidence"
                    && fs::read(&candidate).ok().as_deref() == Some(b"replacement-evidence")
                    && unsynced;
                assert!(preserved, "quarantine identity-bound rollback");
            } else {
                let restored = failed
                    && fs::read(&source).unwrap() == b"source-evidence"
                    && unix_mode(&source) == 0o600
                    && !candidate.exists()
                    && unsynced;
                assert!(
                    restored,
                    "quarantine unlink failure rolls back original candidate"
                );
            }
            rollback.cleanup();
        }
    }

    #[test]
    fn quarantine_failures_preserve_source_and_key_state() {
        let orphan = TestBackend::new("orphan-quarantine-failure");
        fs::create_dir_all(&orphan.directory).unwrap();
        fs::write(orphan.directory.join(METADATA_FILE), b"orphan-evidence").unwrap();
        orphan.fail_fs(FsOperation::QuarantineMetadata);
        assert!(
            resolve_test(&orphan, &Mutex::new(()), b"marker")
                == Err(AccountScopeError::QuarantineFailed)
                && metadata_bytes(&orphan) == b"orphan-evidence"
                && !installation_key_path(&orphan).exists(),
            "orphan quarantine failure preservation"
        );
        orphan.cleanup();

        let corrupt = TestBackend::new("corrupt-quarantine-failure")
            .with_installation_key(vec![0x11; INSTALLATION_KEY_BYTES]);
        fs::write(corrupt.directory.join(METADATA_FILE), b"corrupt-evidence").unwrap();
        corrupt.fail_fs(FsOperation::QuarantineMetadata);
        assert!(
            resolve_authoritative_with(
                &corrupt,
                &Mutex::new(()),
                "codex",
                AuthoritativeIdKind::OpaqueId,
                "acct-direct",
            ) == Err(AccountScopeError::QuarantineFailed)
                && metadata_bytes(&corrupt) == b"corrupt-evidence",
            "AUTHORITATIVE-CORRUPTION-QUARANTINE-ROUTE injected failure preservation"
        );
        corrupt.cleanup();
    }

    #[test]
    fn persisted_artifacts_and_errors_hide_raw_identity_across_storage_modes() {
        let backends = vec![TestBackend::new("privacy-default")];

        for (owner_index, backend) in backends.into_iter().enumerate() {
            let lock = Mutex::new(());
            let raw_values = [
                "fixture-secret-refresh-token",
                "User.LowEntropy@example.com",
                "/private/fixture/auth.json",
                "Fixture Display Label",
                "Provider-Account-ID-ByteCase",
            ];
            let credential_scope = resolve_credential_with(
                &backend,
                &lock,
                "grok",
                "auth-json",
                raw_values[2],
                raw_values[0].as_bytes(),
            )
            .unwrap();
            let email_scope = resolve_authoritative_with(
                &backend,
                &lock,
                "antigravity",
                AuthoritativeIdKind::Email,
                raw_values[1],
            )
            .unwrap();
            let id_scope = resolve_authoritative_with(
                &backend,
                &lock,
                "codex",
                AuthoritativeIdKind::OpaqueId,
                raw_values[4],
            )
            .unwrap();
            let constant_history =
                resolve_history_scope_with(&backend, &lock, "grok", None).unwrap();
            let authoritative_history = resolve_history_scope_with(
                &backend,
                &lock,
                "codex",
                Some((AuthoritativeIdKind::OpaqueId, raw_values[4])),
            )
            .unwrap();
            fs::write(
                backend.directory.join(V3_HISTORY_FILE),
                format!(
                    r#"{{"accountScopes":["{}","{}","{}"],"historyScopes":["{}","{}"]}}"#,
                    credential_scope.as_str(),
                    email_scope.as_str(),
                    id_scope.as_str(),
                    constant_history.as_str(),
                    authoritative_history.as_str()
                ),
            )
            .unwrap();
            let metadata = metadata_bytes(&backend);
            let envelope: MetadataEnvelope = serde_json::from_slice(&metadata).unwrap();
            let payload = STANDARD
                .decode(envelope.payload_bytes_base64.as_bytes())
                .unwrap();
            let debug_values = format!(
                "{credential_scope:?} {email_scope:?} {id_scope:?} {constant_history:?} {authoritative_history:?}"
            )
            .into_bytes();
            let artifacts = vec![
                fs::read(installation_key_path(&backend)).unwrap(),
                metadata,
                payload,
                fs::read(backend.directory.join(V3_HISTORY_FILE)).unwrap(),
                debug_values,
            ];
            assert_no_sensitive(
                &artifacts,
                &raw_values,
                if owner_index == 0 {
                    "common privacy"
                } else {
                    "windows consumer privacy"
                },
            );
            #[cfg(unix)]
            {
                assert!(
                    unix_mode(&backend.directory) == 0o700,
                    "privacy directory mode"
                );
                for path in [
                    backend.directory.join(INSTALLATION_KEY_FILE),
                    backend.directory.join(METADATA_FILE),
                    backend.directory.join(METADATA_LOCK_FILE),
                ] {
                    assert!(unix_mode(&path) == 0o600, "privacy owner-only file mode");
                }
            }

            fs::write(backend.directory.join(METADATA_FILE), b"corrupt-envelope").unwrap();
            let error = resolve_authoritative_with(
                &backend,
                &lock,
                "antigravity",
                AuthoritativeIdKind::Email,
                raw_values[1],
            )
            .unwrap_err();
            assert!(
                error == AccountScopeError::MetadataCorrupt,
                "privacy error type"
            );
            let error_text = format!("{error:?} {error}").to_ascii_lowercase();
            for (value_index, value) in raw_values.iter().enumerate() {
                assert!(
                    !error_text.contains(&value.to_ascii_lowercase()),
                    "privacy error value {value_index}"
                );
            }
            assert!(
                !error_text.contains(&backend.directory.to_string_lossy().to_ascii_lowercase()),
                "privacy error path"
            );
            for (name_index, name) in fs::read_dir(&backend.directory)
                .unwrap()
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().to_ascii_lowercase())
                .enumerate()
            {
                for (value_index, value) in raw_values.iter().enumerate() {
                    assert!(
                        !name.contains(&value.to_ascii_lowercase()),
                        "privacy filename {name_index} value {value_index}"
                    );
                }
            }
            backend.cleanup();
        }
    }
}
