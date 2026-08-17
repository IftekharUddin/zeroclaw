//! In-memory coordination board for a running swarm.
//!
//! One entry per swarm holds every box's published state plus the live task
//! claims, guarded by a single lock so a claim is a real compare-and-set:
//! exactly one box wins a contested `task_key` and the losers are told who
//! holds it. Board state is ephemeral by construction — it lives for the run
//! and is removed wholesale by the reap cascade; the durable copy is the
//! [`BoardSnapshot`] the orchestrator persists into the swarm's run row.

use std::collections::BTreeMap;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// Broadcast capacity per swarm. Slow subscribers lag rather than block.
const BROADCAST_CAPACITY: usize = 128;

/// Maximum number of boards held at once, so a leaked registration cannot
/// grow the map without bound.
pub const MAX_BOARDS: usize = 64;

/// Maximum live claims one swarm may hold. Claims are model-driven, so the
/// ceiling is what keeps a looping box from exhausting memory.
pub const MAX_CLAIMS_PER_SWARM: usize = 512;

/// Maximum length of a published note.
pub const MAX_NOTE_LEN: usize = 2048;

/// Maximum length of a task key.
pub const MAX_TASK_KEY_LEN: usize = 256;

/// What a box says it is doing. A closed set: an unparseable status is
/// rejected rather than recorded as free text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoxStatus {
    /// Registered, nothing in flight.
    #[default]
    Idle,
    /// Working its claim or its job.
    Working,
    /// Stuck and waiting on something.
    Blocked,
    /// Finished its job.
    Done,
}

impl BoxStatus {
    /// Stable wire label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Done => "done",
        }
    }

    /// The inverse of [`Self::as_str`]; `None` for anything else.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "idle" => Some(Self::Idle),
            "working" => Some(Self::Working),
            "blocked" => Some(Self::Blocked),
            "done" => Some(Self::Done),
            _ => None,
        }
    }

    /// Every accepted label, for schemas and error text.
    #[must_use]
    pub const fn all() -> [&'static str; 4] {
        ["idle", "working", "blocked", "done"]
    }
}

/// One box's published state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoxState {
    pub status: BoxStatus,
    /// The task key this box currently holds, if any.
    pub claim: Option<String>,
    pub note: String,
}

/// A change to the board, fanned out to subscribers (the pane renderer, the
/// orchestrator's persistence loop).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "event")]
pub enum BoardEvent {
    Published {
        box_id: String,
        state: BoxState,
    },
    Claimed {
        box_id: String,
        task_key: String,
    },
    Released {
        box_id: String,
        task_key: String,
    },
    /// The board was torn down; no further events follow.
    Reaped,
}

/// Result of a compare-and-set claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum ClaimOutcome {
    /// The caller holds the key (a re-claim by the current holder is granted
    /// again rather than treated as contention).
    Granted,
    /// Someone else holds it; `holder` is the box that won.
    Held { holder: String },
}

/// Result of releasing a claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum ReleaseOutcome {
    Released,
    /// Refused: the key is held by another box. Releasing is not a steal.
    NotHolder {
        holder: String,
    },
    /// Nobody held the key.
    Unclaimed,
}

/// The persistable view of a board.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardSnapshot {
    /// `box_id` -> published state.
    pub boxes: BTreeMap<String, BoxState>,
    /// `task_key` -> holding `box_id`.
    pub claims: BTreeMap<String, String>,
}

/// Why a board operation was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BoardError {
    #[error("no board registered for swarm {swarm_id:?}")]
    UnknownSwarm { swarm_id: String },
    #[error("box {box_id:?} is not on swarm {swarm_id:?}'s board")]
    UnknownBox { swarm_id: String, box_id: String },
    #[error("cannot register another swarm board: the limit of {MAX_BOARDS} is reached")]
    BoardLimit,
    #[error("swarm {swarm_id:?} already holds the maximum of {MAX_CLAIMS_PER_SWARM} claims")]
    ClaimLimit { swarm_id: String },
    #[error("note is {len} bytes; the maximum is {MAX_NOTE_LEN}")]
    NoteTooLong { len: usize },
    #[error("task key is invalid: {reason}")]
    TaskKey { reason: String },
}

struct BoardEntry {
    boxes: BTreeMap<String, BoxState>,
    claims: BTreeMap<String, String>,
    tx: broadcast::Sender<BoardEvent>,
}

impl BoardEntry {
    fn snapshot(&self) -> BoardSnapshot {
        BoardSnapshot {
            boxes: self.boxes.clone(),
            claims: self.claims.clone(),
        }
    }
}

/// Shared per-swarm coordination state. Cheaply cloneable (wraps an `Arc`);
/// every clone sees the same boards.
#[derive(Clone, Default)]
pub struct SwarmStateBoard {
    inner: Arc<RwLock<BTreeMap<String, BoardEntry>>>,
}

impl SwarmStateBoard {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or re-register, on resume) a swarm's boxes. Every named box
    /// starts idle and unclaimed; nothing outside this roster can ever publish
    /// or claim, which is what stops a box from speaking as a sibling.
    pub fn register(
        &self,
        swarm_id: &str,
        box_ids: impl IntoIterator<Item = String>,
    ) -> Result<(), BoardError> {
        let boxes: BTreeMap<String, BoxState> = box_ids
            .into_iter()
            .map(|box_id| (box_id, BoxState::default()))
            .collect();

        let mut boards = self.inner.write();
        if !boards.contains_key(swarm_id) && boards.len() >= MAX_BOARDS {
            return Err(BoardError::BoardLimit);
        }
        boards.insert(
            swarm_id.to_string(),
            BoardEntry {
                boxes,
                claims: BTreeMap::new(),
                tx: broadcast::channel(BROADCAST_CAPACITY).0,
            },
        );
        Ok(())
    }

    /// Record what a box is doing. `box_id` is bound by the caller, never
    /// supplied by the model.
    pub fn publish(
        &self,
        swarm_id: &str,
        box_id: &str,
        status: BoxStatus,
        note: &str,
    ) -> Result<BoxState, BoardError> {
        if note.len() > MAX_NOTE_LEN {
            return Err(BoardError::NoteTooLong { len: note.len() });
        }

        let mut boards = self.inner.write();
        let entry = boards
            .get_mut(swarm_id)
            .ok_or_else(|| BoardError::UnknownSwarm {
                swarm_id: swarm_id.to_string(),
            })?;
        let state = entry
            .boxes
            .get_mut(box_id)
            .ok_or_else(|| BoardError::UnknownBox {
                swarm_id: swarm_id.to_string(),
                box_id: box_id.to_string(),
            })?;
        state.status = status;
        state.note = note.to_string();
        let published = state.clone();
        let _ = entry.tx.send(BoardEvent::Published {
            box_id: box_id.to_string(),
            state: published.clone(),
        });
        Ok(published)
    }

    /// Compare-and-set on `task_key`. Under one write lock, so two boxes
    /// racing the same key produce exactly one [`ClaimOutcome::Granted`].
    pub fn claim(
        &self,
        swarm_id: &str,
        box_id: &str,
        task_key: &str,
    ) -> Result<ClaimOutcome, BoardError> {
        validate_task_key(task_key)?;

        let mut boards = self.inner.write();
        let entry = boards
            .get_mut(swarm_id)
            .ok_or_else(|| BoardError::UnknownSwarm {
                swarm_id: swarm_id.to_string(),
            })?;
        if !entry.boxes.contains_key(box_id) {
            return Err(BoardError::UnknownBox {
                swarm_id: swarm_id.to_string(),
                box_id: box_id.to_string(),
            });
        }
        if let Some(holder) = entry.claims.get(task_key) {
            if holder == box_id {
                return Ok(ClaimOutcome::Granted);
            }
            return Ok(ClaimOutcome::Held {
                holder: holder.clone(),
            });
        }
        if entry.claims.len() >= MAX_CLAIMS_PER_SWARM {
            return Err(BoardError::ClaimLimit {
                swarm_id: swarm_id.to_string(),
            });
        }
        entry
            .claims
            .insert(task_key.to_string(), box_id.to_string());
        if let Some(state) = entry.boxes.get_mut(box_id) {
            state.claim = Some(task_key.to_string());
        }
        let _ = entry.tx.send(BoardEvent::Claimed {
            box_id: box_id.to_string(),
            task_key: task_key.to_string(),
        });
        Ok(ClaimOutcome::Granted)
    }

    /// Drop a claim. Only its holder may release it — a non-holder is refused
    /// with the current holder rather than freeing someone else's work.
    pub fn release(
        &self,
        swarm_id: &str,
        box_id: &str,
        task_key: &str,
    ) -> Result<ReleaseOutcome, BoardError> {
        validate_task_key(task_key)?;

        let mut boards = self.inner.write();
        let entry = boards
            .get_mut(swarm_id)
            .ok_or_else(|| BoardError::UnknownSwarm {
                swarm_id: swarm_id.to_string(),
            })?;
        if !entry.boxes.contains_key(box_id) {
            return Err(BoardError::UnknownBox {
                swarm_id: swarm_id.to_string(),
                box_id: box_id.to_string(),
            });
        }
        match entry.claims.get(task_key) {
            None => Ok(ReleaseOutcome::Unclaimed),
            Some(holder) if holder != box_id => Ok(ReleaseOutcome::NotHolder {
                holder: holder.clone(),
            }),
            Some(_) => {
                entry.claims.remove(task_key);
                if let Some(state) = entry.boxes.get_mut(box_id)
                    && state.claim.as_deref() == Some(task_key)
                {
                    state.claim = None;
                }
                let _ = entry.tx.send(BoardEvent::Released {
                    box_id: box_id.to_string(),
                    task_key: task_key.to_string(),
                });
                Ok(ReleaseOutcome::Released)
            }
        }
    }

    /// The board as the orchestrator persists it into the run row, and as the
    /// `read_board` tool action reports it.
    #[must_use]
    pub fn snapshot(&self, swarm_id: &str) -> Option<BoardSnapshot> {
        self.inner.read().get(swarm_id).map(BoardEntry::snapshot)
    }

    /// Subscribe to this swarm's change feed.
    pub fn subscribe(&self, swarm_id: &str) -> Result<broadcast::Receiver<BoardEvent>, BoardError> {
        self.inner
            .read()
            .get(swarm_id)
            .map(|entry| entry.tx.subscribe())
            .ok_or_else(|| BoardError::UnknownSwarm {
                swarm_id: swarm_id.to_string(),
            })
    }

    /// Tear the board down, returning what it held so the caller can account
    /// for it. Subscribers see [`BoardEvent::Reaped`] first.
    pub fn remove(&self, swarm_id: &str) -> Option<BoardSnapshot> {
        let mut boards = self.inner.write();
        let entry = boards.remove(swarm_id)?;
        let _ = entry.tx.send(BoardEvent::Reaped);
        Some(entry.snapshot())
    }

    /// Number of live boards. The reap tests assert this returns to zero.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }
}

fn validate_task_key(task_key: &str) -> Result<(), BoardError> {
    if task_key.trim().is_empty() {
        return Err(BoardError::TaskKey {
            reason: "must not be empty".to_string(),
        });
    }
    if task_key.len() > MAX_TASK_KEY_LEN {
        return Err(BoardError::TaskKey {
            reason: format!(
                "{} bytes exceeds the maximum of {MAX_TASK_KEY_LEN}",
                task_key.len()
            ),
        });
    }
    if let Some(ch) = task_key.chars().find(|ch| ch.is_control()) {
        return Err(BoardError::TaskKey {
            reason: format!("contains the control character {ch:?}"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board_with_two_boxes() -> SwarmStateBoard {
        let board = SwarmStateBoard::new();
        board
            .register("s1", ["box-1".to_string(), "box-2".to_string()])
            .expect("registering the first board must succeed");
        board
    }

    #[test]
    fn publish_records_status_and_note() {
        let board = board_with_two_boxes();
        let state = board
            .publish("s1", "box-1", BoxStatus::Working, "reading the RFC")
            .expect("publishing as a registered box must succeed");
        assert_eq!(state.status, BoxStatus::Working);
        assert_eq!(state.note, "reading the RFC");
        let snapshot = board.snapshot("s1").expect("board must exist");
        assert_eq!(snapshot.boxes["box-1"].status, BoxStatus::Working);
        assert_eq!(snapshot.boxes["box-2"], BoxState::default());
    }

    #[test]
    fn publish_rejects_an_unregistered_box() {
        let board = board_with_two_boxes();
        let err = board
            .publish("s1", "box-9", BoxStatus::Done, "")
            .expect_err("an unregistered box must be refused");
        assert_eq!(
            err,
            BoardError::UnknownBox {
                swarm_id: "s1".to_string(),
                box_id: "box-9".to_string(),
            }
        );
    }

    #[test]
    fn publish_rejects_an_oversized_note() {
        let board = board_with_two_boxes();
        let note = "x".repeat(MAX_NOTE_LEN + 1);
        assert!(matches!(
            board.publish("s1", "box-1", BoxStatus::Idle, &note),
            Err(BoardError::NoteTooLong { .. })
        ));
    }

    #[test]
    fn claim_is_exclusive_and_names_the_holder() {
        let board = board_with_two_boxes();
        assert_eq!(
            board.claim("s1", "box-1", "task-a"),
            Ok(ClaimOutcome::Granted)
        );
        assert_eq!(
            board.claim("s1", "box-2", "task-a"),
            Ok(ClaimOutcome::Held {
                holder: "box-1".to_string()
            })
        );
        // The holder re-claiming its own key is idempotent, not contention.
        assert_eq!(
            board.claim("s1", "box-1", "task-a"),
            Ok(ClaimOutcome::Granted)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn raced_claims_produce_exactly_one_winner() {
        for _ in 0..64 {
            let board = board_with_two_boxes();
            let barrier = Arc::new(tokio::sync::Barrier::new(2));

            let mut tasks = Vec::new();
            for box_id in ["box-1", "box-2"] {
                let board = board.clone();
                let barrier = Arc::clone(&barrier);
                tasks.push(zeroclaw_spawn::spawn!(async move {
                    barrier.wait().await;
                    (box_id, board.claim("s1", box_id, "task-a"))
                }));
            }

            let mut winners = Vec::new();
            let mut losers = Vec::new();
            for task in tasks {
                let (box_id, outcome) = task.await.expect("claim task must not panic");
                match outcome.expect("claiming on a registered board must not error") {
                    ClaimOutcome::Granted => winners.push(box_id.to_string()),
                    ClaimOutcome::Held { holder } => losers.push((box_id.to_string(), holder)),
                }
            }

            assert_eq!(winners.len(), 1, "exactly one box may win a raced claim");
            assert_eq!(losers.len(), 1, "the other box must lose");
            let (loser, holder) = &losers[0];
            assert_eq!(
                holder, &winners[0],
                "the loser must be told who actually holds the key"
            );
            assert_ne!(loser, holder, "a box cannot both lose and hold");

            let snapshot = board.snapshot("s1").expect("board must exist");
            assert_eq!(snapshot.claims.len(), 1);
            assert_eq!(snapshot.claims["task-a"], winners[0]);
        }
    }

    #[test]
    fn release_refuses_a_non_holder_and_frees_for_the_holder() {
        let board = board_with_two_boxes();
        assert_eq!(
            board.claim("s1", "box-1", "task-a"),
            Ok(ClaimOutcome::Granted)
        );
        assert_eq!(
            board.release("s1", "box-2", "task-a"),
            Ok(ReleaseOutcome::NotHolder {
                holder: "box-1".to_string()
            }),
            "releasing another box's claim must never be a steal"
        );
        assert_eq!(
            board.release("s1", "box-1", "task-a"),
            Ok(ReleaseOutcome::Released)
        );
        assert_eq!(
            board.release("s1", "box-1", "task-a"),
            Ok(ReleaseOutcome::Unclaimed)
        );
        let snapshot = board.snapshot("s1").expect("board must exist");
        assert!(snapshot.claims.is_empty());
        assert_eq!(snapshot.boxes["box-1"].claim, None);
    }

    #[test]
    fn claim_and_release_reject_unusable_task_keys() {
        let board = board_with_two_boxes();
        assert!(matches!(
            board.claim("s1", "box-1", "  "),
            Err(BoardError::TaskKey { .. })
        ));
        assert!(matches!(
            board.claim("s1", "box-1", &"k".repeat(MAX_TASK_KEY_LEN + 1)),
            Err(BoardError::TaskKey { .. })
        ));
        assert!(matches!(
            board.release("s1", "box-1", "bad\nkey"),
            Err(BoardError::TaskKey { .. })
        ));
    }

    #[test]
    fn operations_on_an_unregistered_swarm_are_refused() {
        let board = SwarmStateBoard::new();
        let expected = BoardError::UnknownSwarm {
            swarm_id: "ghost".to_string(),
        };
        assert_eq!(
            board.publish("ghost", "box-1", BoxStatus::Idle, ""),
            Err(expected)
        );
        assert!(board.snapshot("ghost").is_none());
        assert!(board.subscribe("ghost").is_err());
    }

    #[tokio::test]
    async fn subscribers_see_the_change_feed() {
        let board = board_with_two_boxes();
        let mut rx = board.subscribe("s1").expect("board must exist");

        board
            .publish("s1", "box-1", BoxStatus::Working, "on it")
            .expect("publish must succeed");
        board.claim("s1", "box-1", "task-a").expect("claim");
        board.release("s1", "box-1", "task-a").expect("release");
        board.remove("s1");

        let events: Vec<BoardEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(events.len(), 4, "got {events:?}");
        assert!(matches!(events[0], BoardEvent::Published { .. }));
        assert!(matches!(events[1], BoardEvent::Claimed { .. }));
        assert!(matches!(events[2], BoardEvent::Released { .. }));
        assert_eq!(events[3], BoardEvent::Reaped);
    }

    #[test]
    fn remove_returns_what_it_dropped() {
        let board = board_with_two_boxes();
        board.claim("s1", "box-1", "task-a").expect("claim");
        let dropped = board.remove("s1").expect("board must exist");
        assert_eq!(dropped.claims.len(), 1);
        assert!(board.is_empty());
        assert!(board.remove("s1").is_none());
    }

    #[test]
    fn registration_is_bounded() {
        let board = SwarmStateBoard::new();
        for i in 0..MAX_BOARDS {
            board
                .register(&format!("s{i}"), ["box-1".to_string()])
                .expect("registrations below the limit must succeed");
        }
        assert_eq!(board.register("overflow", []), Err(BoardError::BoardLimit));
        // Re-registering an existing swarm is not a new board.
        assert_eq!(board.register("s0", ["box-1".to_string()]), Ok(()));
        assert_eq!(board.len(), MAX_BOARDS);
    }

    #[test]
    fn claims_are_bounded_per_swarm() {
        let board = board_with_two_boxes();
        for i in 0..MAX_CLAIMS_PER_SWARM {
            assert_eq!(
                board.claim("s1", "box-1", &format!("task-{i}")),
                Ok(ClaimOutcome::Granted)
            );
        }
        assert!(matches!(
            board.claim("s1", "box-1", "one-too-many"),
            Err(BoardError::ClaimLimit { .. })
        ));
    }

    #[test]
    fn box_status_labels_round_trip() {
        for label in BoxStatus::all() {
            let parsed = BoxStatus::parse(label).expect("every advertised label must parse");
            assert_eq!(parsed.as_str(), label);
        }
        assert_eq!(BoxStatus::parse("escalated"), None);
    }
}
