//! Reaping a swarm: delete everything the run minted.
//!
//! A swarm's boxes are ephemeral, so their traces must be too. Reaping walks
//! the roster and removes, in an order that cannot orphan anything:
//!
//! 1. every row in the swarm's memory namespace (including anything the
//!    supervising agent wrote there),
//! 2. every remaining row attributed to a box, by its synthetic alias,
//! 3. each box's alias -> UUID binding, which the backend only lets go once
//!    no rows reference it,
//! 4. the board entry, taking its claims with it.
//!
//! Reaping is best-effort and total: one failing box must not leave the other
//! three behind, so failures are collected into [`ReapReport::warnings`]
//! rather than returned early. Callers decide what an unclean reap means; the
//! report says exactly what survived.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use zeroclaw_memory::{Memory, swarm_namespace};

use crate::swarm::board::SwarmStateBoard;
use crate::swarm::roster::SwarmRoster;

/// What a reap removed, and anything it could not.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReapReport {
    pub swarm_id: String,
    /// Rows deleted by namespace.
    pub namespace_rows_purged: usize,
    /// Rows deleted by box attribution, after the namespace sweep.
    pub agent_rows_purged: usize,
    /// Box alias -> UUID bindings removed.
    pub identities_removed: usize,
    pub board_removed: bool,
    pub claims_dropped: usize,
    /// Everything that did not delete cleanly, in the order it was attempted.
    pub warnings: Vec<String>,
}

impl ReapReport {
    /// Whether the cascade completed with nothing left behind.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.warnings.is_empty()
    }
}

/// Delete every trace of `roster`'s swarm.
///
/// `memory` must be an admin handle (the install-wide backend), not a
/// box's scoped handle: cross-agent deletes are exactly what the scoping
/// wrapper refuses, and reaping deletes across the whole roster.
pub async fn reap_swarm(
    memory: &Arc<dyn Memory>,
    board: &SwarmStateBoard,
    roster: &SwarmRoster,
) -> ReapReport {
    let swarm_id = roster.swarm_id().to_string();
    let mut report = ReapReport {
        swarm_id: swarm_id.clone(),
        ..ReapReport::default()
    };

    let namespace = swarm_namespace(&swarm_id);
    match memory.purge_namespace(&namespace).await {
        Ok(rows) => report.namespace_rows_purged = rows,
        Err(err) => report
            .warnings
            .push(format!("purge namespace {namespace}: {err}")),
    }

    for entry in roster.boxes() {
        match memory.purge_agent(&entry.alias).await {
            Ok(rows) => report.agent_rows_purged += rows,
            Err(err) => report
                .warnings
                .push(format!("purge rows for {}: {err}", entry.alias)),
        }
    }

    // Identities last: the backend refuses to drop a binding while rows still
    // point at it, so a failed purge above surfaces here too instead of
    // silently orphaning them.
    for entry in roster.boxes() {
        match memory.purge_agent_identity(&entry.alias).await {
            Ok(true) => report.identities_removed += 1,
            Ok(false) => {}
            Err(err) => report
                .warnings
                .push(format!("purge identity for {}: {err}", entry.alias)),
        }
    }

    if let Some(snapshot) = board.remove(&swarm_id) {
        report.board_removed = true;
        report.claims_dropped = snapshot.claims.len();
    }

    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Delete).with_attrs(
            ::serde_json::json!({
                "swarm_id": swarm_id,
                "boxes": roster.len(),
                "namespace_rows_purged": report.namespace_rows_purged,
                "agent_rows_purged": report.agent_rows_purged,
                "identities_removed": report.identities_removed,
                "board_removed": report.board_removed,
                "claims_dropped": report.claims_dropped,
                "warnings": report.warnings.len(),
            })
        ),
        "swarm reaped"
    );

    report
}

#[cfg(test)]
mod tests {
    use zeroclaw_config::policy::SecurityPolicy;
    use zeroclaw_config::schema::{AliasedAgentConfig, Config, RiskProfileConfig};
    use zeroclaw_memory::{MemoryCategory, SqliteMemory};

    use super::*;
    use crate::swarm::board::BoxStatus;
    use crate::swarm::roster::build_swarm_roster;
    use crate::swarm::store::model::{BoxSpec, SwarmSpec};

    /// The live parent policy the caller threads into the builder.
    fn parent_policy(config: &Config, alias: &str) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy::for_agent(config, alias).expect("parent policy"))
    }

    fn test_config() -> Config {
        let mut config = Config::default();
        config
            .risk_profiles
            .insert("default".to_string(), RiskProfileConfig::default());
        config.agents.insert(
            "default".to_string(),
            AliasedAgentConfig {
                risk_profile: "default".into(),
                ..AliasedAgentConfig::default()
            },
        );
        config
    }

    fn spec_with_boxes(swarm_id: &str, count: usize) -> SwarmSpec {
        let mut spec = SwarmSpec::new(
            swarm_id,
            "reap swarm",
            "anthropic",
            "model",
            "default",
            "supervisor",
            "ship it",
        );
        spec.boxes = (0..count)
            .map(|slot| BoxSpec {
                box_id: format!("box-{}", slot + 1),
                role: "worker".to_string(),
                job: "work".to_string(),
                slot: u8::try_from(slot).unwrap_or(0),
            })
            .collect();
        spec
    }

    /// Count rows straight out of the database, behind every wrapper: what
    /// the store actually holds, not what a handle chooses to report.
    /// Probes by the boxes' exact memory UUIDs — captured before the reap —
    /// rather than by `alias LIKE 'swarm/<id>/%'`. The alias probe is vacuous
    /// once identities are purged (their `agents` rows are gone) and treats a
    /// literal `_` in an id as a SQL wildcard; probing by UUID stays valid
    /// across the deletion and is exact.
    /// Returns `(memory rows, agents rows)` for the given box UUIDs.
    fn residue(data_dir: &std::path::Path, agent_ids: &[String]) -> (i64, i64) {
        let conn = rusqlite::Connection::open(data_dir.join("memory").join("brain.db"))
            .expect("open the memory db");
        if agent_ids.is_empty() {
            return (0, 0);
        }
        let placeholders = vec!["?"; agent_ids.len()].join(",");
        let params: Vec<&dyn rusqlite::ToSql> = agent_ids
            .iter()
            .map(|id| id as &dyn rusqlite::ToSql)
            .collect();
        let rows: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM memories WHERE agent_id IN ({placeholders})"),
                params.as_slice(),
                |row| row.get(0),
            )
            .expect("count memory rows");
        let identities: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM agents WHERE id IN ({placeholders})"),
                params.as_slice(),
                |row| row.get(0),
            )
            .expect("count agents rows");
        (rows, identities)
    }

    #[tokio::test]
    async fn reaping_leaves_no_residual_rows() {
        let config = test_config();
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let sqlite = SqliteMemory::new("sqlite", tmp.path()).expect("sqlite");
        let memory: Arc<dyn Memory> = Arc::new(sqlite);
        let spec = spec_with_boxes("swarm-reap", 4);

        let roster = build_swarm_roster(
            &config,
            &memory,
            &spec,
            "default",
            parent_policy(&config, "default"),
        )
        .await
        .expect("roster");
        let board = SwarmStateBoard::new();
        board
            .register(roster.swarm_id(), roster.box_ids())
            .expect("register");

        // Two boxes work: publish, claim, and write namespaced memories.
        let memories = roster
            .compose_memories(&memory, &config.memory)
            .expect("compose swarm-box memory over sqlite");
        for box_id in ["box-1", "box-3"] {
            let handle = memories.get(box_id).expect("composed handle");
            handle
                .store(
                    &format!("{box_id}-finding"),
                    "the thing we learned",
                    MemoryCategory::Core,
                    None,
                )
                .await
                .expect("store");
            board
                .publish(roster.swarm_id(), box_id, BoxStatus::Working, "working")
                .expect("publish");
            board
                .claim(roster.swarm_id(), box_id, &format!("task-{box_id}"))
                .expect("claim");
        }

        let aliases = roster.aliases();
        // Capture the box UUIDs BEFORE the reap: residue is probed by these
        // exact ids so the post-reap check stays meaningful after the agents
        // rows are deleted.
        let box_agent_ids = roster.agent_ids();
        let (rows_before, identities_before) = residue(tmp.path(), &box_agent_ids);
        assert_eq!(rows_before, 2, "two boxes wrote one row each");
        assert_eq!(identities_before, 4, "every box minted an identity");
        assert_eq!(
            memory
                .recall_namespaced(
                    &swarm_namespace(roster.swarm_id()),
                    "thing",
                    10,
                    None,
                    None,
                    None
                )
                .await
                .expect("recall_namespaced")
                .len(),
            2,
            "writes must carry the swarm namespace"
        );

        let report = reap_swarm(&memory, &board, &roster).await;

        assert!(report.is_clean(), "unclean reap: {:?}", report.warnings);
        assert_eq!(report.namespace_rows_purged, 2);
        assert_eq!(report.agent_rows_purged, 0, "the namespace sweep took them");
        assert_eq!(
            report.identities_removed, 4,
            "every box's binding must go, not just the ones that wrote"
        );
        assert!(report.board_removed);
        assert_eq!(report.claims_dropped, 2);

        let (rows_after, identities_after) = residue(tmp.path(), &box_agent_ids);
        assert_eq!(rows_after, 0, "no memory row may survive a reap");
        assert_eq!(identities_after, 0, "no uuid row may survive a reap");
        assert!(
            memory
                .recall_namespaced(
                    &swarm_namespace(roster.swarm_id()),
                    "thing",
                    10,
                    None,
                    None,
                    None
                )
                .await
                .expect("recall_namespaced")
                .is_empty(),
            "no namespaced row may survive a reap"
        );
        assert!(board.snapshot(roster.swarm_id()).is_none());
        assert!(board.is_empty(), "no board entry may survive a reap");

        // The bindings are gone, not merely detached: re-minting yields fresh
        // UUIDs rather than the reaped ones.
        let mut reminted = Vec::new();
        for alias in &aliases {
            reminted.push(memory.ensure_agent_uuid(alias).await.expect("re-mint"));
        }
        for (old, new) in roster.agent_ids().iter().zip(&reminted) {
            assert_ne!(old, new, "a reaped identity must not come back");
        }
    }

    #[tokio::test]
    async fn reaping_survives_a_backend_that_cannot_purge() {
        let config = test_config();
        let memory: Arc<dyn Memory> = Arc::new(zeroclaw_memory::NoneMemory::new("none"));
        let spec = spec_with_boxes("swarm-none", 2);

        let roster = build_swarm_roster(
            &config,
            &memory,
            &spec,
            "default",
            parent_policy(&config, "default"),
        )
        .await
        .expect("roster");
        let board = SwarmStateBoard::new();
        board
            .register(roster.swarm_id(), roster.box_ids())
            .expect("register");

        let report = reap_swarm(&memory, &board, &roster).await;

        assert!(
            !report.is_clean(),
            "an unsupported purge must be reported, not swallowed"
        );
        assert!(
            report.board_removed,
            "board teardown must still happen after memory failures"
        );
        assert!(board.is_empty());
    }
}
