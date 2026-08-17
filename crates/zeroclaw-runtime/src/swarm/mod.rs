//! Swarms: a named roster of agent boxes driven under one shared budget.
//!
//! Swarm state is runtime state, not configuration — the config schema
//! deliberately has no `[swarms]` table. Everything a swarm is lives in the
//! store this module owns, keyed by swarm id and guarded by a revision.

pub mod board;
pub mod reap;
pub mod roster;
pub mod state_tool;
pub mod store;

pub use board::{
    BoardEvent, BoardSnapshot, BoxState, BoxStatus, ClaimOutcome, ReleaseOutcome, SwarmStateBoard,
};
pub use reap::{ReapReport, reap_swarm};
pub use roster::{RosterBox, RosterError, SwarmRoster, build_swarm_roster, swarm_box_alias};
pub use state_tool::SwarmStateTool;
pub use store::{SwarmStore, build_swarm_store};
