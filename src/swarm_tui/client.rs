//! Minimal newline-delimited JSON-RPC 2.0 client for the `swarm/` family.
//!
//! Deliberately small: the swarm surface is request/response only, so this
//! holds one connection, one in-flight call at a time, and skips any frame
//! that is not the response it is waiting for (the daemon multiplexes
//! notifications onto the same stream). Everything richer — reconnects,
//! notification routing, WSS — belongs to the full dashboard client, not here.
//!
//! Attach-or-spawn: if nothing is listening on the socket, this starts an
//! ephemeral daemon of the same executable and reaps it when the handle drops,
//! so `zeroclaw swarm` works on a machine with no service installed.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use zeroclaw_api::jsonrpc::error_codes::{INTERNAL_ERROR, METHOD_NOT_FOUND};
use zeroclaw_runtime::rpc::dispatch::RPC_PROTOCOL_VERSION;
use zeroclaw_runtime::rpc::types::{
    InitializeResult, SwarmDeleteResult, SwarmFieldsResult, SwarmListResult, SwarmStepShape,
    SwarmSubmission, SwarmValidateResult,
};
use zeroclaw_runtime::swarm::store::PersistedSwarm;

use crate::config::Config;

/// How long one request may wait for its response before the call is treated
/// as a dead connection.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Ceiling on a single response frame. The daemon never emits anything close
/// to this for `swarm/*`; the cap stops a wedged peer from exhausting memory.
const MAX_FRAME_BYTES: u64 = 8 * 1024 * 1024;
/// How long a freshly spawned ephemeral daemon gets to bind its socket.
const SPAWN_READY_TIMEOUT: Duration = Duration::from_secs(30);
/// Gap between connect attempts while waiting for a spawned daemon.
const SPAWN_POLL_INTERVAL: Duration = Duration::from_millis(100);

// ── Platform local-stream shim ──────────────────────────────────

#[cfg(unix)]
type LocalStream = tokio::net::UnixStream;
#[cfg(windows)]
type LocalStream = tokio::net::windows::named_pipe::NamedPipeClient;

#[cfg(unix)]
async fn open_local_stream(path: &Path) -> std::io::Result<LocalStream> {
    tokio::net::UnixStream::connect(path).await
}

#[cfg(windows)]
async fn open_local_stream(path: &Path) -> std::io::Result<LocalStream> {
    tokio::net::windows::named_pipe::ClientOptions::new().open(path)
}

// ── Wire method names ────────────────────────────────────────────

pub mod method {
    pub const INITIALIZE: &str = "initialize";
    pub const SWARM_LIST: &str = "swarm/list";
    pub const SWARM_CREATE: &str = "swarm/create";
    pub const SWARM_DELETE: &str = "swarm/delete";
    pub const SWARM_FIELDS: &str = "swarm/fields";
    pub const SWARM_VALIDATE: &str = "swarm/validate";
}

// ── Failures ─────────────────────────────────────────────────────

/// A JSON-RPC error frame. `data` is always dropped by the daemon, so the code
/// and the message are the whole of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcFailure {
    pub code: i32,
    pub message: String,
}

impl RpcFailure {
    /// The daemon does not know this method — the build predates swarms.
    pub fn is_method_not_found(&self) -> bool {
        self.code == METHOD_NOT_FOUND
    }
}

/// Why a call did not produce a value.
#[derive(Debug)]
pub enum CallError {
    /// The connection itself is unusable; nothing further will succeed.
    Transport(anyhow::Error),
    /// The daemon answered, with an error.
    Rpc(RpcFailure),
}

impl CallError {
    /// The `RpcFailure` behind this error, when the daemon answered at all.
    pub fn failure(&self) -> Option<&RpcFailure> {
        match self {
            Self::Rpc(failure) => Some(failure),
            Self::Transport(_) => None,
        }
    }
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "{e:#}"),
            Self::Rpc(failure) => write!(f, "{}", failure.message),
        }
    }
}

pub type CallResult<T> = std::result::Result<T, CallError>;

fn transport<E: Into<anyhow::Error>>(e: E) -> CallError {
    CallError::Transport(e.into())
}

// ── Ephemeral daemon ─────────────────────────────────────────────

/// A daemon this process started because nothing was listening. Killed on
/// drop, so a TUI that exits — cleanly, by panic, or by `?` — never leaves an
/// orphan behind.
struct EphemeralDaemon {
    child: std::process::Child,
}

impl EphemeralDaemon {
    fn spawn(config_dir: &Path, socket: &Path) -> Result<Self> {
        let exe = std::env::current_exe().context("cannot locate the running zeroclaw binary")?;
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("daemon")
            .arg("--ephemeral")
            .arg("--config-dir")
            .arg(config_dir)
            // The client waits on this exact endpoint, so the child must bind
            // it rather than independently deriving a different path.
            .env("ZEROCLAW_SOCKET", socket)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let child = cmd.spawn().context("failed to spawn an ephemeral daemon")?;
        Ok(Self { child })
    }
}

impl Drop for EphemeralDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── Client ───────────────────────────────────────────────────────

/// One connection to the daemon, scoped to the `swarm/` family.
pub struct SwarmClient {
    reader: BufReader<tokio::io::ReadHalf<LocalStream>>,
    writer: tokio::io::WriteHalf<LocalStream>,
    next_id: u64,
    /// Method names the daemon advertised at `initialize`.
    capabilities: Vec<String>,
    /// Held for its `Drop`: reaps a daemon this process started.
    _daemon: Option<EphemeralDaemon>,
}

impl SwarmClient {
    /// Attach to a running daemon, or start an ephemeral one and attach to
    /// that. The returned client has already completed `initialize`.
    pub async fn connect(config: &Config) -> Result<Self> {
        let socket = zeroclaw_runtime::rpc::local::socket_path(config);
        match open_local_stream(&socket).await {
            Ok(stream) => Self::handshake(stream, None).await,
            Err(_) => {
                let config_dir = config_dir_of(config);
                let daemon = EphemeralDaemon::spawn(&config_dir, &socket)?;
                let stream = await_socket(&socket).await?;
                Self::handshake(stream, Some(daemon)).await
            }
        }
    }

    async fn handshake(stream: LocalStream, daemon: Option<EphemeralDaemon>) -> Result<Self> {
        let (read_half, write_half) = tokio::io::split(stream);
        let mut client = Self {
            reader: BufReader::new(read_half),
            writer: write_half,
            next_id: 0,
            capabilities: Vec::new(),
            _daemon: daemon,
        };
        let init: InitializeResult = client
            .call(
                method::INITIALIZE,
                json!({ "protocol_version": RPC_PROTOCOL_VERSION }),
            )
            .await
            .map_err(|e| anyhow::Error::msg(e.to_string()))?;
        client.capabilities = init.capabilities;
        Ok(client)
    }

    /// `true` when the daemon advertised the swarm read surface. A daemon that
    /// predates swarms answers every `swarm/*` call with METHOD_NOT_FOUND, and
    /// this lets the caller say so before issuing one.
    pub fn supports_swarms(&self) -> bool {
        self.capabilities.iter().any(|m| m == method::SWARM_LIST)
    }

    // ── Typed calls ──────────────────────────────────────────────

    pub async fn list_swarms(&mut self) -> CallResult<Vec<PersistedSwarm>> {
        let result: SwarmListResult = self.call(method::SWARM_LIST, json!({})).await?;
        Ok(result.swarms)
    }

    pub async fn fields(&mut self, provider: Option<&str>) -> CallResult<Vec<SwarmStepShape>> {
        let result: SwarmFieldsResult = self
            .call(method::SWARM_FIELDS, json!({ "provider": provider }))
            .await?;
        Ok(result.steps)
    }

    pub async fn validate(
        &mut self,
        submission: &SwarmSubmission,
    ) -> CallResult<SwarmValidateResult> {
        self.call(method::SWARM_VALIDATE, json!({ "submission": submission }))
            .await
    }

    pub async fn create(&mut self, submission: &SwarmSubmission) -> CallResult<PersistedSwarm> {
        self.call(method::SWARM_CREATE, json!({ "submission": submission }))
            .await
    }

    pub async fn delete(&mut self, swarm_id: &str, force: bool) -> CallResult<SwarmDeleteResult> {
        self.call(
            method::SWARM_DELETE,
            json!({ "swarm_id": swarm_id, "force": force }),
        )
        .await
    }

    // ── Transport ────────────────────────────────────────────────

    /// Issue one request and decode its response. Frames that are not this
    /// call's response — notifications, or a stray id — are skipped.
    async fn call<T: DeserializeOwned>(&mut self, method: &str, params: Value) -> CallResult<T> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let id = Value::String(format!("swarm-{id}"));

        let request = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": id,
        });
        let mut line = serde_json::to_string(&request).map_err(transport)?;
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .await
            .map_err(transport)?;
        self.writer.flush().await.map_err(transport)?;

        let frame = tokio::time::timeout(CALL_TIMEOUT, self.read_response(&id))
            .await
            .map_err(|_| CallError::Transport(anyhow::Error::msg(dropped_message())))??;

        if let Some(error) = frame.get("error") {
            return Err(CallError::Rpc(RpcFailure {
                // A code outside `i32` is not one of ours; treat it the way an
                // unclassifiable failure is treated everywhere else.
                code: error
                    .get("code")
                    .and_then(Value::as_i64)
                    .and_then(|code| i32::try_from(code).ok())
                    .unwrap_or(INTERNAL_ERROR),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            }));
        }
        let result = frame.get("result").cloned().unwrap_or(Value::Null);
        serde_json::from_value(result).map_err(transport)
    }

    /// Read frames until one carries `id`. EOF is a transport failure.
    async fn read_response(&mut self, id: &Value) -> CallResult<Value> {
        loop {
            let mut raw = String::new();
            let read = (&mut self.reader)
                .take(MAX_FRAME_BYTES)
                .read_line(&mut raw)
                .await
                .map_err(transport)?;
            if read == 0 {
                return Err(CallError::Transport(anyhow::Error::msg(dropped_message())));
            }
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(frame) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            if frame.get("id") == Some(id) {
                return Ok(frame);
            }
        }
    }
}

/// The Fluent line shown when the socket goes away mid-call.
fn dropped_message() -> String {
    crate::t(
        "cli-swarm-daemon-dropped",
        "The daemon closed the connection.",
    )
}

/// The config directory an ephemeral daemon should be told to use. Derived
/// from the loaded config so `--config-dir` / `$ZEROCLAW_CONFIG_DIR` carry
/// into the child instead of being re-derived from the environment.
fn config_dir_of(config: &Config) -> PathBuf {
    config
        .config_path
        .parent()
        .map_or_else(|| config.data_dir.clone(), Path::to_path_buf)
}

/// Poll the socket until a spawned daemon binds it.
async fn await_socket(socket: &Path) -> Result<LocalStream> {
    let deadline = std::time::Instant::now() + SPAWN_READY_TIMEOUT;
    loop {
        if let Ok(stream) = open_local_stream(socket).await {
            return Ok(stream);
        }
        if std::time::Instant::now() >= deadline {
            let endpoint = socket.display().to_string();
            return Err(anyhow::Error::msg(crate::ta(
                "cli-swarm-daemon-not-ready",
                &[("socket", endpoint.as_str())],
                "The spawned daemon never bound its socket.",
            )));
        }
        tokio::time::sleep(SPAWN_POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_not_found_is_the_degrade_signal() {
        let failure = RpcFailure {
            code: METHOD_NOT_FOUND,
            message: "Method not found".to_string(),
        };
        assert!(failure.is_method_not_found());
        assert!(
            !RpcFailure {
                code: -32602,
                message: String::new(),
            }
            .is_method_not_found()
        );
    }

    #[test]
    fn a_transport_error_carries_no_daemon_failure() {
        let error = CallError::Transport(anyhow::Error::msg("socket gone"));
        assert!(error.failure().is_none());
    }

    #[test]
    fn config_dir_is_the_parent_of_the_config_file() {
        let mut config = Config::default();
        config.config_path = PathBuf::from("/tmp/zc-probe/config.toml");
        config.data_dir = PathBuf::from("/tmp/zc-probe/data");
        assert_eq!(config_dir_of(&config), PathBuf::from("/tmp/zc-probe"));
    }
}
