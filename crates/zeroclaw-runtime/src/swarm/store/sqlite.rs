//! Durable SQLite-backed [`SwarmStore`].

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};

use super::model::{PersistedSwarm, SwarmClaimToken, SwarmEventRecord};
use super::{StoreError, SwarmStore, mint_claim, renewed_lease, revision_guard};

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA busy_timeout = 5000;
CREATE TABLE IF NOT EXISTS swarms (
    swarm_id   TEXT PRIMARY KEY,
    revision   INTEGER NOT NULL,
    terminal   INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    json       TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_swarms_terminal ON swarms(terminal);
CREATE TABLE IF NOT EXISTS swarm_events (
    seq      INTEGER PRIMARY KEY AUTOINCREMENT,
    swarm_id TEXT NOT NULL,
    ts       TEXT NOT NULL,
    kind     TEXT NOT NULL,
    box_id   TEXT,
    payload  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_swarm_events_swarm ON swarm_events(swarm_id);
CREATE TABLE IF NOT EXISTS swarm_claims (
    swarm_id      TEXT PRIMARY KEY,
    holder        TEXT NOT NULL,
    claimed_at    TEXT NOT NULL,
    lease_expires TEXT NOT NULL,
    boot_id       TEXT NOT NULL DEFAULT ''
);
";

fn sql_err(e: rusqlite::Error) -> StoreError {
    StoreError::Backend(format!("sqlite: {e}"))
}

/// Durable swarm store. Selected by `build_swarm_store` unless the database
/// cannot be opened.
pub struct SqliteSwarmStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteSwarmStore {
    /// Open (creating if absent) a database at `path`.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path).map_err(sql_err)?;
        Self::init(conn)
    }

    /// In-memory database (tests only — not the fallback backend, which is
    /// `InMemorySwarmStore`).
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory().map_err(sql_err)?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> Result<Self, StoreError> {
        conn.execute_batch(SCHEMA).map_err(sql_err)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, StoreError> {
        self.conn
            .lock()
            .map_err(|_| StoreError::Backend("sqlite swarm store lock poisoned".into()))
    }
}

impl SwarmStore for SqliteSwarmStore {
    fn save_swarm(&self, swarm: &PersistedSwarm) -> Result<(), StoreError> {
        let g = self.lock()?;
        let id = swarm.swarm_id();
        let existing: Option<String> = g
            .query_row(
                "SELECT json FROM swarms WHERE swarm_id=?1",
                params![id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        // Guard on the deserialized document rather than the raw text so an
        // equal-revision retry is compared field-for-field, not byte-for-byte
        // against whatever key order a previous writer happened to emit.
        if let Some(raw) = existing.as_deref() {
            let stored: PersistedSwarm = serde_json::from_str(raw)?;
            revision_guard(Some(&stored), swarm)?;
        }
        let json = serde_json::to_string(swarm)?;
        g.execute(
            "INSERT INTO swarms (swarm_id, revision, terminal, created_at, json)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(swarm_id) DO UPDATE SET
                 revision=excluded.revision,
                 terminal=excluded.terminal,
                 created_at=excluded.created_at,
                 json=excluded.json",
            params![
                id,
                swarm.revision as i64,
                i64::from(swarm.run.status.is_terminal()),
                swarm.created_at,
                json
            ],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    fn load_swarm(&self, swarm_id: &str) -> Result<Option<PersistedSwarm>, StoreError> {
        let g = self.lock()?;
        let json: Option<String> = g
            .query_row(
                "SELECT json FROM swarms WHERE swarm_id=?1",
                params![swarm_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        match json {
            Some(j) => Ok(Some(serde_json::from_str(&j)?)),
            None => Ok(None),
        }
    }

    fn list_swarms(&self) -> Result<Vec<PersistedSwarm>, StoreError> {
        let g = self.lock()?;
        let mut stmt = g
            .prepare("SELECT json FROM swarms ORDER BY created_at DESC, swarm_id ASC")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row.map_err(sql_err)?)?);
        }
        Ok(out)
    }

    fn delete_swarm(&self, swarm_id: &str) -> Result<bool, StoreError> {
        let mut g = self.lock()?;
        let tx = g.transaction().map_err(sql_err)?;
        tx.execute(
            "DELETE FROM swarm_events WHERE swarm_id=?1",
            params![swarm_id],
        )
        .map_err(sql_err)?;
        tx.execute(
            "DELETE FROM swarm_claims WHERE swarm_id=?1",
            params![swarm_id],
        )
        .map_err(sql_err)?;
        let removed = tx
            .execute("DELETE FROM swarms WHERE swarm_id=?1", params![swarm_id])
            .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(removed > 0)
    }

    fn try_claim_swarm_with_boot(
        &self,
        swarm_id: &str,
        boot_id: &str,
    ) -> Result<Option<SwarmClaimToken>, StoreError> {
        let g = self.lock()?;
        // A finished swarm is history, not work: never re-claimable.
        let terminal: Option<i64> = g
            .query_row(
                "SELECT terminal FROM swarms WHERE swarm_id=?1",
                params![swarm_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if terminal == Some(1) {
            return Ok(None);
        }
        // The read and the insert both run under the connection lock, so the
        // check-then-claim is atomic within the process; across processes the
        // PRIMARY KEY makes the loser's insert fail rather than double-grant.
        let claimed: Option<i64> = g
            .query_row(
                "SELECT 1 FROM swarm_claims WHERE swarm_id=?1",
                params![swarm_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if claimed.is_some() {
            return Ok(None);
        }
        let token = mint_claim(swarm_id, boot_id);
        g.execute(
            "INSERT INTO swarm_claims (swarm_id, holder, claimed_at, lease_expires, boot_id)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                token.swarm_id,
                token.holder,
                token.claimed_at,
                token.lease_expires,
                token.boot_id
            ],
        )
        .map_err(sql_err)?;
        Ok(Some(token))
    }

    fn heartbeat_claim(&self, token: &SwarmClaimToken) -> Result<(), StoreError> {
        let g = self.lock()?;
        let renewed = g
            .execute(
                "UPDATE swarm_claims SET lease_expires=?1
                 WHERE swarm_id=?2 AND holder=?3 AND claimed_at=?4",
                params![
                    renewed_lease(),
                    token.swarm_id,
                    token.holder,
                    token.claimed_at
                ],
            )
            .map_err(sql_err)?;
        if renewed == 0 {
            return Err(StoreError::ClaimLost {
                swarm_id: token.swarm_id.clone(),
            });
        }
        Ok(())
    }

    fn release_claim(&self, token: &SwarmClaimToken) -> Result<(), StoreError> {
        // Scoped to this grant: a token that no longer owns the swarm must not
        // evict whoever does.
        self.lock()?
            .execute(
                "DELETE FROM swarm_claims WHERE swarm_id=?1 AND holder=?2 AND claimed_at=?3",
                params![token.swarm_id, token.holder, token.claimed_at],
            )
            .map_err(sql_err)?;
        Ok(())
    }

    fn expired_claims(&self, now_iso: &str) -> Result<Vec<SwarmClaimToken>, StoreError> {
        let g = self.lock()?;
        let mut stmt = g
            .prepare(
                "SELECT swarm_id, holder, claimed_at, lease_expires, boot_id FROM swarm_claims
                 WHERE lease_expires <= ?1 ORDER BY swarm_id",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![now_iso], |r| {
                Ok(SwarmClaimToken {
                    swarm_id: r.get(0)?,
                    holder: r.get(1)?,
                    claimed_at: r.get(2)?,
                    lease_expires: r.get(3)?,
                    boot_id: r.get(4)?,
                })
            })
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(sql_err)?);
        }
        Ok(out)
    }

    fn claims_not_held_by_boot(&self, boot_id: &str) -> Result<Vec<SwarmClaimToken>, StoreError> {
        let g = self.lock()?;
        let mut stmt = g
            .prepare(
                "SELECT swarm_id, holder, claimed_at, lease_expires, boot_id FROM swarm_claims
                 WHERE boot_id <> ?1 ORDER BY swarm_id",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![boot_id], |r| {
                Ok(SwarmClaimToken {
                    swarm_id: r.get(0)?,
                    holder: r.get(1)?,
                    claimed_at: r.get(2)?,
                    lease_expires: r.get(3)?,
                    boot_id: r.get(4)?,
                })
            })
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(sql_err)?);
        }
        Ok(out)
    }

    fn append_event(&self, ev: &SwarmEventRecord) -> Result<u64, StoreError> {
        let g = self.lock()?;
        let payload = serde_json::to_string(&ev.payload)?;
        g.execute(
            "INSERT INTO swarm_events (swarm_id, ts, kind, box_id, payload)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![ev.swarm_id, ev.ts, ev.kind, ev.box_id, payload],
        )
        .map_err(sql_err)?;
        Ok(g.last_insert_rowid() as u64)
    }

    fn list_events(&self, swarm_id: &str) -> Result<Vec<SwarmEventRecord>, StoreError> {
        let g = self.lock()?;
        let mut stmt = g
            .prepare(
                "SELECT seq, ts, kind, box_id, payload FROM swarm_events
                 WHERE swarm_id=?1 ORDER BY seq",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![swarm_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, ts, kind, box_id, payload) = row.map_err(sql_err)?;
            out.push(SwarmEventRecord {
                swarm_id: swarm_id.to_string(),
                seq: seq as u64,
                ts,
                kind,
                box_id,
                payload: serde_json::from_str(&payload)?,
            });
        }
        Ok(out)
    }

    fn health_check(&self) -> bool {
        match self.lock() {
            Ok(g) => g.query_row("SELECT 1", [], |r| r.get::<_, i64>(0)).is_ok(),
            Err(_) => false,
        }
    }

    fn backend(&self) -> &'static str {
        "sqlite"
    }
}

#[cfg(test)]
mod tests {
    use super::super::{SwarmPauseReason, SwarmStatus, conformance};
    use super::*;

    fn store() -> SqliteSwarmStore {
        SqliteSwarmStore::open_in_memory().expect("open in-memory sqlite")
    }

    #[test]
    fn sqlite_crud_round_trip() {
        conformance::crud_round_trip(&store());
    }

    #[test]
    fn sqlite_list_is_newest_first_and_stable() {
        conformance::list_is_newest_first_and_stable(&store());
    }

    #[test]
    fn sqlite_save_guards_revision() {
        conformance::save_guards_revision(&store());
    }

    #[test]
    fn sqlite_claim_lifecycle() {
        conformance::claim_lifecycle(&store());
    }

    #[test]
    fn sqlite_terminal_swarms_are_not_claimable() {
        conformance::terminal_swarms_are_not_claimable(&store());
    }

    #[test]
    fn sqlite_expired_claims_match_past_due_leases() {
        conformance::expired_claims_match_past_due_leases(&store());
    }

    #[test]
    fn sqlite_claims_are_scoped_to_owning_boot() {
        conformance::claims_are_scoped_to_owning_boot(&store());
    }

    #[test]
    fn sqlite_events_are_append_only_and_ordered() {
        conformance::events_are_append_only_and_ordered(&store());
    }

    #[test]
    fn sqlite_delete_drops_events_and_claim() {
        conformance::delete_drops_events_and_claim(&store());
    }

    #[test]
    fn sqlite_backend_is_named() {
        let s = store();
        assert_eq!(s.backend(), "sqlite");
        assert!(s.health_check());
    }

    #[test]
    fn swarms_survive_reopen() {
        // Durability: a swarm written by one instance is visible to a fresh
        // instance opening the same file — the daemon-restart guarantee.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("swarms.db");
        let holder = {
            let a = SqliteSwarmStore::open(&path).expect("open");
            let mut paused = conformance::swarm("s1", "2020-01-01T00:00:00Z");
            paused.revision = 2;
            paused.run.status = SwarmStatus::Paused {
                reason: SwarmPauseReason::DaemonRestart,
            };
            paused
                .run
                .sessions
                .insert("box-1".to_string(), "sess-1".to_string());
            a.save_swarm(&paused).expect("save");
            a.append_event(&conformance::event("s1", "swarm_paused"))
                .expect("append");
            let token = a.try_claim_swarm("s1").expect("claim").expect("claim wins");
            token.holder
        }; // drop `a` — simulates daemon shutdown

        let b = SqliteSwarmStore::open(&path).expect("reopen");
        let listed = b.list_swarms().expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].revision, 2);
        assert_eq!(
            listed[0].run.status,
            SwarmStatus::Paused {
                reason: SwarmPauseReason::DaemonRestart
            }
        );
        assert_eq!(
            listed[0].run.sessions.get("box-1").map(String::as_str),
            Some("sess-1")
        );
        assert_eq!(listed[0].spec.boxes.len(), 4);

        // The event log and the claim survive too, so the restarted daemon can
        // see (and reap) what the dead one left behind.
        assert_eq!(b.list_events("s1").expect("events").len(), 1);
        let expired = b.expired_claims("2999-01-01T00:00:00Z").expect("expired");
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].holder, holder);
    }

    #[test]
    fn terminal_column_tracks_the_persisted_status() {
        // The claim guard reads the column, not the document, so the column has
        // to move with every status transition.
        let s = store();
        let mut swarm = conformance::swarm("s1", "2020-01-01T00:00:00Z");
        s.save_swarm(&swarm).expect("save created");
        assert!(s.try_claim_swarm("s1").expect("claim").is_some());

        swarm.revision = 1;
        swarm.run.status = SwarmStatus::Completed;
        s.save_swarm(&swarm).expect("save completed");
        let terminal: i64 = s
            .lock()
            .expect("lock")
            .query_row(
                "SELECT terminal FROM swarms WHERE swarm_id=?1",
                params!["s1"],
                |r| r.get(0),
            )
            .expect("read terminal column");
        assert_eq!(terminal, 1, "completing a swarm flips the terminal column");
    }
}
