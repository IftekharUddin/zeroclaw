//! Durable swarm store — versioned wire shapes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Bump when the persisted envelope layout changes incompatibly. Readers skip
/// + log unknown major versions rather than panicking on boot.
pub const SWARM_STORE_VERSION: u32 = 1;

/// Roster size a swarm is created with when the author does not name one.
pub const DEFAULT_ROSTER_SIZE: usize = 4;

/// The three named budget envelopes a swarm can be created under. Plain code
/// constants, deliberately not config keys: a budget is copied into the spec at
/// create time, so retuning a preset never rewrites a swarm already on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmBudgetPreset {
    Quick,
    Standard,
    Marathon,
}

impl SwarmBudgetPreset {
    /// The turn / token / cost triple this preset expands to.
    pub const fn limits(self) -> SwarmBudgetLimits {
        match self {
            Self::Quick => SwarmBudgetLimits {
                max_turns: 25,
                max_tokens: 250_000,
                max_cost_usd: 5.0,
            },
            Self::Standard => SwarmBudgetLimits {
                max_turns: 100,
                max_tokens: 1_000_000,
                max_cost_usd: 20.0,
            },
            Self::Marathon => SwarmBudgetLimits {
                max_turns: 400,
                max_tokens: 5_000_000,
                max_cost_usd: 100.0,
            },
        }
    }
}

/// The ceilings a swarm run stops at. Every budget resolves to one of these.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SwarmBudgetLimits {
    pub max_turns: u32,
    pub max_tokens: u64,
    pub max_cost_usd: f64,
}

/// A preset envelope or an operator-supplied triple.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmBudget {
    Preset(SwarmBudgetPreset),
    Custom(SwarmBudgetLimits),
}

impl SwarmBudget {
    /// Resolve to concrete ceilings — the only way to read a budget, so a
    /// preset and a hand-written triple are indistinguishable to consumers.
    pub const fn limits(self) -> SwarmBudgetLimits {
        match self {
            Self::Preset(preset) => preset.limits(),
            Self::Custom(limits) => limits,
        }
    }
}

impl Default for SwarmBudget {
    fn default() -> Self {
        Self::Preset(SwarmBudgetPreset::Standard)
    }
}

/// Budget consumed so far. Counters only; the ceilings live in the spec.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct SwarmSpend {
    pub turns: u32,
    pub tokens: u64,
    pub cost_usd: f64,
}

/// Why a running swarm parked. Every pause records one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmPauseReason {
    BudgetExhausted,
    DaemonRestart,
    UserRequested,
}

/// Lifecycle of a swarm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmStatus {
    Created,
    Running,
    Paused { reason: SwarmPauseReason },
    Stopped,
    Completed,
}

impl SwarmStatus {
    /// Terminal states are read-only history: not claimable, not resumable.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Stopped | Self::Completed)
    }

    /// Stable label for the store's status column and log attrs. The pause
    /// reason is not part of it — it stays in the persisted document.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Running => "running",
            Self::Paused { .. } => "paused",
            Self::Stopped => "stopped",
            Self::Completed => "completed",
        }
    }
}

/// One box in the roster: a worker with a job, pinned to a layout slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoxSpec {
    pub box_id: String,
    /// What the box is (`researcher`, `critic`, …).
    pub role: String,
    /// What the box has been told to do.
    pub job: String,
    /// 0-based position in the swarm pane grid.
    pub slot: u8,
}

/// The roster a swarm starts with: `size` unfilled boxes, one per layout slot.
pub fn default_roster(size: usize) -> Vec<BoxSpec> {
    (0..size)
        .map(|slot| BoxSpec {
            box_id: format!("box-{}", slot + 1),
            role: String::new(),
            job: String::new(),
            slot: slot as u8,
        })
        .collect()
}

/// The authored shape of a swarm. Persisted as one revision-guarded document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SwarmSpec {
    pub id: String,
    pub name: String,
    /// Model-provider alias the boxes run on.
    pub provider: String,
    pub model: String,
    /// Risk-profile name applied to every box.
    pub risk_profile: String,
    #[serde(default)]
    pub budget: SwarmBudget,
    /// Channel aliases that receive outbound status notifications. Outbound
    /// only — a swarm is never driven from a channel.
    #[serde(default)]
    pub channels: Vec<String>,
    /// The supervising role the swarm as a whole plays.
    pub role: String,
    /// What the swarm was created to accomplish.
    pub goal: String,
    #[serde(default)]
    pub boxes: Vec<BoxSpec>,
}

impl SwarmSpec {
    /// A spec carrying the default roster and the default budget. The single
    /// place [`DEFAULT_ROSTER_SIZE`] is applied.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        provider: impl Into<String>,
        model: impl Into<String>,
        risk_profile: impl Into<String>,
        role: impl Into<String>,
        goal: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            provider: provider.into(),
            model: model.into(),
            risk_profile: risk_profile.into(),
            budget: SwarmBudget::default(),
            channels: Vec::new(),
            role: role.into(),
            goal: goal.into(),
            boxes: default_roster(DEFAULT_ROSTER_SIZE),
        }
    }
}

/// Live state of a swarm: where it is, what it has spent, and which agent
/// session each box speaks through.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SwarmRun {
    pub status: SwarmStatus,
    #[serde(default)]
    pub spent: SwarmSpend,
    /// `box_id` -> agent session id. Ordered, not hashed: the revision guard
    /// compares serialized bytes, so map iteration order must be stable.
    #[serde(default)]
    pub sessions: BTreeMap<String, String>,
}

impl Default for SwarmRun {
    fn default() -> Self {
        Self {
            status: SwarmStatus::Created,
            spent: SwarmSpend::default(),
            sessions: BTreeMap::new(),
        }
    }
}

/// The stored envelope: spec plus run state under one revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedSwarm {
    /// = [`SWARM_STORE_VERSION`] at write time.
    pub version: u32,
    /// Monotonic per-swarm write counter; the CAS guard on every save.
    pub revision: u64,
    pub spec: SwarmSpec,
    pub run: SwarmRun,
    pub created_at: String,
    /// ISO-8601; bumped on every persisted transition.
    pub updated_at: String,
}

impl PersistedSwarm {
    /// Wrap a freshly authored spec at revision 0, `Created`, unspent.
    pub fn new(spec: SwarmSpec) -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        Self {
            version: SWARM_STORE_VERSION,
            revision: 0,
            spec,
            run: SwarmRun::default(),
            created_at: now.clone(),
            updated_at: now,
        }
    }

    /// The swarm id this envelope is keyed by.
    pub fn swarm_id(&self) -> &str {
        &self.spec.id
    }
}

/// CAS ownership handle for one swarm. Opaque to the orchestrator; the store
/// re-validates it on heartbeat so a reclaimed swarm cannot be driven twice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmClaimToken {
    pub swarm_id: String,
    pub claimed_at: String,
    /// `claimed_at + lease`; the reaper reclaims claims past this.
    pub lease_expires: String,
    /// Process nonce plus a per-grant counter, so a CAS winner is provably this
    /// process *and* this grant — a released token cannot renew its successor.
    pub holder: String,
}

/// One append-only audit row. Never overwritten; `seq` is assigned by the store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SwarmEventRecord {
    pub swarm_id: String,
    /// Monotonic ordinal assigned by the store.
    pub seq: u64,
    pub ts: String,
    /// e.g. `swarm_created` | `swarm_started` | `box_assigned` | `budget_exhausted`
    pub kind: String,
    /// Set when the event belongs to one box rather than the whole swarm.
    pub box_id: Option<String>,
    /// Redacted before append.
    pub payload: serde_json::Value,
}
