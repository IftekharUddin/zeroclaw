//! `zeroclaw swarm` — experimental goal-driven agent swarm.
//!
//! # Status: MVP-1
//!
//! Provisions or reuses a single **queen** orchestrator agent, attaches one or
//! more **ephemeral worker** agents she can delegate to, and drops the user
//! into a live chat with the queen. There is no shared state board or
//! multi-round orchestration loop yet; those land in later iterations.
//!
//! ## Ephemeral workers
//!
//! Workers are **never persisted to disk.** They are synthesized into the
//! in-memory [`Config`] at launch — cloned from the queen so they inherit her
//! provider, risk profile, and runtime profile — and the queen's `delegates`
//! roster is wired to them in-memory. The existing
//! [`DelegateTool`](zeroclaw_runtime::tools::DelegateTool) resolves delegation
//! targets from that in-memory map (never a disk read), so no persistence and
//! no schema change are required. When the swarm process exits, the workers
//! evaporate; the user's `config.toml` only ever gains the one queen. This
//! keeps the agent list uncluttered — workers are runtime constructs, not
//! installed agents.
//!
//! ## Flow
//!
//! 1. Resolve the queen alias:
//!    - `--queen <alias>` reuses an existing configured agent.
//!    - Otherwise, if exactly one agent is configured, offer it as the queen.
//!    - Otherwise, if several exist, ask the user to pick one.
//!    - Otherwise (none configured), run the **queen wizard** to provision one
//!      from scratch via the shared quickstart apply path.
//! 2. Assemble ephemeral workers in-memory and wire the queen's delegates.
//! 3. Print a short Swarm banner describing the queen and worker roster.
//! 4. Launch the interactive queen chat via the shared agent loop.
//!
//! The wizard never hand-writes config: it builds a
//! [`BuilderSubmission`](zeroclaw_config::presets::BuilderSubmission) and calls
//! [`apply_with_surface`](zeroclaw_runtime::quickstart::apply_with_surface) —
//! the same validated, atomic path `zeroclaw quickstart` uses.

use crate::config::Config;

/// Default alias assigned to a freshly-provisioned queen.
const DEFAULT_QUEEN_ALIAS: &str = "queen";

/// Number of ephemeral workers attached to the queen in this MVP. Kept at one
/// until a `--workers N` flag / `[swarm]` config block lands.
const DEFAULT_WORKER_COUNT: usize = 1;

/// System prompt seeded into a wizard-provisioned queen. Describes the
/// orchestrator role so the agent behaves like a swarm lead.
const QUEEN_SYSTEM_PROMPT: &str = "\
You are the queen of a zeroclaw agent swarm. You coordinate work toward a \
single goal the user sets. Break the goal into concrete subtasks and delegate \
each to a worker agent using the delegate tool, then synthesize their results. \
Track progress and keep the user informed with concise status updates. Prefer \
delegating focused, self-contained subtasks over doing everything yourself.";

/// Entry point for `zeroclaw swarm`.
///
/// `queen` optionally names an existing agent alias to run as the queen.
/// `provider_override` / `model_override` pre-seed the creation wizard and,
/// when an existing queen is reused, behave like `zeroclaw agent`'s
/// `--model-provider` / `--model` overrides.
pub(crate) async fn run(
    mut config: Config,
    queen: Option<String>,
    provider_override: Option<String>,
    model_override: Option<String>,
) -> anyhow::Result<()> {
    // Resolving may run the wizard, which mutates and persists `config`.
    // `provisioned` is true when the queen was just created (its provider and
    // model are already baked into config, so we must not re-apply overrides).
    let ResolvedQueen {
        alias: queen_alias,
        provisioned,
    } = resolve_queen(
        &mut config,
        queen,
        provider_override.clone(),
        model_override.clone(),
    )
    .await?;

    // A freshly-provisioned queen already has its provider/model persisted;
    // passing the seed hints again would double-apply. Reuse paths keep them.
    let (provider_override, model_override) = if provisioned {
        (None, None)
    } else {
        (provider_override, model_override)
    };

    // Attach ephemeral workers in-memory and wire the queen's delegate roster.
    // Nothing here is persisted: workers live only for this process. Failure to
    // assemble workers is non-fatal — the queen can still run solo.
    let workers = match assemble_workers(&mut config, &queen_alias, DEFAULT_WORKER_COUNT) {
        Ok(workers) => workers,
        Err(err) => {
            eprintln!("⚠️  could not attach workers ({err}); running queen solo.");
            Vec::new()
        }
    };

    // Resolve the effective temperature the same way `zeroclaw agent` does:
    // fall back to the agent's configured provider entry.
    let temperature = config
        .model_provider_for_agent(&queen_alias)
        .and_then(|e| e.temperature);

    print_banner(&config, &queen_alias, &workers);

    // Wire the CLI channel + channel map exactly as `zeroclaw agent` does, so
    // the interactive queen chat has a terminal to talk on.
    zeroclaw_runtime::agent::loop_::register_cli_channel_fn(Box::new(|| {
        Box::new(zeroclaw_channels::cli::CliChannel::new("cli"))
    }));
    zeroclaw_runtime::agent::loop_::register_channel_map_fn(Box::new({
        let config_clone = config.clone();
        move || zeroclaw_channels::orchestrator::build_channel_map(&config_clone)
    }));

    // Box the large agent-loop future to satisfy `clippy::large_futures`,
    // mirroring every `Box::pin(...)` call site in `main.rs`.
    Box::pin(zeroclaw_runtime::agent::loop_::run(
        config,
        &queen_alias,
        None, // interactive: no single-shot message
        provider_override,
        model_override,
        temperature,
        Vec::new(), // no peripheral overrides
        true,       // interactive
        None,       // no session-state file
        None,       // no allowed-tools restriction
        zeroclaw_api::ingress::TurnOrigin::Interactive,
        zeroclaw_runtime::agent::loop_::AgentRunOverrides::default(),
    ))
    .await
    .map(|_| ())
}

/// Outcome of queen resolution.
#[derive(Debug)]
struct ResolvedQueen {
    /// The queen agent alias to run.
    alias: String,
    /// `true` when the queen was created by the wizard this run.
    provisioned: bool,
}

/// Decide which agent acts as the queen, provisioning one if none exist.
///
/// - `--queen <alias>`: must already exist.
/// - Exactly one configured agent: use it.
/// - Several configured agents: prompt the user to pick.
/// - None configured: run the wizard to create one.
async fn resolve_queen(
    config: &mut Config,
    queen: Option<String>,
    provider_seed: Option<String>,
    model_seed: Option<String>,
) -> anyhow::Result<ResolvedQueen> {
    // Explicit `--queen <alias>`: must exist and be usable.
    if let Some(alias) = queen {
        if config.agent(&alias).is_none() {
            anyhow::bail!(
                "queen agent `{alias}` is not configured (no [agents.{alias}] entry). \
                 Run `zeroclaw swarm` with no --queen to create one interactively."
            );
        }
        return Ok(ResolvedQueen {
            alias,
            provisioned: false,
        });
    }

    // No explicit queen: choose among enabled agents.
    let mut aliases: Vec<String> = config
        .agents
        .iter()
        .filter(|(_, a)| a.enabled)
        .map(|(alias, _)| alias.clone())
        .collect();
    aliases.sort();

    match aliases.len() {
        0 => {
            let alias = provision_queen(config, provider_seed, model_seed).await?;
            Ok(ResolvedQueen {
                alias,
                provisioned: true,
            })
        }
        1 => Ok(ResolvedQueen {
            alias: aliases.remove(0),
            provisioned: false,
        }),
        _ => Ok(ResolvedQueen {
            alias: prompt_pick_queen(&aliases)?,
            provisioned: false,
        }),
    }
}

/// Ask the user which configured agent should be the queen.
fn prompt_pick_queen(aliases: &[String]) -> anyhow::Result<String> {
    use dialoguer::{FuzzySelect, theme::ColorfulTheme};

    let labels: Vec<&str> = aliases.iter().map(String::as_str).collect();
    let idx = FuzzySelect::with_theme(&ColorfulTheme::default())
        .with_prompt("Select the queen agent for this swarm")
        .items(&labels)
        .default(0)
        .interact()?;
    Ok(aliases[idx].clone())
}

/// Gathered inputs for a new queen, separated from the interactive prompts so
/// the submission-building logic is unit-testable without a TTY.
struct QueenSpec {
    /// Canonical provider type (e.g. `anthropic`).
    provider_type: String,
    /// Model id for the provider's `default` alias.
    model: String,
    /// Extra provider fields (e.g. `api_key`) routed by the apply path.
    fields: std::collections::HashMap<String, String>,
    /// Risk preset key from `RISK_PRESETS`.
    risk_preset: String,
    /// Queen agent alias.
    alias: String,
}

/// Build the quickstart submission that provisions a queen agent.
///
/// Memory defaults to Sqlite; no channels or peer groups are bound. Pure and
/// side-effect-free so it can be tested independently of the prompt flow.
fn build_queen_submission(
    spec: &QueenSpec,
    runtime_profile: String,
) -> zeroclaw_config::presets::BuilderSubmission {
    use zeroclaw_config::presets::{
        AgentIdentity, BuilderSubmission, MemoryChoice, ModelProviderChoice, SelectorChoice,
    };

    BuilderSubmission {
        model_provider: SelectorChoice::Fresh(ModelProviderChoice {
            provider_type: spec.provider_type.clone(),
            alias: "default".to_string(),
            model: spec.model.clone(),
            fields: spec.fields.clone(),
        }),
        risk_profile: SelectorChoice::Fresh(spec.risk_preset.clone()),
        runtime_profile: SelectorChoice::Fresh(runtime_profile),
        memory: SelectorChoice::Fresh(MemoryChoice::default()),
        channels: Vec::new(),
        peer_groups: Vec::new(),
        agent: AgentIdentity {
            name: spec.alias.clone(),
            system_prompt: QUEEN_SYSTEM_PROMPT.to_string(),
            personality_file: None,
            personality_files: Vec::new(),
        },
    }
}

/// Interactively provision a brand-new queen agent, then persist it.
///
/// Builds a [`BuilderSubmission`] and applies it through the shared quickstart
/// path (`Surface::Cli`), so validation, secret handling, and the atomic
/// config write are identical to `zeroclaw quickstart`. Returns the new alias.
async fn provision_queen(
    config: &mut Config,
    provider_seed: Option<String>,
    model_seed: Option<String>,
) -> anyhow::Result<String> {
    use dialoguer::{Input, Password, Select, theme::ColorfulTheme};
    use zeroclaw_config::presets::RISK_PRESETS;
    use zeroclaw_runtime::quickstart::{Surface, apply_with_surface};

    eprintln!("🐝 No agents configured — let's create your swarm queen.\n");

    let theme = ColorfulTheme::default();

    // 1. Provider type (from the canonical registry).
    let providers = zeroclaw_providers::list_model_providers();
    let provider = pick_provider(&theme, &providers, provider_seed)?;
    let provider_type = provider.name.to_string();
    let is_local = provider.local;

    // 2. Model id.
    let model: String = match model_seed {
        Some(m) if !m.trim().is_empty() => m,
        _ => Input::with_theme(&theme)
            .with_prompt(format!("Model id for {provider_type}"))
            .interact_text()?,
    };

    // 3. API key — skipped for local providers (ollama, etc.).
    let mut fields = std::collections::HashMap::new();
    if !is_local {
        let key: String = Password::with_theme(&theme)
            .with_prompt(format!("API key for {provider_type} (input hidden)"))
            .allow_empty_password(true)
            .interact()?;
        if !key.trim().is_empty() {
            // Submission field keys are snake_case; the apply path routes
            // `api_key` into the provider's secret store.
            fields.insert("api_key".to_string(), key);
        }
    }

    // 4. Risk preset.
    let risk_labels: Vec<String> = RISK_PRESETS
        .iter()
        .map(|p| format!("{} — {}", p.label, p.preset_name))
        .collect();
    let default_risk_idx = RISK_PRESETS
        .iter()
        .position(|p| p.preset_name == "balanced")
        .unwrap_or(0);
    let risk_idx = Select::with_theme(&theme)
        .with_prompt("Risk profile for the queen")
        .items(&risk_labels)
        .default(default_risk_idx)
        .interact()?;
    let risk_preset = RISK_PRESETS[risk_idx].preset_name.to_string();

    // 5. Queen alias.
    let alias: String = Input::with_theme(&theme)
        .with_prompt("Queen agent name")
        .default(DEFAULT_QUEEN_ALIAS.to_string())
        .interact_text()?;
    let alias = alias.trim().to_string();
    if config.agent(&alias).is_some() {
        anyhow::bail!("an agent named `{alias}` already exists; re-run and pick another name.");
    }

    // Assemble the submission from the gathered inputs.
    let submission = build_queen_submission(
        &QueenSpec {
            provider_type,
            model,
            fields,
            risk_preset,
            alias: alias.clone(),
        },
        default_runtime_profile(config),
    );

    match Box::pin(apply_with_surface(submission, config, Surface::Cli)).await {
        Ok(applied) => {
            eprintln!("\n✅ Queen `{}` created.\n", applied.alias);
            Ok(applied.alias)
        }
        Err(errors) => {
            let joined = errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; ");
            anyhow::bail!("failed to create queen: {joined}");
        }
    }
}

/// Prompt for a provider from the registry, honoring a pre-seed hint.
///
/// When `seed` names a known provider (case-insensitive), it is selected
/// without prompting.
fn pick_provider<'a>(
    theme: &dialoguer::theme::ColorfulTheme,
    providers: &'a [zeroclaw_providers::ModelProviderInfo],
    seed: Option<String>,
) -> anyhow::Result<&'a zeroclaw_providers::ModelProviderInfo> {
    use dialoguer::Select;

    if let Some(seed) = seed.as_deref().map(str::trim).filter(|s| !s.is_empty())
        && let Some(found) = providers.iter().find(|p| p.name.eq_ignore_ascii_case(seed))
    {
        return Ok(found);
    }

    let labels: Vec<&str> = providers.iter().map(|p| p.display_name).collect();
    let idx = Select::with_theme(theme)
        .with_prompt("Model provider for the queen")
        .items(&labels)
        .default(0)
        .interact()?;
    Ok(&providers[idx])
}

/// Pick the runtime profile preset to seed a new agent with. Mirrors the
/// quickstart CLI: prefer the live config's default, else the first preset.
fn default_runtime_profile(config: &Config) -> String {
    let snapshot = zeroclaw_runtime::quickstart::snapshot_state(config);
    if snapshot.default_runtime_profile.trim().is_empty() {
        zeroclaw_config::presets::RUNTIME_PRESETS
            .first()
            .map_or_else(|| "balanced".to_string(), |p| p.preset_name.to_string())
    } else {
        snapshot.default_runtime_profile
    }
}

/// Print a short banner so the user knows a swarm queen was launched and which
/// agent is driving it.
/// Attach `count` ephemeral worker agents to the queen, entirely in-memory.
///
/// Each worker is cloned from the queen's `AliasedAgentConfig` so it inherits
/// her provider, risk profile, and runtime profile (which keeps it both
/// functional and reachable — the [`DelegateTool`] gates on matching risk
/// profiles unless an explicit `delegates` entry is present, and we add one).
/// The worker's system prompt is replaced with [`WORKER_SYSTEM_PROMPT`] and its
/// workspace path is cleared so the runtime derives a per-alias workspace dir
/// (`agents/<alias>/workspace`), avoiding collisions with the queen.
///
/// The queen's `delegates` roster is extended with an explicit, bounded target
/// per worker so delegation authority is intentional rather than ambient. None
/// of this is written to disk; it lives only in the passed-in `config`.
///
/// Returns the list of worker aliases attached. Errors only if the queen
/// herself is missing from the config (a caller bug).
fn assemble_workers(
    config: &mut Config,
    queen_alias: &str,
    count: usize,
) -> anyhow::Result<Vec<String>> {
    use zeroclaw_config::schema::DelegateTargetConfig;

    let Some(queen) = config.agents.get(queen_alias).cloned() else {
        anyhow::bail!("queen `{queen_alias}` is not in the config; cannot attach workers");
    };

    let mut worker_aliases = Vec::with_capacity(count);
    for n in 1..=count {
        // Derive a stable, collision-checked alias. The reserved `swarm-`
        // prefix signals these are runtime constructs, not user agents.
        let mut alias = format!("swarm-worker-{n}");
        let mut suffix = n;
        while config.agents.contains_key(&alias) {
            suffix += count;
            alias = format!("swarm-worker-{suffix}");
        }

        let mut worker = queen.clone();
        worker.enabled = true;
        // Clear any custom workspace path so the runtime derives a per-alias
        // workspace dir (`agents/<alias>/workspace`), avoiding collisions with
        // the queen. Workers inherit the queen's personality/system prompt
        // (which is delivered via workspace personality files, not a config
        // field) — acceptable for MVP since a worker is a focused clone.
        worker.workspace.path = None;
        // Workers do not delegate onward in this MVP.
        worker.delegates = Vec::new();

        config.agents.insert(alias.clone(), worker);
        worker_aliases.push(alias);
    }

    // Wire the queen -> worker delegation authority in-memory.
    if let Some(queen_mut) = config.agents.get_mut(queen_alias) {
        for alias in &worker_aliases {
            if !queen_mut
                .delegates
                .iter()
                .any(|t| t.agent().trim() == alias)
            {
                queen_mut
                    .delegates
                    .push(DelegateTargetConfig::bounded(alias.clone()));
            }
        }
    }

    Ok(worker_aliases)
}

/// Print a short banner so the user knows a swarm queen was launched, which
/// agent is driving it, and which workers are attached.
fn print_banner(config: &Config, queen_alias: &str, workers: &[String]) {
    let model = config
        .model_provider_for_agent(queen_alias)
        .and_then(|e| e.model.clone())
        .unwrap_or_else(|| "<default model>".to_string());
    let risk = config
        .risk_profile_for_agent(queen_alias)
        .map(|p| format!("{:?}", p.level))
        .unwrap_or_else(|| "<no risk_profile>".to_string());

    let workers_line = if workers.is_empty() {
        "none — queen runs solo".to_string()
    } else {
        format!("{} ({})", workers.len(), workers.join(", "))
    };

    eprintln!("🐝 zeroclaw swarm (experimental — MVP-1)");
    eprintln!("   queen  : {queen_alias}");
    eprintln!("   model  : {model}");
    eprintln!("   risk   : {risk}");
    eprintln!("   workers: {workers_line}  (ephemeral — not saved)");
    eprintln!("   Type your goal for the queen, or Ctrl-C to exit.");
    eprintln!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_config::schema::{AliasedAgentConfig, RiskProfileConfig};

    fn config_with_agents(aliases: &[&str]) -> Config {
        let mut config = Config::default();
        config
            .risk_profiles
            .insert("default".to_string(), RiskProfileConfig::default());
        for alias in aliases {
            config.agents.insert(
                (*alias).to_string(),
                AliasedAgentConfig {
                    risk_profile: "default".into(),
                    enabled: true,
                    ..AliasedAgentConfig::default()
                },
            );
        }
        config
    }

    #[tokio::test]
    async fn explicit_queen_that_exists_is_selected() {
        let mut config = config_with_agents(&["alpha", "beta"]);
        let resolved = resolve_queen(&mut config, Some("beta".to_string()), None, None)
            .await
            .unwrap();
        assert_eq!(resolved.alias, "beta");
        assert!(!resolved.provisioned);
    }

    #[tokio::test]
    async fn explicit_queen_that_is_missing_errors() {
        let mut config = config_with_agents(&["alpha"]);
        let err = resolve_queen(&mut config, Some("ghost".to_string()), None, None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("ghost"),
            "error should name the missing alias; got: {err}"
        );
    }

    #[tokio::test]
    async fn single_agent_is_auto_selected_as_queen() {
        let mut config = config_with_agents(&["solo"]);
        let resolved = resolve_queen(&mut config, None, None, None).await.unwrap();
        assert_eq!(resolved.alias, "solo");
        assert!(!resolved.provisioned);
    }

    #[test]
    fn default_runtime_profile_is_non_empty() {
        // The provisioning path relies on a resolvable runtime preset; a blank
        // config must still yield a usable preset name.
        let config = Config::default();
        let profile = default_runtime_profile(&config);
        assert!(!profile.trim().is_empty(), "runtime profile must resolve");
    }

    #[test]
    fn queen_system_prompt_describes_orchestrator_role() {
        assert!(QUEEN_SYSTEM_PROMPT.to_lowercase().contains("queen"));
        assert!(QUEEN_SYSTEM_PROMPT.to_lowercase().contains("swarm"));
    }

    #[tokio::test]
    async fn build_and_apply_queen_submission_creates_agent() {
        // Exercises the real quickstart apply path (the risky integration),
        // bypassing only the TTY prompts.
        let tmp = std::env::temp_dir().join(format!("zc-swarm-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);

        let mut config = Config::default();
        config.data_dir = tmp.clone();

        let spec = QueenSpec {
            provider_type: "anthropic".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            fields: std::collections::HashMap::new(),
            risk_preset: "balanced".to_string(),
            alias: "queen".to_string(),
        };
        let submission = build_queen_submission(&spec, default_runtime_profile(&config));

        let applied = Box::pin(zeroclaw_runtime::quickstart::apply_with_surface(
            submission,
            &mut config,
            zeroclaw_runtime::quickstart::Surface::Cli,
        ))
        .await
        .expect("queen submission should apply cleanly");

        assert_eq!(applied.alias, "queen");
        assert!(
            config.agent("queen").is_some(),
            "queen agent must exist in config after apply"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn assemble_workers_injects_and_wires_delegates() {
        let mut config = config_with_agents(&["queen"]);
        let workers = assemble_workers(&mut config, "queen", 2).unwrap();

        // Two workers attached with the reserved prefix.
        assert_eq!(workers.len(), 2);
        assert!(workers.iter().all(|w| w.starts_with("swarm-worker-")));

        // Each worker exists in the in-memory config and is enabled.
        for w in &workers {
            let agent = config.agent(w).expect("worker must be in config");
            assert!(agent.enabled, "worker must be enabled");
            // Worker shares the queen's risk profile so delegation is reachable.
            assert_eq!(agent.risk_profile.trim(), "default");
            // Worker doesn't delegate onward.
            assert!(agent.delegates.is_empty());
            // Worker gets its own derived workspace (no inherited custom path).
            assert!(agent.workspace.path.is_none());
        }

        // Queen's delegate roster now names every worker exactly once.
        let queen = config.agent("queen").unwrap();
        for w in &workers {
            let hits = queen
                .delegates
                .iter()
                .filter(|t| t.agent().trim() == w)
                .count();
            assert_eq!(hits, 1, "queen must delegate to {w} exactly once");
        }
    }

    #[test]
    fn assemble_workers_is_idempotent_on_delegate_roster() {
        // Re-running must not duplicate delegate entries for the same worker.
        let mut config = config_with_agents(&["queen"]);
        let first = assemble_workers(&mut config, "queen", 1).unwrap();
        // Call again: new distinct workers, but no duplicate roster entries for
        // the originals.
        let second = assemble_workers(&mut config, "queen", 1).unwrap();
        assert_ne!(first, second, "second run must mint fresh aliases");

        let queen = config.agent("queen").unwrap();
        // No delegate alias appears twice.
        let mut seen = std::collections::HashSet::new();
        for t in &queen.delegates {
            assert!(
                seen.insert(t.agent().trim().to_string()),
                "duplicate delegate entry for {}",
                t.agent()
            );
        }
    }

    #[test]
    fn assemble_workers_errors_when_queen_missing() {
        let mut config = config_with_agents(&["someone-else"]);
        let err = assemble_workers(&mut config, "ghost-queen", 1).unwrap_err();
        assert!(
            err.to_string().contains("ghost-queen"),
            "error should name the missing queen; got: {err}"
        );
    }

    #[test]
    fn assemble_zero_workers_is_a_noop() {
        let mut config = config_with_agents(&["queen"]);
        let workers = assemble_workers(&mut config, "queen", 0).unwrap();
        assert!(workers.is_empty());
        assert!(config.agent("queen").unwrap().delegates.is_empty());
        // No stray swarm-worker agents were added.
        assert!(!config.agents.keys().any(|k| k.starts_with("swarm-worker-")));
    }
}
