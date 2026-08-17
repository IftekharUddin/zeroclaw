//! Headless end-to-end smoke for the swarm run-control + live-stream +
//! interjection RPC surface (S7).
//!
//! This is the slice's headline deliverable. It stands up a *real* daemon over a
//! *real* unix socket (`run_local_listener`) with a live [`SwarmEngine`] wired
//! into the RPC context, driven entirely by a scripted `ModelProvider` — no
//! network, no TUI — and exercises the whole daemon-side feature:
//!
//!   create → subscribe → start → (box turn streams over `swarm/subscribe`)
//!     → interject via `swarm/chat` (the box answers the user)
//!     → `resume` (orchestrator regains the box)
//!     → pause → resume → stop (reap is clean)
//!
//! ## Determinism
//!
//! The scripted provider gives fixed replies and a per-box-turn gate that blocks
//! each box turn's first provider call until the test releases exactly one
//! permit — so at most one box turn is ever in flight, and the test drives the
//! lifecycle at precise points. Everything the test then waits on (a run status,
//! a streamed notification) it waits on with a bounded convergence poll, the
//! same idiom the S6 engine tests use. No wall-clock timing is load-bearing.
//!
//! Run it with:
//! ```text
//! cargo nextest run -p zeroclaw-runtime --test swarm_runctl_e2e
//! # or: cargo test -p zeroclaw-runtime --test swarm_runctl_e2e -- --nocapture
//! ```

#![cfg(unix)]

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio_util::sync::CancellationToken;

use zeroclaw_api::attribution::{Attributable, ModelProviderKind, ProviderKind, Role};
use zeroclaw_api::jsonrpc::JsonRpcRequest;
use zeroclaw_api::model_provider::{ModelProvider, ProviderCapabilities, ToolCall};
use zeroclaw_config::policy::SecurityPolicy;
use zeroclaw_config::schema::{AliasedAgentConfig, Config, RiskProfileConfig};
use zeroclaw_memory::{Memory, SqliteMemory};
use zeroclaw_providers::traits::TokenUsage;
use zeroclaw_providers::{ChatRequest, ChatResponse};

use zeroclaw_runtime::rpc::context::RpcContext;
use zeroclaw_runtime::rpc::session::SessionStore;
use zeroclaw_runtime::rpc::tui_identity::TuiRegistry;
use zeroclaw_runtime::swarm::engine::SwarmProviderFactory;
use zeroclaw_runtime::swarm::store::{
    InMemorySwarmStore, PersistedSwarm, SwarmBudget, SwarmBudgetLimits, SwarmSpec, SwarmStore,
    default_roster,
};
use zeroclaw_runtime::swarm::{SwarmEngine, SwarmStateBoard};

const PROVIDER: &str = "custom.default";
const MODEL: &str = "test-model";
const SWARM_ID: &str = "sw-e2e";
const BOX_ID: &str = "box-1";
const INTERJECTION: &str = "INTERJECT please help me directly";

// ── Scripted provider ────────────────────────────────────────────────

/// Coordinates a blocking box turn with the test: the box turn signals its first
/// provider call has begun, then blocks until the test releases one permit. Only
/// the first call of each box turn blocks, so the test advances one whole box
/// turn per permit.
struct TurnGate {
    started: tokio::sync::mpsc::UnboundedSender<()>,
    release: tokio::sync::Semaphore,
}

/// Orchestrator provider: fixed reply queue, `swarm_delegate` tool calls scripted
/// by [`OrchReply`].
enum OrchReply {
    Delegate { box_id: String, subtask: String },
    Final(String),
}

struct OrchProvider {
    name: String,
    queue: Arc<StdMutex<VecDeque<OrchReply>>>,
}

impl Attributable for OrchProvider {
    fn role(&self) -> Role {
        Role::Provider(ProviderKind::Model(ModelProviderKind::Custom))
    }
    fn alias(&self) -> &str {
        &self.name
    }
}

#[async_trait]
impl ModelProvider for OrchProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
            prompt_caching: false,
            extended_thinking: false,
        }
    }

    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: Option<f64>,
    ) -> anyhow::Result<String> {
        Ok("ok".to_string())
    }

    async fn chat(
        &self,
        _request: ChatRequest<'_>,
        _model: &str,
        _temperature: Option<f64>,
    ) -> anyhow::Result<ChatResponse> {
        let reply = self
            .queue
            .lock()
            .expect("orch queue")
            .pop_front()
            .unwrap_or(OrchReply::Final("done".to_string()));
        Ok(match reply {
            OrchReply::Delegate { box_id, subtask } => ChatResponse {
                text: None,
                tool_calls: vec![ToolCall {
                    id: "orch-delegate".to_string(),
                    name: "swarm_delegate".to_string(),
                    arguments: json!({"box_id": box_id, "subtask": subtask}).to_string(),
                    extra_content: None,
                }],
                usage: None,
                reasoning_content: None,
            },
            OrchReply::Final(text) => ChatResponse {
                text: Some(text),
                tool_calls: Vec::new(),
                usage: None,
                reasoning_content: None,
            },
        })
    }
}

/// Box provider: emits a benign `swarm_state` tool call on its first round (so
/// tool events stream), then either answers a user interjection it finds folded
/// into its history or reports plain progress.
struct BoxProvider {
    name: String,
    calls: AtomicU64,
    gate: Arc<TurnGate>,
}

impl Attributable for BoxProvider {
    fn role(&self) -> Role {
        Role::Provider(ProviderKind::Model(ModelProviderKind::Custom))
    }
    fn alias(&self) -> &str {
        &self.name
    }
}

#[async_trait]
impl ModelProvider for BoxProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
            prompt_caching: false,
            extended_thinking: false,
        }
    }

    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: Option<f64>,
    ) -> anyhow::Result<String> {
        Ok("ok".to_string())
    }

    async fn chat(
        &self,
        request: ChatRequest<'_>,
        _model: &str,
        _temperature: Option<f64>,
    ) -> anyhow::Result<ChatResponse> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        // Gate only the first provider call of the turn: the test releases one
        // permit to advance one whole box turn.
        if call == 0 {
            let _ = self.gate.started.send(());
            self.gate
                .release
                .acquire()
                .await
                .expect("gate semaphore must not close")
                .forget();
        }

        // A user interjection folded into this turn is answered first.
        let interjection = request
            .messages
            .iter()
            .rev()
            .find(|m| m.content.contains("INTERJECT"))
            .map(|m| m.content.clone());

        let reply = if let Some(text) = interjection {
            ChatResponse {
                text: Some(format!("USER-HANDLED: {text}")),
                tool_calls: Vec::new(),
                usage: Some(usage(3, 3)),
                reasoning_content: None,
            }
        } else if call == 0 {
            // First round: a real (side-effect-free) tool call so ToolCall /
            // ToolResult events stream over swarm/subscribe.
            ChatResponse {
                text: None,
                tool_calls: vec![ToolCall {
                    id: "box-read".to_string(),
                    name: "swarm_state".to_string(),
                    arguments: json!({"action": "read_board"}).to_string(),
                    extra_content: None,
                }],
                usage: Some(usage(1, 1)),
                reasoning_content: None,
            }
        } else {
            ChatResponse {
                text: Some("working".to_string()),
                tool_calls: Vec::new(),
                usage: Some(usage(1, 1)),
                reasoning_content: None,
            }
        };
        Ok(reply)
    }
}

fn usage(input: u64, output: u64) -> TokenUsage {
    TokenUsage {
        input_tokens: Some(input),
        output_tokens: Some(output),
        cached_input_tokens: None,
    }
}

struct ScriptedFactory {
    orch_queue: Arc<StdMutex<VecDeque<OrchReply>>>,
    box_gate: Arc<TurnGate>,
}

impl SwarmProviderFactory for ScriptedFactory {
    fn create(
        &self,
        provider: &str,
        _model: &str,
        agent_alias: &str,
    ) -> anyhow::Result<Box<dyn ModelProvider>> {
        if agent_alias.ends_with("/orchestrator") {
            Ok(Box::new(OrchProvider {
                name: provider.to_string(),
                queue: Arc::clone(&self.orch_queue),
            }))
        } else {
            Ok(Box::new(BoxProvider {
                name: provider.to_string(),
                calls: AtomicU64::new(0),
                gate: Arc::clone(&self.box_gate),
            }))
        }
    }
}

// ── Daemon harness ───────────────────────────────────────────────────

struct Daemon {
    _tmp: TempDir,
    ctx: Arc<RpcContext>,
    engine: Arc<SwarmEngine>,
    sock_path: std::path::PathBuf,
    cancel: CancellationToken,
    started_rx: tokio::sync::mpsc::UnboundedReceiver<()>,
    gate: Arc<TurnGate>,
    store: Arc<dyn SwarmStore>,
}

impl Daemon {
    fn build(orch_queue: VecDeque<OrchReply>) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace dir");

        let mut config = Config {
            data_dir: tmp.path().to_path_buf(),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        config.memory.backend = "sqlite".to_string();
        config
            .risk_profiles
            .insert("default".to_string(), RiskProfileConfig::default());
        config.agents.insert(
            "default".to_string(),
            AliasedAgentConfig {
                enabled: true,
                model_provider: PROVIDER.into(),
                risk_profile: "default".into(),
                ..AliasedAgentConfig::default()
            },
        );

        let memory: Arc<dyn Memory> =
            Arc::new(SqliteMemory::new("sqlite", tmp.path()).expect("sqlite memory"));
        let policy = Arc::new(SecurityPolicy::for_agent(&config, "default").expect("policy"));
        let config = Arc::new(config);

        let store: Arc<dyn SwarmStore> = Arc::new(InMemorySwarmStore::new());
        // Seed the "created" swarm (the create step; the wizard/CRUD create path
        // is S3's socket-tested surface).
        let mut spec = SwarmSpec::new(
            SWARM_ID,
            "E2E squad",
            PROVIDER,
            MODEL,
            "default",
            "supervisor",
            "prove the run-control surface",
        );
        spec.budget = SwarmBudget::Custom(SwarmBudgetLimits {
            max_turns: 100,
            max_tokens: 10_000_000,
            max_cost_usd: 1_000.0,
        });
        spec.boxes = default_roster(1);
        store
            .save_swarm(&PersistedSwarm::new(spec))
            .expect("seed created swarm");

        let (started_tx, started_rx) = tokio::sync::mpsc::unbounded_channel();
        let gate = Arc::new(TurnGate {
            started: started_tx,
            release: tokio::sync::Semaphore::new(0),
        });

        let session_queue = Arc::new(zeroclaw_infra::session_queue::SessionActorQueue::new(
            8, 30, 600,
        ));
        let sessions = Arc::new(SessionStore::new(64, session_queue));

        let factory: Arc<dyn SwarmProviderFactory> = Arc::new(ScriptedFactory {
            orch_queue: Arc::new(StdMutex::new(orch_queue)),
            box_gate: Arc::clone(&gate),
        });
        let engine = Arc::new(
            SwarmEngine::new(
                Arc::clone(&store),
                SwarmStateBoard::new(),
                Arc::clone(&memory),
                Arc::clone(&config),
                "default",
                policy,
                factory,
                Arc::new(std::collections::HashMap::new()),
                workspace,
                None,
                "boot-e2e",
            )
            .with_sessions(Arc::clone(&sessions)),
        );

        let sock_path = zeroclaw_runtime::rpc::local::socket_path(&config);
        let ctx = build_ctx(&config, Arc::clone(&sessions), Arc::clone(&store), &engine);

        Self {
            _tmp: tmp,
            ctx,
            engine,
            sock_path,
            cancel: CancellationToken::new(),
            started_rx,
            gate,
            store,
        }
    }

    fn serve(&self) {
        let ctx = Arc::clone(&self.ctx);
        let cancel = self.cancel.clone();
        zeroclaw_spawn::spawn!(async move {
            let _ = zeroclaw_runtime::rpc::local::run_local_listener(
                ctx,
                cancel,
                Arc::new(AtomicUsize::new(0)),
                None,
            )
            .await;
        });
    }

    async fn wait_for_box_turn_started(&mut self) {
        tokio::time::timeout(Duration::from_secs(10), self.started_rx.recv())
            .await
            .expect("a box turn must start within the timeout")
            .expect("gate started channel open");
    }

    fn release_one_box_turn(&self) {
        self.gate.release.add_permits(1);
    }
}

fn build_ctx(
    config: &Arc<Config>,
    sessions: Arc<SessionStore>,
    store: Arc<dyn SwarmStore>,
    engine: &Arc<SwarmEngine>,
) -> Arc<RpcContext> {
    Arc::new(RpcContext {
        config: Arc::new(parking_lot::RwLock::new((**config).clone())),
        config_write_lock: Arc::new(tokio::sync::Mutex::new(())),
        sessions,
        session_backend: None,
        memory: None,
        cost_tracker: None,
        event_tx: None,
        reload_tx: None,
        gateway_shutdown_tx: None,
        approval_pending: Arc::new(zeroclaw_runtime::rpc::context::ApprovalPendingMap::default()),
        tui_registry: Arc::new(TuiRegistry::new(&config.data_dir)),
        acp_session_store: None,
        sop_engine: None,
        sop_audit: None,
        swarm_store: store,
        swarm_engine: Some(Arc::clone(engine)),
        hooks: None,
    })
}

// ── Socket client ────────────────────────────────────────────────────

struct Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    /// Every notification frame observed while awaiting responses.
    notes: Vec<Value>,
    next_id: u64,
}

impl Client {
    async fn connect(sock_path: &std::path::Path) -> Self {
        let stream = UnixStream::connect(sock_path)
            .await
            .expect("connect socket");
        let (read_half, writer) = stream.into_split();
        let mut client = Self {
            reader: BufReader::new(read_half),
            writer,
            notes: Vec::new(),
            next_id: 1,
        };
        client
            .call("initialize", json!({"protocol_version": 1}))
            .await;
        client
    }

    /// Encode and send a request, returning the id it was stamped with. Written
    /// eagerly so a caller can release a gate before awaiting the response (the
    /// pause / stop control points do exactly that).
    async fn send(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let req = JsonRpcRequest::new(method, params, Value::Number(id.into()));
        let mut line = serde_json::to_string(&req).expect("encode request");
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .await
            .expect("write request");
        id
    }

    /// Await the response frame carrying `id`, buffering any notifications seen.
    async fn await_response(&mut self, id: u64) -> Value {
        loop {
            let frame = self.next_frame(Duration::from_secs(15)).await;
            let Some(frame) = frame else {
                panic!("timed out awaiting response id={id}");
            };
            if frame.get("method").is_some() {
                self.notes.push(frame);
                continue;
            }
            if frame.get("id").and_then(Value::as_u64) == Some(id) {
                assert!(
                    frame.get("error").map(Value::is_null).unwrap_or(true),
                    "unexpected RPC error for id={id}: {frame}"
                );
                return frame.get("result").cloned().unwrap_or(Value::Null);
            }
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params).await;
        self.await_response(id).await
    }

    /// Pump frames (buffering notifications) until `until` is satisfied by the
    /// accumulated notifications, or the timeout elapses.
    async fn pump_until(&mut self, timeout: Duration, until: impl Fn(&[Value]) -> bool) {
        let deadline = tokio::time::Instant::now() + timeout;
        if until(&self.notes) {
            return;
        }
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                panic!(
                    "notification condition not met within timeout; saw {} notifications",
                    self.notes.len()
                );
            }
            if let Some(frame) = self.next_frame(remaining).await
                && frame.get("method").is_some()
            {
                self.notes.push(frame);
                if until(&self.notes) {
                    return;
                }
            }
        }
    }

    async fn next_frame(&mut self, timeout: Duration) -> Option<Value> {
        let mut line = String::new();
        match tokio::time::timeout(timeout, self.reader.read_line(&mut line)).await {
            Ok(Ok(0)) => None,
            Ok(Ok(_)) => serde_json::from_str(line.trim()).ok(),
            Ok(Err(_)) | Err(_) => None,
        }
    }
}

// ── Notification predicates ──────────────────────────────────────────

fn is_swarm_update(frame: &Value) -> bool {
    frame.get("method").and_then(Value::as_str) == Some("swarm/update")
}

fn update_event_type(frame: &Value) -> Option<&str> {
    frame.get("params")?.get("event")?.get("type")?.as_str()
}

fn saw_box_tool_call(notes: &[Value]) -> bool {
    notes.iter().any(|n| {
        is_swarm_update(n)
            && n.get("params")
                .and_then(|p| p.get("box_id"))
                .and_then(Value::as_str)
                == Some(BOX_ID)
            && update_event_type(n) == Some("tool_call")
    })
}

/// The status label, whether it came over the wire as a plain string (unit
/// variants `running` / `stopped`) or as the tagged object the `paused` variant
/// serializes to (`{"paused": {"reason": ...}}`).
fn status_label(status: &Value) -> String {
    match status {
        Value::String(s) => s.clone(),
        Value::Object(map) => map.keys().next().cloned().unwrap_or_default(),
        _ => String::new(),
    }
}

fn saw_user_handled(notes: &[Value]) -> bool {
    notes.iter().any(|n| {
        if !is_swarm_update(n) {
            return false;
        }
        let Some(params) = n.get("params") else {
            return false;
        };
        let text = params
            .get("event")
            .and_then(|e| e.get("content").or_else(|| e.get("text")))
            .and_then(Value::as_str)
            .unwrap_or("");
        text.contains("USER-HANDLED")
    })
}

// ── The E2E ──────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn swarm_run_control_full_lifecycle_over_the_socket() {
    // Orchestrator script: two delegating rounds (each = delegate + a final to
    // close the round), then it keeps delegating (fallback) so a live box turn is
    // always available for the pause/stop control points.
    let orch_queue = VecDeque::from(vec![
        OrchReply::Delegate {
            box_id: BOX_ID.to_string(),
            subtask: "task one".to_string(),
        },
        OrchReply::Final("round one done".to_string()),
        OrchReply::Delegate {
            box_id: BOX_ID.to_string(),
            subtask: "task two".to_string(),
        },
        OrchReply::Final("round two done".to_string()),
        OrchReply::Delegate {
            box_id: BOX_ID.to_string(),
            subtask: "resumed task".to_string(),
        },
        OrchReply::Final("round three done".to_string()),
    ]);

    let mut daemon = Daemon::build(orch_queue);
    daemon.serve();
    wait_for_socket(&daemon.sock_path).await;

    let mut client = Client::connect(&daemon.sock_path).await;

    // ── start ────────────────────────────────────────────────────────
    let started = client
        .call("swarm/start", json!({"swarm_id": SWARM_ID}))
        .await;
    assert_eq!(
        status_label(&started["status"]),
        "running",
        "start → running: {started}"
    );

    // Opt into the live streams now that the board exists (post-start).
    let sub = client
        .call("swarm/subscribe", json!({"swarm_id": SWARM_ID}))
        .await;
    assert_eq!(sub["subscribed"], true);

    // Round 0 delegates to box-1: its first turn is now in flight (blocked).
    daemon.wait_for_box_turn_started().await;

    // ── interject ──────────────────────────────────────────────────────
    let chat = client
        .call(
            "swarm/chat",
            json!({"swarm_id": SWARM_ID, "box_id": BOX_ID, "message": INTERJECTION}),
        )
        .await;
    assert_eq!(chat["queued"], true, "chat → queued: {chat}");
    assert_eq!(chat["user_engaged"], true, "chat → engaged: {chat}");
    assert!(
        daemon.engine.is_box_user_engaged(SWARM_ID, BOX_ID),
        "the engine must hold the box for the user after an interjection"
    );

    // ── resume (handback) ──────────────────────────────────────────────
    let resume_chat = client
        .call(
            "swarm/chat",
            json!({"swarm_id": SWARM_ID, "box_id": BOX_ID, "message": "resume"}),
        )
        .await;
    assert_eq!(
        resume_chat["released"], true,
        "resume → released: {resume_chat}"
    );
    assert_eq!(resume_chat["user_engaged"], false);
    assert!(
        !daemon.engine.is_box_user_engaged(SWARM_ID, BOX_ID),
        "resume must hand the box back to the orchestrator"
    );

    // Let box turn #1 run to completion. It streams a tool call and then answers
    // the user's interjection first.
    daemon.release_one_box_turn();
    client
        .pump_until(Duration::from_secs(20), |notes| {
            saw_box_tool_call(notes) && saw_user_handled(notes)
        })
        .await;

    // Round 1 delegates to box-1 again — proving the orchestrator regained the
    // box after handback — and blocks its second turn.
    daemon.wait_for_box_turn_started().await;

    // ── pause (in-flight box turn finishes, then holds) ────────────────
    let pause_id = client
        .send("swarm/pause", json!({"swarm_id": SWARM_ID}))
        .await;
    daemon.release_one_box_turn();
    let paused = client.await_response(pause_id).await;
    assert_eq!(
        status_label(&paused["status"]),
        "paused",
        "pause → paused: {paused}"
    );
    assert_eq!(
        paused["status"]["paused"]["reason"], "user_requested",
        "pause reason must be user_requested: {paused}"
    );

    // ── resume the paused run ──────────────────────────────────────────
    let resumed = client
        .call("swarm/resume", json!({"swarm_id": SWARM_ID}))
        .await;
    assert_eq!(
        status_label(&resumed["status"]),
        "running",
        "resume → running: {resumed}"
    );

    // The resumed driver delegates again; its box turn blocks, keeping the run
    // live for the stop control point.
    daemon.wait_for_box_turn_started().await;

    // ── stop (cancel + reap) ───────────────────────────────────────────
    let stop_id = client
        .send("swarm/stop", json!({"swarm_id": SWARM_ID}))
        .await;
    // Release the blocked box turn so the cancelled turn unwinds promptly.
    daemon.gate.release.add_permits(64);
    let stopped = client.await_response(stop_id).await;
    assert_eq!(
        status_label(&stopped["status"]),
        "stopped",
        "stop → stopped: {stopped}"
    );
    let warnings = stopped["reap"]["warnings"]
        .as_array()
        .expect("reap.warnings is an array");
    assert!(
        warnings.is_empty(),
        "stop must reap cleanly: {:?}",
        warnings
    );

    // The persisted document confirms the terminal state end-to-end.
    let final_doc = daemon.store.load_swarm(SWARM_ID).unwrap().unwrap();
    assert_eq!(final_doc.run.status.as_str(), "stopped");

    daemon.cancel.cancel();
}

async fn wait_for_socket(path: &std::path::Path) {
    for _ in 0..100 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("socket never appeared at {}", path.display());
}
