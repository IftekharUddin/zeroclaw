//! Engine-level integration tests for the swarm orchestrator (S6).
//!
//! Every turn — the orchestrator's planning rounds and each box delegation —
//! runs through the real `execute_turn` path against a scripted `ModelProvider`
//! injected via the engine's provider-factory seam, so these exercise the whole
//! driver: roster-only delegation, per-axis budget admission, the full
//! start→run→complete lifecycle, restart recovery, and pause / stop + reap.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use async_trait::async_trait;
use tempfile::TempDir;

use zeroclaw_api::attribution::{Attributable, ModelProviderKind, ProviderKind, Role};
use zeroclaw_api::model_provider::{ModelProvider, ProviderCapabilities, ToolCall};
use zeroclaw_config::policy::SecurityPolicy;
use zeroclaw_config::schema::{AliasedAgentConfig, Config, RiskProfileConfig};
use zeroclaw_memory::{Memory, SqliteMemory};
use zeroclaw_providers::traits::TokenUsage;
use zeroclaw_providers::{ChatRequest, ChatResponse};

use zeroclaw_runtime::control_plane::{SqliteTaskStore, TaskKind, TaskRegistry, TaskStatus};
use zeroclaw_runtime::swarm::SwarmStateBoard;
use zeroclaw_runtime::swarm::engine::{
    BoxTurnError, BudgetAxis, SwarmEngine, SwarmProviderFactory,
};
use zeroclaw_runtime::swarm::store::{
    InMemorySwarmStore, PersistedSwarm, SwarmBudget, SwarmBudgetLimits, SwarmPauseReason,
    SwarmSpec, SwarmStatus, SwarmStore, default_roster,
};

// ── Scripted provider ────────────────────────────────────────────────

#[derive(Clone)]
struct Reply {
    text: Option<String>,
    /// (tool name, JSON args string).
    calls: Vec<(String, String)>,
    usage: Option<(u64, u64)>,
}

impl Reply {
    fn final_text(text: &str) -> Self {
        Self {
            text: Some(text.to_string()),
            calls: Vec::new(),
            usage: None,
        }
    }

    fn delegate(box_id: &str, subtask: &str) -> Self {
        Self {
            text: None,
            calls: vec![(
                "swarm_delegate".to_string(),
                serde_json::json!({"box_id": box_id, "subtask": subtask}).to_string(),
            )],
            usage: None,
        }
    }

    fn box_result(text: &str, input: u64, output: u64) -> Self {
        Self {
            text: Some(text.to_string()),
            calls: Vec::new(),
            usage: Some((input, output)),
        }
    }
}

/// Coordinates a blocking box turn with the test: the box turn signals it has
/// started, then blocks until the test releases it.
struct TurnGate {
    started: tokio::sync::mpsc::UnboundedSender<()>,
    release: tokio::sync::Semaphore,
}

struct ScriptProvider {
    name: String,
    queue: Arc<StdMutex<VecDeque<Reply>>>,
    fallback: Reply,
    gate: Option<Arc<TurnGate>>,
}

impl Attributable for ScriptProvider {
    fn role(&self) -> Role {
        Role::Provider(ProviderKind::Model(ModelProviderKind::Custom))
    }
    fn alias(&self) -> &str {
        &self.name
    }
}

#[async_trait]
impl ModelProvider for ScriptProvider {
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
        if let Some(gate) = &self.gate {
            let _ = gate.started.send(());
            let permit = gate
                .release
                .acquire()
                .await
                .expect("gate semaphore must not close");
            permit.forget();
        }
        let reply = {
            let mut queue = self.queue.lock().expect("script queue lock");
            queue.pop_front()
        }
        .unwrap_or_else(|| self.fallback.clone());

        let tool_calls: Vec<ToolCall> = reply
            .calls
            .iter()
            .enumerate()
            .map(|(i, (name, args))| ToolCall {
                id: format!("call-{i}"),
                name: name.clone(),
                arguments: args.clone(),
                extra_content: None,
            })
            .collect();
        let usage = reply.usage.map(|(input, output)| TokenUsage {
            input_tokens: Some(input),
            output_tokens: Some(output),
            cached_input_tokens: None,
        });
        Ok(ChatResponse {
            text: reply.text,
            tool_calls,
            usage,
            reasoning_content: None,
        })
    }
}

struct ScriptedFactory {
    orch_queue: Arc<StdMutex<VecDeque<Reply>>>,
    orch_fallback: Reply,
    box_reply: Reply,
    box_gate: Option<Arc<TurnGate>>,
}

impl SwarmProviderFactory for ScriptedFactory {
    fn create(
        &self,
        provider: &str,
        _model: &str,
        agent_alias: &str,
    ) -> anyhow::Result<Box<dyn ModelProvider>> {
        if agent_alias.ends_with("/orchestrator") {
            Ok(Box::new(ScriptProvider {
                name: provider.to_string(),
                queue: Arc::clone(&self.orch_queue),
                fallback: self.orch_fallback.clone(),
                gate: None,
            }))
        } else {
            Ok(Box::new(ScriptProvider {
                name: provider.to_string(),
                queue: Arc::new(StdMutex::new(VecDeque::new())),
                fallback: self.box_reply.clone(),
                gate: self.box_gate.clone(),
            }))
        }
    }
}

// ── Harness ──────────────────────────────────────────────────────────

const PROVIDER: &str = "custom.default";
const MODEL: &str = "test-model";

struct Harness {
    _tmp: TempDir,
    config: Arc<Config>,
    memory: Arc<dyn Memory>,
    policy: Arc<SecurityPolicy>,
    workspace: std::path::PathBuf,
}

impl Harness {
    fn new() -> Self {
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
        let policy =
            Arc::new(SecurityPolicy::for_agent(&config, "default").expect("parent policy"));

        Self {
            _tmp: tmp,
            config: Arc::new(config),
            memory,
            policy,
            workspace,
        }
    }

    fn engine(
        &self,
        store: Arc<dyn SwarmStore>,
        factory: Arc<dyn SwarmProviderFactory>,
        pricing: HashMap<String, HashMap<String, f64>>,
        tasks: Option<Arc<dyn TaskRegistry>>,
    ) -> Arc<SwarmEngine> {
        Arc::new(SwarmEngine::new(
            store,
            SwarmStateBoard::new(),
            Arc::clone(&self.memory),
            Arc::clone(&self.config),
            "default",
            Arc::clone(&self.policy),
            factory,
            Arc::new(pricing),
            self.workspace.clone(),
            tasks,
            "boot-test",
        ))
    }
}

fn spec_with(id: &str, boxes: usize, budget: SwarmBudget) -> SwarmSpec {
    let mut spec = SwarmSpec::new(
        id,
        format!("Swarm {id}"),
        PROVIDER,
        MODEL,
        "default",
        "supervisor",
        "ship the thing",
    );
    spec.budget = budget;
    spec.boxes = default_roster(boxes);
    spec
}

fn save_created(store: &Arc<dyn SwarmStore>, spec: SwarmSpec) {
    store
        .save_swarm(&PersistedSwarm::new(spec))
        .expect("save created swarm");
}

fn custom(max_turns: u32, max_tokens: u64, max_cost_usd: f64) -> SwarmBudget {
    SwarmBudget::Custom(SwarmBudgetLimits {
        max_turns,
        max_tokens,
        max_cost_usd,
    })
}

async fn wait_for_status(
    store: &Arc<dyn SwarmStore>,
    id: &str,
    pred: impl Fn(SwarmStatus) -> bool,
    label: &str,
) -> SwarmStatus {
    for _ in 0..300 {
        if let Some(swarm) = store.load_swarm(id).expect("load")
            && pred(swarm.run.status)
        {
            return swarm.run.status;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {label}");
}

fn box_delegated_events(store: &Arc<dyn SwarmStore>, id: &str) -> Vec<String> {
    store
        .list_events(id)
        .expect("events")
        .into_iter()
        .filter(|e| e.kind == "box_delegated")
        .filter_map(|e| e.box_id)
        .collect()
}

// ── (a) roster-only delegation ───────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swarm_out_of_roster_target_rejected_by_engine_primitive() {
    let h = Harness::new();
    let store: Arc<dyn SwarmStore> = Arc::new(InMemorySwarmStore::new());
    save_created(
        &store,
        spec_with("s-roster", 2, custom(100, 10_000_000, 1000.0)),
    );

    let factory: Arc<dyn SwarmProviderFactory> = Arc::new(ScriptedFactory {
        orch_queue: Arc::new(StdMutex::new(VecDeque::new())),
        orch_fallback: Reply::final_text("done"),
        box_reply: Reply::box_result("box ok", 1, 1),
        box_gate: None,
    });
    let engine = h.engine(Arc::clone(&store), factory, HashMap::new(), None);

    engine.begin_run("s-roster").await.expect("begin");
    // The delegate tool routes here; naming a box outside the roster is refused
    // before any turn runs.
    let err = engine
        .run_box_turn("s-roster", "ghost", "do it")
        .await
        .expect_err("an out-of-roster box must be refused");
    assert!(matches!(err, BoxTurnError::UnknownBox(ref b) if b == "ghost"));
    // A real roster box is admitted.
    engine
        .run_box_turn("s-roster", "box-1", "do it")
        .await
        .expect("an in-roster box must be admitted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swarm_orchestrator_out_of_roster_runs_no_box_and_completes() {
    let h = Harness::new();
    let store: Arc<dyn SwarmStore> = Arc::new(InMemorySwarmStore::new());
    save_created(
        &store,
        spec_with("s-ghost", 2, custom(100, 10_000_000, 1000.0)),
    );

    // Orchestrator tries to delegate to a box that is not on the roster; the
    // swarm_delegate tool rejects it, so the round runs no box and the swarm
    // finishes cleanly.
    let orch_queue = VecDeque::from(vec![
        Reply::delegate("ghost", "unreachable subtask"),
        Reply::final_text("could not delegate; finishing"),
    ]);
    let factory: Arc<dyn SwarmProviderFactory> = Arc::new(ScriptedFactory {
        orch_queue: Arc::new(StdMutex::new(orch_queue)),
        orch_fallback: Reply::final_text("done"),
        box_reply: Reply::box_result("box ok", 1, 1),
        box_gate: None,
    });
    let engine = h.engine(Arc::clone(&store), factory, HashMap::new(), None);

    engine.start("s-ghost").await.expect("start");
    wait_for_status(&store, "s-ghost", |s| s.is_terminal(), "completion").await;

    assert_eq!(
        store.load_swarm("s-ghost").unwrap().unwrap().run.status,
        SwarmStatus::Completed
    );
    assert!(
        box_delegated_events(&store, "s-ghost").is_empty(),
        "an out-of-roster delegation must never run a box"
    );
}

// ── (b) per-axis budget halts ────────────────────────────────────────

async fn drive_box_turns_until_budget(
    budget: SwarmBudget,
    box_reply: Reply,
    pricing: HashMap<String, HashMap<String, f64>>,
) -> (Arc<dyn SwarmStore>, BudgetAxis) {
    let h = Harness::new();
    let store: Arc<dyn SwarmStore> = Arc::new(InMemorySwarmStore::new());
    save_created(&store, spec_with("s-budget", 2, budget));
    let factory: Arc<dyn SwarmProviderFactory> = Arc::new(ScriptedFactory {
        orch_queue: Arc::new(StdMutex::new(VecDeque::new())),
        orch_fallback: Reply::final_text("done"),
        box_reply,
        box_gate: None,
    });
    let engine = h.engine(Arc::clone(&store), factory, pricing, None);
    engine.begin_run("s-budget").await.expect("begin");

    let mut axis = None;
    for _ in 0..10 {
        match engine.run_box_turn("s-budget", "box-1", "work").await {
            Ok(_) => {}
            Err(BoxTurnError::BudgetExhausted(a)) => {
                axis = Some(a);
                break;
            }
            Err(other) => panic!("unexpected box turn error: {other}"),
        }
    }
    (store, axis.expect("budget must eventually halt delegation"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swarm_budget_halts_on_the_turns_axis_alone() {
    // Two turns permitted; tokens/cost unbounded. The third is refused.
    let (store, axis) = drive_box_turns_until_budget(
        custom(2, u64::MAX, f64::MAX),
        Reply::box_result("ok", 0, 0),
        HashMap::new(),
    )
    .await;
    assert_eq!(axis, BudgetAxis::Turns);
    let run = store.load_swarm("s-budget").unwrap().unwrap().run;
    assert_eq!(
        run.status,
        SwarmStatus::Paused {
            reason: SwarmPauseReason::BudgetExhausted
        }
    );
    assert_eq!(
        run.spent.turns, 2,
        "exactly the admitted turns are recorded"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swarm_budget_halts_on_the_tokens_axis_alone() {
    // 1000-token ceiling; each turn spends 600 tokens; turns/cost unbounded.
    let (store, axis) = drive_box_turns_until_budget(
        custom(100, 1_000, f64::MAX),
        Reply::box_result("ok", 600, 0),
        HashMap::new(),
    )
    .await;
    assert_eq!(axis, BudgetAxis::Tokens);
    let run = store.load_swarm("s-budget").unwrap().unwrap().run;
    assert_eq!(
        run.status,
        SwarmStatus::Paused {
            reason: SwarmPauseReason::BudgetExhausted
        }
    );
    assert_eq!(run.spent.tokens, 1_200, "two 600-token turns were admitted");
    assert!(run.spent.turns < 100, "the turns axis did not trip first");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swarm_budget_halts_on_the_cost_axis_alone() {
    // $3 ceiling; pricing makes each 1000+1000-token turn cost $2; turns/tokens
    // unbounded, so only cost can trip.
    let mut pricing = HashMap::new();
    pricing.insert(
        PROVIDER.to_string(),
        HashMap::from([
            (format!("{MODEL}.input"), 1_000.0),
            (format!("{MODEL}.output"), 1_000.0),
        ]),
    );
    let (store, axis) = drive_box_turns_until_budget(
        custom(100, u64::MAX, 3.0),
        Reply::box_result("ok", 1_000, 1_000),
        pricing,
    )
    .await;
    assert_eq!(axis, BudgetAxis::Cost);
    let run = store.load_swarm("s-budget").unwrap().unwrap().run;
    assert_eq!(
        run.status,
        SwarmStatus::Paused {
            reason: SwarmPauseReason::BudgetExhausted
        }
    );
    assert!(run.spent.cost_usd >= 3.0, "spend reached the cost ceiling");
    assert!(run.spent.turns < 100, "the turns axis did not trip first");
    // The recorded pause names the axis.
    let axes: Vec<String> = store
        .list_events("s-budget")
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "budget_exhausted")
        .filter_map(|e| {
            e.payload
                .get("axis")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .collect();
    assert_eq!(axes, vec!["cost".to_string()]);
}

// ── (c) full lifecycle start → run → complete ────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swarm_full_lifecycle_start_run_complete() {
    let h = Harness::new();
    let store: Arc<dyn SwarmStore> = Arc::new(InMemorySwarmStore::new());
    save_created(
        &store,
        spec_with("s-life", 2, custom(100, 10_000_000, 1000.0)),
    );

    // First planning pass: delegate to box-1, then finish the pass. Second pass
    // makes no delegation, so the goal is met.
    let orch_queue = VecDeque::from(vec![
        Reply::delegate("box-1", "gather findings"),
        Reply::final_text("round one complete"),
        Reply::final_text("goal met"),
    ]);
    let tasks: Arc<dyn TaskRegistry> =
        Arc::new(SqliteTaskStore::new_in_memory().expect("task store"));
    let factory: Arc<dyn SwarmProviderFactory> = Arc::new(ScriptedFactory {
        orch_queue: Arc::new(StdMutex::new(orch_queue)),
        orch_fallback: Reply::final_text("done"),
        box_reply: Reply::box_result("findings ready", 10, 5),
        box_gate: None,
    });
    let engine = h.engine(
        Arc::clone(&store),
        factory,
        HashMap::new(),
        Some(Arc::clone(&tasks)),
    );

    engine.start("s-life").await.expect("start");
    wait_for_status(&store, "s-life", |s| s.is_terminal(), "completion").await;

    let run = store.load_swarm("s-life").unwrap().unwrap().run;
    assert_eq!(run.status, SwarmStatus::Completed);
    assert!(
        run.spent.turns >= 3,
        "orchestrator + box turns were counted"
    );
    assert!(run.spent.tokens >= 15, "box usage accumulated into spend");
    assert_eq!(
        box_delegated_events(&store, "s-life"),
        vec!["box-1".to_string()]
    );

    // Control-plane: the record is a Swarm-kind task that ended Completed.
    let record = tasks
        .get("s-life")
        .await
        .expect("cp get")
        .expect("cp record");
    assert_eq!(record.kind, TaskKind::Swarm);
    assert_eq!(record.status, TaskStatus::Completed);

    // Reaped on completion: no board survives.
    assert!(
        store.load_swarm("s-life").unwrap().is_some(),
        "the swarm document survives; only the ephemeral run state is reaped"
    );
}

// ── (d) restart recovery ─────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swarm_restart_recovery_parks_and_resume_rehydrates() {
    let h = Harness::new();
    let store: Arc<dyn SwarmStore> = Arc::new(InMemorySwarmStore::new());
    save_created(
        &store,
        spec_with("s-recover", 2, custom(100, 10_000_000, 1000.0)),
    );

    // Simulate a daemon that started the run then died: Running status, a claim
    // whose lease already expired.
    {
        let mut swarm = store.load_swarm("s-recover").unwrap().unwrap();
        swarm.revision += 1;
        swarm.run.status = SwarmStatus::Running;
        swarm
            .run
            .sessions
            .insert("box-1".to_string(), "sess-box-1".to_string());
        swarm
            .run
            .sessions
            .insert("box-2".to_string(), "sess-box-2".to_string());
        store.save_swarm(&swarm).expect("save running");
    }
    // A prior boot claimed it (fresh, unexpired lease). The recovering engine
    // runs under a different boot ("boot-test").
    let _claim = store
        .try_claim_swarm_with_boot("s-recover", "boot-dead")
        .expect("claim")
        .expect("claim wins");

    let factory: Arc<dyn SwarmProviderFactory> = Arc::new(ScriptedFactory {
        orch_queue: Arc::new(StdMutex::new(VecDeque::from(vec![Reply::final_text(
            "nothing to do",
        )]))),
        orch_fallback: Reply::final_text("done"),
        box_reply: Reply::box_result("ok", 1, 1),
        box_gate: None,
    });
    let engine = h.engine(Arc::clone(&store), factory, HashMap::new(), None);

    // The prior boot's lease is still live, but recovery keys on the owning boot
    // id, not the lease age: a claim from a dead process is reclaimed regardless.
    let recovered = engine.recover().await.expect("recover");
    assert_eq!(recovered, vec!["s-recover".to_string()]);
    let run = store.load_swarm("s-recover").unwrap().unwrap().run;
    assert_eq!(
        run.status,
        SwarmStatus::Paused {
            reason: SwarmPauseReason::DaemonRestart
        }
    );

    // Resume: re-claims, rehydrates the recorded sessions, and drives to done.
    engine.resume("s-recover").await.expect("resume");
    wait_for_status(
        &store,
        "s-recover",
        |s| s.is_terminal(),
        "resumed completion",
    )
    .await;
    let run = store.load_swarm("s-recover").unwrap().unwrap().run;
    assert_eq!(run.status, SwarmStatus::Completed);
    assert_eq!(
        run.sessions.get("box-1").map(String::as_str),
        Some("sess-box-1"),
        "resume must rehydrate the recorded box sessions, not mint fresh ones"
    );
    assert_eq!(
        run.sessions.get("box-2").map(String::as_str),
        Some("sess-box-2")
    );
}

// ── (e) pause finishes in-flight then holds; stop reaps ──────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn swarm_pause_finishes_in_flight_then_holds() {
    let h = Harness::new();
    let store: Arc<dyn SwarmStore> = Arc::new(InMemorySwarmStore::new());
    save_created(
        &store,
        spec_with("s-pause", 2, custom(100, 10_000_000, 1000.0)),
    );

    let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
    let gate = Arc::new(TurnGate {
        started: started_tx,
        release: tokio::sync::Semaphore::new(0),
    });
    let factory: Arc<dyn SwarmProviderFactory> = Arc::new(ScriptedFactory {
        orch_queue: Arc::new(StdMutex::new(VecDeque::from(vec![
            Reply::delegate("box-1", "long running subtask"),
            Reply::final_text("round done"),
            Reply::delegate("box-1", "another round"),
            Reply::final_text("round two done"),
        ]))),
        orch_fallback: Reply::final_text("done"),
        box_reply: Reply::box_result("box done", 1, 1),
        box_gate: Some(Arc::clone(&gate)),
    });
    let tasks: Arc<dyn TaskRegistry> =
        Arc::new(SqliteTaskStore::new_in_memory().expect("task store"));
    let engine = h.engine(
        Arc::clone(&store),
        factory,
        HashMap::new(),
        Some(Arc::clone(&tasks)),
    );

    engine.start("s-pause").await.expect("start");
    // Wait until the first box turn is genuinely in-flight.
    started_rx.recv().await.expect("box turn started");

    // Ask to pause while the box turn is blocked.
    let e2 = Arc::clone(&engine);
    let pause_task = zeroclaw_spawn::spawn!(async move { e2.pause("s-pause").await });

    // Let the in-flight box turn (and the round it belongs to) finish.
    gate.release.add_permits(64);
    pause_task.await.expect("join").expect("pause");

    let run = store.load_swarm("s-pause").unwrap().unwrap().run;
    assert_eq!(
        run.status,
        SwarmStatus::Paused {
            reason: SwarmPauseReason::UserRequested
        },
        "pause must hold at UserRequested after finishing the in-flight round"
    );
    // The in-flight delegation completed before the hold.
    assert!(
        !box_delegated_events(&store, "s-pause").is_empty(),
        "the in-flight box turn must finish before the run holds"
    );
    // The control-plane record is the first real consumer of the non-terminal
    // Paused status: the supervising Swarm task parks, it does not terminate.
    let record = tasks
        .get("s-pause")
        .await
        .expect("cp get")
        .expect("cp record");
    assert_eq!(record.kind, TaskKind::Swarm);
    assert_eq!(record.status, TaskStatus::Paused);
    assert!(
        !record.status.is_terminal(),
        "a paused swarm is not terminal"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn swarm_stop_cancels_and_reaps_cleanly() {
    let h = Harness::new();
    let store: Arc<dyn SwarmStore> = Arc::new(InMemorySwarmStore::new());
    save_created(
        &store,
        spec_with("s-stop", 2, custom(100, 10_000_000, 1000.0)),
    );

    let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
    let gate = Arc::new(TurnGate {
        started: started_tx,
        release: tokio::sync::Semaphore::new(0),
    });
    let board = SwarmStateBoard::new();
    let factory: Arc<dyn SwarmProviderFactory> = Arc::new(ScriptedFactory {
        orch_queue: Arc::new(StdMutex::new(VecDeque::from(vec![
            Reply::delegate("box-1", "long running subtask"),
            Reply::final_text("round done"),
        ]))),
        orch_fallback: Reply::delegate("box-1", "keep going"),
        box_reply: Reply::box_result("box done", 1, 1),
        box_gate: Some(Arc::clone(&gate)),
    });
    let engine = Arc::new(SwarmEngine::new(
        Arc::clone(&store),
        board.clone(),
        Arc::clone(&h.memory),
        Arc::clone(&h.config),
        "default",
        Arc::clone(&h.policy),
        factory,
        Arc::new(HashMap::new()),
        h.workspace.clone(),
        None,
        "boot-test",
    ));

    engine.start("s-stop").await.expect("start");
    started_rx.recv().await.expect("box turn started");

    let e2 = Arc::clone(&engine);
    let stop_task = zeroclaw_spawn::spawn!(async move { e2.stop("s-stop").await });
    // Release the blocked box turn so the cancelled turn can unwind promptly.
    gate.release.add_permits(64);
    let report = stop_task.await.expect("join").expect("stop");

    assert!(
        report.is_clean(),
        "stop must reap cleanly: {:?}",
        report.warnings
    );
    assert!(board.is_empty(), "the board must not survive a stop");
    assert_eq!(
        store.load_swarm("s-stop").unwrap().unwrap().run.status,
        SwarmStatus::Stopped
    );
}

// ── (f) atomic budget admission under concurrency ────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn swarm_concurrent_box_turns_admit_at_most_the_turn_budget() {
    let h = Harness::new();
    let store: Arc<dyn SwarmStore> = Arc::new(InMemorySwarmStore::new());
    // Two turns permitted; tokens/cost unbounded so only the turns axis can trip.
    save_created(
        &store,
        spec_with("s-atomic", 2, custom(2, u64::MAX, f64::MAX)),
    );

    // A gate holds each admitted box turn open in the provider call, maximizing
    // the overlap of concurrent delegations: under the old read-then-write
    // admission every racer would read the same stale spend and over-admit.
    let (started_tx, _started_rx) = tokio::sync::mpsc::unbounded_channel();
    let gate = Arc::new(TurnGate {
        started: started_tx,
        release: tokio::sync::Semaphore::new(0),
    });
    let factory: Arc<dyn SwarmProviderFactory> = Arc::new(ScriptedFactory {
        orch_queue: Arc::new(StdMutex::new(VecDeque::new())),
        orch_fallback: Reply::final_text("done"),
        box_reply: Reply::box_result("ok", 0, 0),
        box_gate: Some(Arc::clone(&gate)),
    });
    let engine = h.engine(Arc::clone(&store), factory, HashMap::new(), None);
    engine.begin_run("s-atomic").await.expect("begin");

    // Four delegations race the two-turn budget at once.
    let mut handles = Vec::new();
    for _ in 0..4 {
        let e = Arc::clone(&engine);
        handles.push(zeroclaw_spawn::spawn!(async move {
            e.run_box_turn("s-atomic", "box-1", "work").await
        }));
    }
    // Let every admitted (blocked) box turn finish.
    gate.release.add_permits(64);

    let mut admitted = 0;
    let mut denied = 0;
    for handle in handles {
        match handle.await.expect("join") {
            Ok(_) => admitted += 1,
            // Two legitimate denials for an over-budget racer: it either loses the
            // atomic admission race (`BudgetExhausted`), or it arrives after the
            // racer that tripped the ceiling already parked the run and tore it
            // out of the live-run map (`NotRunning`). Which one is scheduling-
            // dependent; both mean "refused before running, no budget spent".
            Err(BoxTurnError::BudgetExhausted(BudgetAxis::Turns))
            | Err(BoxTurnError::NotRunning(_)) => denied += 1,
            Err(other) => panic!("unexpected box turn error: {other}"),
        }
    }
    assert_eq!(
        admitted, 2,
        "atomic admission admits exactly the turn budget regardless of race order"
    );
    assert_eq!(
        denied, 2,
        "over-budget delegations are refused before running"
    );

    let run = store.load_swarm("s-atomic").unwrap().unwrap().run;
    assert_eq!(
        run.spent.turns, 2,
        "atomic admission never lets recorded spend exceed the ceiling"
    );
    assert_eq!(
        run.status,
        SwarmStatus::Paused {
            reason: SwarmPauseReason::BudgetExhausted
        }
    );
}

// ── (g) claim lifecycle: driver-less pause + prior-boot recovery ──────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swarm_pause_on_begin_run_releases_claim_and_resume_succeeds() {
    let h = Harness::new();
    let store: Arc<dyn SwarmStore> = Arc::new(InMemorySwarmStore::new());
    save_created(
        &store,
        spec_with("s-begin", 2, custom(100, 10_000_000, 1000.0)),
    );

    let factory: Arc<dyn SwarmProviderFactory> = Arc::new(ScriptedFactory {
        orch_queue: Arc::new(StdMutex::new(VecDeque::new())),
        orch_fallback: Reply::final_text("nothing to do"),
        box_reply: Reply::box_result("ok", 1, 1),
        box_gate: None,
    });
    let engine = h.engine(Arc::clone(&store), factory, HashMap::new(), None);

    // A driver-less run (begin_run holds the claim + heartbeat but starts no
    // orchestrator loop).
    engine.begin_run("s-begin").await.expect("begin");
    engine.pause("s-begin").await.expect("pause");

    assert_eq!(
        store.load_swarm("s-begin").unwrap().unwrap().run.status,
        SwarmStatus::Paused {
            reason: SwarmPauseReason::UserRequested
        },
        "a driver-less pause must park the run"
    );

    // The pause released the claim + cancelled the heartbeat, so a resume can
    // re-claim it (before the fix the heartbeat kept the lease alive and this
    // failed AlreadyClaimed).
    engine
        .resume("s-begin")
        .await
        .expect("resume must succeed after a begin_run pause released the claim");
    wait_for_status(&store, "s-begin", |s| s.is_terminal(), "resumed completion").await;
    assert_eq!(
        store.load_swarm("s-begin").unwrap().unwrap().run.status,
        SwarmStatus::Completed
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swarm_recover_reclaims_prior_boot_claim_even_when_paused_with_live_lease() {
    let h = Harness::new();
    let store: Arc<dyn SwarmStore> = Arc::new(InMemorySwarmStore::new());
    save_created(
        &store,
        spec_with("s-wedge", 2, custom(100, 10_000_000, 1000.0)),
    );

    // A prior boot budget-parked the swarm (non-Running) then crashed, leaking
    // its claim with a still-live lease.
    {
        let mut swarm = store.load_swarm("s-wedge").unwrap().unwrap();
        swarm.revision += 1;
        swarm.run.status = SwarmStatus::Paused {
            reason: SwarmPauseReason::BudgetExhausted,
        };
        store.save_swarm(&swarm).expect("save paused");
    }
    let _dead = store
        .try_claim_swarm_with_boot("s-wedge", "boot-dead")
        .expect("claim")
        .expect("claim wins");

    let factory: Arc<dyn SwarmProviderFactory> = Arc::new(ScriptedFactory {
        orch_queue: Arc::new(StdMutex::new(VecDeque::new())),
        orch_fallback: Reply::final_text("done"),
        box_reply: Reply::box_result("ok", 1, 1),
        box_gate: None,
    });
    let engine = h.engine(Arc::clone(&store), factory, HashMap::new(), None);

    // recover runs at real "now" (the dead lease is unexpired) and against a
    // Paused (non-Running) swarm — the two cases old recovery skipped.
    let recovered = engine.recover().await.expect("recover");
    assert!(
        recovered.is_empty(),
        "an already-paused swarm is not re-parked, only its dead claim is freed"
    );
    assert_eq!(
        store.load_swarm("s-wedge").unwrap().unwrap().run.status,
        SwarmStatus::Paused {
            reason: SwarmPauseReason::BudgetExhausted
        },
        "recovery must not touch a non-Running swarm's status (never auto-resume)"
    );

    // The dead claim was freed regardless of lease age or run status: the swarm
    // is reclaimable again.
    assert!(
        store
            .try_claim_swarm_with_boot("s-wedge", "boot-test")
            .expect("claim")
            .is_some(),
        "recover must free a prior-boot claim so the swarm can be resumed"
    );
}

// ── (h) interjection: a user-engaged box holds orchestrator delegation ─

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swarm_user_engaged_box_holds_delegation_until_handback() {
    let h = Harness::new();
    let store: Arc<dyn SwarmStore> = Arc::new(InMemorySwarmStore::new());
    save_created(
        &store,
        spec_with("s-engaged", 2, custom(100, 10_000_000, 1000.0)),
    );

    let factory: Arc<dyn SwarmProviderFactory> = Arc::new(ScriptedFactory {
        orch_queue: Arc::new(StdMutex::new(VecDeque::new())),
        orch_fallback: Reply::final_text("done"),
        box_reply: Reply::box_result("box ok", 1, 1),
        box_gate: None,
    });
    let engine = h.engine(Arc::clone(&store), factory, HashMap::new(), None);

    // A driver-less run so the direct `run_box_turn` primitive stands in for the
    // orchestrator's delegation admission.
    engine.begin_run("s-engaged").await.expect("begin");

    // Baseline: an unengaged box is delegable.
    engine
        .run_box_turn("s-engaged", "box-1", "warm up")
        .await
        .expect("an unengaged box is admitted");
    assert!(!engine.is_box_user_engaged("s-engaged", "box-1"));

    // A user interjects: the box is held; orchestrator delegation is refused
    // before any turn runs.
    assert!(
        engine.engage_box_for_user("s-engaged", "box-1"),
        "engaging a roster box must succeed"
    );
    assert!(engine.is_box_user_engaged("s-engaged", "box-1"));
    let held = engine
        .run_box_turn("s-engaged", "box-1", "orchestrator work")
        .await
        .expect_err("a user-engaged box must refuse orchestrator delegation");
    assert!(
        matches!(held, BoxTurnError::UserEngaged(ref b) if b == "box-1"),
        "expected UserEngaged, got {held:?}"
    );

    // Another box on the roster is unaffected — the hold is per box.
    engine
        .run_box_turn("s-engaged", "box-2", "sibling work")
        .await
        .expect("a different box is still delegable while box-1 is engaged");

    // Handback: the orchestrator regains the box.
    assert!(
        engine.release_box_for_user("s-engaged", "box-1"),
        "releasing a held box reports it was held"
    );
    assert!(!engine.is_box_user_engaged("s-engaged", "box-1"));
    engine
        .run_box_turn("s-engaged", "box-1", "orchestrator work again")
        .await
        .expect("a released box is delegable again");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swarm_engage_and_release_are_scoped_to_a_live_run() {
    let h = Harness::new();
    let store: Arc<dyn SwarmStore> = Arc::new(InMemorySwarmStore::new());
    save_created(
        &store,
        spec_with("s-norun", 2, custom(100, 10_000_000, 1000.0)),
    );

    let factory: Arc<dyn SwarmProviderFactory> = Arc::new(ScriptedFactory {
        orch_queue: Arc::new(StdMutex::new(VecDeque::new())),
        orch_fallback: Reply::final_text("done"),
        box_reply: Reply::box_result("box ok", 1, 1),
        box_gate: None,
    });
    let engine = h.engine(Arc::clone(&store), factory, HashMap::new(), None);

    // No live run: engaging/reading a box is a no-op that reports "not held".
    assert!(!engine.engage_box_for_user("s-norun", "box-1"));
    assert!(!engine.is_box_user_engaged("s-norun", "box-1"));
    assert!(!engine.release_box_for_user("s-norun", "box-1"));

    // A box outside the roster is never engaged, even on a live run.
    engine.begin_run("s-norun").await.expect("begin");
    assert!(!engine.engage_box_for_user("s-norun", "ghost"));
    assert!(!engine.is_box_user_engaged("s-norun", "ghost"));
}
