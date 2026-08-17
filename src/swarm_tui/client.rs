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

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use zeroclaw_api::jsonrpc::error_codes::{INTERNAL_ERROR, METHOD_NOT_FOUND};
use zeroclaw_runtime::rpc::dispatch::RPC_PROTOCOL_VERSION;
use zeroclaw_runtime::rpc::types::{
    InitializeResult, SwarmBoardNotify, SwarmChatResult, SwarmDeleteResult, SwarmFieldsResult,
    SwarmListResult, SwarmRunControlResult, SwarmStepShape, SwarmStopResult, SwarmSubmission,
    SwarmSubscribeResult, SwarmUpdate, SwarmValidateResult,
};
use zeroclaw_runtime::swarm::store::{BoxSpec, PersistedSwarm};

use crate::config::Config;

/// How long one request may wait for its response before the call is treated
/// as a dead connection.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Ceiling on a single response frame. The daemon never emits anything close
/// to this for `swarm/*`; the cap stops a wedged peer from exhausting memory.
const MAX_FRAME_BYTES: u64 = 8 * 1024 * 1024;
/// How many live notifications the client holds while a request/response round
/// trip is in flight. Past this the oldest are dropped and a single [`Gap`]
/// marker is surfaced, so a burst during a call is bounded rather than silent.
///
/// [`Gap`]: SwarmNotification::Gap
const NOTIF_QUEUE_CAP: usize = 512;
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
    pub const SWARM_UPDATE_LAYOUT: &str = "swarm/update-layout";
    pub const SWARM_START: &str = "swarm/start";
    pub const SWARM_PAUSE: &str = "swarm/pause";
    pub const SWARM_RESUME: &str = "swarm/resume";
    pub const SWARM_STOP: &str = "swarm/stop";
    pub const SWARM_SUBSCRIBE: &str = "swarm/subscribe";
    pub const SWARM_CHAT: &str = "swarm/chat";
}

/// Notification method names the daemon pushes onto a subscribed connection.
mod notification {
    pub const SWARM_UPDATE: &str = "swarm/update";
    pub const SWARM_BOARD: &str = "swarm/board";
}

// ── Live notifications ───────────────────────────────────────────

/// A live frame pushed to a subscribed connection, decoded off the wire. The
/// two swarm notification families plus a synthetic [`Gap`] the client raises
/// when it could not buffer a burst — so a drop is visible, never silent.
///
/// [`Gap`]: SwarmNotification::Gap
#[derive(Debug)]
pub enum SwarmNotification {
    /// A per-box `swarm/update` turn event.
    Update(Box<SwarmUpdate>),
    /// A `swarm/board` state-board transition.
    Board(Box<SwarmBoardNotify>),
    /// The client dropped one or more notifications it could not buffer.
    Gap,
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
    /// Bytes read from the socket that have not yet been split into a whole
    /// frame line. Persisting the partial line on the client — rather than in a
    /// `read_line` future's local — is what makes [`SwarmClient::read_frame`]
    /// cancel-safe: a read cut short by a keypress in a `select!` loses nothing.
    line_buf: Vec<u8>,
    /// Notifications captured while a request/response round trip was reading
    /// past them. Drained by the event loop so a call never silently eats the
    /// live stream. Bounded by [`NOTIF_QUEUE_CAP`].
    notif_queue: VecDeque<SwarmNotification>,
    /// A notification was dropped because [`Self::notif_queue`] was full. The
    /// next pull surfaces one [`SwarmNotification::Gap`] and clears this.
    dropped_gap: bool,
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
            line_buf: Vec::new(),
            notif_queue: VecDeque::new(),
            dropped_gap: false,
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

    // ── Run control + live stream (S7) ───────────────────────────

    /// `swarm/subscribe` — opt into the swarm's live `swarm/update` and
    /// `swarm/board` push streams on this connection.
    pub async fn subscribe(&mut self, swarm_id: &str) -> CallResult<SwarmSubscribeResult> {
        self.call(method::SWARM_SUBSCRIBE, json!({ "swarm_id": swarm_id }))
            .await
    }

    pub async fn start(&mut self, swarm_id: &str) -> CallResult<SwarmRunControlResult> {
        self.call(method::SWARM_START, json!({ "swarm_id": swarm_id }))
            .await
    }

    pub async fn pause(&mut self, swarm_id: &str) -> CallResult<SwarmRunControlResult> {
        self.call(method::SWARM_PAUSE, json!({ "swarm_id": swarm_id }))
            .await
    }

    pub async fn resume(&mut self, swarm_id: &str) -> CallResult<SwarmRunControlResult> {
        self.call(method::SWARM_RESUME, json!({ "swarm_id": swarm_id }))
            .await
    }

    pub async fn stop(&mut self, swarm_id: &str) -> CallResult<SwarmStopResult> {
        self.call(method::SWARM_STOP, json!({ "swarm_id": swarm_id }))
            .await
    }

    /// `swarm/chat` — interject a user message into one box, or hand it back
    /// with the literal `resume`.
    pub async fn chat(
        &mut self,
        swarm_id: &str,
        box_id: &str,
        message: &str,
    ) -> CallResult<SwarmChatResult> {
        self.call(
            method::SWARM_CHAT,
            json!({ "swarm_id": swarm_id, "box_id": box_id, "message": message }),
        )
        .await
    }

    /// `swarm/update-layout` — write back the whole box canvas (slots, roles,
    /// jobs). Revision-guarded; the reply carries the persisted document at its
    /// new revision.
    pub async fn update_layout(
        &mut self,
        swarm_id: &str,
        revision: u64,
        boxes: &[BoxSpec],
    ) -> CallResult<PersistedSwarm> {
        self.call(
            method::SWARM_UPDATE_LAYOUT,
            json!({ "swarm_id": swarm_id, "revision": revision, "boxes": boxes }),
        )
        .await
    }

    // ── Live notification pull ───────────────────────────────────

    /// The next live notification, awaiting the socket if none is buffered.
    ///
    /// Cancel-safe: built on [`Self::read_frame`], so dropping this future in a
    /// `select!` (a keypress won the race) loses no bytes and no queued
    /// notification. Non-swarm frames on the shared stream are skipped.
    pub async fn next_notification(&mut self) -> CallResult<SwarmNotification> {
        if let Some(buffered) = self.take_buffered_notification() {
            return Ok(buffered);
        }
        loop {
            let line = self.read_frame().await?;
            if let Some(notification) = parse_notification(&line) {
                return Ok(notification);
            }
        }
    }

    /// A notification that is already buffered, without touching the socket.
    /// Lets the event loop coalesce a burst into one redraw. Returns `None`
    /// when a real await would be required.
    pub fn try_next_notification(&mut self) -> Option<SwarmNotification> {
        if let Some(buffered) = self.take_buffered_notification() {
            return Some(buffered);
        }
        // Drain any whole frames a single socket read already delivered.
        while let Some(line) = self.take_buffered_line() {
            if let Some(notification) = parse_notification(&line) {
                return Some(notification);
            }
        }
        None
    }

    /// Pop a captured notification, or surface the one-shot gap marker.
    fn take_buffered_notification(&mut self) -> Option<SwarmNotification> {
        if let Some(notification) = self.notif_queue.pop_front() {
            return Some(notification);
        }
        if self.dropped_gap {
            self.dropped_gap = false;
            return Some(SwarmNotification::Gap);
        }
        None
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

    /// Read frames until one carries `id`. A swarm notification met along the
    /// way is captured — not dropped — so an in-flight call never eats the live
    /// stream. EOF is a transport failure.
    async fn read_response(&mut self, id: &Value) -> CallResult<Value> {
        loop {
            let line = self.read_frame().await?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(frame) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            if frame.get("id") == Some(id) {
                return Ok(frame);
            }
            if let Some(notification) = notification_from_frame(&frame) {
                self.enqueue_notification(notification);
            }
        }
    }

    /// Buffer a captured notification, or mark a gap if the queue is full.
    fn enqueue_notification(&mut self, notification: SwarmNotification) {
        if self.notif_queue.len() >= NOTIF_QUEUE_CAP {
            self.dropped_gap = true;
            return;
        }
        self.notif_queue.push_back(notification);
    }

    /// Read exactly one newline-terminated frame as a UTF-8 string.
    ///
    /// Cancel-safe. The only await is [`AsyncBufReadExt::fill_buf`], which is
    /// itself cancel-safe, and every byte it yields is copied into the
    /// persistent [`Self::line_buf`] before it is consumed — so a dropped future
    /// leaves the buffer exactly where it was.
    async fn read_frame(&mut self) -> CallResult<String> {
        loop {
            if let Some(line) = self.take_buffered_line() {
                return Ok(line);
            }
            let chunk = {
                let available = self.reader.fill_buf().await.map_err(transport)?;
                if available.is_empty() {
                    return Err(CallError::Transport(anyhow::Error::msg(dropped_message())));
                }
                available.to_vec()
            };
            self.reader.consume(chunk.len());
            self.line_buf.extend_from_slice(&chunk);
            if self.line_buf.len() as u64 > MAX_FRAME_BYTES {
                // A frame with no newline this large is a wedged peer, not a
                // real message; treat it as a dead connection.
                return Err(CallError::Transport(anyhow::Error::msg(dropped_message())));
            }
        }
    }

    /// Split one whole line out of [`Self::line_buf`] if it already holds one,
    /// without touching the socket.
    fn take_buffered_line(&mut self) -> Option<String> {
        split_frame_line(&mut self.line_buf)
    }
}

/// Split the first newline-terminated line out of `buf`, returning it without
/// the trailing `\n` (and a stripped `\r`). `None` when `buf` holds no whole
/// line yet, leaving the partial bytes in place. Free of `self` so the framing
/// can be tested without a socket.
fn split_frame_line(buf: &mut Vec<u8>) -> Option<String> {
    let newline = buf.iter().position(|&b| b == b'\n')?;
    let mut line: Vec<u8> = buf.drain(..=newline).collect();
    line.pop(); // drop '\n'
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Some(String::from_utf8_lossy(&line).into_owned())
}

/// Decode a raw frame line into a swarm notification, or `None` for anything
/// that is not one (a response, a non-swarm notification, or unparseable text).
fn parse_notification(line: &str) -> Option<SwarmNotification> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let frame = serde_json::from_str::<Value>(trimmed).ok()?;
    notification_from_frame(&frame)
}

/// Project a decoded JSON-RPC frame onto a [`SwarmNotification`]. Only
/// `swarm/update` and `swarm/board` notifications survive.
fn notification_from_frame(frame: &Value) -> Option<SwarmNotification> {
    // A response carries `id`; a notification never does.
    if frame.get("id").is_some() {
        return None;
    }
    let method = frame.get("method").and_then(Value::as_str)?;
    let params = frame.get("params")?;
    match method {
        notification::SWARM_UPDATE => serde_json::from_value::<SwarmUpdate>(params.clone())
            .ok()
            .map(|update| SwarmNotification::Update(Box::new(update))),
        notification::SWARM_BOARD => serde_json::from_value::<SwarmBoardNotify>(params.clone())
            .ok()
            .map(|board| SwarmNotification::Board(Box::new(board))),
        _ => None,
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

    #[test]
    fn framing_splits_whole_lines_and_keeps_the_partial_tail() {
        let mut buf: Vec<u8> = b"one\r\ntwo\nthr".to_vec();
        assert_eq!(split_frame_line(&mut buf).as_deref(), Some("one"));
        assert_eq!(split_frame_line(&mut buf).as_deref(), Some("two"));
        // The unterminated tail is left in place for the next read.
        assert_eq!(split_frame_line(&mut buf), None);
        assert_eq!(buf, b"thr");
    }

    #[test]
    fn only_swarm_notifications_survive_parsing() {
        let update = parse_notification(
            r#"{"jsonrpc":"2.0","method":"swarm/update","params":{"swarm_id":"sw-1","box_id":"box-2","event":{"type":"agent_message_chunk","session_id":"s","text":"hi"}}}"#,
        );
        assert!(matches!(update, Some(SwarmNotification::Update(_))));

        let board = parse_notification(
            r#"{"jsonrpc":"2.0","method":"swarm/board","params":{"swarm_id":"sw-1","event":{"event":"claimed","box_id":"box-1","task_key":"k"}}}"#,
        );
        assert!(matches!(board, Some(SwarmNotification::Board(_))));

        // A response frame (has an id) is never a notification.
        assert!(parse_notification(r#"{"jsonrpc":"2.0","id":"swarm-1","result":{}}"#).is_none());
        // A notification for another family is skipped.
        assert!(
            parse_notification(r#"{"jsonrpc":"2.0","method":"logs/event","params":{}}"#).is_none()
        );
        assert!(parse_notification("   ").is_none());
        assert!(parse_notification("not json").is_none());
    }
}
