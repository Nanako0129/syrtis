//! The embedded remote-usage agent (`syrtis-agent`): one `Agent` per process,
//! its management operations, and the copy of the app's pricing files into
//! the agent's pricing directory.
//!
//! Nothing about pairing, grants, authorization, transport, caches or
//! uninstall is computed here. Management operations that exist as frozen
//! control commands (offer create and cancel, pending list, approve, reject,
//! revoke, listen configure) go through `control::Client` against the agent's
//! own socket, with the control token read from the state store and held only
//! in memory for that call. The only direct calls are
//! `Runtime::enable_sharing` / `disable_sharing`, `Agent::approve_scope`, the
//! receiver's `PairingClient` and `PeerStore`, and one `set_listen_config` that
//! writes the node label back with the host and port the listen command just
//! stored (the frozen listen command carries no label).
//!
//! Errors are fixed class strings: never a path, a token, a Pair URL or an
//! address. The FFI entry points in `lib.rs` wrap one process-wide
//! [`RemoteAgent`]; tests build their own instances.

use std::{
    collections::BTreeMap,
    ffi::c_char,
    fs,
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{mpsc, Arc, LazyLock, Mutex, MutexGuard, PoisonError},
    thread::{self, JoinHandle},
    time::Duration,
};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use syrtis_agent::{
    agent::{Agent, AgentConfig, AgentError, ScannerSettings},
    control::{self, Client, Command, ControlError},
    noise::StaticKey,
    pairing::{ClaimAttempts, Claimed, ClientError, Clock, PairingClient, PeerCode},
    peer_store::{PeerRecord, PeerState},
    runtime::{Options, RuntimeError},
    secure_fs::StorageError,
    security::{
        clock_status, decode_fixed_hex, hex_encode, GrantState, MemoryLedger, SecurityError,
    },
    source::owner_inputs_sha256,
    state_store::{Secret32, NODE_LABEL_MAX_BYTES},
};

pub(crate) type Reply = Result<Value, String>;

/// The engine's pricing files (`RemotePricingSnapshot::from_cache_root`).
pub(crate) const PRICING_FILES: [&str; 2] = ["pricing-litellm.json", "pricing-openrouter.json"];
/// How often a newer app pricing file is copied again. The app's pricing
/// refresh has no FFI event to hook, so the copy polls mtimes.
pub(crate) const PRICING_PERIOD: Duration = Duration::from_secs(300);
/// The secure root below an override home. Short on purpose: the frozen
/// suffix below a `/tmp` or `.build` home overflows the 104-byte `sun_path`
/// of the control socket.
pub(crate) const OVERRIDE_SUFFIX: &[&str] = &["a"];
/// How long a (re)start waits for the previous owner lock to be released.
const BUSY_RETRIES: u32 = 100;
const BUSY_PAUSE: Duration = Duration::from_millis(20);

/// Everything an agent is started with. Built under the [`RemoteAgent`] lock
/// so two starts cannot apply an older snapshot after a newer one.
pub(crate) struct Inputs {
    pub home: PathBuf,
    pub state_suffix: &'static [&'static str],
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub settings: ScannerSettings,
    /// The app's pricing cache directory (the copy source).
    pub pricing_source: PathBuf,
    pub pricing_period: Duration,
    pub clock: Arc<dyn Clock>,
}

struct Running {
    // Field order is drop order: the pricing thread ends before the agent.
    _pricing: PricingSync,
    agent: Agent,
    inputs: Inputs,
    digest: String,
    /// Process-wide start counter; a restart gets a new one. Tests read it
    /// to tell a restart from a no-op.
    #[cfg(test)]
    epoch: u64,
}

#[derive(Default)]
struct Claims {
    next: u64,
    open: BTreeMap<u64, Claimed>,
    /// Kept for the life of the process: `PairingClient::claim` needs the
    /// attempts of earlier calls for the same offer.
    attempts: ClaimAttempts,
}

/// One embedded agent and the receiver's claims in progress.
#[derive(Default)]
pub(crate) struct RemoteAgent {
    running: Mutex<Option<Running>>,
    claims: Mutex<Claims>,
}

/// The process's agent, behind the `tb_remote_*` entry points.
pub(crate) static AGENT: LazyLock<RemoteAgent> = LazyLock::new(RemoteAgent::default);

/// An entry point body: the existing `guarded` boundary, with any panic
/// inside the agent answered by the fixed class `panic` (no panic text,
/// which `guarded` would put into `err` and Swift would log).
pub(crate) fn ffi(name: &str, body: impl FnOnce() -> Reply) -> *mut c_char {
    crate::guarded(name, || {
        crate::envelope(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(body))
                .unwrap_or_else(|_| Err("panic".into())),
        )
    })
}

#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
static EPOCH: AtomicU64 = AtomicU64::new(0);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl RemoteAgent {
    /// Starts the agent with the inputs `make_inputs` builds now. Already
    /// running with the same home and the same owner-inputs digest: nothing
    /// happens. Different: the agent is stopped and started with the new
    /// inputs, so a scan root the owner removed is no longer shared (the new
    /// digest has no approved scope until the owner approves it again).
    /// Returns the status.
    pub(crate) fn start(&self, make_inputs: impl FnOnce() -> Result<Inputs, String>) -> Reply {
        let mut running = lock(&self.running);
        let inputs = make_inputs()?;
        if let Some(current) = running.as_ref() {
            let same_root = current.inputs.home == inputs.home
                && current.inputs.state_suffix == inputs.state_suffix;
            let digest = digest_for(&inputs, current.agent.source().source_cache_dir());
            if same_root && digest.as_deref() == Some(current.digest.as_str()) {
                return status_of(current);
            }
        }
        // Drop (stop) the old agent before the new one takes the owner lock.
        *running = None;
        let agent = start_agent(&inputs)?;
        let digest = digest_for(&inputs, agent.source().source_cache_dir())
            .ok_or_else(|| "invalid".to_string())?;
        let destination = agent.pricing_dir().to_path_buf();
        sync_pricing(&inputs.pricing_source, &destination);
        let pricing = PricingSync::spawn(
            inputs.pricing_source.clone(),
            destination,
            inputs.pricing_period,
        );
        let current = running.insert(Running {
            _pricing: pricing,
            agent,
            inputs,
            digest,
            #[cfg(test)]
            epoch: EPOCH.fetch_add(1, Ordering::Relaxed) + 1,
        });
        status_of(current)
    }

    /// `Runtime::stop` (by dropping the agent). Stopped already: the same
    /// answer.
    pub(crate) fn stop(&self) -> Reply {
        let old = lock(&self.running).take();
        drop(old);
        Ok(json!({"running": false}))
    }

    /// Control `status` plus `running`, `scopeApproved`, `listening` and the
    /// identity's short fingerprint; `{"running":false}` when stopped.
    pub(crate) fn status(&self) -> Reply {
        match lock(&self.running).as_ref() {
            Some(current) => status_of(current),
            None => Ok(json!({"running": false})),
        }
    }

    /// `Runtime::enable_sharing` / `disable_sharing` only. Errors: the enable
    /// gate's `Busy` is `busy`; enabling while on is `invalid`; disabling with
    /// nothing to end is `ok`.
    pub(crate) fn sharing_set(&self, enabled: bool) -> Reply {
        self.with(|current| {
            let runtime = current.agent.runtime();
            let result = if enabled {
                runtime.enable_sharing()
            } else {
                runtime.disable_sharing()
            };
            result.map(|()| Value::Null).map_err(storage_class)
        })
    }

    /// The frozen `listenConfigure`, then the node label written with the
    /// host and port that command stored. Returns the status.
    pub(crate) fn listen_configure(&self, host: &str, port: u16, label: Option<String>) -> Reply {
        if label
            .as_ref()
            .is_some_and(|label| label.len() > NODE_LABEL_MAX_BYTES)
        {
            return Err("invalid".into());
        }
        self.with(|current| {
            call(
                current,
                control::LISTEN_CONFIGURE,
                json!({"bindHost": host, "bindPort": port}),
            )?;
            let store = current.agent.runtime().store();
            let config = store.state().map_err(storage_class)?.config;
            store
                .set_listen_config(config.bind_host, config.bind_port, label)
                .map_err(storage_class)?;
            status_of(current)
        })
    }

    /// What the running agent scans, from the inputs it was started with, and
    /// their `ownerInputsSha256`. `envRoots` lists the per-client root
    /// variables set in this process: the local pages follow them, the
    /// shared source does not.
    pub(crate) fn scope_disclosure(&self) -> Reply {
        self.with(|current| {
            let inputs = &current.inputs;
            let text = |path: &Path| path.to_string_lossy().into_owned();
            let map = |paths: &std::collections::BTreeMap<String, Vec<PathBuf>>| {
                paths
                    .iter()
                    .map(|(client, paths)| {
                        (client.clone(), paths.iter().map(|p| text(p)).collect::<Vec<_>>())
                    })
                    .collect::<BTreeMap<_, _>>()
            };
            let env_roots = env_roots();
            Ok(json!({
                "home": text(&inputs.home),
                "configDir": text(&inputs.config_dir),
                "dataDir": text(&inputs.data_dir),
                "extraScanPaths": map(&inputs.settings.extra_scan_paths),
                "excludedScanPaths": map(&inputs.settings.excluded_scan_paths),
                "opencodeDbPaths": inputs.settings.opencode_db_paths.iter().map(|p| text(p)).collect::<Vec<_>>(),
                "clients": tokscale_core::ClientId::iter().map(|c| c.as_str()).collect::<Vec<_>>(),
                "ownerInputsSha256": current.digest,
                "envRootsSet": !env_roots.is_empty(),
                "envRoots": env_roots,
            }))
        })
    }

    /// Owner consent to the disclosed scope: refused (`invalid`, nothing
    /// written) unless `owner_inputs_sha256` is the running agent's digest,
    /// so the consent binds to what the owner was shown.
    pub(crate) fn scope_approve(&self, owner_inputs_sha256: &str) -> Reply {
        self.with(|current| {
            if owner_inputs_sha256 != current.digest {
                return Err("invalid".into());
            }
            current
                .agent
                .approve_scope()
                .map(|()| Value::Null)
                .map_err(storage_class)
        })
    }

    /// The frozen `offerCreate`: `{expiresAt, offerId, pairUrl}`.
    pub(crate) fn offer_create(&self, ttl_seconds: i64) -> Reply {
        self.with(|current| {
            call(
                current,
                control::OFFER_CREATE,
                json!({"ttlSeconds": ttl_seconds}),
            )
        })
    }

    pub(crate) fn offer_cancel(&self, offer_id: &str) -> Reply {
        self.with(|current| call(current, control::OFFER_CANCEL, json!({"offerId": offer_id})))
    }

    /// The frozen `pendingList`: `[{expiresAt, pendingId, sasCode}]`.
    pub(crate) fn pending_list(&self) -> Reply {
        self.with(|current| call(current, control::PENDING_LIST, json!({})))
    }

    /// The frozen `approve` or `reject`.
    pub(crate) fn decide(&self, approve: bool, pending_id: &str, sas_generation: i64) -> Reply {
        let id = if approve {
            control::APPROVE
        } else {
            control::REJECT
        };
        self.with(|current| {
            call(
                current,
                id,
                json!({"pendingId": pending_id, "sasGeneration": sas_generation}),
            )
        })
    }

    /// The producer's grants, read from the state store.
    pub(crate) fn grants(&self) -> Reply {
        self.with(|current| {
            let state = current
                .agent
                .runtime()
                .store()
                .state()
                .map_err(storage_class)?;
            Ok(Value::Array(
                state
                    .reference
                    .grants
                    .iter()
                    .map(|grant| {
                        json!({
                            "grantId": hex_encode(&grant.grant_id),
                            "peerFingerprint": fingerprint(&grant.initiator_static),
                            "createdAt": grant.created_at_ms,
                            "expiresAt": grant.expires_at_ms,
                            "state": match grant.state {
                                GrantState::Active => "active",
                                GrantState::Revoked => "revoked",
                                GrantState::Expired => "expired",
                            },
                        })
                    })
                    .collect(),
            ))
        })
    }

    /// The frozen `revoke`.
    pub(crate) fn revoke(&self, grant_id: &str) -> Reply {
        self.with(|current| call(current, control::REVOKE, json!({"grantId": grant_id})))
    }

    /// Receiver: Pair claim. Returns a handle for [`pair_poll`] and the SAS
    /// to compare with the producer's. The URL is never kept or formatted.
    pub(crate) fn pair_claim(&self, pair_url: &str) -> Reply {
        let (identity, now_ms, clock) = self.with(|current| {
            let state = current
                .agent
                .runtime()
                .store()
                .state()
                .map_err(storage_class)?;
            let identity = StaticKey::from_state(&state).map_err(storage_class)?;
            let os_now = current
                .inputs
                .clock
                .os_now_seconds()
                .map_err(storage_class)?;
            let now = clock_status(os_now, state.reference.last_accepted_wall_unix_seconds());
            let now_ms = now.effective_now.checked_mul(1_000).ok_or("invalid")?;
            Ok((identity, now_ms, Arc::clone(&current.inputs.clock)))
        })?;
        let ledger = MemoryLedger::default();
        let client = PairingClient {
            identity: &identity,
            ledger: &ledger,
            clock: &*clock,
        };
        let mut claims = lock(&self.claims);
        let claimed = client
            .claim(pair_url, now_ms, &mut claims.attempts)
            .map_err(client_class)?;
        claims.next += 1;
        let handle = claims.next;
        let reply = json!({
            "handle": handle,
            "sas": claimed.sas,
            "expiresAt": claimed.expires_at_ms,
            "producerFingerprint": fingerprint(&claimed.responder_static),
        });
        claims.open.insert(handle, claimed);
        Ok(reply)
    }

    /// Receiver: waits (blocking, up to the frozen poll window) for the
    /// producer's approval; on success the producer is written to
    /// `peers.json`. The handle ends with any answer.
    pub(crate) fn pair_poll(&self, handle: u64) -> Reply {
        let claimed = lock(&self.claims)
            .open
            .get(&handle)
            .cloned()
            .ok_or("invalid")?;
        let (identity, clock) = self.with(|current| {
            let state = current
                .agent
                .runtime()
                .store()
                .state()
                .map_err(storage_class)?;
            let identity = StaticKey::from_state(&state).map_err(storage_class)?;
            Ok((identity, Arc::clone(&current.inputs.clock)))
        })?;
        let ledger = MemoryLedger::default();
        let client = PairingClient {
            identity: &identity,
            ledger: &ledger,
            clock: &*clock,
        };
        // No lock is held while this waits.
        let result = client.poll(&claimed);
        lock(&self.claims).open.remove(&handle);
        let confirmed = result.map_err(client_class)?;
        self.with(|current| {
            let now_ms = current
                .inputs
                .clock
                .os_now_seconds()
                .map_err(storage_class)?
                .saturating_mul(1_000);
            current
                .agent
                .runtime()
                .peers()
                .upsert(PeerRecord::from_confirmed(&confirmed, None, now_ms))
                .map_err(storage_class)?;
            Ok(json!({
                "producerStatic": hex_encode(&confirmed.responder_static),
                "fingerprint": fingerprint(&confirmed.responder_static),
                "expiresAt": confirmed.expires_at_ms,
            }))
        })
    }

    /// Receiver: the paired producers.
    pub(crate) fn peers(&self) -> Reply {
        self.with(|current| {
            let peers = current
                .agent
                .runtime()
                .peers()
                .peers()
                .map_err(storage_class)?;
            Ok(Value::Array(
                peers
                    .iter()
                    .map(|peer| {
                        json!({
                            "producerStatic": hex_encode(&peer.responder_static),
                            "fingerprint": fingerprint(&peer.responder_static),
                            "nodeLabel": peer.node_label,
                            "createdAt": peer.created_at_ms,
                            "expiresAt": peer.expires_at_ms,
                            "state": match peer.state {
                                PeerState::Active => "active",
                                PeerState::Revoked => "revoked",
                                PeerState::Expired => "expired",
                            },
                        })
                    })
                    .collect(),
            ))
        })
    }

    /// Receiver: forgets one producer (`producerStatic`, 64 lowercase hex).
    pub(crate) fn peer_remove(&self, producer_static: &str) -> Reply {
        let key = decode_fixed_hex::<32>(producer_static).map_err(|_| "invalid")?;
        self.with(|current| {
            current
                .agent
                .runtime()
                .peers()
                .remove(&key)
                .map(|()| Value::Null)
                .map_err(storage_class)
        })
    }

    fn with<T>(&self, body: impl FnOnce(&Running) -> Result<T, String>) -> Result<T, String> {
        match lock(&self.running).as_ref() {
            Some(current) => body(current),
            None => Err("notRunning".into()),
        }
    }
}

fn start_agent(inputs: &Inputs) -> Result<Agent, String> {
    let mut config = AgentConfig::new(
        inputs.home.clone(),
        inputs.config_dir.clone(),
        inputs.data_dir.clone(),
        inputs.settings.clone(),
    );
    config.state_suffix = inputs.state_suffix;
    config.options = Options {
        clock: Arc::clone(&inputs.clock),
        ..Options::default()
    };
    let mut tries = 0;
    loop {
        match Agent::start(config.clone()) {
            Ok(agent) => return Ok(agent),
            // A just-dropped agent can hold the owner lock for a moment.
            Err(AgentError::Storage(StorageError::Busy))
            | Err(AgentError::Runtime(RuntimeError::Store(StorageError::Busy)))
                if tries < BUSY_RETRIES =>
            {
                tries += 1;
                thread::sleep(BUSY_PAUSE);
            }
            Err(error) => return Err(agent_class(error)),
        }
    }
}

fn digest_for(inputs: &Inputs, source_cache_dir: &Path) -> Option<String> {
    owner_inputs_sha256(
        &inputs.home,
        &inputs.config_dir,
        &inputs.data_dir,
        source_cache_dir,
        &inputs.settings,
    )
}

fn status_of(current: &Running) -> Reply {
    let mut data = call(current, control::STATUS, json!({}))?;
    let runtime = current.agent.runtime();
    let identity = runtime
        .store()
        .state()
        .and_then(|state| state.identity_public())
        .map_err(storage_class)?;
    let object = data.as_object_mut().ok_or("internal")?;
    object.insert("running".into(), Value::Bool(true));
    object.insert(
        "scopeApproved".into(),
        Value::Bool(current.agent.scope_approved()),
    );
    object.insert(
        "listening".into(),
        Value::Bool(runtime.listener_addr().is_some()),
    );
    object.insert(
        "identityFingerprint".into(),
        Value::String(fingerprint(&identity)),
    );
    Ok(data)
}

/// One frozen control command over the agent's own socket. The token is read
/// from the state store for this call and dropped with the client.
fn call(current: &Running, id: u16, body: Value) -> Reply {
    let runtime = current.agent.runtime();
    let state = runtime.store().state().map_err(storage_class)?;
    let client = Client::new(
        Secret32::new(*state.control_token.expose()),
        runtime.socket_path().to_path_buf(),
    );
    let response = client.call(&Command { id, body }).map_err(control_class)?;
    if response.code == control::OK {
        Ok(response.data)
    } else {
        Err(code_class(response.code).into())
    }
}

/// First 8 bytes of SHA-256 of a static key, lowercase hex: for people to
/// compare, not an identifier.
fn fingerprint(key: &[u8; 32]) -> String {
    hex_encode(&Sha256::digest(key)[..8])
}

/// `{client, variable}` for each per-client root variable set (non-empty) in
/// this process.
fn env_roots() -> Vec<Value> {
    use tokscale_core::PathRoot;
    tokscale_core::ClientId::iter()
        .flat_map(|client| {
            let variables: Vec<&'static str> = match client.data().root {
                PathRoot::EnvVar { var, .. } => vec![var],
                PathRoot::ReasonixHome => vec!["REASONIX_STATE_HOME", "REASONIX_HOME"],
                PathRoot::XdgData => vec!["XDG_DATA_HOME"],
                PathRoot::Config => vec!["TOKSCALE_CONFIG_DIR"],
                PathRoot::Home => vec![],
            };
            variables
                .into_iter()
                .filter(|var| std::env::var_os(var).is_some_and(|value| !value.is_empty()))
                .map(move |var| json!({"client": client.as_str(), "variable": var}))
        })
        .collect()
}

fn storage_class(error: StorageError) -> String {
    match error {
        StorageError::Refused(SecurityError::Unauthorized) => "unauthorized",
        StorageError::Refused(SecurityError::Expired) => "expired",
        StorageError::Refused(_) => "invalid",
        StorageError::Busy => "busy",
        StorageError::UnsafeState => "unsafeState",
        _ => "internal",
    }
    .into()
}

fn agent_class(error: AgentError) -> String {
    match error {
        AgentError::Unsupported => "unsupported".into(),
        AgentError::Storage(error) | AgentError::Runtime(RuntimeError::Store(error)) => {
            storage_class(error)
        }
        AgentError::Runtime(RuntimeError::Control(error)) => control_class(error),
        AgentError::Runtime(RuntimeError::Thread) | AgentError::Install(_) => "internal".into(),
    }
}

fn control_class(error: ControlError) -> String {
    match error {
        ControlError::Unsupported => "unsupported",
        ControlError::PathTooLong => "pathTooLong",
        ControlError::UnsafeSocket => "unsafeSocket",
        _ => "control",
    }
    .into()
}

fn code_class(code: u16) -> &'static str {
    match code {
        control::INVALID => "invalid",
        control::UNAUTHORIZED => "unauthorized",
        control::EXPIRED => "expired",
        control::BUSY => "busy",
        control::UNSAFE_STATE => "unsafeState",
        _ => "internal",
    }
}

fn client_class(error: ClientError) -> String {
    match error {
        ClientError::SelfPairing => "selfPairing",
        ClientError::InvalidOffer => "invalidOffer",
        ClientError::OfferUnusable => "offerUnusable",
        ClientError::LostAttempt => "lostAttempt",
        ClientError::Code(PeerCode::Busy) => "busy",
        ClientError::Code(PeerCode::Invalid) => "invalid",
        ClientError::Code(PeerCode::Unauthorized) => "unauthorized",
        ClientError::Code(PeerCode::Incompatible) => "incompatible",
        ClientError::Code(PeerCode::Internal) => "internal",
        ClientError::Rejected => "rejected",
        ClientError::Expired => "expired",
        ClientError::Revoked => "revoked",
        ClientError::Aborted => "aborted",
        ClientError::PollLimit => "pollLimit",
        ClientError::Transport(_) => "transport",
    }
    .into()
}

// ------------------------------------------------------------- pricing ---

/// The periodic pricing copy; dropping it ends and joins the thread.
struct PricingSync {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl PricingSync {
    fn spawn(source: PathBuf, destination: PathBuf, period: Duration) -> Self {
        let (stop, wake) = mpsc::channel::<()>();
        let thread = thread::Builder::new()
            .name("tb-remote-pricing".into())
            .spawn(move || {
                while let Err(mpsc::RecvTimeoutError::Timeout) = wake.recv_timeout(period) {
                    sync_pricing(&source, &destination);
                }
            })
            .ok();
        Self {
            stop: Some(stop),
            thread,
        }
    }
}

impl Drop for PricingSync {
    fn drop(&mut self) {
        // Disconnecting the channel wakes the thread and ends its loop.
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Copies each pricing file whose source is newer than its copy (or has no
/// copy yet). A missing source is skipped; a failed copy is retried next
/// period.
fn sync_pricing(source: &Path, destination: &Path) {
    for name in PRICING_FILES {
        let Ok(modified) = fs::metadata(source.join(name)).and_then(|m| m.modified()) else {
            continue;
        };
        let stale = fs::metadata(destination.join(name))
            .and_then(|m| m.modified())
            .map_or(true, |copied| modified > copied);
        if stale {
            let _ = copy_atomic(&source.join(name), destination, name);
        }
    }
}

/// A `0600` temp file created exclusively in `directory`, synced, renamed
/// over `name`.
fn copy_atomic(source: &Path, directory: &Path, name: &str) -> std::io::Result<()> {
    let bytes = fs::read(source)?;
    let temp = directory.join(format!(".{name}.tmp"));
    let _ = fs::remove_file(&temp);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(&temp, directory.join(name))
}

// ---------------------------------------------------------- app inputs ---

/// The start inputs of the app, read now: the home and its config and data
/// directories, the app's scanner settings (`LocalSourceContext`: registry,
/// OpenCode DBs, Cursor takeover) and the app's pricing cache.
/// `home_override` `None` is the directory-service home and the frozen root;
/// a test build refuses it (`homeRequired`), so no test opens the real root.
pub(crate) fn app_inputs(home_override: Option<PathBuf>) -> Result<Inputs, String> {
    let (home, state_suffix, config_dir, data_dir) = match home_override {
        Some(home) => {
            prepare_override_home(&home)?;
            // The platform directories, moved below the override home.
            let below = |dir: Option<PathBuf>| -> Result<PathBuf, String> {
                dir.zip(dirs::home_dir())
                    .and_then(|(dir, platform)| {
                        dir.strip_prefix(&platform).ok().map(Path::to_path_buf)
                    })
                    .map(|relative| home.join(relative))
                    .ok_or_else(|| "unsupported".into())
            };
            let (config, data) = (below(dirs::config_dir())?, below(dirs::data_dir())?);
            (home, OVERRIDE_SUFFIX, config, data)
        }
        None => product_roots()?,
    };
    Ok(Inputs {
        home,
        state_suffix,
        config_dir,
        data_dir,
        settings: crate::LocalSourceContext::current().scanner_settings(),
        pricing_source: tokscale_core::pricing::cache::get_cache_dir(),
        pricing_period: PRICING_PERIOD,
        clock: Arc::new(syrtis_agent::pairing::SystemClock),
    })
}

type Roots = (PathBuf, &'static [&'static str], PathBuf, PathBuf);

#[cfg(not(test))]
fn product_roots() -> Result<Roots, String> {
    let home = syrtis_agent::secure_fs::trusted_home().map_err(storage_class)?;
    Ok((
        home,
        syrtis_agent::secure_fs::STATE_SUFFIX,
        dirs::config_dir().ok_or("unsupported")?,
        dirs::data_dir().ok_or("unsupported")?,
    ))
}

#[cfg(test)]
fn product_roots() -> Result<Roots, String> {
    Err("homeRequired".into())
}

// ------------------------------------------------------- override home ---

/// Creates a missing override home (and its parent) `0700`. The home must
/// then be a real directory (not a link) owned by this user, below a parent
/// owned by this user or root, so another local user cannot have planted
/// it. `SecureRoot::open_under` checks the rest.
pub(crate) fn prepare_override_home(home: &Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    let unsafe_state = || "unsafeState".to_string();
    if !home.is_absolute() {
        return Err("invalid".into());
    }
    let parent = home.parent().ok_or_else(|| "invalid".to_string())?;
    if !parent.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|_| unsafe_state())?;
    }
    let uid = crate::cursor_desktop::current_uid();
    let parent_meta = fs::metadata(parent).map_err(|_| unsafe_state())?;
    if !parent_meta.is_dir() || (parent_meta.uid() != uid && parent_meta.uid() != 0) {
        return Err(unsafe_state());
    }
    match fs::DirBuilder::new().mode(0o700).create(home) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(unsafe_state()),
    }
    let meta = fs::symlink_metadata(home).map_err(|_| unsafe_state())?;
    if !meta.is_dir() || meta.uid() != uid {
        return Err(unsafe_state());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Every agent here lives in a short temp home below `/tmp` that the test
    //! creates and removes by its exact path, so the control socket path fits
    //! `sun_path`. Nothing opens the real agent root (`app_inputs(None)` is
    //! `homeRequired` in a test build) or scans the real home: the engine
    //! home is the temp home, and the only extra root is one the test makes.

    use super::*;
    use std::{
        ffi::{CStr, CString},
        net::{TcpListener, TcpStream},
        os::unix::fs::PermissionsExt,
        sync::atomic::{AtomicI64, AtomicUsize},
        time::{Instant, SystemTime, UNIX_EPOCH},
    };
    use syrtis_agent::{
        report::Endpoint,
        report_peer::{FetchError, ReportQuery},
        runtime::PullError,
    };

    /// Injected time from the real wall clock; `sleep` advances it, so the
    /// receiver's poll and the grant's rate tokens do not wait.
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
            *lock(&self.mono) += Duration::from_secs(seconds);
            self.os.fetch_add(seconds as i64, Ordering::SeqCst);
        }
    }

    impl Clock for TestClock {
        fn monotonic(&self) -> Duration {
            *lock(&self.mono)
        }
        fn os_now_seconds(&self) -> Result<i64, StorageError> {
            Ok(self.os.load(Ordering::SeqCst))
        }
        fn sleep(&self, duration: Duration) {
            self.advance(duration.as_secs().max(1));
        }
    }

    /// A short temp home below the canonical `/tmp`; only it is removed.
    struct TempHome(PathBuf);

    impl TempHome {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = fs::canonicalize("/tmp").unwrap().join(format!(
                "sa3a-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn inputs(home: &TempHome, clock: &Arc<TestClock>, settings: ScannerSettings) -> Inputs {
        Inputs {
            home: home.0.clone(),
            state_suffix: OVERRIDE_SUFFIX,
            config_dir: home.0.join(".config"),
            data_dir: home.0.join(".local/share"),
            settings,
            pricing_source: home.0.join("pricing-src"),
            pricing_period: Duration::from_millis(100),
            clock: Arc::clone(clock) as Arc<dyn Clock>,
        }
    }

    fn start(
        agent: &RemoteAgent,
        home: &TempHome,
        clock: &Arc<TestClock>,
        settings: ScannerSettings,
    ) -> Value {
        agent.start(|| Ok(inputs(home, clock, settings))).unwrap()
    }

    fn free_port() -> u16 {
        TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn listening(port: u16) -> bool {
        TcpStream::connect(("127.0.0.1", port)).is_ok()
    }

    fn epoch(agent: &RemoteAgent) -> u64 {
        lock(&agent.running).as_ref().unwrap().epoch
    }

    fn pull(receiver: &RemoteAgent, producer: &[u8; 32]) -> Result<(), PullError> {
        let query = ReportQuery {
            endpoint: Endpoint::Agents,
            start_date: "2040-01-01".into(),
            end_date_exclusive: "2040-01-02".into(),
            timezone: "UTC".into(),
            clients: Vec::new(),
        };
        let running = lock(&receiver.running);
        running
            .as_ref()
            .unwrap()
            .agent
            .runtime()
            .pull(producer, &query)
            .map(drop)
    }

    fn claude_root(home: &TempHome) -> ScannerSettings {
        let root = home.0.join("extra/projects");
        fs::create_dir_all(&root).unwrap();
        ScannerSettings {
            extra_scan_paths: BTreeMap::from([("claude".to_string(), vec![root])]),
            ..ScannerSettings::default()
        }
    }

    /// Acceptance (1) and (3b), with two in-process agents (no global): offer
    /// → claim → SAS → approve → poll → peers → pull; then a changed scan
    /// input restarts the producer, its scope is no longer approved and the
    /// existing grant's report request is refused; then revoke.
    #[test]
    fn two_agents_pair_a_changed_scope_is_refused_and_revoke_ends_the_grant() {
        let clock = TestClock::new();
        let (producer_home, receiver_home) = (TempHome::new(), TempHome::new());
        let (producer, receiver) = (RemoteAgent::default(), RemoteAgent::default());
        start(
            &producer,
            &producer_home,
            &clock,
            ScannerSettings::default(),
        );
        let receiver_status = start(
            &receiver,
            &receiver_home,
            &clock,
            ScannerSettings::default(),
        );

        producer
            .listen_configure("127.0.0.1", free_port(), Some("desk".into()))
            .unwrap();
        let digest = producer.scope_disclosure().unwrap()["ownerInputsSha256"]
            .as_str()
            .unwrap()
            .to_owned();
        producer.scope_approve(&digest).unwrap();
        producer.sharing_set(true).unwrap();
        let producer_status = producer.status().unwrap();
        assert_eq!(producer_status["nodeLabel"], "desk");
        assert_eq!(producer_status["scopeApproved"], true);
        assert_eq!(producer_status["sharingEnabled"], true);

        let offer = producer.offer_create(600).unwrap();
        let claim = receiver
            .pair_claim(offer["pairUrl"].as_str().unwrap())
            .unwrap();
        assert_eq!(
            claim["producerFingerprint"],
            producer_status["identityFingerprint"]
        );
        let pending = producer.pending_list().unwrap();
        let pending = &pending.as_array().unwrap()[0];
        assert_eq!(pending["sasCode"], claim["sas"], "both sides show one SAS");
        producer
            .decide(true, pending["pendingId"].as_str().unwrap(), 1)
            .unwrap();
        let paired = receiver
            .pair_poll(claim["handle"].as_u64().unwrap())
            .unwrap();
        let producer_static =
            decode_fixed_hex::<32>(paired["producerStatic"].as_str().unwrap()).unwrap();

        let peers = receiver.peers().unwrap();
        let peers = peers.as_array().unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(
            peers[0]["fingerprint"],
            producer_status["identityFingerprint"]
        );
        assert_eq!(peers[0]["state"], "active");
        let grants = producer.grants().unwrap();
        let grant = &grants.as_array().unwrap()[0];
        assert_eq!(grant["state"], "active");
        assert_eq!(
            grant["peerFingerprint"],
            receiver_status["identityFingerprint"]
        );

        clock.advance(61);
        assert_eq!(pull(&receiver, &producer_static), Ok(()));

        // (3b) The owner adds a scan root: the next start restarts the agent.
        let before = epoch(&producer);
        let restarted = start(
            &producer,
            &producer_home,
            &clock,
            claude_root(&producer_home),
        );
        assert_ne!(epoch(&producer), before, "changed inputs must restart");
        assert_eq!(restarted["scopeApproved"], false);
        clock.advance(61);
        assert_eq!(
            pull(&receiver, &producer_static),
            Err(PullError::Fetch(FetchError::Internal)),
            "the old grant must not read the new scope"
        );
        // The same inputs again: no restart.
        let same = epoch(&producer);
        start(
            &producer,
            &producer_home,
            &clock,
            claude_root(&producer_home),
        );
        assert_eq!(epoch(&producer), same, "unchanged inputs must not restart");

        producer.revoke(grant["grantId"].as_str().unwrap()).unwrap();
        assert_eq!(producer.grants().unwrap()[0]["state"], "revoked");
        clock.advance(61);
        assert_eq!(
            pull(&receiver, &producer_static),
            Err(PullError::Fetch(FetchError::Revoked))
        );
    }

    /// Acceptance (3c): the address goes through the frozen command; after a
    /// change the old port takes no connection.
    #[test]
    fn listen_configure_moves_the_listener_off_the_old_port() {
        let clock = TestClock::new();
        let home = TempHome::new();
        let agent = RemoteAgent::default();
        start(&agent, &home, &clock, ScannerSettings::default());
        let (old, new) = (free_port(), free_port());
        let status = agent.listen_configure("127.0.0.1", old, None).unwrap();
        assert_eq!(status["listening"], true);
        assert!(listening(old));
        agent.listen_configure("127.0.0.1", new, None).unwrap();
        assert!(!listening(old), "the old port still accepts");
        assert!(listening(new));
        // A host the frozen validator refuses is the class, nothing else.
        assert_eq!(
            agent.listen_configure("not a host", new, None),
            Err("invalid".into())
        );
    }

    /// Acceptance (4): both pricing files are copied at start (same bytes,
    /// 0600); a newer source is copied again within a period, no restart.
    #[test]
    fn pricing_is_copied_at_start_and_again_when_the_source_is_newer() {
        let clock = TestClock::new();
        let home = TempHome::new();
        let source = home.0.join("pricing-src");
        fs::create_dir(&source).unwrap();
        for name in PRICING_FILES {
            fs::write(source.join(name), format!("{{\"{name}\":1}}")).unwrap();
        }
        let agent = RemoteAgent::default();
        start(&agent, &home, &clock, ScannerSettings::default());
        let destination = lock(&agent.running)
            .as_ref()
            .unwrap()
            .agent
            .pricing_dir()
            .to_path_buf();
        for name in PRICING_FILES {
            assert_eq!(
                fs::read(destination.join(name)).unwrap(),
                fs::read(source.join(name)).unwrap()
            );
            let mode = fs::metadata(destination.join(name)).unwrap().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let started = epoch(&agent);

        let updated = source.join(PRICING_FILES[0]);
        fs::write(&updated, "{\"newer\":2}").unwrap();
        fs::File::options()
            .write(true)
            .open(&updated)
            .unwrap()
            .set_modified(SystemTime::now() + Duration::from_secs(5))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while fs::read(destination.join(PRICING_FILES[0])).unwrap() != b"{\"newer\":2}" {
            assert!(Instant::now() < deadline, "the newer source was not copied");
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(epoch(&agent), started);
    }

    /// A panic inside a `tb_remote_*` body is the fixed class, without its
    /// text.
    #[test]
    fn a_panic_is_the_fixed_class_without_its_text() {
        let raw = ffi("tb_remote_test", || panic!("/tmp/secret-path"));
        let reply = take(raw);
        assert_eq!(reply, json!({"ok": false, "err": "panic"}));
    }

    fn take(raw: *mut c_char) -> Value {
        let text = unsafe { CStr::from_ptr(raw) }.to_str().unwrap().to_owned();
        unsafe { crate::tb_free(raw) };
        serde_json::from_str(&text).unwrap()
    }

    fn c(text: &str) -> CString {
        CString::new(text).unwrap()
    }

    fn global_epoch() -> u64 {
        lock(&AGENT.running).as_ref().unwrap().epoch
    }

    /// Acceptance (2), (2b), (3) and the input-change rule through the C entry
    /// points and the process-wide agent: a fresh `/tmp` home starts with
    /// sharing off, no listener and no approved scope; a second start does
    /// not restart; registered extra roots appear in the disclosure and the
    /// same inputs give the same digest across a stop and start; a wrong
    /// digest is refused; stop answers "not running"; an unsafe root is a
    /// class string without a path.
    #[test]
    fn ffi_lifecycle_disclosure_scope_approval_and_errors() {
        let _registry = lock(&crate::extra_scan_paths::TEST_LOCK);
        crate::extra_scan_paths::reset_for_test();
        let ffi = crate::remote_ffi::tb_remote_start;

        // (3) A root that is 0755: a class, no path.
        let unsafe_home = TempHome::new();
        fs::create_dir(unsafe_home.0.join("a")).unwrap();
        fs::set_permissions(unsafe_home.0.join("a"), fs::Permissions::from_mode(0o755)).unwrap();
        let path = c(unsafe_home.0.to_str().unwrap());
        assert_eq!(
            take(unsafe { ffi(path.as_ptr()) }),
            json!({"ok": false, "err": "unsafeState"})
        );
        // A test build never opens the real root.
        assert_eq!(
            take(unsafe { ffi(std::ptr::null()) }),
            json!({"ok": false, "err": "homeRequired"})
        );

        // (2) A fresh root.
        let home = TempHome::new();
        let path = c(home.0.to_str().unwrap());
        let started = take(unsafe { ffi(path.as_ptr()) });
        assert_eq!(started["ok"], true, "{started}");
        let data = &started["data"];
        assert_eq!(data["running"], true);
        assert_eq!(data["sharingEnabled"], false);
        assert_eq!(data["listening"], false);
        assert_eq!(data["bindHost"], Value::Null);
        assert_eq!(data["scopeApproved"], false);
        let first = global_epoch();
        let again = take(unsafe { ffi(path.as_ptr()) });
        assert_eq!(again["ok"], true);
        assert_eq!(global_epoch(), first, "a repeated start restarted");
        assert_eq!(
            take(crate::remote_ffi::tb_remote_status())["data"]["running"],
            true
        );

        // Extra roots set before a start are disclosed.
        let root = home.0.join("extra/projects");
        fs::create_dir_all(&root).unwrap();
        let registry = c(&json!({"claude": [root]}).to_string());
        assert_eq!(
            take(unsafe { crate::tb_set_extra_scan_paths(registry.as_ptr()) })["ok"],
            true
        );
        take(unsafe { ffi(path.as_ptr()) });
        assert_ne!(global_epoch(), first, "new inputs did not restart");
        let disclosure = take(crate::remote_ffi::tb_remote_scope_disclosure())["data"].clone();
        assert_eq!(disclosure["extraScanPaths"]["claude"], json!([root]));
        assert_eq!(disclosure["home"], json!(home.0));
        let digest = disclosure["ownerInputsSha256"].as_str().unwrap().to_owned();
        take(crate::remote_ffi::tb_remote_stop());
        take(unsafe { ffi(path.as_ptr()) });
        assert_eq!(
            take(crate::remote_ffi::tb_remote_scope_disclosure())["data"]["ownerInputsSha256"],
            digest,
            "the same inputs gave another digest"
        );

        // (2b) Consent binds to the disclosed digest.
        let wrong = c(&"0".repeat(64));
        assert_eq!(
            take(unsafe { crate::remote_ffi::tb_remote_scope_approve(wrong.as_ptr()) }),
            json!({"ok": false, "err": "invalid"})
        );
        assert_eq!(
            take(crate::remote_ffi::tb_remote_status())["data"]["scopeApproved"],
            false
        );
        let right = c(&digest);
        assert_eq!(
            take(unsafe { crate::remote_ffi::tb_remote_scope_approve(right.as_ptr()) })["ok"],
            true
        );
        assert_eq!(
            take(crate::remote_ffi::tb_remote_status())["data"]["scopeApproved"],
            true
        );

        assert_eq!(
            take(crate::remote_ffi::tb_remote_stop()),
            json!({"ok": true, "data": {"running": false}})
        );
        assert_eq!(
            take(crate::remote_ffi::tb_remote_status()),
            json!({"ok": true, "data": {"running": false}})
        );
        assert_eq!(
            take(crate::remote_ffi::tb_remote_pending_list()),
            json!({"ok": false, "err": "notRunning"})
        );
        crate::extra_scan_paths::reset_for_test();
    }
}
