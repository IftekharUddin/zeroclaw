//! The creation wizard's pure state.
//!
//! Every step but the first is whatever `swarm/fields` returned: the shapes
//! carry their own order, titles, help and controls, so this module renders
//! rows off a [`FieldDescriptor`] and never restates what a step asks. Adding
//! a wizard step is a daemon change; nothing here moves.
//!
//! Two pieces of wire knowledge do live here, because the submission shape
//! forces them: which answer keys the submission consumes (everything else is
//! an advisory config row the daemon returned for context), and how the
//! boundedness answers fold into a [`SwarmBudget`]. Both sit in one place so
//! the wizard has exactly one seam onto the contract.

use std::collections::BTreeMap;

use zeroclaw_config::traits::PropKind;
use zeroclaw_runtime::quickstart::FieldDescriptor;
use zeroclaw_runtime::rpc::types::{
    SwarmStepShape, SwarmSubmission, SwarmValidateResult, SwarmValidationError, SwarmWizardStep,
};
use zeroclaw_runtime::swarm::store::{SwarmBudget, SwarmBudgetLimits, SwarmBudgetPreset};
use zeroclaw_runtime::swarm::wizard::BUDGET_CUSTOM;

use super::state::{Direction, Effect, Input};

/// Answer keys the submission has a home for. A field the daemon returns that
/// is not in this set — the credential rows the provider step adds for an
/// unconfigured provider — is advisory: shown for context, never collected.
const SUBMISSION_KEYS: &[&str] = &[
    "name",
    "provider",
    "model",
    "risk_profile",
    "budget_preset",
    "max_turns",
    "max_tokens",
    "max_cost_usd",
    "channels",
    "role",
    "goal",
];

/// The one answer key the client owns end to end. `swarm/fields` has no step
/// for it — validation errors for `name` arrive with no step either — so the
/// wizard prepends a step of its own.
const NAME_KEY: &str = "name";

/// One rendered step. Mirrors [`SwarmStepShape`] plus the client-owned name
/// step, which has no daemon step to name.
#[derive(Debug, Clone)]
pub struct WizardStep {
    /// The daemon's step tag, or `None` for the client-owned name step.
    /// Validation errors carry the same shape, so routing an error to a step
    /// is a direct comparison.
    pub step: Option<SwarmWizardStep>,
    pub title: String,
    pub help: String,
    pub fields: Vec<FieldDescriptor>,
}

impl WizardStep {
    fn name_step() -> Self {
        Self {
            step: None,
            title: crate::t("cli-swarm-wizard-name-title", "Name"),
            help: crate::t(
                "cli-swarm-wizard-name-help",
                "What this swarm is called in the dashboard.",
            ),
            fields: vec![FieldDescriptor {
                key: NAME_KEY.to_string(),
                label: NAME_KEY.to_string(),
                help: String::new(),
                kind: PropKind::String,
                is_secret: false,
                enum_variants: None,
                required: true,
                default: None,
            }],
        }
    }
}

/// What the wizard wants the driver to do after an input.
#[derive(Debug)]
pub enum WizardAction {
    /// Redraw; nothing to ask the daemon.
    Idle,
    /// Abandon the draft and go back to the dashboard.
    Leave,
    /// Issue a call.
    Call(Effect),
}

/// The draft being authored: the step shapes, the answers so far, and where
/// the cursor is.
#[derive(Debug)]
pub struct WizardState {
    steps: Vec<WizardStep>,
    current: usize,
    /// Index into the current step's *editable* fields.
    field: usize,
    answers: BTreeMap<String, String>,
    errors: Vec<SwarmValidationError>,
    /// The provider the daemon shapes were fetched for. A step change that
    /// leaves this stale triggers the re-fetch that turns the model step into
    /// a live picker.
    fetched_provider: Option<String>,
    /// A `swarm/validate` is in flight for a submit, not for a preview.
    awaiting_submit: bool,
}

impl WizardState {
    /// Start a draft from the daemon's step shapes.
    pub fn new(steps: Vec<SwarmStepShape>) -> Self {
        let mut state = Self {
            steps: vec![WizardStep::name_step()],
            current: 0,
            field: 0,
            answers: BTreeMap::new(),
            errors: Vec::new(),
            fetched_provider: None,
            awaiting_submit: false,
        };
        state.merge_steps(steps, None);
        state
    }

    /// Replace the daemon-supplied steps, keeping the name step, every answer
    /// already given, and the cursor. Called on the post-provider re-fetch.
    pub fn merge_steps(&mut self, steps: Vec<SwarmStepShape>, provider: Option<String>) {
        self.steps.truncate(1);
        for shape in steps {
            self.steps.push(WizardStep {
                step: Some(shape.step),
                title: shape.title,
                help: shape.help,
                fields: shape.fields,
            });
        }
        self.fetched_provider = provider;
        self.seed_defaults();
        self.current = self.current.min(self.steps.len().saturating_sub(1));
        self.clamp_field();
    }

    /// Fill in every prefill the daemon offered that the author has not
    /// already answered, so a step opens on a valid choice rather than empty.
    fn seed_defaults(&mut self) {
        for step in &self.steps {
            for field in &step.fields {
                if let Some(default) = &field.default
                    && !self.answers.contains_key(&field.key)
                {
                    self.answers.insert(field.key.clone(), default.clone());
                }
            }
        }
    }

    // ── Read side (render) ───────────────────────────────────────

    pub fn steps(&self) -> &[WizardStep] {
        &self.steps
    }

    pub fn current_index(&self) -> usize {
        self.current
    }

    pub fn current_step(&self) -> Option<&WizardStep> {
        self.steps.get(self.current)
    }

    pub fn field_index(&self) -> usize {
        self.field
    }

    pub fn answer(&self, key: &str) -> &str {
        self.answers.get(key).map_or("", String::as_str)
    }

    /// Errors the daemon rejected the draft with, filtered to `field`.
    pub fn field_errors<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a str> {
        self.errors
            .iter()
            .filter(move |e| e.field == key)
            .map(|e| e.message.as_str())
    }

    /// Every error, in daemon order — rendered as a footer so a rejection
    /// that belongs to another step is still visible.
    pub fn errors(&self) -> &[SwarmValidationError] {
        &self.errors
    }

    pub fn awaiting_submit(&self) -> bool {
        self.awaiting_submit
    }

    /// The rows to draw for the current step, in daemon order.
    pub fn visible_fields(&self) -> Vec<&FieldDescriptor> {
        let Some(step) = self.current_step() else {
            return Vec::new();
        };
        step.fields.iter().filter(|f| self.is_visible(f)).collect()
    }

    /// The rows the author can put a cursor on: the visible rows the
    /// submission actually consumes.
    pub fn editable_fields(&self) -> Vec<&FieldDescriptor> {
        self.visible_fields()
            .into_iter()
            .filter(|f| is_submission_key(&f.key))
            .collect()
    }

    /// A field the submission has no home for. Rendered for context and
    /// skipped by the cursor, so a wizard never collects a value it would
    /// silently drop.
    pub fn is_advisory(field: &FieldDescriptor) -> bool {
        !is_submission_key(&field.key)
    }

    /// The custom-budget rule, the wizard's one piece of step knowledge: the
    /// three ceilings the daemon marks optional are only asked for when the
    /// preset picker sitting beside them says `custom`.
    fn is_visible(&self, field: &FieldDescriptor) -> bool {
        if !is_custom_budget_field(&field.key) {
            return true;
        }
        self.answer("budget_preset") == BUDGET_CUSTOM
    }

    // ── Write side (reducer) ─────────────────────────────────────

    pub fn on_input(&mut self, input: Input) -> WizardAction {
        match input {
            Input::Escape => self.back(),
            Input::Enter => self.advance(),
            Input::Tab => {
                self.move_field(Direction::Forward);
                WizardAction::Idle
            }
            Input::BackTab => {
                self.move_field(Direction::Backward);
                WizardAction::Idle
            }
            Input::Up => self.vertical(Direction::Backward),
            Input::Down => self.vertical(Direction::Forward),
            Input::Backspace => {
                self.edit(|value| {
                    value.pop();
                });
                WizardAction::Idle
            }
            Input::Char(c) => {
                self.edit(|value| value.push(c));
                WizardAction::Idle
            }
            Input::Interrupt => WizardAction::Leave,
        }
    }

    /// Fold a `swarm/validate` answer back in. An accepted draft that was
    /// waiting on a submit goes straight to `swarm/create`.
    pub fn on_validation(&mut self, result: &SwarmValidateResult) -> WizardAction {
        match result {
            SwarmValidateResult::Ok if self.awaiting_submit => {
                self.errors.clear();
                self.awaiting_submit = false;
                WizardAction::Call(Effect::Create(Box::new(self.submission())))
            }
            SwarmValidateResult::Ok => {
                self.errors.clear();
                WizardAction::Idle
            }
            SwarmValidateResult::Errors { errors } => {
                self.awaiting_submit = false;
                self.errors = errors.clone();
                if let Some(index) = self.first_rejected_step() {
                    self.current = index;
                    self.field = 0;
                }
                WizardAction::Idle
            }
        }
    }

    /// A call failed outright; the draft is intact but no longer submitting.
    pub fn on_call_failed(&mut self) {
        self.awaiting_submit = false;
    }

    fn back(&mut self) -> WizardAction {
        if self.current == 0 {
            return WizardAction::Leave;
        }
        self.current -= 1;
        self.field = 0;
        WizardAction::Idle
    }

    fn advance(&mut self) -> WizardAction {
        if self.current + 1 >= self.steps.len() {
            self.awaiting_submit = true;
            return WizardAction::Call(Effect::Validate(Box::new(self.submission())));
        }
        let refetch = self.provider_refetch();
        self.current += 1;
        self.field = 0;
        match refetch {
            Some(provider) => WizardAction::Call(Effect::Fields {
                provider: Some(provider),
            }),
            None => WizardAction::Idle,
        }
    }

    /// The provider answer the shapes are now stale against, if leaving this
    /// step changed it. Re-fetching is what fills the model step's catalog.
    fn provider_refetch(&self) -> Option<String> {
        let step = self.current_step()?;
        if !step.fields.iter().any(|f| f.key == "provider") {
            return None;
        }
        let chosen = self.answer("provider").trim();
        if chosen.is_empty() || self.fetched_provider.as_deref() == Some(chosen) {
            return None;
        }
        Some(chosen.to_string())
    }

    /// Up / down: cycle a picker's choices when the cursor is on one, move
    /// between rows otherwise.
    fn vertical(&mut self, direction: Direction) -> WizardAction {
        let Some(variants) = self.focused_variants().filter(|v| !v.is_empty()) else {
            self.move_field(direction);
            return WizardAction::Idle;
        };
        let Some(key) = self.focused_key() else {
            return WizardAction::Idle;
        };
        let current = self.answer(&key);
        // An answer outside the closed set (nothing chosen yet) lands on the
        // first choice, whichever way the cursor moved.
        let next = match variants.iter().position(|v| v == current) {
            Some(at) => direction.wrap(at, variants.len()),
            None => 0,
        };
        if let Some(choice) = variants.get(next) {
            self.answers.insert(key, choice.clone());
        }
        self.clamp_field();
        WizardAction::Idle
    }

    fn move_field(&mut self, direction: Direction) {
        let len = self.editable_fields().len();
        if len == 0 {
            self.field = 0;
            return;
        }
        self.field = direction.wrap(self.field, len);
    }

    fn clamp_field(&mut self) {
        let len = self.editable_fields().len();
        if self.field >= len {
            self.field = len.saturating_sub(1);
        }
    }

    /// Apply an edit to the focused free-text field. Pickers ignore typing —
    /// their value is always one of the daemon's choices.
    fn edit(&mut self, apply: impl FnOnce(&mut String)) {
        if self.focused_variants().is_some_and(|v| !v.is_empty()) {
            return;
        }
        let Some(key) = self.focused_key() else {
            return;
        };
        let value = self.answers.entry(key).or_default();
        apply(value);
    }

    fn focused_key(&self) -> Option<String> {
        self.editable_fields()
            .get(self.field)
            .map(|f| f.key.clone())
    }

    /// The closed set behind the focused field, when it has one.
    fn focused_variants(&self) -> Option<Vec<String>> {
        let field = *self.editable_fields().get(self.field)?;
        if matches!(field.kind, PropKind::StringArray) {
            // A list field is typed as comma-separated text; its variants are
            // suggestions, not a picker.
            return None;
        }
        field.enum_variants.clone()
    }

    fn first_rejected_step(&self) -> Option<usize> {
        let rejected = self.errors.first()?.step;
        self.steps.iter().position(|s| s.step == rejected)
    }

    // ── Submission ───────────────────────────────────────────────

    /// The draft as the daemon takes it. `swarm_id` and `boxes` stay absent:
    /// the daemon generates the id and the default roster, and the box canvas
    /// owns the roster after that.
    pub fn submission(&self) -> SwarmSubmission {
        SwarmSubmission {
            swarm_id: None,
            name: self.answer(NAME_KEY).trim().to_string(),
            provider: self.answer("provider").trim().to_string(),
            model: self.answer("model").trim().to_string(),
            risk_profile: self.answer("risk_profile").trim().to_string(),
            budget: self.budget(),
            channels: split_list(self.answer("channels")),
            role: self.answer("role").trim().to_string(),
            goal: self.answer("goal").trim().to_string(),
            boxes: None,
        }
    }

    /// Fold the boundedness answers into a budget. An unparseable ceiling
    /// becomes zero on purpose: the daemon's validator rejects that with a
    /// per-field message, which is a better error than one this client made up.
    fn budget(&self) -> SwarmBudget {
        let preset = self.answer("budget_preset").trim();
        if preset == BUDGET_CUSTOM {
            return SwarmBudget::Custom(SwarmBudgetLimits {
                max_turns: self.answer("max_turns").trim().parse().unwrap_or(0),
                max_tokens: self.answer("max_tokens").trim().parse().unwrap_or(0),
                max_cost_usd: self.answer("max_cost_usd").trim().parse().unwrap_or(0.0),
            });
        }
        SwarmBudgetPreset::from_wire(preset).map_or_else(SwarmBudget::default, SwarmBudget::Preset)
    }
}

fn is_submission_key(key: &str) -> bool {
    SUBMISSION_KEYS.contains(&key)
}

fn is_custom_budget_field(key: &str) -> bool {
    matches!(key, "max_turns" | "max_tokens" | "max_cost_usd")
}

/// Split a comma-separated list answer. Blanks are dropped so a trailing
/// comma is not an empty alias the validator has to reject.
fn split_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(key: &str, kind: PropKind) -> FieldDescriptor {
        FieldDescriptor {
            key: key.to_string(),
            label: key.to_string(),
            help: String::new(),
            kind,
            is_secret: false,
            enum_variants: None,
            required: true,
            default: None,
        }
    }

    fn enum_field(key: &str, variants: &[&str], default: &str) -> FieldDescriptor {
        FieldDescriptor {
            enum_variants: Some(variants.iter().map(ToString::to_string).collect()),
            default: Some(default.to_string()),
            ..descriptor(key, PropKind::Enum)
        }
    }

    /// The seven shapes the daemon returns, trimmed to what the wizard reads.
    fn shapes() -> Vec<SwarmStepShape> {
        vec![
            SwarmStepShape {
                step: SwarmWizardStep::Provider,
                title: "Model provider".to_string(),
                help: String::new(),
                fields: vec![FieldDescriptor {
                    enum_variants: Some(vec!["anthropic".to_string(), "openai".to_string()]),
                    ..descriptor("provider", PropKind::AliasRef)
                }],
            },
            SwarmStepShape {
                step: SwarmWizardStep::Model,
                title: "Model".to_string(),
                help: String::new(),
                fields: vec![descriptor("model", PropKind::String)],
            },
            SwarmStepShape {
                step: SwarmWizardStep::RiskProfile,
                title: "Risk profile".to_string(),
                help: String::new(),
                fields: vec![enum_field(
                    "risk_profile",
                    &["balanced", "strict"],
                    "balanced",
                )],
            },
            SwarmStepShape {
                step: SwarmWizardStep::Boundedness,
                title: "Boundedness".to_string(),
                help: String::new(),
                fields: vec![
                    enum_field(
                        "budget_preset",
                        &["quick", "standard", "marathon", "custom"],
                        "standard",
                    ),
                    FieldDescriptor {
                        required: false,
                        default: Some("100".to_string()),
                        ..descriptor("max_turns", PropKind::Integer)
                    },
                    FieldDescriptor {
                        required: false,
                        default: Some("1000000".to_string()),
                        ..descriptor("max_tokens", PropKind::Integer)
                    },
                    FieldDescriptor {
                        required: false,
                        default: Some("20".to_string()),
                        ..descriptor("max_cost_usd", PropKind::Float)
                    },
                ],
            },
            SwarmStepShape {
                step: SwarmWizardStep::Channels,
                title: "Channels".to_string(),
                help: String::new(),
                fields: vec![FieldDescriptor {
                    required: false,
                    enum_variants: Some(vec!["ops".to_string()]),
                    ..descriptor("channels", PropKind::StringArray)
                }],
            },
            SwarmStepShape {
                step: SwarmWizardStep::Role,
                title: "Role".to_string(),
                help: String::new(),
                fields: vec![descriptor("role", PropKind::String)],
            },
            SwarmStepShape {
                step: SwarmWizardStep::Goal,
                title: "Goal".to_string(),
                help: String::new(),
                fields: vec![descriptor("goal", PropKind::String)],
            },
        ]
    }

    fn typed(state: &mut WizardState, text: &str) {
        for c in text.chars() {
            state.on_input(Input::Char(c));
        }
    }

    #[test]
    fn step_order_is_the_daemons_order_behind_the_client_owned_name_step() {
        let state = WizardState::new(shapes());
        let order: Vec<Option<SwarmWizardStep>> = state.steps().iter().map(|s| s.step).collect();
        assert_eq!(
            order,
            vec![
                None,
                Some(SwarmWizardStep::Provider),
                Some(SwarmWizardStep::Model),
                Some(SwarmWizardStep::RiskProfile),
                Some(SwarmWizardStep::Boundedness),
                Some(SwarmWizardStep::Channels),
                Some(SwarmWizardStep::Role),
                Some(SwarmWizardStep::Goal),
            ]
        );
    }

    #[test]
    fn daemon_defaults_prefill_the_answers() {
        let state = WizardState::new(shapes());
        assert_eq!(state.answer("risk_profile"), "balanced");
        assert_eq!(state.answer("budget_preset"), "standard");
        assert_eq!(state.answer("name"), "", "the name step has no prefill");
    }

    #[test]
    fn leaving_the_provider_step_refetches_the_shapes_for_that_provider() {
        let mut state = WizardState::new(shapes());
        state.on_input(Input::Enter); // leave the name step
        state.on_input(Input::Down); // pick the first provider variant
        let action = state.on_input(Input::Enter);
        match action {
            WizardAction::Call(Effect::Fields { provider }) => {
                assert_eq!(provider.as_deref(), Some("anthropic"));
            }
            other => panic!("expected a shape re-fetch, got {other:?}"),
        }
    }

    #[test]
    fn a_refetch_keeps_answers_and_position() {
        let mut state = WizardState::new(shapes());
        typed(&mut state, "Research squad");
        state.on_input(Input::Enter);
        state.on_input(Input::Down);
        state.on_input(Input::Enter);
        let at = state.current_index();

        let mut refreshed = shapes();
        refreshed[1].fields = vec![FieldDescriptor {
            enum_variants: Some(vec!["claude-sonnet-4".to_string()]),
            default: Some("claude-sonnet-4".to_string()),
            ..descriptor("model", PropKind::Enum)
        }];
        state.merge_steps(refreshed, Some("anthropic".to_string()));

        assert_eq!(state.answer("name"), "Research squad");
        assert_eq!(state.answer("provider"), "anthropic");
        assert_eq!(state.answer("model"), "claude-sonnet-4");
        assert_eq!(state.current_index(), at);
    }

    #[test]
    fn the_provider_is_not_refetched_twice_for_the_same_answer() {
        let mut state = WizardState::new(shapes());
        state.on_input(Input::Enter);
        state.on_input(Input::Down);
        state.on_input(Input::Enter);
        state.merge_steps(shapes(), Some("anthropic".to_string()));
        // Walk back onto the provider step and off it again, unchanged.
        state.on_input(Input::Escape);
        assert!(matches!(state.on_input(Input::Enter), WizardAction::Idle));
    }

    #[test]
    fn the_custom_ceilings_appear_only_under_the_custom_preset() {
        let mut state = WizardState::new(shapes());
        for _ in 0..4 {
            state.on_input(Input::Enter);
        }
        let visible: Vec<&str> = state
            .visible_fields()
            .iter()
            .map(|f| f.key.as_str())
            .collect();
        assert_eq!(visible, vec!["budget_preset"]);

        // standard -> marathon -> custom
        state.on_input(Input::Down);
        state.on_input(Input::Down);
        assert_eq!(state.answer("budget_preset"), "custom");
        let visible: Vec<&str> = state
            .visible_fields()
            .iter()
            .map(|f| f.key.as_str())
            .collect();
        assert_eq!(
            visible,
            vec!["budget_preset", "max_turns", "max_tokens", "max_cost_usd"]
        );
    }

    #[test]
    fn a_custom_preset_submits_the_typed_triple() {
        let mut state = WizardState::new(shapes());
        for _ in 0..4 {
            state.on_input(Input::Enter);
        }
        state.on_input(Input::Down);
        state.on_input(Input::Down);
        assert_eq!(
            state.submission().budget,
            SwarmBudget::Custom(SwarmBudgetLimits {
                max_turns: 100,
                max_tokens: 1_000_000,
                max_cost_usd: 20.0,
            })
        );
    }

    #[test]
    fn an_unparseable_ceiling_becomes_zero_for_the_daemon_to_reject() {
        let mut state = WizardState::new(shapes());
        for _ in 0..4 {
            state.on_input(Input::Enter);
        }
        state.on_input(Input::Down);
        state.on_input(Input::Down);
        state.on_input(Input::Tab); // onto max_turns
        for _ in 0..8 {
            state.on_input(Input::Backspace);
        }
        typed(&mut state, "lots");
        let SwarmBudget::Custom(limits) = state.submission().budget else {
            panic!("custom preset must submit a custom budget");
        };
        assert_eq!(limits.max_turns, 0);
    }

    #[test]
    fn a_preset_answer_submits_a_preset_budget() {
        let state = WizardState::new(shapes());
        assert_eq!(
            state.submission().budget,
            SwarmBudget::Preset(SwarmBudgetPreset::Standard)
        );
    }

    #[test]
    fn channels_are_split_on_commas_and_blanks_dropped() {
        let mut state = WizardState::new(shapes());
        for _ in 0..5 {
            state.on_input(Input::Enter);
        }
        typed(&mut state, " ops , , alerts ");
        assert_eq!(
            state.submission().channels,
            vec!["ops".to_string(), "alerts".to_string()]
        );
    }

    #[test]
    fn the_last_step_validates_before_it_creates() {
        let mut state = WizardState::new(shapes());
        for _ in 0..7 {
            state.on_input(Input::Enter);
        }
        assert!(matches!(
            state.on_input(Input::Enter),
            WizardAction::Call(Effect::Validate(_))
        ));
        assert!(state.awaiting_submit());
        assert!(matches!(
            state.on_validation(&SwarmValidateResult::Ok),
            WizardAction::Call(Effect::Create(_))
        ));
    }

    #[test]
    fn rejections_jump_to_the_step_that_owns_the_first_bad_field() {
        let mut state = WizardState::new(shapes());
        for _ in 0..8 {
            state.on_input(Input::Enter);
        }
        state.on_validation(&SwarmValidateResult::Errors {
            errors: vec![SwarmValidationError {
                step: Some(SwarmWizardStep::Role),
                field: "role".to_string(),
                message: "describe the supervising role the swarm plays".to_string(),
            }],
        });
        assert_eq!(
            state.current_step().and_then(|s| s.step),
            Some(SwarmWizardStep::Role)
        );
        assert!(!state.awaiting_submit());
        assert_eq!(
            state.field_errors("role").collect::<Vec<_>>(),
            vec!["describe the supervising role the swarm plays"]
        );
    }

    #[test]
    fn a_stepless_rejection_lands_on_the_client_owned_name_step() {
        let mut state = WizardState::new(shapes());
        for _ in 0..8 {
            state.on_input(Input::Enter);
        }
        state.on_validation(&SwarmValidateResult::Errors {
            errors: vec![SwarmValidationError {
                step: None,
                field: "name".to_string(),
                message: "a swarm needs a name".to_string(),
            }],
        });
        assert_eq!(state.current_index(), 0);
        assert_eq!(state.current_step().and_then(|s| s.step), None);
    }

    #[test]
    fn escape_walks_back_and_then_leaves() {
        let mut state = WizardState::new(shapes());
        state.on_input(Input::Enter);
        assert!(matches!(state.on_input(Input::Escape), WizardAction::Idle));
        assert_eq!(state.current_index(), 0);
        assert!(matches!(state.on_input(Input::Escape), WizardAction::Leave));
    }

    #[test]
    fn a_picker_ignores_typing_and_cycles_instead() {
        let mut state = WizardState::new(shapes());
        state.on_input(Input::Enter);
        typed(&mut state, "zzz");
        assert_eq!(state.answer("provider"), "");
        state.on_input(Input::Up);
        assert_eq!(state.answer("provider"), "anthropic");
        state.on_input(Input::Up);
        assert_eq!(state.answer("provider"), "openai");
    }

    #[test]
    fn advisory_rows_are_shown_but_never_collected() {
        let mut shapes = shapes();
        shapes[0].fields.push(FieldDescriptor {
            is_secret: true,
            ..descriptor("api-key", PropKind::String)
        });
        let mut state = WizardState::new(shapes);
        state.on_input(Input::Enter);
        assert_eq!(state.visible_fields().len(), 2);
        assert_eq!(state.editable_fields().len(), 1);
        assert!(WizardState::is_advisory(state.visible_fields()[1]));
        state.on_input(Input::Tab);
        assert_eq!(state.field_index(), 0, "the cursor cannot reach a secret");
        typed(&mut state, "secret");
        assert_eq!(state.answer("api-key"), "");
    }

    #[test]
    fn a_list_field_takes_typed_text_rather_than_cycling_its_suggestions() {
        let mut state = WizardState::new(shapes());
        for _ in 0..5 {
            state.on_input(Input::Enter);
        }
        typed(&mut state, "ops");
        assert_eq!(state.answer("channels"), "ops");
    }
}
