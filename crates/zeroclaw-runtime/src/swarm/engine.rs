//! The swarm orchestrator engine — the per-run driver.
//!
//! One [`SwarmEngine`] supervises many swarms; each live swarm gets a single
//! driver task that holds the store claim (single-driver-per-swarm), renders
//! the goal and the state board into an orchestrator planning turn, lets that
//! orchestrator delegate focused subtasks to the roster's boxes through
//! [`SwarmDelegateTool`], and feeds the results back. Every turn — the
//! orchestrator's own planning rounds and each box delegation — is admitted
//! against the swarm's budget first: exhausting any of the three axes (turns,
//! tokens, cost) parks the run at [`SwarmStatus::Paused`] with
//! [`SwarmPauseReason::BudgetExhausted`] rather than hard-failing.
//!
//! The engine is "dark" here: nothing wires it to RPC yet. It exposes the
//! lifecycle primitives (`start` / `pause` / `resume` / `stop` / `recover`) as
//! plain engine methods; S7 drives them over the local RPC surface.
//!
//! Boxes run under [`TurnOrigin::Swarm`], which every origin gate treats as
//! restrictively as a sub-turn: a box turn is a bare tool-loop turn (no
//! operator origin, no memory-context injection) whose tool set is scoped
//! through [`ScopedToolRegistry::assemble`] with the re-entrant agent-spawning
//! tools removed and the `swarm_state` board tool added — a box can neither
//! spawn nor delegate, only coordinate.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::json;
use tokio::sync::Mutex as TokioMutex;
use tokio_util::sync::CancellationToken;

use zeroclaw_api::ingress::TurnOrigin;
use zeroclaw_api::model_provider::ModelProvider;
use zeroclaw_config::policy::SecurityPolicy;
use zeroclaw_config::schema::Config;
use zeroclaw_memory::{Memory, NoneMemory};

use crate::agent::agent::{Agent, TurnEvent};
use crate::agent::cost::{ModelProviderPricing, ToolLoopCostTrackingContext, TurnUsage};
use crate::agent::dispatcher::NativeToolDispatcher;
use crate::control_plane::{TaskKind, TaskRecord, TaskRegistry, TaskStatus};
use crate::observability::{NoopObserver, Observer};
use crate::platform::RuntimeAdapter;
use crate::rpc::turn::{TurnOutcome, execute_turn};
use crate::swarm::board::{BoxStatus, SwarmStateBoard};
use crate::swarm::delegate_tool::SwarmDelegateTool;
use crate::swarm::reap::{ReapReport, reap_swarm};
use crate::swarm::roster::{RosterBox, SwarmRoster, build_swarm_roster};
use crate::swarm::state_tool::SwarmStateTool;
use crate::swarm::store::model::{
    PersistedSwarm, SwarmBudgetLimits, SwarmClaimToken, SwarmEventRecord, SwarmPauseReason,
    SwarmRun, SwarmSpec, SwarmSpend, SwarmStatus,
};
use crate::swarm::store::{StoreError, SwarmStore};
use crate::tools::scoped::{ScopedAssembly, ScopedToolRegistry};
use crate::tools::{REENTRANT_AGENT_TOOLS, Tool, all_tools_with_runtime};

/// How often the driver renews its store claim and beats its control-plane
/// record. Well under the store's default claim lease so a live driver never
/// loses its claim to the reaper.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Hard ceiling on orchestrator planning rounds. The budget stops a run long
/// before this in practice; the bound only guarantees the loop can never spin
/// forever if an orchestrator keeps delegating without ever finishing.
const MAX_ROUNDS: usize = 10_000;

/// The turn budget's three axes. A run parks when spend reaches any one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetAxis {
    Turns,
    Tokens,
    Cost,
}

impl BudgetAxis {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Turns => "turns",
            Self::Tokens => "tokens",
            Self::Cost => "cost",
        }
    }
}

/// Result of admitting one turn against the budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BudgetAdmission {
    Admitted,
    Exhausted(BudgetAxis),
}

/// Why a box delegation could not produce a result.
#[derive(Debug)]
pub enum BoxTurnError {
    /// The budget was exhausted before the turn could be admitted; no turn ran.
    BudgetExhausted(BudgetAxis),
    /// The named box is not on this swarm's roster.
    UnknownBox(String),
    /// The box turn itself failed (build or agent error).
    Turn(String),
}

impl std::fmt::Display for BoxTurnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BudgetExhausted(axis) => {
                write!(f, "swarm budget exhausted on the {} axis", axis.as_str())
            }
            Self::UnknownBox(box_id) => write!(f, "box {box_id:?} is not on this swarm's roster"),
            Self::Turn(msg) => write!(f, "box turn failed: {msg}"),
        }
    }
}

impl std::error::Error for BoxTurnError {}

/// Why an engine lifecycle call was refused.
#[derive(Debug, thiserror::Error)]
pub enum SwarmEngineError {
    #[error("no swarm found for id {0:?}")]
    NotFound(String),
    #[error("swarm {0:?} is terminal and cannot be driven")]
    Terminal(String),
    #[error("swarm {0:?} is already claimed by another driver")]
    AlreadyClaimed(String),
    #[error("swarm {swarm_id:?} is not running")]
    NotRunning { swarm_id: String },
    #[error("swarm store error: {0}")]
    Store(#[from] StoreError),
    #[error("roster build failed: {0}")]
    Roster(String),
    #[error("board registration failed: {0}")]
    Board(String),
}

/// Constructs the model provider a swarm's turns run on. The seam that lets a
/// scripted provider stand in for the routed one in tests while production
/// resolves the real provider from config.
pub trait SwarmProviderFactory: Send + Sync {
    /// Build a provider for `provider` (a `<type>.<alias>` ref) running `model`,
    /// on behalf of `agent_alias` (for option resolution / attribution).
    fn create(
        &self,
        provider: &str,
        model: &str,
        agent_alias: &str,
    ) -> anyhow::Result<Box<dyn ModelProvider>>;
}

/// Production factory: resolves the routed provider from the loaded config, the
/// same path `agent::run` takes.
pub struct ConfigSwarmProviderFactory {
    config: Arc<Config>,
}

impl ConfigSwarmProviderFactory {
    #[must_use]
    pub fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

impl SwarmProviderFactory for ConfigSwarmProviderFactory {
    fn create(
        &self,
        provider: &str,
        model: &str,
        agent_alias: &str,
    ) -> anyhow::Result<Box<dyn ModelProvider>> {
        let (api_key, uri) = match provider.split_once('.') {
            Some((fam, al)) => self
                .config
                .providers
                .models
                .find(fam, al)
                .map_or((None, None), |entry| {
                    (entry.api_key.clone(), entry.uri.clone())
                }),
            None => (None, None),
        };
        let base =
            zeroclaw_providers::provider_runtime_options_for_agent(&self.config, agent_alias);
        let options = zeroclaw_providers::options_for_provider_ref(&self.config, provider, &base);
        zeroclaw_providers::create_routed_model_provider_with_options(
            &self.config,
            provider,
            api_key.as_deref(),
            uri.as_deref(),
            &self.config.reliability,
            &self.config.model_routes,
            model,
            &options,
        )
    }
}

/// A live swarm run entry in the engine's registry. The hard-stop token and the
/// pause flag are reached through `state` and this `pause` handle respectively.
struct LiveRun {
    state: Arc<SwarmRunState>,
    driver: Option<tokio::task::JoinHandle<DriverExit>>,
    pause: Arc<AtomicBool>,
}

/// How a driver loop finished, deciding the terminal handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriverExit {
    /// The orchestrator finished the goal.
    Completed,
    /// A turn could not be admitted — the run is parked at
    /// `Paused{BudgetExhausted}`.
    BudgetExhausted,
    /// An operator pause was honored at a round boundary.
    Paused,
    /// A hard stop cancelled the run; `stop` finalizes the reap + terminal state.
    Cancelled,
}

/// Everything one live swarm run needs, shared between the driver loop and the
/// [`SwarmDelegateTool`] instances the orchestrator holds. The live
/// [`PersistedSwarm`] is the authoritative copy; every persisted transition
/// goes through [`Self::persist_run`] (bump revision, stamp `updated_at`, save).
pub struct SwarmRunState {
    swarm_id: String,
    spec: SwarmSpec,
    claim: SwarmClaimToken,
    store: Arc<dyn SwarmStore>,
    board: SwarmStateBoard,
    roster: SwarmRoster,
    memories: BTreeMap<String, zeroclaw_memory::SwarmBoxMemory>,
    limits: SwarmBudgetLimits,
    provider: Arc<dyn SwarmProviderFactory>,
    pricing: Arc<ModelProviderPricing>,
    config: Arc<Config>,
    parent_alias: String,
    workspace_dir: PathBuf,
    tasks: Option<Arc<dyn TaskRegistry>>,
    boot_id: String,
    cp_task_id: Option<String>,
    /// Authoritative live document; the CAS guard is the revision it carries.
    current: Mutex<PersistedSwarm>,
    /// Hard-stop signal, threaded into every turn so `stop` interrupts in-flight
    /// work.
    cancel: CancellationToken,
    /// Stops the heartbeat task; cancelled by the driver on any exit.
    heartbeat_cancel: CancellationToken,
    /// Graceful operator pause: honored at the next round boundary.
    pause: Arc<AtomicBool>,
    /// Monotonic count of admitted delegations, for round completion detection.
    delegations: AtomicU64,
    /// Install-wide (admin) memory handle for the reap cascade on natural
    /// completion. A box's scoped handle refuses the cross-agent deletes reaping
    /// performs, so the driver keeps the admin handle here.
    reap_memory: Arc<dyn Memory>,
}

impl SwarmRunState {
    /// The validated roster the orchestrator may delegate to.
    #[must_use]
    pub fn roster(&self) -> &SwarmRoster {
        &self.roster
    }

    /// The swarm this run drives.
    #[must_use]
    pub fn swarm_id(&self) -> &str {
        &self.swarm_id
    }

    /// The concrete budget ceilings this run stops at.
    #[must_use]
    pub fn limits(&self) -> SwarmBudgetLimits {
        self.limits
    }

    fn append_event(&self, kind: &str, box_id: Option<String>, payload: serde_json::Value) {
        let record = SwarmEventRecord {
            swarm_id: self.swarm_id.clone(),
            seq: 0,
            ts: chrono::Utc::now().to_rfc3339(),
            kind: kind.to_string(),
            box_id,
            payload,
        };
        if let Err(err) = self.store.append_event(&record) {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(
                        json!({"swarm_id": self.swarm_id, "kind": kind, "error": err.to_string()})
                    ),
                "swarm: failed to append event"
            );
        }
    }

    /// Persist a run-state mutation under the store write protocol. Held across
    /// the (synchronous) save so concurrent delegations serialize and no write
    /// loses the revision race with itself.
    fn persist_run<F: FnOnce(&mut SwarmRun)>(&self, mutate: F) -> Result<(), StoreError> {
        let mut cur = self.current.lock();
        mutate(&mut cur.run);
        cur.revision += 1;
        cur.updated_at = chrono::Utc::now().to_rfc3339();
        self.store.save_swarm(&cur)
    }

    fn status(&self) -> SwarmStatus {
        self.current.lock().run.status
    }

    fn spent(&self) -> SwarmSpend {
        self.current.lock().run.spent
    }

    /// Check spend against every axis. On exhaustion, park the run at
    /// `Paused{BudgetExhausted}` (idempotent) and report which axis tripped.
    fn admit(&self) -> BudgetAdmission {
        let spent = self.spent();
        let limits = self.limits;
        let axis = if spent.turns >= limits.max_turns {
            Some(BudgetAxis::Turns)
        } else if spent.tokens >= limits.max_tokens {
            Some(BudgetAxis::Tokens)
        } else if spent.cost_usd >= limits.max_cost_usd {
            Some(BudgetAxis::Cost)
        } else {
            None
        };
        match axis {
            None => BudgetAdmission::Admitted,
            Some(axis) => {
                self.park_for_budget(axis);
                BudgetAdmission::Exhausted(axis)
            }
        }
    }

    fn park_for_budget(&self, axis: BudgetAxis) {
        {
            let cur = self.current.lock();
            if cur.run.status.is_terminal() || matches!(cur.run.status, SwarmStatus::Paused { .. })
            {
                return;
            }
        }
        if let Err(err) = self.persist_run(|run| {
            run.status = SwarmStatus::Paused {
                reason: SwarmPauseReason::BudgetExhausted,
            };
        }) {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(json!({"swarm_id": self.swarm_id, "error": err.to_string()})),
                "swarm: failed to persist budget pause"
            );
        }
        self.append_event("budget_exhausted", None, json!({"axis": axis.as_str()}));
    }

    fn record_spend(&self, delta: SwarmSpend) {
        if let Err(err) = self.persist_run(|run| {
            run.spent.turns = run.spent.turns.saturating_add(delta.turns);
            run.spent.tokens = run.spent.tokens.saturating_add(delta.tokens);
            run.spent.cost_usd += delta.cost_usd;
        }) {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(json!({"swarm_id": self.swarm_id, "error": err.to_string()})),
                "swarm: failed to persist spend"
            );
        }
    }

    /// Run one turn for `agent` with `prompt`, measuring tokens + cost. Always
    /// reports a spend of at least one turn, even when the turn errors: a failed
    /// turn still consumed provider calls.
    async fn run_turn_with_prompt(
        &self,
        agent_alias: &str,
        prompt: String,
        agent: Agent,
    ) -> (SwarmSpend, Result<String, String>) {
        let usage = Arc::new(Mutex::new(TurnUsage::default()));
        let cost_ctx = ToolLoopCostTrackingContext {
            tracker: None,
            model_provider_pricing: Arc::clone(&self.pricing),
            turn_usage: Arc::clone(&usage),
            agent_alias: Some(agent_alias.to_string()),
        };
        let attribution = crate::rpc::turn::TurnAttribution {
            session_key: None,
            agent_alias: agent_alias.to_string(),
            model_provider: self.spec.provider.clone(),
            model: self.spec.model.clone(),
            channel: "swarm",
        };
        let outcome = execute_turn(
            Arc::new(TokioMutex::new(agent)),
            prompt,
            self.cancel.clone(),
            attribution,
            Some(cost_ctx),
            None,
            ignore_event,
        )
        .await;
        let observed = *usage.lock();
        let spend = SwarmSpend {
            turns: 1,
            tokens: observed.input_tokens.saturating_add(observed.output_tokens),
            cost_usd: observed.cost_usd,
        };
        let result = match outcome {
            Ok(TurnOutcome::Completed { text, .. }) => Ok(text),
            Ok(TurnOutcome::Cancelled { partial_text, .. }) => Ok(partial_text),
            Err(err) => Err(err.to_string()),
        };
        (spend, result)
    }

    /// Delegate a subtask to one box: admit against the budget, run the box's
    /// agent turn under [`TurnOrigin::Swarm`], accumulate its spend, and return
    /// the box's response. Called by [`SwarmDelegateTool`] and by the engine's
    /// `run_box_turn` primitive.
    pub(crate) async fn run_box_turn(
        &self,
        box_id: &str,
        subtask: &str,
    ) -> Result<String, BoxTurnError> {
        let entry = self
            .roster
            .get(box_id)
            .ok_or_else(|| BoxTurnError::UnknownBox(box_id.to_string()))?
            .clone();

        match self.admit() {
            BudgetAdmission::Exhausted(axis) => return Err(BoxTurnError::BudgetExhausted(axis)),
            BudgetAdmission::Admitted => {}
        }
        self.delegations.fetch_add(1, Ordering::SeqCst);

        let _ = self.board.publish(
            &self.swarm_id,
            box_id,
            BoxStatus::Working,
            &truncate(subtask, 200),
        );

        let agent = self
            .build_box_agent(&entry)
            .await
            .map_err(BoxTurnError::Turn)?;
        let (spend, result) = self
            .run_turn_with_prompt(&entry.alias, subtask.to_string(), agent)
            .await;
        self.record_spend(spend);
        // A box turn is a bare tool-loop turn with the swarm origin: every
        // origin gate treats `TurnOrigin::Swarm` exactly as restrictively as a
        // sub-turn (never operator-driven, no memory-context injection), which
        // `execute_turn` already provides by construction. Recorded here so the
        // audit trail names the origin the turn ran under.
        self.append_event(
            "box_delegated",
            Some(box_id.to_string()),
            json!({
                "tokens": spend.tokens,
                "cost_usd": spend.cost_usd,
                "origin": TurnOrigin::Swarm,
            }),
        );
        result.map_err(BoxTurnError::Turn)
    }

    /// Assemble the box's scoped tool set: the standard built-ins through the
    /// one gated seam, minus the re-entrant agent-spawning tools, plus the
    /// `swarm_state` board tool. A box can coordinate but cannot spawn or
    /// delegate.
    async fn assemble_box_tools(&self, entry: &RosterBox) -> Vec<Box<dyn Tool>> {
        let risk_profile = self
            .config
            .risk_profile_for_agent(&self.parent_alias)
            .cloned()
            .unwrap_or_default();
        let runtime: Arc<dyn RuntimeAdapter> = Arc::new(crate::platform::NativeRuntime::new());
        let security = &entry.context.policy;
        let mem_handle: Arc<dyn Memory> = self.memories.get(&entry.box_id).map_or_else(
            || Arc::new(NoneMemory::new("none")) as Arc<dyn Memory>,
            |m| m.handle(),
        );

        let built = all_tools_with_runtime(
            Arc::clone(&self.config),
            security,
            &risk_profile,
            &entry.alias,
            Arc::clone(&runtime),
            mem_handle,
            None,
            None,
            &self.config.browser,
            &self.config.http_request,
            &self.config.web_fetch,
            &self.workspace_dir,
            &self.config.agents,
            None,
            self.config.as_ref(),
            None,
            true,
            None,
            None,
            None,
            None,
        );

        let assembled = ScopedToolRegistry::assemble(ScopedAssembly {
            config: &self.config,
            agent_alias: &entry.alias,
            security,
            built,
            skills: &[],
            runtime,
            caller_allowed: None,
            connect_mcp: false,
            connect_peripherals: false,
            exclude_memory: false,
            acp_delivery: false,
            list_deferred_mcp_specs: false,
            emit_assembly_logs: false,
            mcp_registry: None,
        })
        .await;

        let mut tools = assembled.registry.into_inner();
        // A box never gets the re-entrant agent-spawning tools, even if the
        // parent envelope grants them: fan-out is the orchestrator's job.
        tools.retain(|tool| !REENTRANT_AGENT_TOOLS.contains(&tool.name()));
        tools.push(Box::new(SwarmStateTool::new(
            self.board.clone(),
            self.swarm_id.clone(),
            entry.box_id.clone(),
        )));
        tools
    }

    async fn build_box_agent(&self, entry: &RosterBox) -> Result<Agent, String> {
        let provider = self
            .provider
            .create(&self.spec.provider, &self.spec.model, &entry.alias)
            .map_err(|err| format!("provider build failed: {err:#}"))?;
        let tools = self.assemble_box_tools(entry).await;
        let memory = self
            .memories
            .get(&entry.box_id)
            .map(zeroclaw_memory::SwarmBoxMemory::handle)
            .ok_or_else(|| format!("no composed memory for box {}", entry.box_id))?;
        let session_id = self.current.lock().run.sessions.get(&entry.box_id).cloned();

        Agent::builder()
            .model_provider(provider)
            .tools(tools)
            .memory(memory)
            .observer(Arc::new(NoopObserver {}) as Arc<dyn Observer>)
            .tool_dispatcher(Box::new(NativeToolDispatcher))
            .workspace_dir(self.workspace_dir.clone())
            .model_name(self.spec.model.clone())
            .model_provider_name(self.spec.provider.clone())
            .agent_alias(entry.alias.clone())
            .memory_session_id(session_id)
            .build()
            .map_err(|err| format!("box agent build failed: {err:#}"))
    }

    async fn build_orchestrator_agent(self: &Arc<Self>) -> Result<Agent, String> {
        let alias = format!("swarm/{}/orchestrator", self.swarm_id);
        let provider = self
            .provider
            .create(&self.spec.provider, &self.spec.model, &alias)
            .map_err(|err| format!("provider build failed: {err:#}"))?;
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(SwarmDelegateTool::new(Arc::clone(self)))];
        Agent::builder()
            .model_provider(provider)
            .tools(tools)
            .observer(Arc::new(NoopObserver {}) as Arc<dyn Observer>)
            .tool_dispatcher(Box::new(NativeToolDispatcher))
            .workspace_dir(self.workspace_dir.clone())
            .model_name(self.spec.model.clone())
            .model_provider_name(self.spec.provider.clone())
            .agent_alias(alias)
            .exclude_memory(true)
            .build()
            .map_err(|err| format!("orchestrator agent build failed: {err:#}"))
    }

    fn render_orchestrator_prompt(&self, round: usize) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "You are the orchestrator of swarm {:?} (role: {}).\n",
            self.spec.name, self.spec.role
        ));
        out.push_str(&format!("Goal: {}\n\n", self.spec.goal));
        out.push_str("Roster (delegate to these boxes only):\n");
        for entry in self.roster.boxes() {
            out.push_str(&format!(
                "- {} — role: {}, job: {}\n",
                entry.box_id,
                if entry.role.is_empty() {
                    "(unset)"
                } else {
                    &entry.role
                },
                if entry.job.is_empty() {
                    "(unset)"
                } else {
                    &entry.job
                },
            ));
        }
        out.push('\n');
        if let Some(snapshot) = self.board.snapshot(&self.swarm_id) {
            out.push_str("State board:\n");
            for (box_id, state) in &snapshot.boxes {
                let claim = state
                    .claim
                    .as_deref()
                    .map_or_else(String::new, |key| format!(" holding {key:?}"));
                out.push_str(&format!(
                    "- {box_id}: {}{claim}{}\n",
                    state.status.as_str(),
                    if state.note.is_empty() {
                        String::new()
                    } else {
                        format!(" — {}", state.note)
                    }
                ));
            }
            out.push('\n');
        }
        out.push_str(&format!(
            "Planning round {round}. Delegate focused subtasks to boxes with the \
             `swarm_delegate` tool. When the goal is met, reply with a final \
             summary and delegate nothing further.\n"
        ));
        out
    }

    fn spawn_heartbeat(self: &Arc<Self>) {
        let state = Arc::clone(self);
        // Detached: the task lives until `heartbeat_cancel` fires (the driver
        // cancels it on any exit). Dropping the handle does not abort it.
        let _handle = zeroclaw_spawn::spawn!(async move {
            loop {
                tokio::select! {
                    biased;
                    () = state.heartbeat_cancel.cancelled() => break,
                    () = tokio::time::sleep(HEARTBEAT_INTERVAL) => {
                        if state.store.heartbeat_claim(&state.claim).is_err() {
                            // The claim is gone — another driver owns the swarm.
                            // Stop driving.
                            state.cancel.cancel();
                            break;
                        }
                        if let (Some(tasks), Some(id)) = (state.tasks.as_ref(), state.cp_task_id.as_ref()) {
                            let _ = tasks.heartbeat(id, &state.boot_id).await;
                        }
                    }
                }
            }
        });
    }

    async fn cp_update(&self, status: TaskStatus) {
        if let (Some(tasks), Some(id)) = (self.tasks.as_ref(), self.cp_task_id.as_ref()) {
            let _ = tasks.update_status(id, status, None, None).await;
        }
    }

    /// The orchestrator planning loop. Returns how it exited so the driver can
    /// finalize.
    async fn run_loop(self: &Arc<Self>) -> DriverExit {
        for round in 0..MAX_ROUNDS {
            if self.cancel.is_cancelled() {
                return DriverExit::Cancelled;
            }
            if self.pause.load(Ordering::SeqCst) {
                if let Err(err) = self.persist_run(|run| {
                    run.status = SwarmStatus::Paused {
                        reason: SwarmPauseReason::UserRequested,
                    };
                }) {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(
                                json!({"swarm_id": self.swarm_id, "error": err.to_string()})
                            ),
                        "swarm: failed to persist user pause"
                    );
                }
                self.append_event("swarm_paused", None, json!({"reason": "user_requested"}));
                return DriverExit::Paused;
            }
            match self.admit() {
                BudgetAdmission::Exhausted(_) => return DriverExit::BudgetExhausted,
                BudgetAdmission::Admitted => {}
            }

            let before = self.delegations.load(Ordering::SeqCst);
            let agent = match self.build_orchestrator_agent().await {
                Ok(agent) => agent,
                Err(err) => {
                    self.append_event("orchestrator_error", None, json!({"error": err}));
                    return DriverExit::Completed;
                }
            };
            let prompt = self.render_orchestrator_prompt(round);
            let orch_alias = format!("swarm/{}/orchestrator", self.swarm_id);
            let (spend, result) = self.run_turn_with_prompt(&orch_alias, prompt, agent).await;
            self.record_spend(spend);
            if let Err(err) = result {
                self.append_event("orchestrator_error", None, json!({"error": err}));
                return DriverExit::Completed;
            }
            // A box delegation may have exhausted the budget mid-turn.
            if matches!(self.status(), SwarmStatus::Paused { .. }) {
                return DriverExit::BudgetExhausted;
            }
            if self.cancel.is_cancelled() {
                return DriverExit::Cancelled;
            }
            let after = self.delegations.load(Ordering::SeqCst);
            if after == before {
                // The orchestrator delegated nothing: the goal is met (or there
                // is nothing left to do).
                return DriverExit::Completed;
            }
        }
        DriverExit::Completed
    }

    /// Drive one swarm run to a stopping point. Consumes the shared handle.
    async fn drive(self: Arc<Self>) -> DriverExit {
        self.append_event("swarm_started", None, json!({}));
        let exit = self.run_loop().await;
        match exit {
            DriverExit::Completed => {
                if let Err(err) = self.persist_run(|run| run.status = SwarmStatus::Completed) {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(
                                json!({"swarm_id": self.swarm_id, "error": err.to_string()})
                            ),
                        "swarm: failed to persist completion"
                    );
                }
                self.append_event("swarm_completed", None, json!({}));
                self.cp_update(TaskStatus::Completed).await;
                let report = reap_swarm(&self.reap_memory(), &self.board, &self.roster).await;
                self.append_event("swarm_reaped", None, json!({"clean": report.is_clean()}));
                let _ = self.store.release_claim(&self.claim);
            }
            DriverExit::BudgetExhausted | DriverExit::Paused => {
                self.cp_update(TaskStatus::Paused).await;
                let _ = self.store.release_claim(&self.claim);
            }
            DriverExit::Cancelled => {
                // `stop` owns the reap + terminal transition + claim release.
            }
        }
        self.heartbeat_cancel.cancel();
        exit
    }

    /// The admin memory handle reaping walks. Boxes hold scoped handles that
    /// refuse cross-agent deletes; reaping needs the install-wide backend.
    fn reap_memory(&self) -> Arc<dyn Memory> {
        Arc::clone(&self.reap_memory)
    }
}

/// Supervises swarm runs. One engine per daemon; many swarms.
pub struct SwarmEngine {
    store: Arc<dyn SwarmStore>,
    board: SwarmStateBoard,
    /// Install-wide memory backend (admin handle) for roster identity, box
    /// memory composition, and the reap cascade.
    memory: Arc<dyn Memory>,
    config: Arc<Config>,
    parent_alias: String,
    parent_policy: Arc<SecurityPolicy>,
    provider: Arc<dyn SwarmProviderFactory>,
    pricing: Arc<ModelProviderPricing>,
    workspace_dir: PathBuf,
    tasks: Option<Arc<dyn TaskRegistry>>,
    boot_id: String,
    runs: Arc<Mutex<HashMap<String, LiveRun>>>,
}

impl SwarmEngine {
    /// Build an engine. `parent_alias` + `parent_policy` are the daemon's
    /// default agent and its *live* security policy — the envelope every box
    /// inherits.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<dyn SwarmStore>,
        board: SwarmStateBoard,
        memory: Arc<dyn Memory>,
        config: Arc<Config>,
        parent_alias: impl Into<String>,
        parent_policy: Arc<SecurityPolicy>,
        provider: Arc<dyn SwarmProviderFactory>,
        pricing: Arc<ModelProviderPricing>,
        workspace_dir: PathBuf,
        tasks: Option<Arc<dyn TaskRegistry>>,
        boot_id: impl Into<String>,
    ) -> Self {
        Self {
            store,
            board,
            memory,
            config,
            parent_alias: parent_alias.into(),
            parent_policy,
            provider,
            pricing,
            workspace_dir,
            tasks,
            boot_id: boot_id.into(),
            runs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Claim a swarm and bring it to `Running` without starting the orchestrator
    /// driver. Used by `start`/`resume`, and directly by callers that drive box
    /// turns themselves.
    pub async fn begin_run(&self, swarm_id: &str) -> Result<(), SwarmEngineError> {
        let _ = self.build_run(swarm_id).await?;
        Ok(())
    }

    async fn build_run(&self, swarm_id: &str) -> Result<Arc<SwarmRunState>, SwarmEngineError> {
        let persisted = self
            .store
            .load_swarm(swarm_id)?
            .ok_or_else(|| SwarmEngineError::NotFound(swarm_id.to_string()))?;
        if persisted.run.status.is_terminal() {
            return Err(SwarmEngineError::Terminal(swarm_id.to_string()));
        }

        let claim = self
            .store
            .try_claim_swarm(swarm_id)?
            .ok_or_else(|| SwarmEngineError::AlreadyClaimed(swarm_id.to_string()))?;

        let roster = build_swarm_roster(
            &self.config,
            &self.memory,
            &persisted.spec,
            &self.parent_alias,
            Arc::clone(&self.parent_policy),
        )
        .await
        .map_err(|err| {
            let _ = self.store.release_claim(&claim);
            SwarmEngineError::Roster(err.to_string())
        })?;

        let memories = roster
            .compose_memories(&self.memory, &self.config.memory)
            .map_err(|err| {
                let _ = self.store.release_claim(&claim);
                SwarmEngineError::Roster(err.to_string())
            })?;

        // Reuse a live board on resume within the same daemon; register a fresh
        // one otherwise. AlreadyRegistered is not an error here.
        match self.board.register(swarm_id, roster.box_ids()) {
            Ok(()) => {}
            Err(crate::swarm::board::BoardError::AlreadyRegistered { .. }) => {}
            Err(err) => {
                let _ = self.store.release_claim(&claim);
                return Err(SwarmEngineError::Board(err.to_string()));
            }
        }

        let cp_task_id = self.create_cp_record(swarm_id).await;
        let limits = persisted.spec.budget.limits();
        let spec = persisted.spec.clone();

        let state = Arc::new(SwarmRunState {
            swarm_id: swarm_id.to_string(),
            spec,
            claim: claim.clone(),
            store: Arc::clone(&self.store),
            board: self.board.clone(),
            roster,
            memories,
            limits,
            provider: Arc::clone(&self.provider),
            pricing: Arc::clone(&self.pricing),
            config: Arc::clone(&self.config),
            parent_alias: self.parent_alias.clone(),
            workspace_dir: self.workspace_dir.clone(),
            tasks: self.tasks.clone(),
            boot_id: self.boot_id.clone(),
            cp_task_id,
            current: Mutex::new(persisted),
            cancel: CancellationToken::new(),
            heartbeat_cancel: CancellationToken::new(),
            pause: Arc::new(AtomicBool::new(false)),
            delegations: AtomicU64::new(0),
            reap_memory: Arc::clone(&self.memory),
        });

        // Bring the run to Running and mint a session per box (rehydrating any
        // sessions a paused run already recorded).
        let box_ids = state.roster.box_ids();
        if let Err(err) = state.persist_run(|run| {
            run.status = SwarmStatus::Running;
            for box_id in box_ids {
                run.sessions
                    .entry(box_id)
                    .or_insert_with(|| uuid::Uuid::new_v4().to_string());
            }
        }) {
            let _ = self.store.release_claim(&claim);
            return Err(SwarmEngineError::Store(err));
        }

        {
            let mut runs = self.runs.lock();
            runs.insert(
                swarm_id.to_string(),
                LiveRun {
                    state: Arc::clone(&state),
                    driver: None,
                    pause: Arc::clone(&state.pause),
                },
            );
        }
        state.spawn_heartbeat();
        Ok(state)
    }

    async fn create_cp_record(&self, swarm_id: &str) -> Option<String> {
        let tasks = self.tasks.as_ref()?;
        let now = chrono::Utc::now().to_rfc3339();
        let record = TaskRecord {
            id: swarm_id.to_string(),
            kind: TaskKind::Swarm,
            agent: self.parent_alias.clone(),
            status: TaskStatus::Running,
            owner_pid: std::process::id(),
            owner_boot_id: self.boot_id.clone(),
            heartbeat_at: Some(now.clone()),
            depth: 0,
            parent_id: None,
            originator_route: None,
            delivered: false,
            idem_key: None,
            principal_id: None,
            started_at: now,
            finished_at: None,
        };
        if let Err(err) = tasks.create(record).await {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(json!({"swarm_id": swarm_id, "error": err.to_string()})),
                "swarm: failed to create control-plane record"
            );
        }
        Some(swarm_id.to_string())
    }

    /// Claim a swarm, bring it to `Running`, and start its orchestrator driver.
    pub async fn start(&self, swarm_id: &str) -> Result<(), SwarmEngineError> {
        let state = self.build_run(swarm_id).await?;
        self.spawn_driver(swarm_id, state);
        Ok(())
    }

    fn spawn_driver(&self, swarm_id: &str, state: Arc<SwarmRunState>) {
        let runs = Arc::clone(&self.runs);
        let id = swarm_id.to_string();
        let driver = zeroclaw_spawn::spawn!(async move {
            let exit = Arc::clone(&state).drive().await;
            // On a self-exit (completed / budget-parked / user-paused) the run
            // is no longer live: drop its registry entry so the map does not
            // grow with finished runs. `stop` owns removal on the Cancelled path.
            if !matches!(exit, DriverExit::Cancelled) {
                runs.lock().remove(&id);
            }
            exit
        });
        if let Some(live) = self.runs.lock().get_mut(swarm_id) {
            live.driver = Some(driver);
        }
    }

    /// Run one box delegation directly (the primitive `SwarmDelegateTool` and
    /// S7's single-box RPC path share).
    pub async fn run_box_turn(
        &self,
        swarm_id: &str,
        box_id: &str,
        subtask: &str,
    ) -> Result<String, BoxTurnError> {
        let state = {
            let runs = self.runs.lock();
            runs.get(swarm_id).map(|live| Arc::clone(&live.state))
        }
        .ok_or_else(|| BoxTurnError::Turn(format!("swarm {swarm_id:?} is not running")))?;
        state.run_box_turn(box_id, subtask).await
    }

    /// Read a swarm's current lifecycle state from the store.
    pub fn status(&self, swarm_id: &str) -> Result<Option<SwarmStatus>, SwarmEngineError> {
        Ok(self.store.load_swarm(swarm_id)?.map(|s| s.run.status))
    }

    /// Finish any in-flight round, then hold the run at `Paused{UserRequested}`.
    pub async fn pause(&self, swarm_id: &str) -> Result<(), SwarmEngineError> {
        let (pause, driver) = {
            let mut runs = self.runs.lock();
            let live = runs
                .get_mut(swarm_id)
                .ok_or_else(|| SwarmEngineError::NotRunning {
                    swarm_id: swarm_id.to_string(),
                })?;
            (Arc::clone(&live.pause), live.driver.take())
        };
        pause.store(true, Ordering::SeqCst);
        if let Some(driver) = driver {
            let _ = driver.await;
        }
        // The parked run is no longer live; drop its registry entry so a resume
        // rebuilds it. (The driver already released the claim + heartbeat.)
        self.runs.lock().remove(swarm_id);
        Ok(())
    }

    /// Hard-stop a run: cancel in-flight work, reap every trace of the roster,
    /// and mark the swarm `Stopped`.
    pub async fn stop(&self, swarm_id: &str) -> Result<ReapReport, SwarmEngineError> {
        let (state, driver) = {
            let mut runs = self.runs.lock();
            let live = runs
                .get_mut(swarm_id)
                .ok_or_else(|| SwarmEngineError::NotRunning {
                    swarm_id: swarm_id.to_string(),
                })?;
            (Arc::clone(&live.state), live.driver.take())
        };
        state.cancel.cancel();
        if let Some(driver) = driver {
            let _ = driver.await;
        }
        state.heartbeat_cancel.cancel();

        let report = reap_swarm(&self.memory, &self.board, &state.roster).await;
        if let Err(err) = state.persist_run(|run| run.status = SwarmStatus::Stopped) {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(json!({"swarm_id": swarm_id, "error": err.to_string()})),
                "swarm: failed to persist stop"
            );
        }
        state.append_event("swarm_stopped", None, json!({"clean": report.is_clean()}));
        state.cp_update(TaskStatus::Cancelled).await;
        let _ = self.store.release_claim(&state.claim);
        self.runs.lock().remove(swarm_id);
        Ok(report)
    }

    /// Re-claim a paused swarm, rehydrate its box sessions, and resume driving.
    /// A budget-exhausted swarm resumed without a raised budget simply re-parks.
    pub async fn resume(&self, swarm_id: &str) -> Result<(), SwarmEngineError> {
        let state = self.build_run(swarm_id).await?;
        state.append_event("swarm_resumed", None, json!({}));
        self.spawn_driver(swarm_id, state);
        Ok(())
    }

    /// Restart recovery: park every swarm whose claim lease expired while
    /// `Running` at `Paused{DaemonRestart}`. Resumable, but never auto-resumed
    /// — spend only advances again on a deliberate `resume`. Run once at engine
    /// boot.
    pub async fn recover(&self) -> Result<Vec<String>, SwarmEngineError> {
        let now = chrono::Utc::now().to_rfc3339();
        self.recover_at(&now).await
    }

    /// Restart recovery as of an explicit clock. `recover` is the boot entry
    /// (`now`); this variant lets a caller replay recovery against a supplied
    /// time.
    pub async fn recover_at(&self, now_iso: &str) -> Result<Vec<String>, SwarmEngineError> {
        let now = now_iso.to_string();
        let expired = self.store.expired_claims(&now)?;
        let mut recovered = Vec::new();
        for claim in expired {
            let Some(mut persisted) = self.store.load_swarm(&claim.swarm_id)? else {
                continue;
            };
            if persisted.run.status.is_terminal()
                || !matches!(persisted.run.status, SwarmStatus::Running)
            {
                continue;
            }
            persisted.revision += 1;
            persisted.updated_at = now.clone();
            persisted.run.status = SwarmStatus::Paused {
                reason: SwarmPauseReason::DaemonRestart,
            };
            self.store.save_swarm(&persisted)?;
            self.store.append_event(&SwarmEventRecord {
                swarm_id: claim.swarm_id.clone(),
                seq: 0,
                ts: now.clone(),
                kind: "swarm_recovered".to_string(),
                box_id: None,
                payload: json!({"reason": "daemon_restart"}),
            })?;
            // Free the dead driver's claim so a deliberate resume can re-take it.
            self.store.release_claim(&claim)?;
            if let Some(tasks) = self.tasks.as_ref() {
                let _ = tasks
                    .update_status(&claim.swarm_id, TaskStatus::Paused, None, None)
                    .await;
            }
            recovered.push(claim.swarm_id);
        }
        Ok(recovered)
    }
}

fn ignore_event(_event: TurnEvent) -> std::future::Ready<()> {
    std::future::ready(())
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_axis_labels_are_stable() {
        assert_eq!(BudgetAxis::Turns.as_str(), "turns");
        assert_eq!(BudgetAxis::Tokens.as_str(), "tokens");
        assert_eq!(BudgetAxis::Cost.as_str(), "cost");
    }

    #[test]
    fn truncate_keeps_short_text_and_marks_long_text() {
        assert_eq!(truncate("hi", 8), "hi");
        assert_eq!(truncate("abcdef", 3), "abc…");
    }
}
