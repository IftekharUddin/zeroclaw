//! Swarm authoring: the submission shape, its validator, and the wizard
//! field shapes a client renders from.
//!
//! Two rules hold this module together:
//!
//! 1. **One validator.** `swarm/create`, `swarm/update` and `swarm/validate`
//!    all go through [`prepare_spec`], so a submission the dry run accepted is
//!    exactly the one that persists, and neither surface can drift.
//! 2. **The daemon owns the shape.** [`wizard_steps`] describes every step the
//!    client renders — order, labels, controls, choices, defaults. Adding a
//!    step is a change here and nowhere else.
//!
//! The field shapes are not hand-rolled: they are Quickstart's own
//! [`FieldDescriptor`], filled from Quickstart's [`field_shape`] and model
//! catalog plus the canonical `RISK_PRESETS` table.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use zeroclaw_config::presets::RISK_PRESETS;
use zeroclaw_config::schema::Config;
use zeroclaw_config::traits::{AliasSource, PropKind};

use crate::quickstart::{FieldDescriptor, FieldSection, field_shape};
use crate::swarm::store::{
    BoxSpec, DEFAULT_ROSTER_SIZE, SwarmBudget, SwarmBudgetPreset, SwarmSpec, default_roster,
};

/// Longest accepted swarm name.
pub const MAX_SWARM_NAME_LEN: usize = 120;
/// Longest accepted supervising role.
pub const MAX_SWARM_ROLE_LEN: usize = 120;
/// Longest accepted goal statement.
pub const MAX_SWARM_GOAL_LEN: usize = 4_000;
/// Longest accepted caller-supplied swarm id.
pub const MAX_SWARM_ID_LEN: usize = 64;
/// Most boxes one swarm may carry. The pane grid addresses slots by index, so
/// this is also the exclusive upper bound on [`BoxSpec::slot`]. A plain code
/// constant like the budget presets: swarms have no config table.
pub const MAX_ROSTER_SIZE: usize = 16;

/// The picker value that means "not a preset, read the triple below".
pub const BUDGET_CUSTOM: &str = "custom";

/// Wizard steps, in the order a client walks them. `swarm/fields` returns one
/// [`SwarmStepShape`] per entry, so this array is the wizard's running order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmWizardStep {
    Provider,
    Model,
    RiskProfile,
    Boundedness,
    Channels,
    Role,
    Goal,
}

impl SwarmWizardStep {
    /// Every step, in wizard order.
    pub const ALL: [Self; 7] = [
        Self::Provider,
        Self::Model,
        Self::RiskProfile,
        Self::Boundedness,
        Self::Channels,
        Self::Role,
        Self::Goal,
    ];

    /// Fluent key for the step heading.
    const fn title_key(self) -> &'static str {
        match self {
            Self::Provider => "swarm-wizard-provider-title",
            Self::Model => "swarm-wizard-model-title",
            Self::RiskProfile => "swarm-wizard-risk-profile-title",
            Self::Boundedness => "swarm-wizard-boundedness-title",
            Self::Channels => "swarm-wizard-channels-title",
            Self::Role => "swarm-wizard-role-title",
            Self::Goal => "swarm-wizard-goal-title",
        }
    }

    /// Fluent key for the one-line blurb under the heading.
    const fn help_key(self) -> &'static str {
        match self {
            Self::Provider => "swarm-wizard-provider-help",
            Self::Model => "swarm-wizard-model-help",
            Self::RiskProfile => "swarm-wizard-risk-profile-help",
            Self::Boundedness => "swarm-wizard-boundedness-help",
            Self::Channels => "swarm-wizard-channels-help",
            Self::Role => "swarm-wizard-role-help",
            Self::Goal => "swarm-wizard-goal-help",
        }
    }
}

/// One rendered wizard step: what to call it, what to say about it, and the
/// inputs it collects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmStepShape {
    pub step: SwarmWizardStep,
    /// Localized heading.
    pub title: String,
    /// Localized one-line blurb.
    pub help: String,
    /// Inputs to render, in order. Never empty.
    pub fields: Vec<FieldDescriptor>,
}

/// One rejected field. The step is what a client jumps back to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmValidationError {
    /// Wizard step that owns the field, or `None` for the fields no step
    /// owns: the swarm id, its name, and the box roster (the box canvas
    /// edits that, not the wizard).
    #[serde(default)]
    pub step: Option<SwarmWizardStep>,
    pub field: String,
    pub message: String,
}

impl SwarmValidationError {
    fn new(
        step: Option<SwarmWizardStep>,
        field: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            step,
            field: field.into(),
            message: message.into(),
        }
    }
}

/// Everything the wizard collects for one swarm. Optional fields carry the
/// daemon's default rather than forcing a client to restate it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmSubmission {
    /// Caller-chosen id. Generated when absent, which is the normal path;
    /// supplied on update so the round trip keeps the same swarm.
    #[serde(default)]
    pub swarm_id: Option<String>,
    pub name: String,
    pub provider: String,
    pub model: String,
    pub risk_profile: String,
    #[serde(default)]
    pub budget: SwarmBudget,
    #[serde(default)]
    pub channels: Vec<String>,
    pub role: String,
    pub goal: String,
    /// Roster override. Absent means the default unassigned roster.
    #[serde(default)]
    pub boxes: Option<Vec<BoxSpec>>,
}

impl SwarmSubmission {
    /// The submission a stored spec came from. Lets an update be expressed as
    /// "the authored submission with these fields replaced" and re-run through
    /// [`prepare_spec`], so edits and creations share one validator.
    pub fn from_spec(spec: &SwarmSpec) -> Self {
        Self {
            swarm_id: Some(spec.id.clone()),
            name: spec.name.clone(),
            provider: spec.provider.clone(),
            model: spec.model.clone(),
            risk_profile: spec.risk_profile.clone(),
            budget: spec.budget,
            channels: spec.channels.clone(),
            role: spec.role.clone(),
            goal: spec.goal.clone(),
            boxes: Some(spec.boxes.clone()),
        }
    }
}

/// A partial edit of an authored swarm. Every field is optional; an absent
/// field is left as stored. Deliberately carries no roster — box slots, roles
/// and jobs move through `swarm/update-layout`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SwarmSpecPatch {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub risk_profile: Option<String>,
    #[serde(default)]
    pub budget: Option<SwarmBudget>,
    #[serde(default)]
    pub channels: Option<Vec<String>>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub goal: Option<String>,
}

impl SwarmSpecPatch {
    /// Overlay the set fields onto a submission. Normalization and validation
    /// stay in [`prepare_spec`], so this only chooses values.
    pub fn apply(&self, submission: &mut SwarmSubmission) {
        if let Some(v) = &self.name {
            submission.name = v.clone();
        }
        if let Some(v) = &self.provider {
            submission.provider = v.clone();
        }
        if let Some(v) = &self.model {
            submission.model = v.clone();
        }
        if let Some(v) = &self.risk_profile {
            submission.risk_profile = v.clone();
        }
        if let Some(v) = self.budget {
            submission.budget = v;
        }
        if let Some(v) = &self.channels {
            submission.channels = v.clone();
        }
        if let Some(v) = &self.role {
            submission.role = v.clone();
        }
        if let Some(v) = &self.goal {
            submission.goal = v.clone();
        }
    }
}

// ── Validation ─────────────────────────────────────────────────────

/// Normalize a submission and validate it against live config. The single
/// gate every write goes through: `swarm/validate` throws the result away,
/// `swarm/create` and `swarm/update` persist it.
pub fn prepare_spec(
    submission: &SwarmSubmission,
    config: &Config,
) -> Result<SwarmSpec, Vec<SwarmValidationError>> {
    let mut errors = Vec::new();

    let id = match submission.swarm_id.as_deref().map(str::trim) {
        Some(id) if !id.is_empty() => {
            errors.extend(validate_id(id));
            id.to_string()
        }
        _ => new_swarm_id(),
    };

    let spec = SwarmSpec {
        id,
        name: submission.name.trim().to_string(),
        provider: submission.provider.trim().to_string(),
        model: submission.model.trim().to_string(),
        risk_profile: submission.risk_profile.trim().to_string(),
        budget: submission.budget,
        channels: submission
            .channels
            .iter()
            .map(|c| c.trim().to_string())
            .collect(),
        role: submission.role.trim().to_string(),
        goal: submission.goal.trim().to_string(),
        boxes: match &submission.boxes {
            Some(boxes) => normalize_roster(boxes),
            None => default_roster(DEFAULT_ROSTER_SIZE),
        },
    };

    errors.extend(validate_spec(&spec, config));
    if errors.is_empty() {
        Ok(spec)
    } else {
        Err(errors)
    }
}

/// Validate an already-normalized spec. Shared by [`prepare_spec`] and any
/// caller holding a stored spec it is about to write back.
pub fn validate_spec(spec: &SwarmSpec, config: &Config) -> Vec<SwarmValidationError> {
    let mut errors = Vec::new();

    if spec.name.is_empty() {
        errors.push(SwarmValidationError::new(
            None,
            "name",
            "a swarm needs a name",
        ));
    } else if spec.name.chars().count() > MAX_SWARM_NAME_LEN {
        errors.push(SwarmValidationError::new(
            None,
            "name",
            format!("a swarm name is at most {MAX_SWARM_NAME_LEN} characters"),
        ));
    }

    errors.extend(validate_provider(&spec.provider, config));

    if spec.model.is_empty() {
        errors.push(SwarmValidationError::new(
            Some(SwarmWizardStep::Model),
            "model",
            "pick the model every box runs",
        ));
    }

    errors.extend(validate_risk_profile(&spec.risk_profile, config));
    errors.extend(validate_budget(spec.budget));
    errors.extend(validate_channels(&spec.channels, config));

    if spec.role.is_empty() {
        errors.push(SwarmValidationError::new(
            Some(SwarmWizardStep::Role),
            "role",
            "describe the supervising role the swarm plays",
        ));
    } else if spec.role.chars().count() > MAX_SWARM_ROLE_LEN {
        errors.push(SwarmValidationError::new(
            Some(SwarmWizardStep::Role),
            "role",
            format!("a swarm role is at most {MAX_SWARM_ROLE_LEN} characters"),
        ));
    }

    if spec.goal.is_empty() {
        errors.push(SwarmValidationError::new(
            Some(SwarmWizardStep::Goal),
            "goal",
            "state what this swarm is being created to accomplish",
        ));
    } else if spec.goal.chars().count() > MAX_SWARM_GOAL_LEN {
        errors.push(SwarmValidationError::new(
            Some(SwarmWizardStep::Goal),
            "goal",
            format!("a swarm goal is at most {MAX_SWARM_GOAL_LEN} characters"),
        ));
    }

    errors.extend(validate_roster(&spec.boxes));
    errors
}

/// Trim every authored string in a roster. Applied before validation so a
/// stored box never carries stray whitespace in its id.
pub fn normalize_roster(boxes: &[BoxSpec]) -> Vec<BoxSpec> {
    boxes
        .iter()
        .map(|b| BoxSpec {
            box_id: b.box_id.trim().to_string(),
            role: b.role.trim().to_string(),
            job: b.job.trim().to_string(),
            slot: b.slot,
        })
        .collect()
}

/// Validate a roster on its own. `swarm/update-layout` uses this rather than
/// the whole-spec validator: moving a box must not fail because a provider
/// alias was renamed out from under a swarm that already exists.
pub fn validate_roster(boxes: &[BoxSpec]) -> Vec<SwarmValidationError> {
    let mut errors = Vec::new();
    if boxes.is_empty() {
        errors.push(SwarmValidationError::new(
            None,
            "boxes",
            "a swarm needs at least one box",
        ));
        return errors;
    }
    if boxes.len() > MAX_ROSTER_SIZE {
        errors.push(SwarmValidationError::new(
            None,
            "boxes",
            format!(
                "a swarm carries at most {MAX_ROSTER_SIZE} boxes, got {}",
                boxes.len()
            ),
        ));
    }

    let mut seen_ids: BTreeSet<&str> = BTreeSet::new();
    let mut seen_slots: BTreeSet<u8> = BTreeSet::new();
    for b in boxes {
        if b.box_id.is_empty() {
            errors.push(SwarmValidationError::new(
                None,
                "boxes",
                "every box needs a box_id",
            ));
        } else if !seen_ids.insert(b.box_id.as_str()) {
            errors.push(SwarmValidationError::new(
                None,
                "boxes",
                format!("box_id '{}' is used by more than one box", b.box_id),
            ));
        }

        if usize::from(b.slot) >= MAX_ROSTER_SIZE {
            errors.push(SwarmValidationError::new(
                None,
                "boxes",
                format!(
                    "box '{}' asks for slot {} but the pane grid has slots 0-{}",
                    b.box_id,
                    b.slot,
                    MAX_ROSTER_SIZE - 1
                ),
            ));
        } else if !seen_slots.insert(b.slot) {
            errors.push(SwarmValidationError::new(
                None,
                "boxes",
                format!("slot {} is claimed by more than one box", b.slot),
            ));
        }
    }
    errors
}

fn validate_id(id: &str) -> Vec<SwarmValidationError> {
    let mut errors = Vec::new();
    if id.chars().count() > MAX_SWARM_ID_LEN {
        errors.push(SwarmValidationError::new(
            None,
            "swarm_id",
            format!("a swarm id is at most {MAX_SWARM_ID_LEN} characters"),
        ));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        errors.push(SwarmValidationError::new(
            None,
            "swarm_id",
            format!(
                "swarm id '{id}' may only use ASCII letters, digits, '-' and '_'; \
                 omit it to have one generated"
            ),
        ));
    }
    errors
}

fn validate_provider(provider: &str, config: &Config) -> Vec<SwarmValidationError> {
    let step = Some(SwarmWizardStep::Provider);
    if provider.is_empty() {
        return vec![SwarmValidationError::new(
            step,
            "provider",
            "pick the model provider every box talks to",
        )];
    }
    let configured = config.resolve_alias_source(AliasSource::ModelProviders);
    if configured.iter().any(|c| c == provider) {
        return Vec::new();
    }
    // A bare family name is accepted too: the runtime resolves one the same
    // way `config/catalog-models` does, so the wizard must not be stricter
    // than the request path it feeds.
    if zeroclaw_providers::list_model_providers()
        .iter()
        .any(|p| p.name == provider_family(provider))
    {
        return Vec::new();
    }
    let known = if configured.is_empty() {
        "no model provider is configured yet; run quickstart, or name a provider family".to_string()
    } else {
        format!("configured providers: {}", configured.join(", "))
    };
    vec![SwarmValidationError::new(
        step,
        "provider",
        format!("unknown model provider '{provider}' ({known})"),
    )]
}

fn validate_risk_profile(risk_profile: &str, config: &Config) -> Vec<SwarmValidationError> {
    let step = Some(SwarmWizardStep::RiskProfile);
    if risk_profile.is_empty() {
        return vec![SwarmValidationError::new(
            step,
            "risk_profile",
            "pick the risk profile every box runs under",
        )];
    }
    let known = risk_profile_names(config);
    if known.iter().any(|n| n == risk_profile) {
        return Vec::new();
    }
    vec![SwarmValidationError::new(
        step,
        "risk_profile",
        format!(
            "unknown risk profile '{risk_profile}'; pick one of: {}",
            known.join(", ")
        ),
    )]
}

fn validate_budget(budget: SwarmBudget) -> Vec<SwarmValidationError> {
    let SwarmBudget::Custom(limits) = budget else {
        return Vec::new();
    };
    let step = Some(SwarmWizardStep::Boundedness);
    let mut errors = Vec::new();
    if limits.max_turns == 0 {
        errors.push(SwarmValidationError::new(
            step,
            "max_turns",
            "a custom budget needs at least one turn",
        ));
    }
    if limits.max_tokens == 0 {
        errors.push(SwarmValidationError::new(
            step,
            "max_tokens",
            "a custom budget needs a non-zero token ceiling",
        ));
    }
    if !limits.max_cost_usd.is_finite() || limits.max_cost_usd <= 0.0 {
        errors.push(SwarmValidationError::new(
            step,
            "max_cost_usd",
            "a custom budget needs a positive cost ceiling in USD",
        ));
    }
    errors
}

fn validate_channels(channels: &[String], config: &Config) -> Vec<SwarmValidationError> {
    let step = Some(SwarmWizardStep::Channels);
    if channels.is_empty() {
        return Vec::new();
    }
    let configured = config.resolve_alias_source(AliasSource::Channels);
    let mut errors = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for channel in channels {
        if channel.is_empty() {
            errors.push(SwarmValidationError::new(
                step,
                "channels",
                "a channel entry is blank; remove it or name a configured channel",
            ));
            continue;
        }
        if !seen.insert(channel.as_str()) {
            errors.push(SwarmValidationError::new(
                step,
                "channels",
                format!("channel '{channel}' is listed more than once"),
            ));
            continue;
        }
        if !configured.iter().any(|c| c == channel) {
            let known = if configured.is_empty() {
                "no channel is configured yet".to_string()
            } else {
                format!("configured channels: {}", configured.join(", "))
            };
            errors.push(SwarmValidationError::new(
                step,
                "channels",
                format!("unknown channel '{channel}' ({known})"),
            ));
        }
    }
    errors
}

/// `anthropic.main` -> `anthropic`. A bare family name passes through.
fn provider_family(provider: &str) -> &str {
    provider
        .split_once('.')
        .map_or(provider, |(family, _)| family)
}

/// Configured risk profiles first, then the canonical presets a client can
/// install. One list, so the picker and the validator cannot disagree.
fn risk_profile_names(config: &Config) -> Vec<String> {
    let mut names = config.resolve_alias_source(AliasSource::RiskProfiles);
    for preset in RISK_PRESETS {
        if !names.iter().any(|n| n == preset.preset_name) {
            names.push(preset.preset_name.to_string());
        }
    }
    names
}

fn new_swarm_id() -> String {
    format!("sw-{}", uuid::Uuid::new_v4().simple())
}

// ── Wizard field shapes ────────────────────────────────────────────

/// Describe every wizard step for a client to render.
///
/// `provider` is the provider chosen so far (`None` before the first step is
/// answered); `models` is the catalog the caller already resolved for it — it
/// is passed in rather than fetched here so this stays synchronous and the
/// network call has exactly one call site.
pub fn wizard_steps(
    config: &Config,
    provider: Option<&str>,
    models: &[String],
) -> Vec<SwarmStepShape> {
    SwarmWizardStep::ALL
        .into_iter()
        .map(|step| SwarmStepShape {
            step,
            title: crate::i18n::get_required_cli_string(step.title_key()),
            help: crate::i18n::get_required_cli_string(step.help_key()),
            fields: step_fields(step, config, provider, models),
        })
        .collect()
}

fn step_fields(
    step: SwarmWizardStep,
    config: &Config,
    provider: Option<&str>,
    models: &[String],
) -> Vec<FieldDescriptor> {
    match step {
        SwarmWizardStep::Provider => provider_fields(config, provider),
        SwarmWizardStep::Model => vec![model_field(models)],
        SwarmWizardStep::RiskProfile => vec![risk_profile_field(config)],
        SwarmWizardStep::Boundedness => boundedness_fields(),
        SwarmWizardStep::Channels => vec![channels_field(config)],
        SwarmWizardStep::Role => vec![descriptor("role", PropKind::String, true)],
        SwarmWizardStep::Goal => vec![descriptor("goal", PropKind::String, true)],
    }
}

/// A required-by-default descriptor with no choices and no prefill. Help is
/// left empty: a step whose only input is the step itself says everything in
/// [`SwarmStepShape::help`], the same way Quickstart leaves help empty for a
/// schema field with no doc.
fn descriptor(key: &str, kind: PropKind, required: bool) -> FieldDescriptor {
    FieldDescriptor {
        key: key.to_string(),
        label: key.to_string(),
        help: String::new(),
        kind,
        is_secret: false,
        enum_variants: None,
        required,
        default: None,
    }
}

fn provider_fields(config: &Config, chosen: Option<&str>) -> Vec<FieldDescriptor> {
    let configured = config.resolve_alias_source(AliasSource::ModelProviders);
    let mut variants = configured.clone();
    for family in zeroclaw_providers::list_model_providers() {
        if !variants.iter().any(|v| v == family.name) {
            variants.push(family.name.to_string());
        }
    }

    let mut out = vec![FieldDescriptor {
        enum_variants: Some(variants),
        default: configured.first().cloned(),
        ..descriptor("provider", PropKind::AliasRef, true)
    }];

    // A provider with no configured alias still needs its credentials before
    // a box can issue a request. Rather than restate them, borrow Quickstart's
    // own essentials shape for the chosen family. These rows are advisory:
    // they are config fields, applied through the config surface, not part of
    // the swarm submission. `model` is dropped because the model step owns
    // that choice for the whole swarm.
    if let Some(chosen) = chosen.map(str::trim).filter(|p| !p.is_empty())
        && !configured.iter().any(|c| c == chosen)
    {
        out.extend(
            field_shape(FieldSection::ModelProvider, provider_family(chosen))
                .into_iter()
                .filter(|d| d.key != "model"),
        );
    }
    out
}

fn model_field(models: &[String]) -> FieldDescriptor {
    // No provider chosen yet, or the catalog was unreachable: fall back to a
    // free-text field rather than an empty picker the client cannot answer.
    if models.is_empty() {
        return descriptor("model", PropKind::String, true);
    }
    FieldDescriptor {
        enum_variants: Some(models.to_vec()),
        default: models.first().cloned(),
        ..descriptor("model", PropKind::Enum, true)
    }
}

fn risk_profile_field(config: &Config) -> FieldDescriptor {
    let names = risk_profile_names(config);
    // Prefer the recommended preset when it is on offer; otherwise the first
    // name, so the picker always lands on something valid.
    let default = names
        .iter()
        .find(|n| n.as_str() == RECOMMENDED_RISK_PROFILE)
        .or_else(|| names.first())
        .cloned();
    FieldDescriptor {
        enum_variants: Some(names),
        default,
        ..descriptor("risk_profile", PropKind::Enum, true)
    }
}

/// The `RISK_PRESETS` row the wizard lands on when nothing else is chosen.
const RECOMMENDED_RISK_PROFILE: &str = "balanced";

fn boundedness_fields() -> Vec<FieldDescriptor> {
    let mut variants: Vec<String> = SwarmBudgetPreset::ALL
        .iter()
        .map(|p| p.wire_name().to_string())
        .collect();
    variants.push(BUDGET_CUSTOM.to_string());
    let standard = SwarmBudgetPreset::Standard.limits();

    vec![
        FieldDescriptor {
            help: crate::i18n::get_required_cli_string("swarm-wizard-budget-preset-help"),
            enum_variants: Some(variants),
            default: Some(SwarmBudgetPreset::Standard.wire_name().to_string()),
            ..descriptor("budget_preset", PropKind::Enum, true)
        },
        FieldDescriptor {
            help: crate::i18n::get_required_cli_string("swarm-wizard-max-turns-help"),
            default: Some(standard.max_turns.to_string()),
            ..descriptor("max_turns", PropKind::Integer, false)
        },
        FieldDescriptor {
            help: crate::i18n::get_required_cli_string("swarm-wizard-max-tokens-help"),
            default: Some(standard.max_tokens.to_string()),
            ..descriptor("max_tokens", PropKind::Integer, false)
        },
        FieldDescriptor {
            help: crate::i18n::get_required_cli_string("swarm-wizard-max-cost-help"),
            default: Some(standard.max_cost_usd.to_string()),
            ..descriptor("max_cost_usd", PropKind::Float, false)
        },
    ]
}

fn channels_field(config: &Config) -> FieldDescriptor {
    FieldDescriptor {
        enum_variants: Some(config.resolve_alias_source(AliasSource::Channels)),
        ..descriptor("channels", PropKind::StringArray, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swarm::store::SwarmBudgetLimits;

    fn submission() -> SwarmSubmission {
        SwarmSubmission {
            swarm_id: None,
            name: "  Research squad  ".to_string(),
            provider: " anthropic ".to_string(),
            model: " claude-sonnet-4 ".to_string(),
            risk_profile: "balanced".to_string(),
            budget: SwarmBudget::default(),
            channels: Vec::new(),
            role: "supervisor".to_string(),
            goal: "survey the field".to_string(),
            boxes: None,
        }
    }

    #[test]
    fn a_clean_submission_normalizes_into_a_spec() {
        let spec = prepare_spec(&submission(), &Config::default()).expect("accepted");
        assert_eq!(spec.name, "Research squad", "authored strings are trimmed");
        assert_eq!(spec.provider, "anthropic");
        assert_eq!(spec.model, "claude-sonnet-4");
        assert!(
            spec.id.starts_with("sw-"),
            "an id is generated when none is supplied, got {}",
            spec.id
        );
        assert_eq!(
            spec.boxes.len(),
            DEFAULT_ROSTER_SIZE,
            "an absent roster becomes the default unassigned roster"
        );
        assert_eq!(spec.budget, SwarmBudget::default());
    }

    #[test]
    fn garbage_submissions_are_rejected_field_by_field() {
        let mut sub = submission();
        sub.name = "   ".to_string();
        sub.provider = "definitely-not-a-provider".to_string();
        sub.model = String::new();
        sub.risk_profile = "reckless".to_string();
        sub.role = String::new();
        sub.goal = "  ".to_string();
        sub.budget = SwarmBudget::Custom(SwarmBudgetLimits {
            max_turns: 0,
            max_tokens: 0,
            max_cost_usd: 0.0,
        });
        sub.channels = vec!["telegram.ghost".to_string()];

        let errors = prepare_spec(&sub, &Config::default()).expect_err("rejected");
        let fields: Vec<&str> = errors.iter().map(|e| e.field.as_str()).collect();
        for expected in [
            "name",
            "provider",
            "model",
            "risk_profile",
            "max_turns",
            "max_tokens",
            "max_cost_usd",
            "channels",
            "role",
            "goal",
        ] {
            assert!(
                fields.contains(&expected),
                "expected a rejection for {expected}, got {fields:?}"
            );
        }

        // The messages have to be actionable, not just "invalid".
        let provider_error = errors
            .iter()
            .find(|e| e.field == "provider")
            .expect("provider error");
        assert!(
            provider_error.message.contains("definitely-not-a-provider"),
            "the provider rejection names the offending value, got {:?}",
            provider_error.message
        );
        assert_eq!(provider_error.step, Some(SwarmWizardStep::Provider));
        let risk_error = errors
            .iter()
            .find(|e| e.field == "risk_profile")
            .expect("risk error");
        assert!(
            risk_error.message.contains("balanced"),
            "the risk rejection lists what is valid, got {:?}",
            risk_error.message
        );
    }

    #[test]
    fn a_bare_provider_family_is_accepted_without_a_configured_alias() {
        // The request path resolves a bare family, so the wizard must not be
        // stricter than what will actually run.
        let mut sub = submission();
        sub.provider = "ollama".to_string();
        prepare_spec(&sub, &Config::default()).expect("a known family is enough");
    }

    #[test]
    fn a_supplied_id_is_kept_but_must_be_slug_safe() {
        let mut sub = submission();
        sub.swarm_id = Some("sw-keeper".to_string());
        let spec = prepare_spec(&sub, &Config::default()).expect("accepted");
        assert_eq!(spec.id, "sw-keeper");

        sub.swarm_id = Some("sw keeper/../etc".to_string());
        let errors = prepare_spec(&sub, &Config::default()).expect_err("rejected");
        assert!(errors.iter().any(|e| e.field == "swarm_id"));
    }

    #[test]
    fn roster_overrides_are_checked_for_collisions_and_size() {
        let dup = vec![
            BoxSpec {
                box_id: "box-1".to_string(),
                role: "researcher".to_string(),
                job: "read".to_string(),
                slot: 0,
            },
            BoxSpec {
                box_id: "box-1".to_string(),
                role: "critic".to_string(),
                job: "poke".to_string(),
                slot: 0,
            },
        ];
        let errors = validate_roster(&dup);
        assert_eq!(
            errors.len(),
            2,
            "a duplicate id and a duplicate slot are two separate problems: {errors:?}"
        );

        assert!(
            !validate_roster(&[]).is_empty(),
            "an empty roster is not a swarm"
        );

        let oversized = default_roster(MAX_ROSTER_SIZE + 1);
        let errors = validate_roster(&oversized);
        assert!(
            errors.iter().any(|e| e.message.contains("at most")),
            "an oversized roster is refused: {errors:?}"
        );
    }

    #[test]
    fn a_slot_past_the_pane_grid_is_refused() {
        let out_of_grid = vec![BoxSpec {
            box_id: "box-1".to_string(),
            role: String::new(),
            job: String::new(),
            slot: MAX_ROSTER_SIZE as u8,
        }];
        let errors = validate_roster(&out_of_grid);
        assert!(
            errors.iter().any(|e| e.message.contains("slot")),
            "expected a slot rejection, got {errors:?}"
        );
    }

    #[test]
    fn a_patch_only_replaces_the_fields_it_sets() {
        let base = prepare_spec(&submission(), &Config::default()).expect("accepted");
        let mut sub = SwarmSubmission::from_spec(&base);
        SwarmSpecPatch {
            goal: Some("ship it".to_string()),
            ..Default::default()
        }
        .apply(&mut sub);
        let updated = prepare_spec(&sub, &Config::default()).expect("accepted");

        assert_eq!(updated.goal, "ship it");
        assert_eq!(updated.id, base.id, "the round trip keeps the same swarm");
        assert_eq!(updated.name, base.name);
        assert_eq!(updated.provider, base.provider);
        assert_eq!(updated.boxes, base.boxes, "a patch never edits the roster");
    }

    #[test]
    fn every_wizard_step_renders_with_a_non_empty_shape() {
        let steps = wizard_steps(&Config::default(), None, &[]);
        assert_eq!(
            steps.len(),
            SwarmWizardStep::ALL.len(),
            "swarm/fields returns one shape per step"
        );
        assert_eq!(
            steps.iter().map(|s| s.step).collect::<Vec<_>>(),
            SwarmWizardStep::ALL.to_vec(),
            "the steps come back in wizard order"
        );
        for shape in &steps {
            assert!(
                !shape.title.is_empty(),
                "{:?} has no title — the Fluent key is missing",
                shape.step
            );
            assert!(
                !shape.help.is_empty(),
                "{:?} has no help — the Fluent key is missing",
                shape.step
            );
            assert!(!shape.fields.is_empty(), "{:?} renders nothing", shape.step);
        }
    }

    #[test]
    fn the_boundedness_step_offers_every_preset_plus_custom() {
        let steps = wizard_steps(&Config::default(), None, &[]);
        let boundedness = steps
            .iter()
            .find(|s| s.step == SwarmWizardStep::Boundedness)
            .expect("boundedness step");
        let keys: Vec<&str> = boundedness.fields.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(
            keys,
            vec!["budget_preset", "max_turns", "max_tokens", "max_cost_usd"]
        );

        let preset = &boundedness.fields[0];
        assert_eq!(
            preset.enum_variants.as_deref(),
            Some(
                ["quick", "standard", "marathon", "custom"]
                    .map(String::from)
                    .as_slice()
            )
        );
        assert_eq!(preset.default.as_deref(), Some("standard"));
        let standard = SwarmBudgetPreset::Standard.limits();
        assert_eq!(
            boundedness.fields[1].default.as_deref(),
            Some(standard.max_turns.to_string().as_str()),
            "the custom triple is prefilled from the default preset"
        );
    }

    #[test]
    fn the_risk_step_offers_the_canonical_presets() {
        let steps = wizard_steps(&Config::default(), None, &[]);
        let risk = steps
            .iter()
            .find(|s| s.step == SwarmWizardStep::RiskProfile)
            .expect("risk step");
        let variants = risk.fields[0]
            .enum_variants
            .clone()
            .expect("the risk step is a picker");
        for preset in RISK_PRESETS {
            assert!(
                variants.iter().any(|v| v == preset.preset_name),
                "{} is missing from the risk picker: {variants:?}",
                preset.preset_name
            );
        }
        assert_eq!(risk.fields[0].default.as_deref(), Some("balanced"));
    }

    #[test]
    fn the_model_step_becomes_a_picker_once_a_catalog_is_known() {
        let free_text = wizard_steps(&Config::default(), None, &[]);
        let model = &free_text
            .iter()
            .find(|s| s.step == SwarmWizardStep::Model)
            .expect("model step")
            .fields[0];
        assert_eq!(model.kind, PropKind::String);
        assert!(model.enum_variants.is_none());

        let catalog = vec!["claude-sonnet-4".to_string(), "claude-opus-4".to_string()];
        let picker = wizard_steps(&Config::default(), Some("anthropic"), &catalog);
        let model = &picker
            .iter()
            .find(|s| s.step == SwarmWizardStep::Model)
            .expect("model step")
            .fields[0];
        assert_eq!(model.kind, PropKind::Enum);
        assert_eq!(model.enum_variants.as_deref(), Some(catalog.as_slice()));
        assert_eq!(model.default.as_deref(), Some("claude-sonnet-4"));
    }

    #[test]
    fn an_unconfigured_provider_carries_its_quickstart_credential_shape() {
        let steps = wizard_steps(&Config::default(), Some("anthropic"), &[]);
        let provider = steps
            .iter()
            .find(|s| s.step == SwarmWizardStep::Provider)
            .expect("provider step");
        let keys: Vec<&str> = provider.fields.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys.first(), Some(&"provider"));
        assert!(
            keys.contains(&"api_key"),
            "an unconfigured provider shows what it still needs: {keys:?}"
        );
        assert!(
            !keys.contains(&"model"),
            "the model step owns the model choice, not the provider step: {keys:?}"
        );
        assert!(
            provider.fields.iter().any(|f| f.is_secret),
            "the borrowed credential rows keep their secret flag"
        );
    }

    #[test]
    fn the_provider_picker_lists_known_families_when_nothing_is_configured() {
        let steps = wizard_steps(&Config::default(), None, &[]);
        let provider = steps
            .iter()
            .find(|s| s.step == SwarmWizardStep::Provider)
            .expect("provider step");
        assert_eq!(provider.fields.len(), 1, "no provider chosen, no extras");
        let variants = provider.fields[0]
            .enum_variants
            .clone()
            .expect("the provider step is a picker");
        assert!(variants.iter().any(|v| v == "anthropic"));
    }
}
