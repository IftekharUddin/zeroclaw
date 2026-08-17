---
id: ADR-013
title: Swarms are runtime state with roster-scoped delegation and memory boundaries
date: 2026-08-17
status: accepted
relates-to:
  - https://github.com/zeroclaw-labs/zeroclaw/issues/10025
  - ADR-004
  - ADR-005
  - ADR-008
  - ADR-010
  - ADR-011
  - docs/book/src/agents/swarms.md
  - docs/book/src/agents/swarm-known-limitations.md
  - docs/book/src/architecture/runtime-state-and-persistence.md
---

# ADR-013: Swarms Are Runtime State With Roster-Scoped Delegation and Memory Boundaries

This records the architecture accepted in RFC
[#10025](https://github.com/zeroclaw-labs/zeroclaw/issues/10025) and shipped as
the `zeroclaw swarm` feature. A swarm is a supervised, bounded run of a small
roster of ephemeral worker agents ("boxes") that coordinate on one goal. The
decision below fixes the boundaries the feature must keep: where its state
lives, how its identities are minted and destroyed, how a box may delegate, what
memory it can read and write, how it is budgeted, and which control-plane and
protocol surfaces it consumes.

This ADR extends [ADR-011](./ADR-011-multi-agent-runtime-boundaries.md)
(configured agents have explicit runtime boundaries) to a second class of
identity that ADR-011 explicitly did not ratify: an ephemeral, non-configured
roster. It does not weaken any ADR-011 boundary. It reuses the memory contract
owned by [ADR-005](./ADR-005-pluggable-memory-backends.md), the memory-authority
split of [ADR-010](./ADR-010-memory-authority-boundaries.md), the tool
shared-state rules of [ADR-004](./ADR-004-tool-shared-state-ownership.md), and
the durable task control plane of
[ADR-008](./ADR-008-goal-mode-control-plane-and-usage-accounting.md).

## Context

ZeroClaw already supports configured multi-agent installs (ADR-011): named
`[agents.<alias>]` identities with per-agent workspace, memory, policy, and
channel scope, supervised by one daemon. A swarm is a different shape of work: a
short-lived team spun up to attack one goal, then torn down. Modeling that as
configured agents would be wrong on several axes at once. It would pollute the
canonical config with transient identities, require config writes and reloads on
the hot path, leak worker identities into `[agents.*]` and every agent-aware
entry point, and give workers the full parent tool envelope including the ability
to spawn yet more agents.

RFC #10025 accepted that a swarm is runtime state, not configuration, and that
its workers are derived (never authored) identities that must be strictly
bounded in what they can delegate to, what memory they can touch, and how much
they can spend before a human is asked to intervene. The v3 config schema had
already dropped the never-shipped `[swarms]` config table (see ADR-011's schema
note); this decision ratifies that drop as permanent.

## Decision

### Swarm state is runtime state, not config

Swarm state lives in an always-durable SQLite store at
`<data_dir>/swarms/swarms.db`, opened unconditionally at daemon boot and in the
RPC context, alongside the other runtime stores catalogued in
[runtime state and persistence](../runtime-state-and-persistence.md). It is not
config. The v2-to-v3 migration that dropped the legacy `[swarms]` config table
stands permanently: a config carrying a legacy `[swarms]` block migrates with
those keys dropped, and nothing resurrects the table. Retuning a budget preset
or editing config never rewrites a swarm already on disk, because a swarm's spec
(including its resolved budget) is copied into the persisted document at create
time and guarded by a monotonic per-swarm revision (compare-and-set on every
save).

### Ephemeral, derived roster identity and a reap cascade

A box is not a configured agent. The roster is built in memory from the swarm's
box specs, lives for the run, and is deleted by a reap cascade. Two identities
are minted per box and both are derived, never authored:

- A synthetic alias `swarm/<swarm_id>/<box_id>`. Configured aliases cannot
  contain `/`, so this namespace is disjoint from `[agents.*]` by construction;
  the builder re-checks and refuses a collision. Nothing in the roster path
  writes `[agents.*]`, and no config reload happens.
- A real memory UUID (from `Memory::ensure_agent_uuid`), so every row a box
  writes is attributed to that box and is deletable by attribution.

The permissions envelope is the live parent agent's, threaded in by the caller
and installed through the same `SubAgentSpawn` machinery as any other
runtime-spawned sub-agent. In v1 every box inherits that policy verbatim; the
roster builder takes no caller-authored per-box policy, so a box can never be
handed a wider or differently-shaped envelope than the parent holds. When a run
ends, the reap cascade removes the boxes: their memory attribution is purged, and
a backend that cannot purge identity is surfaced as a warning rather than
silently reported as clean.

### Roster-only delegation through a dedicated tool

Delegation inside a swarm is roster-only and flows exclusively through a new
`SwarmDelegateTool` (tool name `swarm_delegate`). The orchestrator's tool
registry is an explicit list containing that tool; it never receives the general
`DelegateTool` or the sub-agent spawn tool, and the general `DelegateTool` is
left untouched. The delegate target is validated against the immutable roster
both when the tool enumerates its choices and again at execution time, reading
the same roster snapshot, so there is no time-of-check/time-of-use gap and no
case, whitespace, empty, or parent-alias escape. Boxes themselves do not receive
agent-launching or scheduling tools: the box tool set strips the reentrant agent
tools plus a swarm-box exclusion set (the external coding-agent launchers and the
cron/schedule tools), so a box cannot create more agents or queue future
autonomous work. That exclusion set lives in the swarm module and does not edit
the shared tool registry.

### Shared-memory boundary by decorator composition only

A box's memory handle is composed from the existing memory decorator stack
(ADR-005), not a new backend or new refusal logic. The box is scoped by the
same `AgentScoped` allowlist mechanism configured agents use, with the allowlist
set to the roster and the shared namespace set to `swarm/<swarm_id>`. No memory
refusal logic changes. Allowlist-blind backends (Markdown, None) are rejected at
compose time rather than silently collapsing isolation: swarm memory requires a
backend that honors the allowlist (SQLite, Postgres, or a vector backend). The
`swarm/<swarm_id>` namespace is a coordination surface, not a capability grant.

### Stigmergic coordination via a state board

Boxes coordinate through a `SwarmStateBoard` and a `swarm_state` tool that
performs compare-and-set updates on shared task keys (a stigmergy surface: boxes
leave and read status marks such as idle, working, blocked, done). The board is
bound from the roster's box ids only. Registering a board for a swarm that
already has a live one is refused rather than silently clobbering claims and
orphaning subscribers; resume and reconcile are separate explicit operations.

### Budget presets as code constants, exhaustion pauses

A swarm runs under one of three named budget envelopes, defined as plain code
constants (not config keys): Quick (25 turns / 250,000 tokens / $5), Standard
(100 turns / 1,000,000 tokens / $20), and Marathon (400 turns / 5,000,000 tokens
/ $100). Standard is the default. An operator may also supply a custom
turn/token/cost triple. Crossing any axis ceiling parks the run at
`Paused{BudgetExhausted}` rather than hard-failing; the accumulated spend is
persisted and recovery never auto-resumes spending. Budget admission is atomic:
a per-swarm reservation is debited under lock before a box turn runs and
reconciled against actual usage afterwards, so concurrent delegations cannot
over-admit.

### Control-plane and turn-origin taxonomy

A supervised swarm run is a `TaskKind::Swarm` record in the durable task control
plane (ADR-008). It is the first real consumer of the non-terminal
`TaskStatus::Paused`: a swarm parks (budget exhausted, operator request, daemon
restart) without becoming terminal and resumes from the same record. A new
`TurnOrigin::Swarm` names the origin of swarm work in the closed origin taxonomy.
Claims carry the owning process boot id; restart recovery reclaims any claim not
held by the current boot regardless of lease age or run status, and parks a
recovered `Running` swarm at `Paused{DaemonRestart}` so it is resumable rather
than wedged.

### Additive protocol surface within v1

The feature adds a `swarm/*` JSON-RPC method family (list, get, create, update,
update-layout, delete, fields, validate, start, pause, resume, stop, subscribe,
chat) plus `swarm/update` and `swarm/board` notifications, all additive within
protocol v1 with no version bump. Steering a box mid-turn reuses the existing
`session/steer` path; user-originated steers take priority. Channels attached to
a swarm are outbound-only status sinks: a swarm is never driven from a channel.

### Non-goals

Gateway and web-dashboard parity for swarms is a non-goal for v1. The authoring
and live-canvas surfaces are the CLI TUI over the RPC socket.

## Consequences

Positive consequences:

- Transient team identities never touch canonical config, config validation, or
  config reload, and never appear as `[agents.*]` or at agent-aware entry points.
- A swarm's cost is bounded by construction and a human is asked to intervene at
  the ceiling instead of the run failing or overspending silently.
- Delegation and box tool scope are closed by default: a box cannot delegate off
  the roster, spawn agents, or schedule future work.
- Reusing the ADR-005 decorator stack means the swarm memory boundary rides on
  the same allowlist and attribution guarantees configured agents already have,
  with zero new refusal logic to audit.
- The control-plane record makes a run recoverable across daemon restarts
  without auto-resuming spend.

Negative consequences and limitations:

- Swarm memory requires an allowlist-honoring backend; installs on Markdown or
  no memory backend cannot run swarms and are refused at compose time.
- Because box turns currently execute under `AgentDirect` rather than
  `TurnOrigin::Swarm` (see follow-up below), the intended `Swarm`-origin
  memory-inject posture is not yet applied at the turn entry; the roster-scoped
  handle still enforces the allowlist, so this is a preamble-shape gap, not a
  boundary breach.
- A single box turn can overshoot a ceiling by any magnitude in one turn
  (admission is admit-then-deny by design); there is no per-turn magnitude cap
  yet.
- Spend accounting excludes cached input tokens today.
- The `session/steer` path has no TUI-ownership gate; this is deliberate for
  orchestrator-class steers and is flagged for upstream review.

### Known follow-ups and limitations

These are recorded so reviewers and maintainers see them explicitly. They are
tracked in full in
[swarm known limitations](../../agents/swarm-known-limitations.md).

1. Thread `TurnOrigin::Swarm` into box turns via an `AgentBuilder` origin setter.
   This needs the frozen `agent/agent.rs` turn entry. Today box turns run under
   `AgentDirect`, so they get an auto memory preamble from their roster-scoped
   handle instead of a recall-only posture. This is not a boundary breach: the
   handle still enforces the roster allowlist regardless of origin.
2. Make `ensure_no_escalation_beyond` exhaustive by destructuring bind, so a new
   `SecurityPolicy` field is a compile error rather than a silent gap. This
   touches the shared, sensitive `policy.rs` and belongs in a supervised
   hardening PR. The swarm feature already removed its exposure to it by not
   accepting any caller-authored policy on the roster path.
3. Add a per-turn budget magnitude cap, and include cached-input-token
   accounting in `SwarmSpend`.
4. Decide the `session/steer` TUI-ownership gate posture.
5. Reserve the `swarm/` memory-namespace prefix as a capability once a
   namespace-choosing tool ships (today the prefix is a naming convention, not a
   guarded capability).
6. Emit a live-spend `swarm/update` budget tick. The TUI budget bar currently
   refreshes on run-control replies; per-box context usage already ticks live.
7. Handle CJK column width in the TUI (needs the `unicode-width` crate, not a
   current root dependency).

Also deferred, inert in v1:

- `store_procedural` attribution and the SQLite COALESCE-to-default fallback: no
  backend implements the procedural path today.
- `reindex` and `ensure_agent_uuid` forwarding are unrestricted but not
  model-reachable (pre-existing, out of swarm scope).
- Reap quiescence (stop boxes before reaping) belongs to the run lifecycle.
- `send_message_to_peer` survives the box filter but is inert for synthetic
  aliases (no peer-group membership), so it is harmless.

## References

- [RFC #10025: Swarm](https://github.com/zeroclaw-labs/zeroclaw/issues/10025)
- [Swarms](../../agents/swarms.md)
- [Swarm known limitations](../../agents/swarm-known-limitations.md)
- [ADR-004: Tool shared state ownership](./ADR-004-tool-shared-state-ownership.md)
- [ADR-005: Pluggable memory backends](./ADR-005-pluggable-memory-backends.md)
- [ADR-008: Goal mode control plane and usage accounting](./ADR-008-goal-mode-control-plane-and-usage-accounting.md)
- [ADR-010: Memory authority boundaries](./ADR-010-memory-authority-boundaries.md)
- [ADR-011: Multi-agent runtime boundaries](./ADR-011-multi-agent-runtime-boundaries.md)
- [Runtime state and persistence](../runtime-state-and-persistence.md)
- `crates/zeroclaw-runtime/src/swarm/`
- `crates/zeroclaw-runtime/src/swarm/store/model.rs`
- `crates/zeroclaw-runtime/src/swarm/roster.rs`
- `crates/zeroclaw-runtime/src/swarm/delegate_tool.rs`
- `crates/zeroclaw-runtime/src/swarm/engine.rs`
- `crates/zeroclaw-runtime/src/control_plane/task_registry.rs`
- `crates/zeroclaw-config/src/schema/v2.rs`
