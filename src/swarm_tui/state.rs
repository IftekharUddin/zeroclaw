//! The swarm TUI's state machine.
//!
//! Nothing in this module touches a terminal or a socket. Input arrives as an
//! [`Input`], daemon answers arrive as an [`Update`], and the only thing that
//! leaves is an [`Effect`] the driver is asked to perform. That split is what
//! lets the screens be tested without a TTY and the flows without a daemon.
//!
//! ## Adding a screen
//!
//! A new screen is three edits and no more: a variant on [`Screen`], an arm in
//! [`App::on_input`], and an arm in [`super::render::draw`]. Anything the
//! screen needs from the daemon becomes an [`Effect`] variant plus the
//! matching [`Update`]; the driver in [`super`] wires the pair together.

use std::collections::HashSet;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use zeroclaw_api::jsonrpc::error_codes::SWARM_RUN_ACTIVE;
use zeroclaw_runtime::rpc::types::{
    SwarmBoardNotify, SwarmChatResult, SwarmStepShape, SwarmSubmission, SwarmUpdate,
    SwarmValidateResult,
};
use zeroclaw_runtime::swarm::store::{BoxSpec, PersistedSwarm, SwarmSpend, SwarmStatus};

use super::client::RpcFailure;
use super::view::{ViewAction, ViewState};
use super::wizard::{WizardAction, WizardState};

/// Which way a wrapping cursor moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Backward,
}

impl Direction {
    /// Step `at` one position within `len`, wrapping at both ends. `len` of
    /// zero stays at zero.
    pub fn wrap(self, at: usize, len: usize) -> usize {
        if len == 0 {
            return 0;
        }
        match self {
            Self::Forward => (at + 1) % len,
            Self::Backward => (at + len - 1) % len,
        }
    }
}

/// A key press, stripped of terminal detail. The reducer speaks only this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Escape,
    Tab,
    BackTab,
    Backspace,
    Char(char),
    /// Ctrl+C. Always leaves.
    Interrupt,
}

impl Input {
    /// Translate a terminal key press. Releases and repeats of modifier-only
    /// keys map to nothing.
    pub fn from_key(key: KeyEvent) -> Option<Self> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C'))
        {
            return Some(Self::Interrupt);
        }
        match key.code {
            KeyCode::Up => Some(Self::Up),
            KeyCode::Down => Some(Self::Down),
            KeyCode::Left => Some(Self::Left),
            KeyCode::Right => Some(Self::Right),
            KeyCode::Enter => Some(Self::Enter),
            KeyCode::Esc => Some(Self::Escape),
            KeyCode::Tab => Some(Self::Tab),
            KeyCode::BackTab => Some(Self::BackTab),
            KeyCode::Backspace => Some(Self::Backspace),
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Self::Char(c))
            }
            _ => None,
        }
    }
}

/// Work the driver performs on the reducer's behalf.
#[derive(Debug)]
pub enum Effect {
    /// Redraw and wait.
    None,
    /// Leave the TUI.
    Quit,
    /// `swarm/list`.
    List,
    /// `swarm/fields`, for the provider answered so far.
    Fields { provider: Option<String> },
    /// `swarm/validate`.
    Validate(Box<SwarmSubmission>),
    /// `swarm/create`.
    Create(Box<SwarmSubmission>),
    /// `swarm/delete`.
    Delete { swarm_id: String, force: bool },
    /// `swarm/subscribe` — opt into the swarm's live streams.
    Subscribe { swarm_id: String },
    /// `swarm/start`.
    Start { swarm_id: String },
    /// `swarm/pause`.
    Pause { swarm_id: String },
    /// `swarm/resume`.
    Resume { swarm_id: String },
    /// `swarm/stop`.
    Stop { swarm_id: String },
    /// `swarm/chat` — interject a user message into one box.
    Chat {
        swarm_id: String,
        box_id: String,
        message: String,
    },
    /// `swarm/update-layout` — persist the box canvas (slots, roles, jobs).
    SaveLayout {
        swarm_id: String,
        revision: u64,
        boxes: Vec<BoxSpec>,
    },
}

/// A daemon answer, folded back into the state.
#[derive(Debug)]
pub enum Update {
    Swarms(Vec<PersistedSwarm>),
    Fields {
        provider: Option<String>,
        steps: Vec<SwarmStepShape>,
    },
    Validated(SwarmValidateResult),
    Created(Box<PersistedSwarm>),
    Deleted {
        swarm_id: String,
    },
    /// `swarm/subscribe` was accepted; the live view can start streaming.
    Subscribed {
        swarm_id: String,
    },
    /// A `swarm/start|pause|resume` reply: the post-transition run state.
    RunControl {
        swarm_id: String,
        status: SwarmStatus,
        spent: SwarmSpend,
    },
    /// A `swarm/stop` reply: the terminal status.
    Stopped {
        swarm_id: String,
        status: SwarmStatus,
    },
    /// A `swarm/chat` reply: the box's new engagement state.
    Chatted(Box<SwarmChatResult>),
    /// A `swarm/update-layout` reply: the persisted document at its new revision.
    LayoutSaved(Box<PersistedSwarm>),
    /// A live `swarm/update` turn event.
    Stream(Box<SwarmUpdate>),
    /// A live `swarm/board` transition.
    Board(Box<SwarmBoardNotify>),
    /// The client dropped notifications it could not buffer.
    StreamGap,
    /// A call came back as an error frame, or the transport gave out.
    Failed(RpcFailure),
}

/// Where the TUI is. One variant per screen; the router is the match in
/// [`App::on_input`] and its twin in the renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    Dashboard,
    /// The live multi-box canvas for one swarm: a grid of streaming cells, the
    /// budget/run status bar, and the run-control, chat, and layout-edit keys.
    /// Its working state lives in [`App::view`].
    SwarmView {
        swarm_id: String,
    },
    Wizard,
    /// The daemon has no `swarm/` family — an older build behind the same
    /// socket. Shown instead of crashing.
    Unsupported,
}

/// A blocking overlay. At most one is up at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Modal {
    None,
    /// Delete confirmation. `force` is set on the retry after the daemon
    /// refused because a live run holds the swarm.
    ConfirmDelete {
        swarm_id: String,
        force: bool,
    },
    Error {
        message: String,
    },
}

/// The whole TUI.
#[derive(Debug)]
pub struct App {
    screen: Screen,
    swarms: Vec<PersistedSwarm>,
    /// Row cursor on the dashboard. The row past the last swarm is the
    /// "new swarm" row, so the list is never empty.
    selected: usize,
    modal: Modal,
    /// `None` while the wizard's shapes are still in flight.
    wizard: Option<WizardState>,
    /// The live canvas's state, mounted while [`Screen::SwarmView`] is up.
    view: Option<ViewState>,
    /// Swarm ids already subscribed on this connection. A `swarm/subscribe`
    /// fan-out lives until the connection closes and there is no unsubscribe, so
    /// re-opening a swarm must not spawn a second fan-out that doubles events.
    subscribed: HashSet<String>,
    status: String,
    busy: bool,
    quit: bool,
    /// A freshly created swarm to put the cursor on once the list reloads.
    pending_select: Option<String>,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self {
            screen: Screen::Dashboard,
            swarms: Vec::new(),
            selected: 0,
            modal: Modal::None,
            wizard: None,
            view: None,
            subscribed: HashSet::new(),
            status: String::new(),
            busy: true,
            quit: false,
            pending_select: None,
        }
    }

    /// A session that opens straight onto the degrade screen: the daemon told
    /// us at `initialize` that it has no swarm methods.
    pub fn unsupported() -> Self {
        Self {
            screen: Screen::Unsupported,
            busy: false,
            ..Self::new()
        }
    }

    // ── Read side ────────────────────────────────────────────────

    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    pub fn swarms(&self) -> &[PersistedSwarm] {
        &self.swarms
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn modal(&self) -> &Modal {
        &self.modal
    }

    pub fn wizard(&self) -> Option<&WizardState> {
        self.wizard.as_ref()
    }

    pub fn view(&self) -> Option<&ViewState> {
        self.view.as_ref()
    }

    /// The live view is up and its subscription is confirmed, so the driver
    /// should pull the socket's notification stream.
    pub fn is_streaming(&self) -> bool {
        matches!(self.screen, Screen::SwarmView { .. })
            && self.view.as_ref().is_some_and(ViewState::subscribed)
    }

    /// A change-detector the driver watches to re-arm its layout debounce timer;
    /// it bumps on every layout-changing edit.
    pub fn layout_generation(&self) -> u64 {
        self.view.as_ref().map_or(0, ViewState::layout_generation)
    }

    /// Emit any pending layout write once the debounce has elapsed. Called by
    /// the driver's timer arm.
    pub fn flush_layout(&mut self) -> Effect {
        self.view
            .as_mut()
            .map_or(Effect::None, ViewState::flush_layout)
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn busy(&self) -> bool {
        self.busy
    }

    pub fn quit(&self) -> bool {
        self.quit
    }

    /// Rows on the dashboard: every swarm plus the "new swarm" row.
    pub fn row_count(&self) -> usize {
        self.swarms.len() + 1
    }

    /// `true` when the cursor is on the "new swarm" row.
    pub fn on_new_row(&self) -> bool {
        self.selected >= self.swarms.len()
    }

    pub fn selected_swarm(&self) -> Option<&PersistedSwarm> {
        self.swarms.get(self.selected)
    }

    pub fn swarm(&self, swarm_id: &str) -> Option<&PersistedSwarm> {
        self.swarms.iter().find(|s| s.spec.id == swarm_id)
    }

    // ── Reducer ──────────────────────────────────────────────────

    pub fn on_input(&mut self, input: Input) -> Effect {
        if input == Input::Interrupt {
            self.quit = true;
            return Effect::Quit;
        }
        if !matches!(self.modal, Modal::None) {
            return self.modal_input(input);
        }
        match &self.screen {
            Screen::Dashboard => self.dashboard_input(input),
            Screen::SwarmView { .. } => self.swarmview_input(input),
            Screen::Wizard => self.wizard_input(input),
            Screen::Unsupported => {
                self.quit = true;
                Effect::Quit
            }
        }
    }

    fn dashboard_input(&mut self, input: Input) -> Effect {
        match input {
            Input::Up | Input::BackTab => {
                self.selected = Direction::Backward.wrap(self.selected, self.row_count());
                Effect::None
            }
            Input::Down | Input::Tab => {
                self.selected = Direction::Forward.wrap(self.selected, self.row_count());
                Effect::None
            }
            Input::Enter => {
                if self.on_new_row() {
                    self.screen = Screen::Wizard;
                    self.wizard = None;
                    self.busy = true;
                    self.status.clear();
                    return Effect::Fields { provider: None };
                }
                if let Some(swarm) = self.selected_swarm() {
                    let swarm = swarm.clone();
                    return self.open_view(swarm);
                }
                Effect::None
            }
            Input::Char('d') => {
                let Some(swarm) = self.selected_swarm() else {
                    return Effect::None;
                };
                self.modal = Modal::ConfirmDelete {
                    swarm_id: swarm.spec.id.clone(),
                    force: false,
                };
                Effect::None
            }
            Input::Char('r') => {
                self.busy = true;
                Effect::List
            }
            Input::Char('q') | Input::Escape => {
                self.quit = true;
                Effect::Quit
            }
            _ => Effect::None,
        }
    }

    /// Mount the live canvas for a swarm and subscribe to its streams. A swarm
    /// already subscribed on this connection skips a second `swarm/subscribe`
    /// (no unsubscribe exists, so a re-subscribe would double every event) and
    /// streams straight away.
    fn open_view(&mut self, swarm: PersistedSwarm) -> Effect {
        let swarm_id = swarm.spec.id.clone();
        let mut view = ViewState::new(&swarm);
        self.screen = Screen::SwarmView {
            swarm_id: swarm_id.clone(),
        };
        if self.subscribed.contains(&swarm_id) {
            view.mark_subscribed();
            self.view = Some(view);
            Effect::None
        } else {
            self.view = Some(view);
            self.busy = true;
            Effect::Subscribe { swarm_id }
        }
    }

    fn swarmview_input(&mut self, input: Input) -> Effect {
        let Some(view) = self.view.as_mut() else {
            self.screen = Screen::Dashboard;
            return Effect::None;
        };
        match view.on_input(input) {
            ViewAction::Idle => Effect::None,
            ViewAction::Leave => {
                self.screen = Screen::Dashboard;
                self.view = None;
                Effect::None
            }
            ViewAction::Quit => {
                self.quit = true;
                Effect::Quit
            }
            ViewAction::Call(effect) => effect,
        }
    }

    fn wizard_input(&mut self, input: Input) -> Effect {
        let Some(wizard) = self.wizard.as_mut() else {
            // Shapes still in flight: the only useful key is "give up".
            if matches!(input, Input::Escape) {
                self.screen = Screen::Dashboard;
                self.busy = false;
            }
            return Effect::None;
        };
        match wizard.on_input(input) {
            WizardAction::Idle => Effect::None,
            WizardAction::Leave => {
                self.screen = Screen::Dashboard;
                self.wizard = None;
                self.busy = false;
                Effect::None
            }
            WizardAction::Call(effect) => {
                self.busy = true;
                effect
            }
        }
    }

    fn modal_input(&mut self, input: Input) -> Effect {
        match self.modal.clone() {
            Modal::ConfirmDelete { swarm_id, force } => match input {
                Input::Enter | Input::Char('y' | 'Y') => {
                    self.busy = true;
                    Effect::Delete { swarm_id, force }
                }
                Input::Escape | Input::Char('n' | 'N') => {
                    self.modal = Modal::None;
                    Effect::None
                }
                _ => Effect::None,
            },
            Modal::Error { .. } => {
                self.modal = Modal::None;
                Effect::None
            }
            Modal::None => Effect::None,
        }
    }

    pub fn on_update(&mut self, update: Update) -> Effect {
        self.busy = false;
        match update {
            Update::Swarms(swarms) => {
                self.swarms = swarms;
                if let Some(id) = self.pending_select.take()
                    && let Some(at) = self.swarms.iter().position(|s| s.spec.id == id)
                {
                    self.selected = at;
                }
                if self.selected >= self.row_count() {
                    self.selected = self.swarms.len();
                }
                Effect::None
            }
            Update::Fields { provider, steps } => {
                match self.wizard.as_mut() {
                    Some(wizard) => wizard.merge_steps(steps, provider),
                    None => self.wizard = Some(WizardState::new(steps)),
                }
                Effect::None
            }
            Update::Validated(result) => {
                let Some(wizard) = self.wizard.as_mut() else {
                    return Effect::None;
                };
                match wizard.on_validation(&result) {
                    WizardAction::Call(effect) => {
                        self.busy = true;
                        effect
                    }
                    WizardAction::Leave => {
                        self.screen = Screen::Dashboard;
                        self.wizard = None;
                        Effect::None
                    }
                    WizardAction::Idle => Effect::None,
                }
            }
            Update::Created(swarm) => {
                self.status = crate::ta(
                    "cli-swarm-tui-created",
                    &[("name", swarm.spec.name.as_str())],
                    "Swarm created.",
                );
                self.pending_select = Some(swarm.spec.id.clone());
                self.screen = Screen::Dashboard;
                self.wizard = None;
                self.busy = true;
                Effect::List
            }
            Update::Deleted { swarm_id } => {
                self.status = crate::ta(
                    "cli-swarm-tui-deleted",
                    &[("swarm_id", swarm_id.as_str())],
                    "Swarm deleted.",
                );
                self.modal = Modal::None;
                if matches!(&self.screen, Screen::SwarmView { swarm_id: shown } if *shown == swarm_id)
                {
                    self.screen = Screen::Dashboard;
                    self.view = None;
                }
                self.busy = true;
                Effect::List
            }
            Update::Subscribed { swarm_id } => {
                self.subscribed.insert(swarm_id.clone());
                if let Some(view) = self.view.as_mut()
                    && view.swarm_id() == swarm_id
                {
                    view.mark_subscribed();
                }
                Effect::None
            }
            Update::RunControl {
                swarm_id,
                status,
                spent,
            } => {
                if let Some(view) = self.view.as_mut()
                    && view.swarm_id() == swarm_id
                {
                    view.set_run_state(status, spent);
                }
                self.sync_run_state(&swarm_id, Some(status), Some(spent));
                Effect::None
            }
            Update::Stopped { swarm_id, status } => {
                if let Some(view) = self.view.as_mut()
                    && view.swarm_id() == swarm_id
                {
                    view.set_status(status);
                }
                self.sync_run_state(&swarm_id, Some(status), None);
                Effect::None
            }
            Update::Chatted(result) => {
                if let Some(view) = self.view.as_mut()
                    && view.swarm_id() == result.swarm_id
                {
                    view.apply_chat(&result);
                }
                Effect::None
            }
            Update::LayoutSaved(saved) => {
                if let Some(view) = self.view.as_mut()
                    && view.swarm_id() == saved.spec.id
                {
                    view.apply_layout_saved(&saved);
                }
                if let Some(at) = self.swarms.iter().position(|s| s.spec.id == saved.spec.id) {
                    self.swarms[at] = *saved;
                }
                Effect::None
            }
            Update::Stream(update) => {
                if let Some(view) = self.view.as_mut() {
                    view.apply_update(&update);
                }
                Effect::None
            }
            Update::Board(board) => {
                if let Some(view) = self.view.as_mut()
                    && view.swarm_id() == board.swarm_id
                {
                    view.apply_board(&board.event);
                }
                Effect::None
            }
            Update::StreamGap => {
                if let Some(view) = self.view.as_mut() {
                    view.note_gap();
                }
                Effect::None
            }
            Update::Failed(failure) => self.on_failure(failure),
        }
    }

    /// Keep the dashboard row for a swarm in step with a run-control reply, so
    /// leaving the live view shows the status the run actually landed on.
    fn sync_run_state(
        &mut self,
        swarm_id: &str,
        status: Option<SwarmStatus>,
        spent: Option<SwarmSpend>,
    ) {
        if let Some(swarm) = self.swarms.iter_mut().find(|s| s.spec.id == swarm_id) {
            if let Some(status) = status {
                swarm.run.status = status;
            }
            if let Some(spent) = spent {
                swarm.run.spent = spent;
            }
        }
    }

    fn on_failure(&mut self, failure: RpcFailure) -> Effect {
        if let Some(wizard) = self.wizard.as_mut() {
            wizard.on_call_failed();
        }
        // A rejected layout write must not strand the in-flight guard.
        if let Some(view) = self.view.as_mut() {
            view.clear_layout_saving();
        }
        if failure.is_method_not_found() {
            self.screen = Screen::Unsupported;
            self.modal = Modal::None;
            return Effect::None;
        }
        // A live run holds the swarm. Re-ask with the escalation spelled out
        // rather than silently forcing.
        if failure.code == SWARM_RUN_ACTIVE
            && let Modal::ConfirmDelete { swarm_id, .. } = &self.modal
        {
            self.modal = Modal::ConfirmDelete {
                swarm_id: swarm_id.clone(),
                force: true,
            };
            return Effect::None;
        }
        self.modal = Modal::Error {
            message: failure.message,
        };
        Effect::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_api::jsonrpc::error_codes::METHOD_NOT_FOUND;
    use zeroclaw_runtime::swarm::store::{SwarmSpec, SwarmStatus};

    fn swarm(id: &str, name: &str) -> PersistedSwarm {
        PersistedSwarm::new(SwarmSpec::new(
            id,
            name,
            "anthropic",
            "claude-sonnet-4",
            "balanced",
            "supervisor",
            "survey the field",
        ))
    }

    fn loaded() -> App {
        let mut app = App::new();
        app.on_update(Update::Swarms(vec![
            swarm("sw-1", "Research squad"),
            swarm("sw-2", "Triage squad"),
        ]));
        app
    }

    #[test]
    fn a_wrapping_cursor_never_leaves_the_list() {
        assert_eq!(Direction::Forward.wrap(2, 3), 0);
        assert_eq!(Direction::Backward.wrap(0, 3), 2);
        assert_eq!(Direction::Forward.wrap(0, 0), 0);
    }

    #[test]
    fn the_dashboard_always_offers_a_new_swarm_row() {
        let app = App::new();
        assert_eq!(app.row_count(), 1);
        assert!(app.on_new_row());
        assert_eq!(loaded().row_count(), 3);
    }

    #[test]
    fn enter_on_the_new_row_asks_for_the_wizard_shapes() {
        let mut app = loaded();
        app.on_input(Input::Up); // wrap onto the new-swarm row
        assert!(app.on_new_row());
        assert!(matches!(
            app.on_input(Input::Enter),
            Effect::Fields { provider: None }
        ));
        assert_eq!(app.screen(), &Screen::Wizard);
        assert!(app.wizard().is_none(), "the wizard waits for its shapes");
    }

    #[test]
    fn enter_on_a_swarm_opens_the_live_view_and_subscribes() {
        let mut app = loaded();
        assert!(matches!(
            app.on_input(Input::Enter),
            Effect::Subscribe { swarm_id } if swarm_id == "sw-1"
        ));
        assert_eq!(
            app.screen(),
            &Screen::SwarmView {
                swarm_id: "sw-1".to_string()
            }
        );
        assert!(app.view().is_some(), "the live canvas is mounted");
        app.on_input(Input::Escape);
        assert_eq!(app.screen(), &Screen::Dashboard);
        assert!(app.view().is_none(), "leaving tears the canvas down");
    }

    #[test]
    fn a_second_open_of_the_same_swarm_does_not_resubscribe() {
        let mut app = loaded();
        app.on_input(Input::Enter);
        app.on_update(Update::Subscribed {
            swarm_id: "sw-1".to_string(),
        });
        app.on_input(Input::Escape);
        // The fan-out already exists on this connection; re-opening streams
        // straight away without a second subscribe.
        assert!(matches!(app.on_input(Input::Enter), Effect::None));
        assert!(app.is_streaming());
    }

    #[test]
    fn q_quits_from_the_dashboard_and_the_live_view() {
        let mut app = loaded();
        assert!(matches!(app.on_input(Input::Char('q')), Effect::Quit));
        assert!(app.quit());

        let mut app = loaded();
        app.on_input(Input::Enter);
        assert!(matches!(app.on_input(Input::Char('q')), Effect::Quit));
    }

    #[test]
    fn delete_confirms_before_it_calls() {
        let mut app = loaded();
        app.on_input(Input::Char('d'));
        assert_eq!(
            app.modal(),
            &Modal::ConfirmDelete {
                swarm_id: "sw-1".to_string(),
                force: false
            }
        );
        match app.on_input(Input::Char('y')) {
            Effect::Delete { swarm_id, force } => {
                assert_eq!(swarm_id, "sw-1");
                assert!(!force);
            }
            other => panic!("expected a delete, got {other:?}"),
        }
    }

    #[test]
    fn declining_the_confirmation_calls_nothing() {
        let mut app = loaded();
        app.on_input(Input::Char('d'));
        assert!(matches!(app.on_input(Input::Char('n')), Effect::None));
        assert_eq!(app.modal(), &Modal::None);
    }

    #[test]
    fn a_live_run_re_asks_with_force_rather_than_forcing() {
        let mut app = loaded();
        app.on_input(Input::Char('d'));
        app.on_input(Input::Char('y'));
        app.on_update(Update::Failed(RpcFailure {
            code: SWARM_RUN_ACTIVE,
            message: "swarm 'sw-1' is being driven by a live run".to_string(),
        }));
        assert_eq!(
            app.modal(),
            &Modal::ConfirmDelete {
                swarm_id: "sw-1".to_string(),
                force: true
            }
        );
        match app.on_input(Input::Enter) {
            Effect::Delete { force, .. } => assert!(force),
            other => panic!("expected a forced delete, got {other:?}"),
        }
    }

    #[test]
    fn a_deleted_swarm_reloads_the_list_and_clears_the_modal() {
        let mut app = loaded();
        app.on_input(Input::Char('d'));
        app.on_input(Input::Char('y'));
        assert!(matches!(
            app.on_update(Update::Deleted {
                swarm_id: "sw-1".to_string()
            }),
            Effect::List
        ));
        assert_eq!(app.modal(), &Modal::None);
        assert!(!app.status().is_empty());
    }

    #[test]
    fn deleting_the_swarm_on_screen_returns_to_the_dashboard() {
        let mut app = loaded();
        app.on_input(Input::Enter);
        app.on_update(Update::Deleted {
            swarm_id: "sw-1".to_string(),
        });
        assert_eq!(app.screen(), &Screen::Dashboard);
    }

    #[test]
    fn method_not_found_degrades_instead_of_crashing() {
        let mut app = loaded();
        app.on_update(Update::Failed(RpcFailure {
            code: METHOD_NOT_FOUND,
            message: "Method not found".to_string(),
        }));
        assert_eq!(app.screen(), &Screen::Unsupported);
        assert_eq!(app.modal(), &Modal::None);
    }

    #[test]
    fn any_other_failure_is_a_dismissible_modal() {
        let mut app = loaded();
        app.on_update(Update::Failed(RpcFailure {
            code: -32602,
            message: "swarm 'sw-9' not found".to_string(),
        }));
        assert_eq!(
            app.modal(),
            &Modal::Error {
                message: "swarm 'sw-9' not found".to_string()
            }
        );
        app.on_input(Input::Char('x'));
        assert_eq!(app.modal(), &Modal::None);
    }

    #[test]
    fn a_created_swarm_lands_back_on_the_dashboard_selected() {
        let mut app = loaded();
        app.on_input(Input::Up);
        app.on_input(Input::Enter);
        let created = swarm("sw-3", "Fresh squad");
        assert!(matches!(
            app.on_update(Update::Created(Box::new(created.clone()))),
            Effect::List
        ));
        assert_eq!(app.screen(), &Screen::Dashboard);
        app.on_update(Update::Swarms(vec![
            swarm("sw-1", "Research squad"),
            swarm("sw-2", "Triage squad"),
            created,
        ]));
        assert_eq!(app.selected(), 2);
        assert_eq!(
            app.selected_swarm().map(|s| s.spec.id.as_str()),
            Some("sw-3")
        );
    }

    #[test]
    fn a_shrinking_list_pulls_the_cursor_back_onto_a_row() {
        let mut app = loaded();
        app.on_input(Input::Down);
        assert_eq!(app.selected(), 1);
        app.on_update(Update::Swarms(Vec::new()));
        assert_eq!(app.selected(), 0);
        assert!(app.on_new_row());
    }

    #[test]
    fn ctrl_c_leaves_from_anywhere() {
        let mut app = loaded();
        app.on_input(Input::Enter);
        assert!(matches!(app.on_input(Input::Interrupt), Effect::Quit));
        assert!(app.quit());
    }

    #[test]
    fn the_unsupported_screen_only_quits() {
        let mut app = App::unsupported();
        assert_eq!(app.screen(), &Screen::Unsupported);
        assert!(matches!(app.on_input(Input::Enter), Effect::Quit));
    }

    #[test]
    fn key_translation_drops_releases_and_catches_ctrl_c() {
        let press = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert_eq!(Input::from_key(press), Some(Input::Char('q')));

        let mut release = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(Input::from_key(release), None);

        let interrupt = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(Input::from_key(interrupt), Some(Input::Interrupt));
    }

    #[test]
    fn a_swarm_can_be_found_by_id_for_the_detail_pane() {
        let app = loaded();
        assert_eq!(
            app.swarm("sw-2").map(|s| s.spec.name.as_str()),
            Some("Triage squad")
        );
        assert!(app.swarm("sw-9").is_none());
        assert_eq!(
            app.swarms().first().map(|s| s.run.status),
            Some(SwarmStatus::Created)
        );
    }

    /// Open the live view for the first swarm and confirm its subscription.
    fn streaming() -> App {
        let mut app = loaded();
        app.on_input(Input::Enter);
        app.on_update(Update::Subscribed {
            swarm_id: "sw-1".to_string(),
        });
        app
    }

    #[test]
    fn subscription_gates_streaming() {
        let mut app = loaded();
        app.on_input(Input::Enter);
        assert!(
            !app.is_streaming(),
            "no streaming before the subscribe reply"
        );
        app.on_update(Update::Subscribed {
            swarm_id: "sw-1".to_string(),
        });
        assert!(app.is_streaming());
    }

    #[test]
    fn a_run_control_reply_updates_both_the_view_and_the_dashboard_row() {
        let mut app = streaming();
        app.on_update(Update::RunControl {
            swarm_id: "sw-1".to_string(),
            status: SwarmStatus::Running,
            spent: SwarmSpend {
                turns: 5,
                tokens: 100,
                cost_usd: 0.25,
            },
        });
        assert_eq!(
            app.view().map(ViewState::status),
            Some(SwarmStatus::Running)
        );
        // Leaving the view, the dashboard row reflects the landed status.
        app.on_input(Input::Escape);
        assert_eq!(
            app.swarm("sw-1").map(|s| s.run.status),
            Some(SwarmStatus::Running)
        );
        assert_eq!(app.swarm("sw-1").map(|s| s.run.spent.turns), Some(5));
    }

    #[test]
    fn live_notifications_route_into_the_mounted_view() {
        use zeroclaw_runtime::rpc::types::SessionUpdateEvent;
        use zeroclaw_runtime::swarm::board::{BoardEvent, BoxState, BoxStatus};

        let mut app = streaming();
        app.on_update(Update::Stream(Box::new(SwarmUpdate {
            swarm_id: "sw-1".to_string(),
            box_id: "box-1".to_string(),
            event: SessionUpdateEvent::AgentMessageChunk {
                session_id: "s".to_string(),
                text: "hello\n".to_string(),
            },
        })));
        app.on_update(Update::Board(Box::new(SwarmBoardNotify {
            swarm_id: "sw-1".to_string(),
            event: BoardEvent::Published {
                box_id: "box-1".to_string(),
                state: BoxState {
                    status: BoxStatus::Working,
                    claim: None,
                    note: String::new(),
                },
            },
        })));
        app.on_update(Update::StreamGap);

        let view = app.view().expect("view is mounted");
        assert_eq!(
            view.stream("box-1")
                .and_then(|s| s.lines().next())
                .map(|l| l.text.as_str()),
            Some("hello")
        );
        assert_eq!(
            view.badge("box-1").map(|b| b.status),
            Some(BoxStatus::Working)
        );
        assert!(
            view.feed().any(|line| line.contains("gap")),
            "a dropped burst is marked on the feed"
        );
    }

    #[test]
    fn a_layout_reply_refreshes_the_stored_revision() {
        let mut app = streaming();
        let mut saved = swarm("sw-1", "Research squad");
        saved.revision = 9;
        app.on_update(Update::LayoutSaved(Box::new(saved)));
        assert_eq!(app.swarm("sw-1").map(|s| s.revision), Some(9));
    }

    #[test]
    fn a_daemon_that_cannot_subscribe_degrades_instead_of_crashing() {
        let mut app = loaded();
        // Opening the view asks to subscribe.
        assert!(matches!(
            app.on_input(Input::Enter),
            Effect::Subscribe { .. }
        ));
        // An older daemon answers the subscribe with method-not-found.
        app.on_update(Update::Failed(RpcFailure {
            code: METHOD_NOT_FOUND,
            message: "Method not found".to_string(),
        }));
        assert_eq!(app.screen(), &Screen::Unsupported);
        assert!(!app.is_streaming());
    }
}
