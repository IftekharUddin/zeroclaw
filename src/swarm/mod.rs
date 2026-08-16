//! `zeroclaw swarm` — experimental goal-driven agent swarm.
//!
//! # Status: MVP-0
//!
//! This is the first, deliberately minimal slice of the Swarm feature. It
//! provisions or reuses a single **queen** orchestrator agent and drops the
//! user into a live chat with it. There are no worker agents, no
//! orchestration loop, no shared state board, and no persistence beyond what
//! the underlying agent already does. Those land in later iterations.
//!
//! The queen is, for now, just an ordinary configured agent. Swarm is a thin
//! launcher over the existing interactive agent loop so we have a runnable,
//! demoable surface to grow the real orchestration layer onto.
//!
//! ## Flow
//!
//! 1. Resolve the queen alias:
//!    - `--queen <alias>` reuses an existing configured agent.
//!    - Otherwise, if exactly one agent is configured, offer it as the queen.
//!    - Otherwise, list configured agents and ask the user to pick one, or
//!      point them at `zeroclaw quickstart` when none exist.
//! 2. Print a short Swarm banner describing the queen.
//! 3. Launch the interactive queen chat via the shared agent loop.

use crate::config::Config;

/// Entry point for `zeroclaw swarm`.
///
/// `queen` optionally names an existing agent alias to run as the queen.
/// `provider_override` / `model_override` are pre-seed hints reserved for the
/// creation wizard (not yet built in MVP-0); they are threaded through to the
/// chat launch so `--model`/`--model-provider` behave like `zeroclaw agent`.
pub(crate) async fn run(
    config: Config,
    queen: Option<String>,
    provider_override: Option<String>,
    model_override: Option<String>,
) -> anyhow::Result<()> {
    let queen_alias = resolve_queen(&config, queen)?;

    // Resolve the effective temperature the same way `zeroclaw agent` does:
    // fall back to the agent's configured provider entry.
    let temperature = config
        .model_provider_for_agent(&queen_alias)
        .and_then(|e| e.temperature);

    print_banner(&config, &queen_alias);

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

/// Decide which configured agent acts as the queen.
///
/// Never creates an agent (MVP-0 has no wizard yet); it only selects among
/// what is already configured, or bails with an actionable message.
fn resolve_queen(config: &Config, queen: Option<String>) -> anyhow::Result<String> {
    // Explicit `--queen <alias>`: must exist and be usable.
    if let Some(alias) = queen {
        if config.agent(&alias).is_none() {
            anyhow::bail!(
                "queen agent `{alias}` is not configured (no [agents.{alias}] entry). \
                 Run `zeroclaw quickstart` to create it, then `zeroclaw swarm --queen {alias}`."
            );
        }
        return Ok(alias);
    }

    // No explicit queen: choose among enabled agents.
    let mut aliases: Vec<&String> = config
        .agents
        .iter()
        .filter(|(_, a)| a.enabled)
        .map(|(alias, _)| alias)
        .collect();
    aliases.sort();

    match aliases.as_slice() {
        [] => anyhow::bail!(
            "no agents configured to act as the queen. \
             Run `zeroclaw quickstart` to create one, then re-run `zeroclaw swarm`."
        ),
        [only] => Ok((*only).clone()),
        many => prompt_pick_queen(many),
    }
}

/// Ask the user which configured agent should be the queen.
fn prompt_pick_queen(aliases: &[&String]) -> anyhow::Result<String> {
    use dialoguer::{FuzzySelect, theme::ColorfulTheme};

    let labels: Vec<&str> = aliases.iter().map(|a| a.as_str()).collect();
    let idx = FuzzySelect::with_theme(&ColorfulTheme::default())
        .with_prompt("Select the queen agent for this swarm")
        .items(&labels)
        .default(0)
        .interact()?;
    Ok(aliases[idx].clone())
}

/// Print a short banner so the user knows a swarm queen was launched and which
/// agent is driving it.
fn print_banner(config: &Config, queen_alias: &str) {
    let model = config
        .model_provider_for_agent(queen_alias)
        .and_then(|e| e.model.clone())
        .unwrap_or_else(|| "<default model>".to_string());
    let risk = config
        .risk_profile_for_agent(queen_alias)
        .map(|p| format!("{:?}", p.level))
        .unwrap_or_else(|| "<no risk_profile>".to_string());

    eprintln!("🐝 zeroclaw swarm (experimental — MVP-0)");
    eprintln!("   queen : {queen_alias}");
    eprintln!("   model : {model}");
    eprintln!("   risk  : {risk}");
    eprintln!("   workers: none yet — this is a queen-only chat.");
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

    #[test]
    fn explicit_queen_that_exists_is_selected() {
        let config = config_with_agents(&["alpha", "beta"]);
        let queen = resolve_queen(&config, Some("beta".to_string())).unwrap();
        assert_eq!(queen, "beta");
    }

    #[test]
    fn explicit_queen_that_is_missing_errors() {
        let config = config_with_agents(&["alpha"]);
        let err = resolve_queen(&config, Some("ghost".to_string())).unwrap_err();
        assert!(
            err.to_string().contains("ghost"),
            "error should name the missing alias; got: {err}"
        );
    }

    #[test]
    fn single_agent_is_auto_selected_as_queen() {
        let config = config_with_agents(&["solo"]);
        let queen = resolve_queen(&config, None).unwrap();
        assert_eq!(queen, "solo");
    }

    #[test]
    fn no_agents_errors_with_quickstart_hint() {
        let config = config_with_agents(&[]);
        let err = resolve_queen(&config, None).unwrap_err();
        assert!(
            err.to_string().contains("quickstart"),
            "error should point at quickstart; got: {err}"
        );
    }
}
