# Swarm known limitations

This page consolidates the deferred items and known limitations of the
`zeroclaw swarm` feature (RFC
[#10025](https://github.com/zeroclaw-labs/zeroclaw/issues/10025)). The
architecture and the boundaries the feature must keep are in
[ADR-013](../architecture/decisions/ADR-013-swarm-runtime-boundaries.md); this
page is the maintainer-facing list of what is deliberately left for follow-up
work. None of these is a boundary breach; each is a scoped improvement.

## Follow-ups

1. **Thread `TurnOrigin::Swarm` into box turns.** A box turn should carry the
   `Swarm` origin so the memory-inject gate applies the intended recall-only
   posture. That needs an `AgentBuilder` origin setter on the frozen
   `agent/agent.rs` turn entry. Today box turns run under `AgentDirect`, so a box
   gets an auto memory preamble from its roster-scoped handle instead of a
   recall-only posture. This is not a boundary breach: the handle still enforces
   the roster allowlist regardless of origin. This is a preamble-shape gap only.
2. **Exhaustive `ensure_no_escalation_beyond`.** Harden the escalation check by
   destructuring bind so that adding a new `SecurityPolicy` field is a compile
   error rather than a silent gap. This touches the shared, sensitive
   `policy.rs` and belongs in a supervised hardening PR. The swarm feature
   already removed its own exposure by not accepting any caller-authored policy
   on the roster path; boxes inherit the live parent policy verbatim.
3. **Per-turn budget magnitude cap and cached-token accounting.** Budget
   admission is admit-then-deny, so a single box turn can overshoot a ceiling by
   any magnitude before the run parks. A per-turn magnitude cap is a design
   addition. Separately, `SwarmSpend` currently excludes cached input tokens.
4. **`session/steer` TUI-ownership gate posture.** The steer path has no
   TUI-ownership gate today. That is deliberate for orchestrator-class steers,
   but the posture should be decided at upstream review.
5. **Reserve the `swarm/` memory-namespace prefix as a capability.** The
   `swarm/<swarm_id>` namespace is a naming convention today, not a guarded
   capability. Reserve the prefix once a namespace-choosing tool ships.
6. **Live-spend `swarm/update` budget tick.** The TUI budget bar refreshes on
   run-control replies rather than on a live-spend notification. Per-box context
   usage already ticks live; the budget total should too.
7. **CJK column width in the TUI.** Correct wide-character column widths need the
   `unicode-width` crate, which is not a current root dependency.

## Deferred, inert in v1

These are recorded for completeness. They have no effect in v1 because the code
paths they concern are not reachable or not implemented:

- `store_procedural` attribution and the SQLite COALESCE-to-default fallback: no
  backend implements the procedural store path today.
- `reindex` and `ensure_agent_uuid` forwarding are unrestricted but not
  model-reachable. This is pre-existing and outside swarm scope.
- Reap quiescence (stopping boxes before reaping) belongs to the run lifecycle.
- `send_message_to_peer` survives the box tool filter but is inert for synthetic
  box aliases, which have no peer-group membership, so it is harmless.
