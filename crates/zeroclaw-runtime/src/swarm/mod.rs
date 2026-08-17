//! Swarms: a named roster of agent boxes driven under one shared budget.
//!
//! Swarm state is runtime state, not configuration — the config schema
//! deliberately has no `[swarms]` table. Everything a swarm is lives in the
//! store this module owns, keyed by swarm id and guarded by a revision.

pub mod store;

pub use store::{SwarmStore, build_swarm_store};
