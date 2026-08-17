# Swarms

A swarm is a supervised, bounded run of a small team of ephemeral worker agents,
called boxes, that coordinate on one goal and are torn down when the run ends. A
swarm is not a set of configured agents: nothing a swarm creates is written to
`[agents.*]`, and its state is runtime state, not configuration. See
[ADR-013](../architecture/decisions/ADR-013-swarm-runtime-boundaries.md) for the
architecture and boundaries, and [Delegation and SubAgents](./delegation.md) for
the single-agent building block a swarm is built on.

Swarm state lives in a durable store at `<install>/data/swarms/swarms.db`. It is
never part of `config.toml`.

## Commands

```text
zeroclaw swarm                 # open the dashboard TUI (author, inspect, run)
zeroclaw swarm list            # list stored swarms as plain lines (pipe-safe)
zeroclaw swarm delete <id>     # delete a stored swarm
zeroclaw swarm delete <id> --force   # delete even while a live run holds it
```

Bare `zeroclaw swarm` opens the dashboard terminal UI, where swarms are authored
and driven. `list` and `delete` are one-shot, pipe-safe verbs that need no
terminal. Run control (start, pause, stop) is intentionally not a CLI verb: a
swarm is authored and driven from the TUI, not from a shell script.

The TUI attaches to a running daemon over the local RPC socket if one is
listening, and otherwise starts a short-lived in-process runtime, so
`zeroclaw swarm` works on a machine with no service installed.

## Authoring a swarm: the wizard

Creating a swarm walks a seven-step wizard. Each step offers sensible defaults so
you can accept your way through it:

1. **Provider**: the model-provider alias the boxes run on. Configured aliases
   are offered first, then the known provider families. Choosing a family that
   has no configured alias yet also collects that family's credentials.
2. **Model**: the model the whole swarm runs on. When the provider's catalog is
   known this is a picker; otherwise it is free text.
3. **Risk profile**: the risk profile applied to every box. Configured profiles
   are offered first, then the canonical presets.
4. **Boundedness**: the budget envelope (see [Budgets](#budgets) below). Pick a
   preset or supply a custom turn, token, and cost triple.
5. **Channels**: channel aliases that receive outbound status notifications.
   This is outbound only. A swarm is never driven from a channel.
6. **Role**: the supervising role the swarm as a whole plays.
7. **Goal**: what the swarm is being created to accomplish.

A new swarm starts with a default roster of four unfilled boxes, one per layout
slot. You fill in each box's role and job from the live canvas.

## The live canvas

Opening a swarm shows the live canvas: a grid of box panes, one per roster slot.
Each pane has a header showing that box's role and job, which you can edit in
place, and you can move a box to a different slot. While a run is active, each
pane streams that box's output live, with coalescing and gap markers so a long
run stays readable.

You interact with a box through chat interjection: type a message to a box and it
is held for you while you talk to it. Orchestrator delegation to that box is
paused during the interjection. The hold is released when you send the literal
message `resume`, when you release the box explicitly, or after 90 seconds of
idle. No box turn runs and no budget is spent while a box is merely held.

Run controls on the canvas start, pause, resume, and stop the run. Pausing
finishes any in-flight round and then holds the run; stopping is terminal.

## Budgets

Every swarm runs under a budget. The three named presets are code constants, not
config keys, so retuning a preset never rewrites a swarm already on disk:

| Preset | Turns | Tokens | Cost |
|---|---|---|---|
| Quick | 25 | 250,000 | $5 |
| Standard | 100 | 1,000,000 | $20 |
| Marathon | 400 | 5,000,000 | $100 |

Standard is the default. You can also supply a custom turn, token, and cost
triple in the boundedness step.

When a run crosses any one of its ceilings, it does not fail. It parks at
`Paused` with the reason `BudgetExhausted`, the spend so far is saved, and you
can inspect what happened. Recovery never auto-resumes spending: a paused run
resumes only when you choose to resume it. The budget bar on the canvas reflects
consumption; per-box context usage ticks live, and the budget total refreshes on
run-control replies.

## Boxes are ephemeral

Boxes are not configured agents and never become one:

- No box is ever written to `[agents.*]`. Each box's identity is derived, not
  authored: a synthetic alias of the form `swarm/<swarm_id>/<box_id>` (a shape
  configured aliases can never take, because they cannot contain a slash) plus a
  memory UUID so its memory rows are attributable and deletable.
- Boxes coordinate through a shared state board and a roster-scoped shared memory
  namespace (`swarm/<swarm_id>`). A box can only delegate to other boxes in the
  same roster, and cannot spawn new agents or schedule future work.
- When a run ends, a reap cascade evaporates the roster: the boxes are gone and
  their memory attribution is purged.

Swarm memory requires a backend that honors the per-agent allowlist (SQLite,
Postgres, or a vector backend). On a Markdown or no-memory install a swarm is
refused at creation rather than silently losing isolation.

## Restarts are resumable

A swarm run is tracked as a supervised task in the durable control plane, so it
survives a daemon restart. When the daemon comes back up, a run that was
`Running` is parked at `Paused` with the reason `DaemonRestart` rather than being
lost or wedged, and you can resume it from where it left off. The restart never
auto-resumes spending.

## What swarms do not do (yet)

Gateway and web-dashboard parity for swarms is a non-goal for now: swarms are
authored and driven from the CLI TUI over the RPC socket, not from the HTTP
gateway or the web dashboard. Other deferred items and known limitations are
listed in [swarm known limitations](./swarm-known-limitations.md).
