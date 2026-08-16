# Swarm — Open Design Questions

*Compiled 2026-08-16 for morning review. Branch `feat/swarm` (local only). Answer inline; I'll turn answers into implementation.*

Legend: 🔴 blocks progress · 🟡 shapes near-term work · 🟢 future-facing / can defer

---

## A. TUI Shell (Option A → B seam) — the active workstream

**A1. 🔴 Where does the observer inject?** B needs `loop_::run` to accept a caller-supplied `SwarmObserver` composed via the existing `MultiObserver`. Do you want me to (a) make that runtime change *now* so A and B share one code path, or (b) build A against a `SwarmState` seeded from the roster and add the injection when we do B? *(My lean: (a) — the injection is small/additive and doing it now means A's panes are already live-capable, no throwaway seeding code.)*

**A2. 🔴 Does `swarm --tui` replace the chat, or run beside it?** Three shapes:
- (i) Dashboard-only mode: `--tui` shows the board, no chat input (observe a swarm driven elsewhere).
- (ii) Dashboard-then-chat: show board, key to drop into normal chat, board frozen.
- (iii) Split: board + chat input in one screen (this is really B).
*(My lean: ship (i) now, it's honest about being a view; (iii) is the B goal.)*

**A3. 🟡 What does a worker "box" show?** Candidates: alias · status (idle/working/done/error) · current tool · last subtask summary · tokens/cost · elapsed. How much per box before it's noise? Fixed grid (2×N) or responsive?

**A4. 🟡 Queen pane contents?** Current goal · delegation count · aggregate token/cost · active vs done workers · last status line?

**A5. 🟡 Refresh model.** Event-driven redraw (redraw on each `ObserverEvent`) vs fixed tick (e.g. 10fps)? Event-driven is cheaper and snappier but needs a redraw signal channel. *(Lean: tick + dirty flag.)*

**A6. 🟢 Theme/colors.** Reuse `apps/zerocode/theme` palette for consistency, or keep swarm self-contained with a minimal palette? (zerocode's theme lives in a different crate; importing couples us.)

**A7. 🟢 Mouse/scroll/keyboard-enhancement flags?** zerocode enables mouse capture + bracketed paste + keyboard enhancement. For a read-only board we may only need raw mode + alt screen + quit key. Minimize surface?

**A8. 🟡 Terminal restore safety.** Must restore on panic *and* on Ctrl-C. Install a panic hook + signal handling that always leaves the alt screen? (Non-negotiable IMO, just confirming scope.)

**A9. 🟢 Non-TTY / piped stdout.** If `swarm --tui` runs without a TTY (CI, pipe), fall back to the plain launcher automatically, or error? *(Lean: auto-fallback with a note.)*

---

## B. Workers & Delegation

**B1. 🔴 Worker count control.** Hardcoded 1 today. Add `--workers N` flag now, a `[swarm]` config block, or leave at 1 until orchestration needs more? Cap N?

**B2. 🔴 Per-worker system prompt.** Blocked: `AliasedAgentConfig` has no `system_prompt` field; prompts come from workspace personality files (disk). Options: (a) accept workers inherit queen's personality (current), (b) stage an ephemeral personality file in a temp workspace and clean up on exit (breaks "zero disk" but is contained), (c) push a small runtime change to allow an in-memory system-prompt override on the agent config. *(This is a real fork in the road — (c) is cleanest long-term but touches the config/loop contract.)*

**B3. 🟡 Heterogeneous workers.** All workers clone the queen (same provider/model/risk). Do we want role-typed workers (e.g. a cheap-model "researcher" + a strong-model "coder") in this phase, or keep uniform until orchestration?

**B4. 🟡 Delegation mode.** Workers wired as `Bounded` (parent-scoped). Ever want `Independent` workers? Bounded is safer and matches the swarm mental model; confirming we stay bounded.

**B5. 🟢 Worker→worker delegation.** Currently disabled (worker `delegates` emptied). Keep flat (queen is sole delegator) or allow a worker to spawn sub-workers later? Flat is simpler to reason about + visualize.

**B6. 🟡 Worker lifecycle.** Ephemeral per-process today. Do workers persist across turns within one swarm session (warm, reusable) — they already do since they live in the in-memory config for the process. Any reason to tear down/recreate between goals?

**B7. 🟢 Risk profile of workers.** Inherit queen's. Should workers ever be *more* restricted than the queen (defense-in-depth: queen orchestrates broadly, workers run locked-down)? Security-wise attractive.

---

## C. Orchestration (the "swarm" substance — currently absent)

**C1. 🔴 Is there a real orchestration loop, or is the queen's LLM the orchestrator?** Today the queen is just an agent with a delegate tool + a prompt telling her to delegate. Do we want (a) pure LLM-driven (queen decides via tool calls — zero new infra), or (b) a structured task board / plan the runtime tracks (goal → subtasks → assignments → results → synthesis)? *(a) ships now; (b) is the ambitious version and is what makes swarm more than "an agent that can delegate.")*

**C2. 🟡 Shared state board.** If (C1b): where does it live? In-memory struct for the session, or persisted so a swarm can resume? What's the schema (tasks, status, deps, results)?

**C3. 🟡 Result synthesis.** How does the queen combine worker outputs — just feed tool results back into her context (current), or a structured reduce step?

**C4. 🟢 Parallel vs sequential delegation.** The existing `delegate` tool supports background/parallel. Should the queen fan out subtasks concurrently and await, or go one at a time? Parallel is the swarm payoff but complicates the TUI (multiple workers "working" at once — which A's grid is actually designed to show).

**C5. 🟢 Goal completion / termination.** How does a swarm know it's done? Queen declares it? A goal-state check? Max-iteration budget?

---

## D. Config & Persistence

**D1. 🔴 `[swarm]` config block — yes/no and shape.** Worker count, default worker risk profile, worker model override, orchestration mode. Or keep everything ephemeral/flag-driven to avoid schema commitment while experimental?

**D2. 🟡 Queen persistence vs ephemerality.** Queen is persisted (durable entry point); workers ephemeral. Confirm this split is the permanent model, or should a swarm be nameable/save-able as a unit ("my research swarm")?

**D3. 🟢 Naming collisions.** Workers use reserved `swarm-worker-N`. Should this prefix be *reserved/validated* at config load so a user can't create a conflicting persistent agent? Currently we just collision-skip at assembly.

**D4. 🟢 Localization.** Per repo contract, user-facing swarm strings (banner, TUI labels, errors) must route through Fluent + `cli.ftl` across all locales. Do this now or batch before PR? *(Must happen before any PR.)*

---

## E. Productization / Shipping

**E1. 🔴 Feature gating.** Swarm pulls ratatui (already behind `agent-runtime`). Should `swarm` be behind its own `swarm` cargo feature (experimental opt-in) or ride in the default build? Experimental feature flag seems right given maturity.

**E2. 🟡 Push `feat/swarm` to fork?** Still local-only, 3 commits, disk was at 100%. Back it up? *(Standing question from last night.)*

**E3. 🟡 PR strategy.** One big swarm PR, or a stack (launcher → wizard → workers → TUI)? Given zeroclaw's review culture, a stack of small PRs is probably kinder to reviewers. Is there an issue to file first describing the feature/RFC?

**E4. 🟢 Docs / examples.** README section + a `zeroclaw swarm` walkthrough. Later.

**E5. 🟢 Experimental labeling.** Banner says "experimental — MVP-1." Keep an explicit experimental warning in CLI help + docs until stable?

---

## F. Security (autistic-focus per AGENTS.md)

**F1. 🔴 Do ephemeral workers inherit secrets correctly & safely?** They clone the queen's provider/credential refs. Confirm no secret material is *copied* into the in-memory worker config (should be a ref/lookup, not a literal). I want to audit the clone path.

**F2. 🟡 Workspace isolation.** Each worker derives `agents/<alias>/workspace`. These dirs get created on first use — do ephemeral workers leave empty workspace dirs on disk (violating "zero disk")? Need to verify + possibly clean up.

**F3. 🟡 Delegation authority audit.** We add explicit `delegates` entries in-memory. Confirm this can't be exploited to reach a *persistent* user agent (worker alias must never shadow a real agent; assembly must fail closed).

**F4. 🟢 Blast radius of `unrestricted_filesystem` / cross-agent access inheritance.** If the queen has broad workspace access, do cloned workers inherit it? Should they?

---

## G. Naming / UX polish (🟢 all deferrable)

**G1.** "Queen"/"worker"/"swarm" — keep the bee metaphor throughout, or neutral ("coordinator"/"agent")? Affects every user-facing string.

**G2.** Command surface: `zeroclaw swarm` + `--tui` + `--queen` + `--workers`. Any subcommands (`swarm status`, `swarm list`)?

**G3.** Emoji/tone in banner and panes — how much?

---

## My recommended morning decision order
1. **C1** (LLM-orchestrator vs task-board) — this is the identity of the whole feature; everything downstream depends on it.
2. **A1 + A2** (observer injection now?, TUI shape) — unblocks the active TUI work.
3. **B2** (per-worker prompt fork) and **B1** (worker count).
4. **E1/E3** (feature flag + PR strategy) so we build toward a shippable shape.
5. Everything else can be answered as we hit it.

*Sleep well. I'll leave the TUI half-built against the SwarmState/SwarmObserver seam so we're ready to move once C1/A1 are decided.*
