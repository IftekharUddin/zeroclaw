//! The live multi-box canvas's pure state.
//!
//! Nothing here touches a terminal or a socket. It folds the two live streams —
//! per-box `swarm/update` turn events and `swarm/board` transitions — into a
//! grid of cells, and turns key presses into the run-control, chat, and
//! layout-write [`Effect`]s the driver performs. Keeping it `self`-only is what
//! lets the whole canvas — coalescing, focus, gap handling, header-edit → save —
//! be tested without a TTY or a daemon.

use std::collections::{BTreeMap, VecDeque};

use zeroclaw_runtime::rpc::types::{
    SessionUpdateEvent, SwarmChatResult, SwarmUpdate, TurnCompletionOutcome,
};
use zeroclaw_runtime::swarm::board::{BoardEvent, BoxStatus};
use zeroclaw_runtime::swarm::store::{
    BoxSpec, PersistedSwarm, SwarmBudgetLimits, SwarmSpend, SwarmStatus,
};

use super::state::{Direction, Effect, Input};

/// Per-box scrollback cap. Beyond this the oldest lines are evicted and the
/// cell flags a gap, so a dropped tail is visible rather than silent.
const STREAM_CAP: usize = 200;
/// Cap on the swarm-level broadcast feed.
const FEED_CAP: usize = 100;
/// Ceiling on one coalesced line so a chunk stream with no newline cannot grow
/// a cell's partial line without bound.
const MAX_LINE_CHARS: usize = 2000;

/// What a coalesced stream line is, so the renderer can tint it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// Model output (`agent_message_chunk`).
    Message,
    /// Model reasoning (`agent_thought_chunk`).
    Thought,
    /// A tool call or result.
    Tool,
    /// Client-rendered chrome: turn completion, a plan, a trim marker, a gap.
    System,
}

/// One line in a box's scrollback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamLine {
    pub kind: LineKind,
    pub text: String,
}

/// One box's coalesced activity stream. Chunk events accrete into the tail
/// partial line until a newline or a different event kind flushes it, so the
/// scrollback reads as whole lines rather than a blizzard of fragments.
#[derive(Debug, Default)]
pub struct BoxStream {
    lines: VecDeque<StreamLine>,
    /// The tail line still accreting chunks: its kind and text so far.
    partial: Option<(LineKind, String)>,
    /// The ring buffer evicted content that was never shown.
    gap: bool,
}

impl BoxStream {
    fn apply(&mut self, event: &SessionUpdateEvent) {
        match event {
            SessionUpdateEvent::AgentMessageChunk { text, .. } => {
                self.append_chunk(LineKind::Message, text);
            }
            SessionUpdateEvent::AgentThoughtChunk { text, .. } => {
                self.append_chunk(LineKind::Thought, text);
            }
            SessionUpdateEvent::ToolCall { name, .. } => {
                self.flush_partial();
                self.push(
                    LineKind::Tool,
                    crate::ta("cli-swarm-tui-line-tool", &[("tool", name)], "tool"),
                );
            }
            SessionUpdateEvent::ToolResult { name, .. } => {
                self.flush_partial();
                self.push(
                    LineKind::Tool,
                    crate::ta(
                        "cli-swarm-tui-line-tool-result",
                        &[("tool", name)],
                        "tool done",
                    ),
                );
            }
            SessionUpdateEvent::ApprovalRequest { tool_name, .. } => {
                self.flush_partial();
                self.push(
                    LineKind::System,
                    crate::ta(
                        "cli-swarm-tui-line-approval",
                        &[("tool", tool_name)],
                        "approval",
                    ),
                );
            }
            SessionUpdateEvent::Plan { entries, .. } => {
                self.flush_partial();
                let count = entries.len().to_string();
                self.push(
                    LineKind::System,
                    crate::ta("cli-swarm-tui-line-plan", &[("count", &count)], "plan"),
                );
            }
            SessionUpdateEvent::TurnComplete { outcome, .. } => {
                self.flush_partial();
                let key = match outcome {
                    TurnCompletionOutcome::Completed => "cli-swarm-tui-line-turn-done",
                    TurnCompletionOutcome::Cancelled => "cli-swarm-tui-line-turn-cancelled",
                    TurnCompletionOutcome::Failed => "cli-swarm-tui-line-turn-failed",
                };
                self.push(LineKind::System, crate::t(key, "turn complete"));
            }
            SessionUpdateEvent::HistoryTrimmed { .. } => {
                self.flush_partial();
                self.push(
                    LineKind::System,
                    crate::t("cli-swarm-tui-line-trimmed", "context trimmed"),
                );
            }
            // Context usage feeds the badge, not the scrollback.
            SessionUpdateEvent::ContextUsage { .. } => {}
        }
    }

    /// Fold a chunk into the tail partial, breaking off whole lines at newlines.
    fn append_chunk(&mut self, kind: LineKind, text: &str) {
        // A different partial kind flushes first, so a thought never merges into
        // a message line.
        if let Some((current, _)) = &self.partial
            && *current != kind
        {
            self.flush_partial();
        }
        // Work on an owned buffer so completed lines can be pushed without
        // holding a borrow of `self.partial` across the call.
        let mut buffer = self.partial.take().map_or_else(String::new, |(_, buf)| buf);
        for ch in text.chars() {
            if ch == '\n' {
                let line = std::mem::take(&mut buffer);
                self.push(kind, line);
            } else if buffer.chars().count() < MAX_LINE_CHARS {
                buffer.push(ch);
            }
        }
        if !buffer.is_empty() {
            self.partial = Some((kind, buffer));
        }
    }

    /// Commit the tail partial as a finished line.
    fn flush_partial(&mut self) {
        if let Some((kind, text)) = self.partial.take()
            && !text.is_empty()
        {
            self.push(kind, text);
        }
    }

    fn push(&mut self, kind: LineKind, text: String) {
        if self.lines.len() >= STREAM_CAP {
            self.lines.pop_front();
            self.gap = true;
        }
        self.lines.push_back(StreamLine { kind, text });
    }

    /// The finished lines, oldest first.
    pub fn lines(&self) -> impl Iterator<Item = &StreamLine> {
        self.lines.iter()
    }

    /// The tail line still accreting, if any.
    pub fn partial(&self) -> Option<(LineKind, &str)> {
        self.partial
            .as_ref()
            .map(|(kind, text)| (*kind, text.as_str()))
    }

    /// The ring buffer dropped never-shown lines.
    pub fn has_gap(&self) -> bool {
        self.gap
    }

    fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.partial.is_none()
    }
}

/// A box's board-derived badges plus the client-tracked user-engagement flag.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BoxBadge {
    pub status: BoxStatus,
    pub claim: Option<String>,
    pub note: String,
    /// The box is held for the user after a `swarm/chat` interjection.
    pub engaged: bool,
    /// Latest per-turn context usage: (input tokens, context ceiling).
    pub context: Option<(u64, Option<u64>)>,
}

/// What the canvas wants the driver to do after an input.
#[derive(Debug)]
pub enum ViewAction {
    /// Redraw; nothing else.
    Idle,
    /// Return to the dashboard.
    Leave,
    /// Leave the whole TUI.
    Quit,
    /// Perform a daemon call.
    Call(Effect),
}

/// Which header line a box edit is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditLine {
    Role,
    Job,
}

/// The interaction mode a focused cell is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Move focus, run the swarm, open an edit or a chat.
    Nav,
    /// Typing a message to the focused box.
    Chat { buffer: String },
    /// Editing the focused box's role and job header.
    Edit {
        line: EditLine,
        role: String,
        job: String,
    },
    /// The focused box is grabbed; arrows swap it with a neighbour.
    Move,
}

/// The live canvas for one swarm.
#[derive(Debug)]
pub struct ViewState {
    swarm_id: String,
    /// Working roster, always sorted by slot. The grid cell index is the index
    /// into this vector.
    boxes: Vec<BoxSpec>,
    streams: BTreeMap<String, BoxStream>,
    badges: BTreeMap<String, BoxBadge>,
    feed: VecDeque<String>,
    status: SwarmStatus,
    spent: SwarmSpend,
    limits: SwarmBudgetLimits,
    revision: u64,
    focus: usize,
    mode: Mode,
    subscribed: bool,
    /// Local layout edits await a `swarm/update-layout`.
    layout_dirty: bool,
    /// A layout write is in flight; a second is held until its reply lands so
    /// the revision guard never trips.
    layout_saving: bool,
    /// Bumped on every layout-changing edit (and again if edits arrived while a
    /// save was in flight), so the driver can re-arm its debounce timer.
    layout_generation: u64,
}

impl ViewState {
    /// Build the canvas from the swarm as last read. The roster is sorted into
    /// its slot order once so the grid is stable.
    pub fn new(swarm: &PersistedSwarm) -> Self {
        let mut boxes = swarm.spec.boxes.clone();
        boxes.sort_by_key(|b| b.slot);
        Self {
            swarm_id: swarm.spec.id.clone(),
            boxes,
            streams: BTreeMap::new(),
            badges: BTreeMap::new(),
            feed: VecDeque::new(),
            status: swarm.run.status,
            spent: swarm.run.spent,
            limits: swarm.spec.budget.limits(),
            revision: swarm.revision,
            focus: 0,
            mode: Mode::Nav,
            subscribed: false,
            layout_dirty: false,
            layout_saving: false,
            layout_generation: 0,
        }
    }

    // ── Read side ────────────────────────────────────────────────

    pub fn swarm_id(&self) -> &str {
        &self.swarm_id
    }

    pub fn boxes(&self) -> &[BoxSpec] {
        &self.boxes
    }

    pub fn status(&self) -> SwarmStatus {
        self.status
    }

    pub fn spent(&self) -> SwarmSpend {
        self.spent
    }

    pub fn limits(&self) -> SwarmBudgetLimits {
        self.limits
    }

    pub fn focus(&self) -> usize {
        self.focus
    }

    pub fn mode(&self) -> &Mode {
        &self.mode
    }

    pub fn subscribed(&self) -> bool {
        self.subscribed
    }

    pub fn feed(&self) -> impl Iterator<Item = &String> {
        self.feed.iter()
    }

    pub fn stream(&self, box_id: &str) -> Option<&BoxStream> {
        self.streams.get(box_id)
    }

    pub fn badge(&self, box_id: &str) -> Option<&BoxBadge> {
        self.badges.get(box_id)
    }

    /// The box under the focus cell, if the roster is non-empty.
    pub fn focused_box(&self) -> Option<&BoxSpec> {
        self.boxes.get(self.focus)
    }

    /// Grid shape (rows, cols) that packs the roster into a near-square grid.
    pub fn grid_shape(&self) -> (usize, usize) {
        grid_shape(self.boxes.len())
    }

    pub fn layout_generation(&self) -> u64 {
        self.layout_generation
    }

    // ── Subscription + run-control replies ───────────────────────

    pub fn mark_subscribed(&mut self) {
        self.subscribed = true;
    }

    pub fn set_run_state(&mut self, status: SwarmStatus, spent: SwarmSpend) {
        self.status = status;
        self.spent = spent;
    }

    pub fn set_status(&mut self, status: SwarmStatus) {
        self.status = status;
    }

    /// Fold a `swarm/chat` reply into the focused box's engagement badge.
    pub fn apply_chat(&mut self, result: &SwarmChatResult) {
        let badge = self.badges.entry(result.box_id.clone()).or_default();
        badge.engaged = result.user_engaged;
        let key = if result.released {
            "cli-swarm-tui-feed-handback"
        } else {
            "cli-swarm-tui-feed-chat"
        };
        let note = crate::ta(key, &[("box", result.box_id.as_str())], "chat");
        self.push_feed(note);
    }

    // ── Live streams ─────────────────────────────────────────────

    /// Route one `swarm/update` turn event into its box's stream. Events for a
    /// different swarm (a stale fan-out on the shared connection) are ignored.
    pub fn apply_update(&mut self, update: &SwarmUpdate) {
        if update.swarm_id != self.swarm_id {
            return;
        }
        if let SessionUpdateEvent::ContextUsage {
            input_tokens: Some(tokens),
            max_context_tokens,
            ..
        } = &update.event
        {
            self.badges
                .entry(update.box_id.clone())
                .or_default()
                .context = Some((*tokens, *max_context_tokens));
        }
        self.streams
            .entry(update.box_id.clone())
            .or_default()
            .apply(&update.event);
    }

    /// Route one `swarm/board` transition into badges and the broadcast feed.
    pub fn apply_board(&mut self, event: &BoardEvent) {
        match event {
            BoardEvent::Published { box_id, state } => {
                let badge = self.badges.entry(box_id.clone()).or_default();
                badge.status = state.status;
                badge.claim = state.claim.clone();
                badge.note = state.note.clone();
                let note = crate::ta(
                    "cli-swarm-tui-feed-published",
                    &[("box", box_id.as_str()), ("status", state.status.as_str())],
                    "published",
                );
                self.push_feed(note);
            }
            BoardEvent::Claimed { box_id, task_key } => {
                self.badges.entry(box_id.clone()).or_default().claim = Some(task_key.clone());
                let note = crate::ta(
                    "cli-swarm-tui-feed-claimed",
                    &[("box", box_id.as_str()), ("key", task_key.as_str())],
                    "claimed",
                );
                self.push_feed(note);
            }
            BoardEvent::Released { box_id, task_key } => {
                if let Some(badge) = self.badges.get_mut(box_id)
                    && badge.claim.as_deref() == Some(task_key.as_str())
                {
                    badge.claim = None;
                }
                let note = crate::ta(
                    "cli-swarm-tui-feed-released",
                    &[("box", box_id.as_str()), ("key", task_key.as_str())],
                    "released",
                );
                self.push_feed(note);
            }
            BoardEvent::Reaped => {
                for badge in self.badges.values_mut() {
                    badge.status = BoxStatus::Idle;
                    badge.claim = None;
                }
                self.push_feed(crate::t("cli-swarm-tui-feed-reaped", "board reaped"));
            }
        }
    }

    /// The client dropped notifications it could not buffer: leave a visible
    /// marker on the feed rather than hiding the loss.
    pub fn note_gap(&mut self) {
        self.push_feed(crate::t("cli-swarm-tui-feed-gap", "… (stream gap)"));
    }

    fn push_feed(&mut self, note: String) {
        if self.feed.len() >= FEED_CAP {
            self.feed.pop_front();
        }
        self.feed.push_back(note);
    }

    // ── Layout persistence ───────────────────────────────────────

    /// A layout write is ready when local edits are pending and none is already
    /// in flight (so the revision guard cannot trip).
    pub fn layout_ready(&self) -> bool {
        self.layout_dirty && !self.layout_saving
    }

    /// Emit the pending layout write and mark it in flight. `Effect::None` when
    /// nothing is pending or a write is already running.
    pub fn flush_layout(&mut self) -> Effect {
        if !self.layout_ready() {
            return Effect::None;
        }
        self.layout_dirty = false;
        self.layout_saving = true;
        Effect::SaveLayout {
            swarm_id: self.swarm_id.clone(),
            revision: self.revision,
            boxes: self.boxes.clone(),
        }
    }

    /// Fold a `swarm/update-layout` reply back in: adopt the fresh revision, and
    /// the server's normalised roster unless newer local edits are pending.
    pub fn apply_layout_saved(&mut self, saved: &PersistedSwarm) {
        self.revision = saved.revision;
        self.layout_saving = false;
        if self.layout_dirty {
            // Edits arrived while the save was in flight; re-arm the debounce.
            self.layout_generation = self.layout_generation.wrapping_add(1);
        } else {
            let mut boxes = saved.spec.boxes.clone();
            boxes.sort_by_key(|b| b.slot);
            self.boxes = boxes;
            if self.focus >= self.boxes.len() {
                self.focus = self.boxes.len().saturating_sub(1);
            }
        }
    }

    fn mark_layout_dirty(&mut self) {
        self.layout_dirty = true;
        self.layout_generation = self.layout_generation.wrapping_add(1);
    }

    /// Release the in-flight guard after a rejected layout write, so the edit
    /// can be retried rather than wedging every later save. The edit stays
    /// pending (it was never persisted), re-armed for the next debounce.
    pub fn clear_layout_saving(&mut self) {
        if self.layout_saving {
            self.layout_saving = false;
            self.layout_dirty = true;
            self.layout_generation = self.layout_generation.wrapping_add(1);
        }
    }

    // ── Input ────────────────────────────────────────────────────

    pub fn on_input(&mut self, input: Input) -> ViewAction {
        match &self.mode {
            Mode::Nav => self.nav_input(input),
            Mode::Chat { .. } => self.chat_input(input),
            Mode::Edit { .. } => self.edit_input(input),
            Mode::Move => self.move_input(input),
        }
    }

    fn nav_input(&mut self, input: Input) -> ViewAction {
        match input {
            Input::Escape | Input::Backspace => ViewAction::Leave,
            Input::Char('q') => ViewAction::Quit,
            Input::Up => self.move_focus(GridStep::Up),
            Input::Down => self.move_focus(GridStep::Down),
            Input::Left => self.move_focus(GridStep::Left),
            Input::Right => self.move_focus(GridStep::Right),
            Input::Tab => self.move_focus(GridStep::Next),
            Input::BackTab => self.move_focus(GridStep::Prev),
            Input::Enter => {
                if self.focused_box().is_some() {
                    self.mode = Mode::Chat {
                        buffer: String::new(),
                    };
                }
                ViewAction::Idle
            }
            Input::Char('e') => {
                if let Some(b) = self.focused_box() {
                    self.mode = Mode::Edit {
                        line: EditLine::Role,
                        role: b.role.clone(),
                        job: b.job.clone(),
                    };
                }
                ViewAction::Idle
            }
            Input::Char('m') => {
                if self.boxes.len() > 1 {
                    self.mode = Mode::Move;
                }
                ViewAction::Idle
            }
            Input::Char('s') => self.run_control(
                SwarmStatus::Created,
                Effect::Start {
                    swarm_id: self.swarm_id.clone(),
                },
            ),
            Input::Char('p') => self.run_control(
                SwarmStatus::Running,
                Effect::Pause {
                    swarm_id: self.swarm_id.clone(),
                },
            ),
            Input::Char('r') => self.resume_control(),
            Input::Char('x') => self.stop_control(),
            _ => ViewAction::Idle,
        }
    }

    /// Fire an effect only when the status matches, so a key that cannot apply
    /// is a no-op rather than a daemon round trip that only errors.
    fn run_control(&self, required: SwarmStatus, effect: Effect) -> ViewAction {
        if self.status == required {
            ViewAction::Call(effect)
        } else {
            ViewAction::Idle
        }
    }

    fn resume_control(&self) -> ViewAction {
        if matches!(self.status, SwarmStatus::Paused { .. }) {
            ViewAction::Call(Effect::Resume {
                swarm_id: self.swarm_id.clone(),
            })
        } else {
            ViewAction::Idle
        }
    }

    fn stop_control(&self) -> ViewAction {
        if matches!(
            self.status,
            SwarmStatus::Running | SwarmStatus::Paused { .. }
        ) {
            ViewAction::Call(Effect::Stop {
                swarm_id: self.swarm_id.clone(),
            })
        } else {
            ViewAction::Idle
        }
    }

    fn chat_input(&mut self, input: Input) -> ViewAction {
        let Mode::Chat { buffer } = &mut self.mode else {
            return ViewAction::Idle;
        };
        match input {
            Input::Escape => {
                self.mode = Mode::Nav;
                ViewAction::Idle
            }
            Input::Backspace => {
                buffer.pop();
                ViewAction::Idle
            }
            Input::Char(c) => {
                buffer.push(c);
                ViewAction::Idle
            }
            Input::Enter => {
                let message = buffer.trim().to_string();
                self.mode = Mode::Nav;
                let Some(b) = self.focused_box() else {
                    return ViewAction::Idle;
                };
                if message.is_empty() {
                    return ViewAction::Idle;
                }
                ViewAction::Call(Effect::Chat {
                    swarm_id: self.swarm_id.clone(),
                    box_id: b.box_id.clone(),
                    message,
                })
            }
            _ => ViewAction::Idle,
        }
    }

    fn edit_input(&mut self, input: Input) -> ViewAction {
        let Mode::Edit { line, role, job } = &mut self.mode else {
            return ViewAction::Idle;
        };
        match input {
            Input::Escape => {
                self.mode = Mode::Nav;
                ViewAction::Idle
            }
            Input::Tab | Input::BackTab | Input::Up | Input::Down => {
                *line = match line {
                    EditLine::Role => EditLine::Job,
                    EditLine::Job => EditLine::Role,
                };
                ViewAction::Idle
            }
            Input::Backspace => {
                match line {
                    EditLine::Role => role.pop(),
                    EditLine::Job => job.pop(),
                };
                ViewAction::Idle
            }
            Input::Char(c) => {
                match line {
                    EditLine::Role => role.push(c),
                    EditLine::Job => job.push(c),
                }
                ViewAction::Idle
            }
            Input::Enter => {
                let (role, job) = (role.clone(), job.clone());
                self.commit_header(role, job);
                self.mode = Mode::Nav;
                ViewAction::Idle
            }
            _ => ViewAction::Idle,
        }
    }

    /// Write an edited header back onto the focused box and mark the roster for
    /// a debounced save. Unchanged text saves nothing.
    fn commit_header(&mut self, role: String, job: String) {
        let Some(b) = self.boxes.get_mut(self.focus) else {
            return;
        };
        if b.role == role && b.job == job {
            return;
        }
        b.role = role;
        b.job = job;
        self.mark_layout_dirty();
    }

    fn move_input(&mut self, input: Input) -> ViewAction {
        match input {
            Input::Escape | Input::Enter | Input::Char('m') => {
                self.mode = Mode::Nav;
                ViewAction::Idle
            }
            Input::Up => self.swap_toward(GridStep::Up),
            Input::Down => self.swap_toward(GridStep::Down),
            Input::Left => self.swap_toward(GridStep::Left),
            Input::Right => self.swap_toward(GridStep::Right),
            _ => ViewAction::Idle,
        }
    }

    /// Swap the grabbed box with the neighbour in `step`'s direction, exchanging
    /// their slots so the move persists, and carry focus with the box.
    fn swap_toward(&mut self, step: GridStep) -> ViewAction {
        let (rows, cols) = self.grid_shape();
        if let Some(target) = neighbour(self.focus, self.boxes.len(), rows, cols, step) {
            let a = self.boxes[self.focus].slot;
            let b = self.boxes[target].slot;
            self.boxes[self.focus].slot = b;
            self.boxes[target].slot = a;
            self.boxes.sort_by_key(|bx| bx.slot);
            self.focus = target;
            self.mark_layout_dirty();
        }
        ViewAction::Idle
    }

    fn move_focus(&mut self, step: GridStep) -> ViewAction {
        let n = self.boxes.len();
        if n == 0 {
            return ViewAction::Idle;
        }
        match step {
            GridStep::Next => self.focus = Direction::Forward.wrap(self.focus, n),
            GridStep::Prev => self.focus = Direction::Backward.wrap(self.focus, n),
            other => {
                let (rows, cols) = self.grid_shape();
                if let Some(target) = neighbour(self.focus, n, rows, cols, other) {
                    self.focus = target;
                }
            }
        }
        ViewAction::Idle
    }
}

/// A direction the focus or a grabbed box can travel on the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GridStep {
    Up,
    Down,
    Left,
    Right,
    /// Linear next cell, wrapping.
    Next,
    /// Linear previous cell, wrapping.
    Prev,
}

/// The neighbour cell in `step`'s direction, or `None` when that move would
/// leave the packed grid (an empty trailing cell, or an edge). Cell index is
/// the position in the slot-sorted roster.
fn neighbour(
    focus: usize,
    count: usize,
    _rows: usize,
    cols: usize,
    step: GridStep,
) -> Option<usize> {
    if count == 0 || cols == 0 {
        return None;
    }
    let row = focus / cols;
    let col = focus % cols;
    let target = match step {
        GridStep::Up => focus.checked_sub(cols)?,
        GridStep::Down => {
            if col + cols * (row + 1) < count {
                focus + cols
            } else {
                return None;
            }
        }
        GridStep::Left => {
            if col == 0 {
                return None;
            }
            focus - 1
        }
        GridStep::Right => {
            if col + 1 >= cols || focus + 1 >= count {
                return None;
            }
            focus + 1
        }
        GridStep::Next | GridStep::Prev => return None,
    };
    (target < count).then_some(target)
}

/// Pack `count` cells into a near-square grid, columns first. Zero cells is a
/// 1×1 grid so the renderer always has a shape.
pub fn grid_shape(count: usize) -> (usize, usize) {
    if count <= 1 {
        return (1, 1);
    }
    // Ceiling of the integer square root, without a float cast: the smallest
    // `cols` whose square covers `count`.
    let mut cols = 1;
    while cols * cols < count {
        cols += 1;
    }
    let rows = count.div_ceil(cols);
    (rows, cols)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_runtime::rpc::types::SwarmChatResult;
    use zeroclaw_runtime::swarm::board::BoxState;
    use zeroclaw_runtime::swarm::store::{SwarmPauseReason, SwarmSpec};

    fn swarm_with(boxes: usize) -> PersistedSwarm {
        let mut swarm = PersistedSwarm::new(SwarmSpec::new(
            "sw-1",
            "Research squad",
            "anthropic",
            "claude-sonnet-4",
            "balanced",
            "supervisor",
            "survey the field",
        ));
        swarm.spec.boxes.truncate(boxes.max(1));
        for (slot, b) in swarm.spec.boxes.iter_mut().enumerate() {
            b.slot = u8::try_from(slot).unwrap_or(0);
        }
        swarm
    }

    fn view(boxes: usize) -> ViewState {
        ViewState::new(&swarm_with(boxes))
    }

    fn chunk(box_id: &str, text: &str) -> SwarmUpdate {
        SwarmUpdate {
            swarm_id: "sw-1".to_string(),
            box_id: box_id.to_string(),
            event: SessionUpdateEvent::AgentMessageChunk {
                session_id: "s".to_string(),
                text: text.to_string(),
            },
        }
    }

    #[test]
    fn the_grid_packs_into_a_near_square() {
        assert_eq!(grid_shape(0), (1, 1));
        assert_eq!(grid_shape(1), (1, 1));
        assert_eq!(grid_shape(2), (1, 2));
        assert_eq!(grid_shape(3), (2, 2));
        assert_eq!(grid_shape(4), (2, 2));
        assert_eq!(grid_shape(5), (2, 3));
        assert_eq!(grid_shape(6), (2, 3));
    }

    #[test]
    fn chunks_coalesce_into_lines_broken_on_newlines() {
        let mut v = view(4);
        v.apply_update(&chunk("box-1", "hello "));
        v.apply_update(&chunk("box-1", "world\nnext bit"));
        let stream = v.stream("box-1").expect("box-1 has a stream");
        let lines: Vec<&str> = stream.lines().map(|l| l.text.as_str()).collect();
        assert_eq!(lines, vec!["hello world"], "the first line is complete");
        assert_eq!(
            stream.partial(),
            Some((LineKind::Message, "next bit")),
            "the tail keeps accreting"
        );
    }

    #[test]
    fn a_different_event_flushes_the_partial() {
        let mut v = view(4);
        v.apply_update(&chunk("box-1", "thinking about it"));
        v.apply_update(&SwarmUpdate {
            swarm_id: "sw-1".to_string(),
            box_id: "box-1".to_string(),
            event: SessionUpdateEvent::ToolCall {
                session_id: "s".to_string(),
                tool_call_id: "t1".to_string(),
                name: "read_file".to_string(),
                raw_input: serde_json::Value::Null,
            },
        });
        let stream = v.stream("box-1").expect("stream");
        let lines: Vec<&str> = stream.lines().map(|l| l.text.as_str()).collect();
        assert_eq!(lines.len(), 2, "the message line then the tool line");
        assert_eq!(lines[0], "thinking about it");
        assert!(lines[1].contains("read_file"), "tool line names the tool");
        assert!(stream.partial().is_none());
    }

    #[test]
    fn a_full_ring_buffer_drops_the_head_but_flags_a_gap() {
        let mut v = view(4);
        for i in 0..(STREAM_CAP + 5) {
            v.apply_update(&chunk("box-1", &format!("line {i}\n")));
        }
        let stream = v.stream("box-1").expect("stream");
        assert!(stream.has_gap(), "eviction is visible, not silent");
        assert!(stream.lines().count() <= STREAM_CAP);
    }

    #[test]
    fn an_update_for_another_swarm_is_ignored() {
        let mut v = view(4);
        v.apply_update(&SwarmUpdate {
            swarm_id: "sw-OTHER".to_string(),
            box_id: "box-1".to_string(),
            event: SessionUpdateEvent::AgentMessageChunk {
                session_id: "s".to_string(),
                text: "not mine".to_string(),
            },
        });
        assert!(
            v.stream("box-1").is_none(),
            "a stale fan-out is filtered out"
        );
    }

    #[test]
    fn board_events_drive_the_badges() {
        let mut v = view(4);
        v.apply_board(&BoardEvent::Published {
            box_id: "box-2".to_string(),
            state: BoxState {
                status: BoxStatus::Working,
                claim: Some("scan".to_string()),
                note: "reading".to_string(),
            },
        });
        let badge = v.badge("box-2").expect("badge");
        assert_eq!(badge.status, BoxStatus::Working);
        assert_eq!(badge.claim.as_deref(), Some("scan"));

        v.apply_board(&BoardEvent::Released {
            box_id: "box-2".to_string(),
            task_key: "scan".to_string(),
        });
        assert!(
            v.badge("box-2").unwrap().claim.is_none(),
            "release clears it"
        );

        v.apply_board(&BoardEvent::Reaped);
        assert_eq!(v.badge("box-2").unwrap().status, BoxStatus::Idle);
    }

    #[test]
    fn a_dropped_burst_leaves_a_visible_feed_marker() {
        let mut v = view(4);
        v.note_gap();
        assert!(
            v.feed().any(|line| line.contains("gap")),
            "the gap is on the feed, not hidden"
        );
    }

    #[test]
    fn focus_snaps_around_a_two_by_two_grid() {
        let mut v = view(4); // cells 0,1 / 2,3
        assert_eq!(v.focus(), 0);
        v.on_input(Input::Right);
        assert_eq!(v.focus(), 1);
        v.on_input(Input::Down);
        assert_eq!(v.focus(), 3);
        v.on_input(Input::Left);
        assert_eq!(v.focus(), 2);
        v.on_input(Input::Up);
        assert_eq!(v.focus(), 0);
        // An edge move does not wrap.
        v.on_input(Input::Up);
        assert_eq!(v.focus(), 0);
        // Tab wraps linearly.
        v.on_input(Input::BackTab);
        assert_eq!(v.focus(), 3);
        v.on_input(Input::Tab);
        assert_eq!(v.focus(), 0);
    }

    #[test]
    fn editing_a_header_marks_the_layout_and_flushes_once() {
        let mut v = view(4);
        v.on_input(Input::Char('e')); // enter edit mode on the role line
        for c in "critic".chars() {
            v.on_input(Input::Char(c));
        }
        v.on_input(Input::Tab); // to the job line
        for c in "poke holes".chars() {
            v.on_input(Input::Char(c));
        }
        v.on_input(Input::Enter); // commit
        assert_eq!(v.boxes()[0].role, "critic");
        assert_eq!(v.boxes()[0].job, "poke holes");
        assert!(v.layout_ready(), "a committed edit is pending a save");

        let effect = v.flush_layout();
        match effect {
            Effect::SaveLayout { boxes, .. } => {
                assert_eq!(boxes[0].role, "critic");
            }
            other => panic!("expected a layout save, got {other:?}"),
        }
        // A second flush while the write is in flight sends nothing.
        assert!(matches!(v.flush_layout(), Effect::None));

        // The reply releases the guard.
        let mut saved = swarm_with(4);
        saved.revision = 7;
        v.apply_layout_saved(&saved);
        assert!(!v.layout_ready());
    }

    #[test]
    fn an_unchanged_header_edit_saves_nothing() {
        let mut v = view(4);
        v.on_input(Input::Char('e'));
        v.on_input(Input::Enter); // commit with no change
        assert!(!v.layout_ready());
    }

    #[test]
    fn moving_a_box_swaps_slots_and_carries_focus() {
        let mut v = view(4);
        let first = v.boxes()[0].box_id.clone();
        v.on_input(Input::Char('m')); // grab
        v.on_input(Input::Right); // swap cell 0 with cell 1
        assert_eq!(v.focus(), 1, "focus follows the grabbed box");
        assert_eq!(v.boxes()[1].box_id, first, "the box moved to cell 1");
        assert_eq!(v.boxes()[1].slot, 1, "its slot was exchanged");
        assert!(v.layout_ready(), "the move is pending a save");
    }

    #[test]
    fn enter_opens_chat_and_sends_to_the_focused_box() {
        let mut v = view(4);
        v.on_input(Input::Right); // focus box-2
        v.on_input(Input::Enter); // open chat
        for c in "status?".chars() {
            v.on_input(Input::Char(c));
        }
        match v.on_input(Input::Enter) {
            ViewAction::Call(Effect::Chat {
                box_id, message, ..
            }) => {
                assert_eq!(box_id, "box-2");
                assert_eq!(message, "status?");
            }
            other => panic!("expected a chat call, got {other:?}"),
        }
        // An empty message sends nothing.
        v.on_input(Input::Enter);
        assert!(matches!(v.on_input(Input::Enter), ViewAction::Idle));
    }

    #[test]
    fn a_chat_reply_toggles_the_engaged_badge() {
        let mut v = view(4);
        v.apply_chat(&SwarmChatResult {
            swarm_id: "sw-1".to_string(),
            box_id: "box-1".to_string(),
            queued: true,
            user_engaged: true,
            released: false,
        });
        assert!(v.badge("box-1").unwrap().engaged);
        v.apply_chat(&SwarmChatResult {
            swarm_id: "sw-1".to_string(),
            box_id: "box-1".to_string(),
            queued: false,
            user_engaged: false,
            released: true,
        });
        assert!(!v.badge("box-1").unwrap().engaged, "handback clears it");
    }

    #[test]
    fn run_control_keys_only_fire_when_the_status_allows() {
        let mut v = view(4);
        // Created: start applies, pause does not.
        assert!(matches!(
            v.on_input(Input::Char('s')),
            ViewAction::Call(Effect::Start { .. })
        ));
        assert!(matches!(v.on_input(Input::Char('p')), ViewAction::Idle));

        v.set_run_state(SwarmStatus::Running, SwarmSpend::default());
        assert!(matches!(
            v.on_input(Input::Char('p')),
            ViewAction::Call(Effect::Pause { .. })
        ));
        assert!(matches!(
            v.on_input(Input::Char('x')),
            ViewAction::Call(Effect::Stop { .. })
        ));

        v.set_run_state(
            SwarmStatus::Paused {
                reason: SwarmPauseReason::BudgetExhausted,
            },
            SwarmSpend::default(),
        );
        assert!(matches!(
            v.on_input(Input::Char('r')),
            ViewAction::Call(Effect::Resume { .. })
        ));
    }

    #[test]
    fn escape_leaves_the_canvas() {
        let mut v = view(4);
        assert!(matches!(v.on_input(Input::Escape), ViewAction::Leave));
        // But not while typing a chat message.
        let mut v = view(4);
        v.on_input(Input::Enter);
        assert!(matches!(v.on_input(Input::Escape), ViewAction::Idle));
        assert!(
            matches!(v.mode(), Mode::Nav),
            "escape only cancels the chat"
        );
    }
}
