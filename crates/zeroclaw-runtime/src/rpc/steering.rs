//! Mid-turn steering for the local RPC path.
//!
//! A steering message is text a caller pushes into a turn that is *already*
//! streaming, so the agent folds it in at the next round boundary instead of
//! the caller having to cancel and re-prompt. The gateway WebSocket path has
//! always had this (a plain FIFO channel drained by the running turn); this
//! module generalizes it to the JSON-RPC socket, where the turn is driven by
//! [`crate::rpc::turn::execute_turn`] and there is no per-connection read loop
//! to hang a channel off.
//!
//! ## Shape
//!
//! [`crate::agent::agent::Agent::turn_streamed_with_steering_state`] consumes a
//! plain `tokio::sync::mpsc::Receiver<String>` and drains it, non-blocking, at
//! every round boundary. That contract is fixed and shared with the gateway, so
//! everything this module adds lives on the producing side: [`SteeringQueue`]
//! owns the buffer of messages it has *not yet* handed over, and a pump task
//! moves them into the turn's channel in priority order.
//!
//! ## Ordering semantics
//!
//! * Messages the queue still owns are ordered [`SteeringClass::User`] first,
//!   then [`SteeringClass::Orchestrator`]. Arrival order (FIFO) breaks ties
//!   within a class, so a single-class workload behaves exactly like the
//!   gateway's FIFO channel.
//! * The pump hands over a batch only once the running turn has consumed
//!   everything handed over before it. Anything that arrives while the previous
//!   batch is still outstanding is folded into the next batch and re-ordered
//!   with it. That waiting window is what makes the class ordering observable:
//!   once a message is in the turn's channel it is committed, and no later
//!   high-priority message can jump ahead of it.
//! * Delivery is therefore asynchronous. [`SteeringQueue::send`] returns as soon
//!   as the message is queued, not once the turn has observed it. A message
//!   queued in the instant a turn is finishing can still miss that turn;
//!   [`SteeringQueue::close`] reports how many were still queued so the caller
//!   logs the loss instead of swallowing it.

use std::collections::VecDeque;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, mpsc};

/// Capacity of the handoff channel between a [`SteeringQueue`] and the turn
/// draining it. Matches the gateway WebSocket path's steering channel so both
/// surfaces absorb the same burst before backpressure shows up.
pub const STEERING_CHANNEL_CAPACITY: usize = 32;

/// Upper bound on messages a [`SteeringQueue`] will hold on top of whatever is
/// already committed to the handoff channel. A caller that blows through this
/// gets [`SteeringError::QueueFull`] rather than an unbounded buffer.
pub const STEERING_MAX_PENDING: usize = 64;

/// Priority class of a steering message.
///
/// Variants are declared highest priority first and the derived [`Ord`] follows
/// declaration order, so `User < Orchestrator` and a plain sort puts human
/// interjections ahead of programmatic ones.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize, Hash,
)]
#[serde(rename_all = "snake_case")]
pub enum SteeringClass {
    /// A human interjecting mid-turn. The default for every existing caller:
    /// the gateway's WebSocket steering and any client that omits the field
    /// land here, so generalizing the RPC path changes no existing ordering.
    #[default]
    User,
    /// A programmatic steer from an orchestrator or automation. Yields to
    /// queued `User` messages so a person talking over a machine always wins.
    Orchestrator,
}

impl SteeringClass {
    /// Every class in drain order, highest priority first.
    pub const ALL: [Self; 2] = [Self::User, Self::Orchestrator];

    /// Stable token for wire payloads and log attributes.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Orchestrator => "orchestrator",
        }
    }

    /// Index of this class's lane in [`SteeringBuffer`]. Mirrors [`Self::ALL`].
    const fn lane(self) -> usize {
        self as usize
    }
}

/// Why a steering message was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteeringError {
    /// No turn is in flight for the session, so there is nothing to steer.
    NoTurnInFlight,
    /// The caller aimed at a turn that has already ended. Dropping the message
    /// is the point: a steer written for turn N must never leak into turn N+1,
    /// where it would read as an unexplained user message.
    StaleGeneration { requested: u64, current: u64 },
    /// The queue is saturated; the caller should back off and retry.
    QueueFull,
    /// The turn ended (or was torn down) while the queue was still registered.
    TurnEnded,
}

impl std::fmt::Display for SteeringError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoTurnInFlight => write!(f, "no turn in flight for this session"),
            Self::StaleGeneration { requested, current } => write!(
                f,
                "steering message targets turn generation {requested}; the session is on generation {current}"
            ),
            Self::QueueFull => write!(f, "steering queue is full for the running turn"),
            Self::TurnEnded => write!(f, "the running turn is no longer accepting steering"),
        }
    }
}

impl std::error::Error for SteeringError {}

/// Per-class lanes of messages the queue still owns.
#[derive(Debug, Default)]
struct SteeringBuffer {
    /// One lane per [`SteeringClass`], indexed by [`SteeringClass::lane`].
    lanes: [VecDeque<String>; SteeringClass::ALL.len()],
    closed: bool,
}

impl SteeringBuffer {
    fn len(&self) -> usize {
        self.lanes.iter().map(VecDeque::len).sum()
    }

    fn push(&mut self, class: SteeringClass, message: String) {
        self.lanes[class.lane()].push_back(message);
    }

    /// Take up to `max` messages in drain order: every queued `User` message
    /// (oldest first) before any `Orchestrator` message.
    fn drain_prioritized(&mut self, max: usize) -> Vec<String> {
        let mut batch = Vec::with_capacity(max.min(self.len()));
        for lane in &mut self.lanes {
            while batch.len() < max {
                match lane.pop_front() {
                    Some(message) => batch.push(message),
                    None => break,
                }
            }
            if batch.len() >= max {
                break;
            }
        }
        batch
    }
}

/// Producer side of mid-turn steering for one turn.
///
/// Created per turn by [`crate::rpc::turn::execute_turn`]'s caller, registered
/// on the session store under that turn's generation, and dropped when the turn
/// ends. See the module docs for the ordering and delivery contract.
pub struct SteeringQueue {
    tx: mpsc::Sender<String>,
    channel_capacity: usize,
    max_pending: usize,
    buffer: parking_lot::Mutex<SteeringBuffer>,
    notify: Notify,
}

impl SteeringQueue {
    /// Start a queue bound to a fresh handoff channel. The returned receiver is
    /// what the turn drains; the queue is dead once it is dropped.
    pub fn start() -> (Arc<Self>, mpsc::Receiver<String>) {
        Self::start_with_capacity(STEERING_CHANNEL_CAPACITY, STEERING_MAX_PENDING)
    }

    /// [`Self::start`] with explicit bounds. Tests use a tiny handoff channel to
    /// make the "previous batch still outstanding" window deterministic.
    pub(crate) fn start_with_capacity(
        channel_capacity: usize,
        max_pending: usize,
    ) -> (Arc<Self>, mpsc::Receiver<String>) {
        let channel_capacity = channel_capacity.max(1);
        let (tx, rx) = mpsc::channel(channel_capacity);
        let queue = Arc::new(Self {
            tx,
            channel_capacity,
            max_pending,
            buffer: parking_lot::Mutex::new(SteeringBuffer::default()),
            notify: Notify::new(),
        });
        let pump = Arc::clone(&queue);
        zeroclaw_spawn::spawn!(pump.pump());
        (queue, rx)
    }

    /// Queue a steering message for the running turn.
    ///
    /// Returns as soon as the message is buffered. The pump forwards it to the
    /// turn once the turn has consumed the previous batch; see the module docs
    /// for why that delay is what makes [`SteeringClass`] ordering meaningful.
    pub fn send(&self, class: SteeringClass, message: String) -> Result<(), SteeringError> {
        if self.tx.is_closed() {
            return Err(SteeringError::TurnEnded);
        }
        {
            let mut buffer = self.buffer.lock();
            if buffer.closed {
                return Err(SteeringError::TurnEnded);
            }
            if buffer.len() >= self.max_pending {
                return Err(SteeringError::QueueFull);
            }
            buffer.push(class, message);
        }
        self.notify.notify_one();
        Ok(())
    }

    /// Messages the queue still owns, i.e. queued but not yet handed to the
    /// turn. Excludes anything already committed to the handoff channel.
    pub fn pending(&self) -> usize {
        self.buffer.lock().len()
    }

    /// Stop accepting messages and discard whatever is still queued. Returns
    /// the number discarded so the turn teardown can log a real loss instead of
    /// dropping it silently. Idempotent; subsequent calls return 0.
    pub fn close(&self) -> usize {
        let undelivered = {
            let mut buffer = self.buffer.lock();
            buffer.closed = true;
            let undelivered = buffer.len();
            for lane in &mut buffer.lanes {
                lane.clear();
            }
            undelivered
        };
        self.notify.notify_one();
        undelivered
    }

    /// Forward buffered messages into the turn's channel, priority order first,
    /// one batch at a time.
    ///
    /// Reserving the whole channel is the throttle: it only completes once the
    /// turn has drained every message handed over previously, so messages that
    /// arrive in the meantime accumulate here where they can still be
    /// re-ordered. Exits when the turn's receiver is dropped or the queue is
    /// closed.
    async fn pump(self: Arc<Self>) {
        loop {
            let pending = {
                let buffer = self.buffer.lock();
                if buffer.closed {
                    return;
                }
                buffer.len()
            };
            if pending == 0 {
                self.notify.notified().await;
                continue;
            }
            let Ok(permits) = self.tx.reserve_many(self.channel_capacity).await else {
                return;
            };
            let batch = {
                let mut buffer = self.buffer.lock();
                if buffer.closed {
                    return;
                }
                buffer.drain_prioritized(self.channel_capacity)
            };
            for (permit, message) in permits.zip(batch) {
                permit.send(message);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bounded spin so a test can wait on the pump without a sleep. The pump is
    /// a separate task, so the only way to observe a handoff is to yield to it.
    async fn wait_until_drained(queue: &SteeringQueue) {
        for _ in 0..1_000 {
            if queue.pending() == 0 {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("steering pump never handed the queued messages to the turn");
    }

    #[test]
    fn class_order_puts_user_ahead_of_orchestrator() {
        assert!(
            SteeringClass::User < SteeringClass::Orchestrator,
            "the derived Ord is the priority order the buffer relies on; \
             reordering the variants silently demotes human interjections"
        );
        assert_eq!(
            SteeringClass::ALL,
            [SteeringClass::User, SteeringClass::Orchestrator]
        );
        assert_eq!(SteeringClass::default(), SteeringClass::User);
        assert_eq!(SteeringClass::User.lane(), 0);
        assert_eq!(SteeringClass::Orchestrator.lane(), 1);
    }

    #[test]
    fn class_round_trips_over_the_wire() {
        for class in SteeringClass::ALL {
            let encoded = serde_json::to_value(class).expect("class serializes");
            assert_eq!(
                encoded,
                serde_json::Value::String(class.as_str().to_string())
            );
            let decoded: SteeringClass =
                serde_json::from_value(encoded).expect("class deserializes");
            assert_eq!(decoded, class);
        }
    }

    #[test]
    fn buffer_drains_user_class_before_orchestrator() {
        let mut buffer = SteeringBuffer::default();
        buffer.push(SteeringClass::Orchestrator, "o1".into());
        buffer.push(SteeringClass::User, "u1".into());
        buffer.push(SteeringClass::Orchestrator, "o2".into());
        buffer.push(SteeringClass::User, "u2".into());

        assert_eq!(
            buffer.drain_prioritized(16),
            vec!["u1", "u2", "o1", "o2"],
            "every queued User message must precede every Orchestrator one, \
             FIFO within each class"
        );
        assert_eq!(buffer.len(), 0);
    }

    #[test]
    fn buffer_respects_the_batch_bound_and_keeps_the_remainder() {
        let mut buffer = SteeringBuffer::default();
        buffer.push(SteeringClass::Orchestrator, "o1".into());
        buffer.push(SteeringClass::User, "u1".into());
        buffer.push(SteeringClass::User, "u2".into());

        assert_eq!(buffer.drain_prioritized(2), vec!["u1", "u2"]);
        assert_eq!(
            buffer.drain_prioritized(2),
            vec!["o1"],
            "a bounded batch must leave the rest queued, not drop it"
        );
    }

    #[tokio::test]
    async fn queue_hands_a_single_message_to_the_turn() {
        let (queue, mut rx) = SteeringQueue::start();
        queue
            .send(SteeringClass::User, "keep going".into())
            .expect("a live queue accepts a steer");
        assert_eq!(rx.recv().await.as_deref(), Some("keep going"));
        assert_eq!(queue.pending(), 0);
    }

    #[tokio::test]
    async fn queued_user_message_overtakes_a_queued_orchestrator_message() {
        // Handoff capacity of 1: the first message occupies the channel, so the
        // pump is parked until the turn drains it. Both later messages are
        // therefore provably still owned by the queue when it re-orders them —
        // no scheduler assumption, the channel is simply full.
        let (queue, mut rx) = SteeringQueue::start_with_capacity(1, 8);
        queue
            .send(SteeringClass::Orchestrator, "committed".into())
            .expect("live queue");
        wait_until_drained(&queue).await;

        queue
            .send(SteeringClass::Orchestrator, "o1".into())
            .expect("live queue");
        queue
            .send(SteeringClass::User, "u1".into())
            .expect("live queue");
        assert_eq!(queue.pending(), 2, "the full channel must hold both back");

        assert_eq!(
            rx.recv().await.as_deref(),
            Some("committed"),
            "a message already handed to the turn is committed and cannot be \
             overtaken"
        );
        assert_eq!(
            rx.recv().await.as_deref(),
            Some("u1"),
            "the User-class steer must jump the queued Orchestrator one"
        );
        assert_eq!(rx.recv().await.as_deref(), Some("o1"));
    }

    #[tokio::test]
    async fn send_after_the_turn_receiver_is_dropped_reports_turn_ended() {
        let (queue, rx) = SteeringQueue::start();
        drop(rx);
        assert_eq!(
            queue.send(SteeringClass::User, "too late".into()),
            Err(SteeringError::TurnEnded)
        );
    }

    #[tokio::test]
    async fn send_after_close_reports_turn_ended_and_close_counts_the_loss() {
        let (queue, mut rx) = SteeringQueue::start_with_capacity(1, 8);
        queue
            .send(SteeringClass::User, "committed".into())
            .expect("live queue");
        wait_until_drained(&queue).await;
        queue
            .send(SteeringClass::User, "stranded".into())
            .expect("live queue");

        assert_eq!(
            queue.close(),
            1,
            "close must report the still-queued message so the turn teardown \
             can log the loss rather than swallow it"
        );
        assert_eq!(queue.close(), 0, "close is idempotent");
        assert_eq!(
            queue.send(SteeringClass::User, "after".into()),
            Err(SteeringError::TurnEnded)
        );
        // The already-committed message stays readable by the turn.
        assert_eq!(rx.recv().await.as_deref(), Some("committed"));
    }

    #[tokio::test]
    async fn queue_rejects_messages_past_the_pending_bound() {
        let (queue, _rx) = SteeringQueue::start_with_capacity(1, 2);
        // Capacity-1 channel: one message is committed, the rest pile up in the
        // buffer until the bound trips.
        for i in 0..8 {
            let outcome = queue.send(SteeringClass::User, format!("m{i}"));
            if outcome == Err(SteeringError::QueueFull) {
                return;
            }
            assert_eq!(outcome, Ok(()), "unexpected refusal for m{i}");
            tokio::task::yield_now().await;
        }
        panic!("an unbounded steering buffer is a memory-growth path; the queue must refuse");
    }
}
