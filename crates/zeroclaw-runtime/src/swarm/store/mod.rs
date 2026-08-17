//! Durable swarm-state store — the persistence contract for swarms.

pub mod model;
pub mod sqlite;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::{Duration, Utc};

pub use model::{
    BoxSpec, DEFAULT_ROSTER_SIZE, PersistedSwarm, SWARM_STORE_VERSION, SwarmBudget,
    SwarmBudgetLimits, SwarmBudgetPreset, SwarmClaimToken, SwarmEventRecord, SwarmPauseReason,
    SwarmRun, SwarmSpec, SwarmSpend, SwarmStatus, default_roster,
};
pub use sqlite::SqliteSwarmStore;

/// Claim lease length. The orchestrator renews via `heartbeat_claim`; a claim
/// past this is reclaimable, which is how a crashed daemon releases its swarms.
const DEFAULT_CLAIM_LEASE_SECS: i64 = 3600;

pub trait SwarmStore: Send + Sync {
    // ── swarm documents (spec + run state under one revision) ──
    /// Persist-before-mutate. Revision-guarded: a strictly-older revision is
    /// rejected as `StaleRevision`; an equal revision is accepted only as a
    /// byte-identical idempotent retry, else `RevisionConflict`; a newer
    /// revision wins.
    fn save_swarm(&self, swarm: &PersistedSwarm) -> Result<(), StoreError>;
    /// Single swarm by id (latest revision), terminal or not.
    fn load_swarm(&self, swarm_id: &str) -> Result<Option<PersistedSwarm>, StoreError>;
    /// Every swarm, newest-created first, ties broken by id so the listing is
    /// stable across calls.
    fn list_swarms(&self) -> Result<Vec<PersistedSwarm>, StoreError>;
    /// Drop a swarm with its event history and any live claim. Returns whether
    /// a record was actually removed.
    fn delete_swarm(&self, swarm_id: &str) -> Result<bool, StoreError>;

    // ── CAS ownership (exactly one driver per swarm) ──
    /// Take ownership of a non-terminal, unclaimed swarm. `Ok(None)` means a
    /// concurrent winner holds it, or the swarm has already finished.
    fn try_claim_swarm(&self, swarm_id: &str) -> Result<Option<SwarmClaimToken>, StoreError>;
    /// Extend this token's lease. `ClaimLost` when the claim is gone or now
    /// belongs to a different holder — the caller must stop driving the swarm.
    fn heartbeat_claim(&self, token: &SwarmClaimToken) -> Result<(), StoreError>;
    /// Release a claim, freeing the swarm for a new driver. Releasing a claim
    /// this token no longer owns is a no-op, never a steal.
    fn release_claim(&self, token: &SwarmClaimToken) -> Result<(), StoreError>;
    /// Reaper source: claims whose lease expired at/<= `now_iso`.
    fn expired_claims(&self, now_iso: &str) -> Result<Vec<SwarmClaimToken>, StoreError>;

    // ── append-only event log ──
    /// Append-only, monotonic-seq, never-overwrite. Returns the assigned seq.
    fn append_event(&self, ev: &SwarmEventRecord) -> Result<u64, StoreError>;
    /// Ordered event history for a swarm.
    fn list_events(&self, swarm_id: &str) -> Result<Vec<SwarmEventRecord>, StoreError>;

    // ── maintenance ──
    fn health_check(&self) -> bool;
    /// Backend name (for logs + the "never a silent no-op" guard).
    fn backend(&self) -> &'static str;
}

/// Errors a swarm store may surface. Never swallowed by callers.
#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
    Serde(serde_json::Error),
    Backend(String),
    /// A save lost the revision race (a newer revision already persisted).
    StaleRevision {
        swarm_id: String,
        have: u64,
        found: u64,
    },
    /// A same-revision write whose content diverges from the stored document.
    /// The caller must bump `revision` to record new state; only a
    /// byte-identical retry at the same revision is accepted (idempotent).
    RevisionConflict {
        swarm_id: String,
        revision: u64,
    },
    /// A claim was lost to a concurrent winner (reclaimed after lease expiry,
    /// or released and re-taken by another holder).
    ClaimLost {
        swarm_id: String,
    },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "swarm store io error: {e}"),
            Self::Serde(e) => write!(f, "swarm store serde error: {e}"),
            Self::Backend(m) => write!(f, "swarm store backend error: {m}"),
            Self::StaleRevision {
                swarm_id,
                have,
                found,
            } => write!(
                f,
                "swarm store stale revision for swarm {swarm_id}: have {have}, found {found}"
            ),
            Self::RevisionConflict { swarm_id, revision } => write!(
                f,
                "swarm store revision conflict for swarm {swarm_id}: divergent write at revision {revision}"
            ),
            Self::ClaimLost { swarm_id } => write!(
                f,
                "swarm store claim for swarm {swarm_id} lost to a concurrent winner"
            ),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        Self::Serde(e)
    }
}

/// Build the swarm store rooted at the daemon data dir.
///
/// Always durable [`SqliteSwarmStore`] at `<data_dir>/swarms/swarms.db` (dir
/// created mode-0700). There is no config gate: swarms are runtime state, so
/// the store is built unconditionally rather than behind a schema key.
///
/// A backend-open failure is non-fatal — the daemon logs and falls back to the
/// ephemeral [`InMemorySwarmStore`], the same posture `build_sop_engine` takes
/// for the SOP run store, so a read-only data dir degrades instead of blocking
/// boot.
pub fn build_swarm_store(data_dir: &Path) -> Arc<dyn SwarmStore> {
    match open_durable_store(data_dir) {
        Ok(store) => Arc::new(store),
        Err(e) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"error": e.to_string()})),
                "Swarm: store init failed; falling back to in-memory"
            );
            Arc::new(InMemorySwarmStore::new())
        }
    }
}

fn open_durable_store(data_dir: &Path) -> Result<SqliteSwarmStore, StoreError> {
    let dir = data_dir.join("swarms");
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Best-effort tighten to 0700; ignore if the filesystem rejects it.
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    SqliteSwarmStore::open(&dir.join("swarms.db"))
}

/// Distinguishes successive grants within one process. Two claims on the same
/// swarm taken microseconds apart must not compare equal, or a released token
/// could renew its successor's lease.
static CLAIM_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Mint a fresh claim for this process at the default lease.
fn mint_claim(swarm_id: &str) -> SwarmClaimToken {
    let now = Utc::now();
    let nonce = CLAIM_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    SwarmClaimToken {
        swarm_id: swarm_id.to_string(),
        claimed_at: now.to_rfc3339(),
        lease_expires: (now + Duration::seconds(DEFAULT_CLAIM_LEASE_SECS)).to_rfc3339(),
        holder: format!("pid-{}-{nonce}", std::process::id()),
    }
}

/// A renewed lease bound for a claim being heartbeated.
fn renewed_lease() -> String {
    (Utc::now() + Duration::seconds(DEFAULT_CLAIM_LEASE_SECS)).to_rfc3339()
}

/// Identity of a claim across heartbeats. `lease_expires` moves on every
/// renewal, so it is deliberately not part of the comparison.
fn same_claim(stored: &SwarmClaimToken, token: &SwarmClaimToken) -> bool {
    stored.holder == token.holder && stored.claimed_at == token.claimed_at
}

fn revision_guard(
    existing: Option<&PersistedSwarm>,
    incoming: &PersistedSwarm,
) -> Result<(), StoreError> {
    let Some(existing) = existing else {
        return Ok(());
    };
    if incoming.revision < existing.revision {
        return Err(StoreError::StaleRevision {
            swarm_id: incoming.swarm_id().to_string(),
            have: incoming.revision,
            found: existing.revision,
        });
    }
    if incoming.revision == existing.revision
        && serde_json::to_vec(existing)? != serde_json::to_vec(incoming)?
    {
        return Err(StoreError::RevisionConflict {
            swarm_id: incoming.swarm_id().to_string(),
            revision: incoming.revision,
        });
    }
    Ok(())
}

/// Newest-created first; ties broken by id so repeated listings agree.
fn sort_listing(out: &mut [PersistedSwarm]) {
    out.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.spec.id.cmp(&b.spec.id))
    });
}

// ── In-memory fallback backend ─────────────────────────────────────

#[derive(Default)]
struct Inner {
    swarms: HashMap<String, PersistedSwarm>,
    events: HashMap<String, Vec<SwarmEventRecord>>,
    claims: HashMap<String, SwarmClaimToken>,
    seq: u64,
}

/// Process-local, non-durable store. Everything is lost on restart; selected
/// only when the durable backend cannot be opened.
pub struct InMemorySwarmStore {
    inner: Mutex<Inner>,
}

impl Default for InMemorySwarmStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemorySwarmStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>, StoreError> {
        self.inner
            .lock()
            .map_err(|_| StoreError::Backend("in-memory swarm store lock poisoned".into()))
    }
}

impl SwarmStore for InMemorySwarmStore {
    fn save_swarm(&self, swarm: &PersistedSwarm) -> Result<(), StoreError> {
        let mut g = self.lock()?;
        revision_guard(g.swarms.get(swarm.swarm_id()), swarm)?;
        g.swarms.insert(swarm.swarm_id().to_string(), swarm.clone());
        Ok(())
    }

    fn load_swarm(&self, swarm_id: &str) -> Result<Option<PersistedSwarm>, StoreError> {
        Ok(self.lock()?.swarms.get(swarm_id).cloned())
    }

    fn list_swarms(&self) -> Result<Vec<PersistedSwarm>, StoreError> {
        let mut out: Vec<PersistedSwarm> = self.lock()?.swarms.values().cloned().collect();
        sort_listing(&mut out);
        Ok(out)
    }

    fn delete_swarm(&self, swarm_id: &str) -> Result<bool, StoreError> {
        let mut g = self.lock()?;
        let removed = g.swarms.remove(swarm_id).is_some();
        g.events.remove(swarm_id);
        g.claims.remove(swarm_id);
        Ok(removed)
    }

    fn try_claim_swarm(&self, swarm_id: &str) -> Result<Option<SwarmClaimToken>, StoreError> {
        let mut g = self.lock()?;
        if g.claims.contains_key(swarm_id) {
            return Ok(None);
        }
        // A finished swarm is history, not work: never re-claimable.
        if g.swarms
            .get(swarm_id)
            .is_some_and(|s| s.run.status.is_terminal())
        {
            return Ok(None);
        }
        let token = mint_claim(swarm_id);
        g.claims.insert(swarm_id.to_string(), token.clone());
        Ok(Some(token))
    }

    fn heartbeat_claim(&self, token: &SwarmClaimToken) -> Result<(), StoreError> {
        let mut g = self.lock()?;
        let Some(stored) = g.claims.get_mut(&token.swarm_id) else {
            return Err(StoreError::ClaimLost {
                swarm_id: token.swarm_id.clone(),
            });
        };
        if !same_claim(stored, token) {
            return Err(StoreError::ClaimLost {
                swarm_id: token.swarm_id.clone(),
            });
        }
        stored.lease_expires = renewed_lease();
        Ok(())
    }

    fn release_claim(&self, token: &SwarmClaimToken) -> Result<(), StoreError> {
        let mut g = self.lock()?;
        if g.claims
            .get(&token.swarm_id)
            .is_some_and(|stored| same_claim(stored, token))
        {
            g.claims.remove(&token.swarm_id);
        }
        Ok(())
    }

    fn expired_claims(&self, now_iso: &str) -> Result<Vec<SwarmClaimToken>, StoreError> {
        let g = self.lock()?;
        let mut out: Vec<SwarmClaimToken> = g
            .claims
            .values()
            .filter(|c| c.lease_expires.as_str() <= now_iso)
            .cloned()
            .collect();
        out.sort_by(|a, b| a.swarm_id.cmp(&b.swarm_id));
        Ok(out)
    }

    fn append_event(&self, ev: &SwarmEventRecord) -> Result<u64, StoreError> {
        let mut g = self.lock()?;
        g.seq += 1;
        let seq = g.seq;
        let mut rec = ev.clone();
        rec.seq = seq;
        g.events.entry(ev.swarm_id.clone()).or_default().push(rec);
        Ok(seq)
    }

    fn list_events(&self, swarm_id: &str) -> Result<Vec<SwarmEventRecord>, StoreError> {
        let mut v = self
            .lock()?
            .events
            .get(swarm_id)
            .cloned()
            .unwrap_or_default();
        v.sort_by_key(|e| e.seq);
        Ok(v)
    }

    fn health_check(&self) -> bool {
        self.inner.lock().is_ok()
    }

    fn backend(&self) -> &'static str {
        "in-memory"
    }
}

/// Behavior every [`SwarmStore`] backend must exhibit identically. Both the
/// durable and the in-memory backend run the same assertions, so the fallback
/// can never silently diverge from the store it stands in for.
#[cfg(test)]
pub(crate) mod conformance {
    use super::*;
    use serde_json::json;

    pub(crate) fn spec(id: &str) -> SwarmSpec {
        SwarmSpec::new(
            id,
            format!("Swarm {id}"),
            "anthropic",
            "claude-sonnet-4",
            "standard",
            "supervisor",
            "ship the thing",
        )
    }

    pub(crate) fn swarm(id: &str, created_at: &str) -> PersistedSwarm {
        let mut s = PersistedSwarm::new(spec(id));
        s.created_at = created_at.to_string();
        s.updated_at = created_at.to_string();
        s
    }

    pub(crate) fn event(swarm_id: &str, kind: &str) -> SwarmEventRecord {
        SwarmEventRecord {
            swarm_id: swarm_id.to_string(),
            seq: 0,
            ts: "t".to_string(),
            kind: kind.to_string(),
            box_id: None,
            payload: json!({}),
        }
    }

    pub(crate) fn crud_round_trip(store: &dyn SwarmStore) {
        let created = swarm("s1", "2020-01-01T00:00:00Z");
        store.save_swarm(&created).expect("create");
        let loaded = store.load_swarm("s1").expect("load").expect("present");
        assert_eq!(loaded, created, "the stored document round-trips verbatim");
        assert_eq!(loaded.spec.boxes.len(), DEFAULT_ROSTER_SIZE);
        assert_eq!(loaded.run.status, SwarmStatus::Created);

        // Update: a bumped revision carrying new run state wins.
        let mut running = created.clone();
        running.revision = 1;
        running.run.status = SwarmStatus::Running;
        running
            .run
            .sessions
            .insert("box-1".to_string(), "sess-1".to_string());
        running.run.spent.turns = 3;
        store.save_swarm(&running).expect("update");
        let loaded = store.load_swarm("s1").expect("load").expect("present");
        assert_eq!(loaded.revision, 1);
        assert_eq!(loaded.run.status, SwarmStatus::Running);
        assert_eq!(
            loaded.run.sessions.get("box-1").map(String::as_str),
            Some("sess-1")
        );
        assert_eq!(loaded.run.spent.turns, 3);

        assert_eq!(store.list_swarms().expect("list").len(), 1);
        assert!(store.delete_swarm("s1").expect("delete"));
        assert!(store.load_swarm("s1").expect("load").is_none());
        assert!(
            !store.delete_swarm("s1").expect("delete"),
            "deleting an absent swarm reports no removal rather than erroring"
        );
        assert!(store.health_check());
    }

    pub(crate) fn list_is_newest_first_and_stable(store: &dyn SwarmStore) {
        for (id, created) in [
            ("b", "2020-01-01T00:00:00Z"),
            ("c", "2020-03-01T00:00:00Z"),
            ("a", "2020-02-01T00:00:00Z"),
        ] {
            store.save_swarm(&swarm(id, created)).expect("save");
        }
        let ids: Vec<String> = store
            .list_swarms()
            .expect("list")
            .into_iter()
            .map(|s| s.spec.id)
            .collect();
        assert_eq!(ids, vec!["c", "a", "b"], "newest-created first");
    }

    pub(crate) fn save_guards_revision(store: &dyn SwarmStore) {
        let mut base = swarm("s1", "2020-01-01T00:00:00Z");
        base.revision = 5;
        store.save_swarm(&base).expect("save at revision 5");

        // A lower revision loses the race.
        let mut older = base.clone();
        older.revision = 4;
        let err = store.save_swarm(&older).expect_err("stale write refused");
        assert!(
            matches!(
                err,
                StoreError::StaleRevision {
                    have: 4,
                    found: 5,
                    ..
                }
            ),
            "expected StaleRevision, got {err}"
        );

        // A byte-identical same-revision write is an idempotent retry.
        store.save_swarm(&base).expect("idempotent retry");

        // A divergent same-revision write is refused, not silently applied.
        let mut divergent = base.clone();
        divergent.run.status = SwarmStatus::Running;
        let err = store
            .save_swarm(&divergent)
            .expect_err("divergent same-revision write refused");
        assert!(
            matches!(err, StoreError::RevisionConflict { revision: 5, .. }),
            "expected RevisionConflict, got {err}"
        );

        // Neither rejected payload landed.
        let stored = store.load_swarm("s1").expect("load").expect("present");
        assert_eq!(stored.revision, 5);
        assert_eq!(stored.run.status, SwarmStatus::Created);

        // A newer revision wins.
        let mut newer = base.clone();
        newer.revision = 6;
        newer.run.status = SwarmStatus::Running;
        store.save_swarm(&newer).expect("newer revision wins");
        assert_eq!(
            store
                .load_swarm("s1")
                .expect("load")
                .expect("present")
                .revision,
            6
        );
    }

    pub(crate) fn claim_lifecycle(store: &dyn SwarmStore) {
        store
            .save_swarm(&swarm("s1", "2020-01-01T00:00:00Z"))
            .expect("save");

        let token = store
            .try_claim_swarm("s1")
            .expect("claim")
            .expect("first claim wins");
        assert_eq!(token.swarm_id, "s1");
        assert!(!token.holder.is_empty());
        assert!(
            store.try_claim_swarm("s1").expect("claim").is_none(),
            "a claimed swarm is refused to the next caller"
        );

        // Heartbeat renews the live claim.
        store.heartbeat_claim(&token).expect("heartbeat");

        // Release frees the slot for a new driver.
        store.release_claim(&token).expect("release");
        let retaken = store
            .try_claim_swarm("s1")
            .expect("claim")
            .expect("released swarm is claimable again");

        // The stale token can neither renew nor steal the new holder's claim.
        let err = store
            .heartbeat_claim(&token)
            .expect_err("a released token must not renew");
        assert!(
            matches!(err, StoreError::ClaimLost { .. }),
            "expected ClaimLost, got {err}"
        );
        store
            .release_claim(&token)
            .expect("stale release is a no-op");
        assert!(
            store.try_claim_swarm("s1").expect("claim").is_none(),
            "the live claim survived a stale release"
        );

        store.release_claim(&retaken).expect("release");
    }

    pub(crate) fn terminal_swarms_are_not_claimable(store: &dyn SwarmStore) {
        let mut done = swarm("s1", "2020-01-01T00:00:00Z");
        done.run.status = SwarmStatus::Completed;
        store.save_swarm(&done).expect("save");
        assert!(
            store.try_claim_swarm("s1").expect("claim").is_none(),
            "a completed swarm is history, not claimable work"
        );
    }

    pub(crate) fn expired_claims_match_past_due_leases(store: &dyn SwarmStore) {
        store
            .save_swarm(&swarm("s1", "2020-01-01T00:00:00Z"))
            .expect("save");
        let token = store
            .try_claim_swarm("s1")
            .expect("claim")
            .expect("claim wins");
        assert!(
            store
                .expired_claims("2000-01-01T00:00:00Z")
                .expect("expired")
                .is_empty(),
            "a fresh lease is not past due"
        );
        let expired = store
            .expired_claims("2999-01-01T00:00:00Z")
            .expect("expired");
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].swarm_id, "s1");
        assert_eq!(expired[0].holder, token.holder);
    }

    pub(crate) fn events_are_append_only_and_ordered(store: &dyn SwarmStore) {
        assert_eq!(
            store
                .append_event(&event("s1", "swarm_created"))
                .expect("append"),
            1
        );
        assert_eq!(
            store
                .append_event(&event("s1", "swarm_started"))
                .expect("append"),
            2
        );
        assert_eq!(
            store
                .append_event(&event("s2", "swarm_created"))
                .expect("append"),
            3
        );

        let mut boxed = event("s1", "box_assigned");
        boxed.box_id = Some("box-2".to_string());
        boxed.payload = json!({"role": "critic"});
        assert_eq!(store.append_event(&boxed).expect("append"), 4);

        let s1 = store.list_events("s1").expect("list");
        assert_eq!(s1.len(), 3);
        assert_eq!(
            s1.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2, 4],
            "seq is monotonic across swarms and preserved per swarm"
        );
        assert_eq!(s1[1].kind, "swarm_started");
        assert_eq!(s1[2].box_id.as_deref(), Some("box-2"));
        assert_eq!(s1[2].payload, json!({"role": "critic"}));
        assert_eq!(store.list_events("s2").expect("list").len(), 1);
        assert!(store.list_events("nope").expect("list").is_empty());
    }

    pub(crate) fn delete_drops_events_and_claim(store: &dyn SwarmStore) {
        store
            .save_swarm(&swarm("s1", "2020-01-01T00:00:00Z"))
            .expect("save");
        store
            .append_event(&event("s1", "swarm_created"))
            .expect("append");
        let token = store
            .try_claim_swarm("s1")
            .expect("claim")
            .expect("claim wins");

        assert!(store.delete_swarm("s1").expect("delete"));
        assert!(
            store.list_events("s1").expect("list").is_empty(),
            "event history goes with the swarm"
        );
        let err = store
            .heartbeat_claim(&token)
            .expect_err("the claim went with the swarm");
        assert!(matches!(err, StoreError::ClaimLost { .. }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> InMemorySwarmStore {
        InMemorySwarmStore::new()
    }

    #[test]
    fn in_memory_crud_round_trip() {
        conformance::crud_round_trip(&store());
    }

    #[test]
    fn in_memory_list_is_newest_first_and_stable() {
        conformance::list_is_newest_first_and_stable(&store());
    }

    #[test]
    fn in_memory_save_guards_revision() {
        conformance::save_guards_revision(&store());
    }

    #[test]
    fn in_memory_claim_lifecycle() {
        conformance::claim_lifecycle(&store());
    }

    #[test]
    fn in_memory_terminal_swarms_are_not_claimable() {
        conformance::terminal_swarms_are_not_claimable(&store());
    }

    #[test]
    fn in_memory_expired_claims_match_past_due_leases() {
        conformance::expired_claims_match_past_due_leases(&store());
    }

    #[test]
    fn in_memory_events_are_append_only_and_ordered() {
        conformance::events_are_append_only_and_ordered(&store());
    }

    #[test]
    fn in_memory_delete_drops_events_and_claim() {
        conformance::delete_drops_events_and_claim(&store());
    }

    #[test]
    fn in_memory_backend_is_named() {
        assert_eq!(store().backend(), "in-memory");
    }

    #[test]
    fn budget_presets_expand_to_the_documented_triples() {
        assert_eq!(
            SwarmBudgetPreset::Quick.limits(),
            SwarmBudgetLimits {
                max_turns: 25,
                max_tokens: 250_000,
                max_cost_usd: 5.0,
            }
        );
        assert_eq!(
            SwarmBudgetPreset::Standard.limits(),
            SwarmBudgetLimits {
                max_turns: 100,
                max_tokens: 1_000_000,
                max_cost_usd: 20.0,
            }
        );
        assert_eq!(
            SwarmBudgetPreset::Marathon.limits(),
            SwarmBudgetLimits {
                max_turns: 400,
                max_tokens: 5_000_000,
                max_cost_usd: 100.0,
            }
        );
        // A custom triple resolves to itself, so consumers only read `limits`.
        let custom = SwarmBudgetLimits {
            max_turns: 7,
            max_tokens: 8,
            max_cost_usd: 9.0,
        };
        assert_eq!(SwarmBudget::Custom(custom).limits(), custom);
        // The default budget is the Standard preset.
        assert_eq!(
            SwarmBudget::default().limits(),
            SwarmBudgetPreset::Standard.limits()
        );
    }

    #[test]
    fn budget_preset_wire_names_match_the_serde_tags() {
        // `wire_name` exists so pickers can name a preset without a serde
        // round-trip. If the two ever diverge, a wizard would offer a name
        // the store cannot parse back.
        for preset in SwarmBudgetPreset::ALL {
            // A budget encodes as `{"preset": "<wire name>"}`; the picker only
            // ever handles the inner tag.
            let encoded = serde_json::to_value(SwarmBudget::Preset(preset)).expect("encode");
            assert_eq!(
                encoded.get("preset").and_then(serde_json::Value::as_str),
                Some(preset.wire_name()),
                "serde encoded {preset:?} as {encoded}"
            );
            assert_eq!(
                SwarmBudgetPreset::from_wire(preset.wire_name()),
                Some(preset)
            );
        }
        assert_eq!(SwarmBudgetPreset::from_wire("marathon-plus"), None);
    }

    #[test]
    fn default_roster_fills_every_layout_slot() {
        let roster = default_roster(DEFAULT_ROSTER_SIZE);
        assert_eq!(roster.len(), 4);
        assert_eq!(
            roster.iter().map(|b| b.slot).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert_eq!(roster[0].box_id, "box-1");
        assert!(
            roster.iter().all(|b| b.role.is_empty() && b.job.is_empty()),
            "a fresh roster is unassigned until the author fills it"
        );
    }

    #[test]
    fn status_terminality_and_labels() {
        assert!(!SwarmStatus::Created.is_terminal());
        assert!(!SwarmStatus::Running.is_terminal());
        assert!(
            !SwarmStatus::Paused {
                reason: SwarmPauseReason::DaemonRestart
            }
            .is_terminal(),
            "a restart-paused swarm is resumable, not finished"
        );
        assert!(SwarmStatus::Stopped.is_terminal());
        assert!(SwarmStatus::Completed.is_terminal());
        assert_eq!(
            SwarmStatus::Paused {
                reason: SwarmPauseReason::BudgetExhausted
            }
            .as_str(),
            "paused"
        );
    }

    #[test]
    fn pause_reason_survives_a_store_round_trip() {
        let s = store();
        let mut paused = conformance::swarm("s1", "2020-01-01T00:00:00Z");
        paused.run.status = SwarmStatus::Paused {
            reason: SwarmPauseReason::BudgetExhausted,
        };
        s.save_swarm(&paused).expect("save");
        assert_eq!(
            s.load_swarm("s1")
                .expect("load")
                .expect("present")
                .run
                .status,
            SwarmStatus::Paused {
                reason: SwarmPauseReason::BudgetExhausted
            }
        );
    }

    #[test]
    fn factory_builds_a_durable_store_under_the_data_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = build_swarm_store(tmp.path());
        assert_eq!(store.backend(), "sqlite");
        assert!(tmp.path().join("swarms").join("swarms.db").exists());
    }

    #[test]
    fn factory_falls_back_to_in_memory_when_the_path_is_unusable() {
        // A regular file where the swarms directory belongs makes create_dir_all
        // fail; the daemon must still get a working store.
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("swarms"), b"not a directory").expect("write blocker");
        let store = build_swarm_store(tmp.path());
        assert_eq!(store.backend(), "in-memory");
        // Still a real store, not a silent no-op.
        store
            .save_swarm(&conformance::swarm("s1", "2020-01-01T00:00:00Z"))
            .expect("fallback store accepts writes");
        assert!(store.load_swarm("s1").expect("load").is_some());
    }
}
