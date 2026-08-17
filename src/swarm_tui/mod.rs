//! `zeroclaw swarm` — the dashboard TUI and its one-shot siblings.
//!
//! Everything here talks to the daemon over the local JSON-RPC socket; nothing
//! reaches into the swarm store directly, so the CLI sees exactly what any
//! other client sees. The terminal half is this module: [`state`] holds the
//! reducer, [`render`] draws it, [`wizard`] owns the draft, and [`client`]
//! carries the wire.
//!
//! The driver is a two-beat loop: an input produces an
//! [`Effect`](state::Effect), the effect is performed against the daemon, and
//! its answer folds back in as an [`Update`](state::Update) — which may itself
//! ask for one more call. That is the whole of the I/O; the screens never make
//! a request of their own.

mod client;
mod render;
mod state;
mod wizard;

use std::io::{IsTerminal, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{event::Event, execute};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::SwarmCommands;
use crate::config::Config;

use client::{CallError, CallResult, RpcFailure, SwarmClient};
use render::Palette;
use state::{App, Effect, Input, Screen, Update};

type Term = Terminal<CrosstermBackend<Stdout>>;

/// Set while the alternate screen is up, so the panic hook knows whether it
/// has a terminal to put back.
static TERMINAL_ACTIVE: AtomicBool = AtomicBool::new(false);
static PANIC_HOOK: std::sync::Once = std::sync::Once::new();

/// `zeroclaw swarm [subcommand]`.
pub async fn handle_command(command: Option<SwarmCommands>, config: &Config) -> Result<()> {
    match command {
        None => run_dashboard(config).await,
        Some(SwarmCommands::List) => list_swarms(config).await,
        Some(SwarmCommands::Delete { swarm_id, force }) => {
            delete_swarm(config, &swarm_id, force).await
        }
    }
}

// ── One-shot verbs ───────────────────────────────────────────────

/// `zeroclaw swarm list` — one line per swarm, no terminal required.
async fn list_swarms(config: &Config) -> Result<()> {
    let mut client = connect(config).await?;
    let swarms = client.list_swarms().await.map_err(call_failed)?;
    if swarms.is_empty() {
        println!("{}", crate::t("cli-swarm-list-empty", "No swarms stored."));
        return Ok(());
    }
    for swarm in swarms {
        let boxes = swarm.spec.boxes.len().to_string();
        let status = render::status_label(swarm.run.status);
        let budget = render::budget_label(swarm.spec.budget);
        println!(
            "{}",
            crate::ta(
                "cli-swarm-list-row",
                &[
                    ("swarm_id", swarm.spec.id.as_str()),
                    ("name", swarm.spec.name.as_str()),
                    ("status", status.as_str()),
                    ("boxes", boxes.as_str()),
                    ("budget", budget.as_str()),
                    ("updated", swarm.updated_at.as_str()),
                ],
                "swarm",
            )
        );
    }
    Ok(())
}

/// `zeroclaw swarm delete <id> [--force]`.
async fn delete_swarm(config: &Config, swarm_id: &str, force: bool) -> Result<()> {
    let mut client = connect(config).await?;
    let result = client.delete(swarm_id, force).await.map_err(call_failed)?;
    let key = if result.deleted {
        "cli-swarm-deleted"
    } else {
        "cli-swarm-delete-absent"
    };
    println!(
        "{}",
        crate::ta(key, &[("swarm_id", result.swarm_id.as_str())], "Deleted.")
    );
    Ok(())
}

// ── Dashboard ────────────────────────────────────────────────────

async fn run_dashboard(config: &Config) -> Result<()> {
    if !std::io::stdout().is_terminal() {
        bail!(crate::t(
            "cli-swarm-needs-tty",
            "`zeroclaw swarm` needs a terminal. Use `zeroclaw swarm list` when output is piped.",
        ));
    }

    let mut client = SwarmClient::connect(config).await?;
    let mut app = if client.supports_swarms() {
        App::new()
    } else {
        App::unsupported()
    };
    let palette = Palette::detect();

    install_panic_hook();
    let mut terminal = init_terminal()?;
    let outcome = event_loop(&mut terminal, &mut app, &mut client, palette).await;
    let restored = restore_terminal(&mut terminal);
    outcome.and(restored)
}

async fn event_loop(
    terminal: &mut Term,
    app: &mut App,
    client: &mut SwarmClient,
    palette: Palette,
) -> Result<()> {
    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut events = crossterm::event::EventStream::new();

    // A supported daemon opens on its swarm list; a degraded one opens on the
    // explanation and asks the daemon nothing.
    let mut effect = if matches!(app.screen(), Screen::Unsupported) {
        Effect::None
    } else {
        Effect::List
    };

    loop {
        pump(terminal, app, client, palette, effect).await?;
        effect = Effect::None;
        if app.quit() {
            return Ok(());
        }
        terminal.draw(|frame| render::draw(frame, app, palette))?;

        #[cfg(unix)]
        let next = tokio::select! {
            event = events.next() => event,
            _ = sigterm.recv() => None,
        };
        #[cfg(not(unix))]
        let next = events.next().await;

        let Some(event) = next else {
            return Ok(());
        };
        let Event::Key(key) = event? else {
            continue;
        };
        if let Some(input) = Input::from_key(key) {
            effect = app.on_input(input);
        }
    }
}

/// Perform an effect, fold the answer back in, and keep going while the
/// reducer asks for one more call. Redraws before each round trip so the
/// in-flight state is visible.
async fn pump(
    terminal: &mut Term,
    app: &mut App,
    client: &mut SwarmClient,
    palette: Palette,
    effect: Effect,
) -> Result<()> {
    let mut effect = effect;
    loop {
        if matches!(effect, Effect::None | Effect::Quit) {
            return Ok(());
        }
        terminal.draw(|frame| render::draw(frame, app, palette))?;
        let Some(update) = perform(client, effect).await? else {
            return Ok(());
        };
        effect = app.on_update(update);
    }
}

/// Run one effect. A daemon error becomes an [`Update`] the reducer handles;
/// a dead socket ends the session, because nothing after it can succeed.
async fn perform(client: &mut SwarmClient, effect: Effect) -> Result<Option<Update>> {
    let update = match effect {
        Effect::None | Effect::Quit => return Ok(None),
        Effect::List => fold(client.list_swarms().await, Update::Swarms)?,
        Effect::Fields { provider } => {
            let steps = client.fields(provider.as_deref()).await;
            fold(steps, |steps| Update::Fields { provider, steps })?
        }
        Effect::Validate(submission) => {
            fold(client.validate(&submission).await, Update::Validated)?
        }
        Effect::Create(submission) => fold(client.create(&submission).await, |swarm| {
            Update::Created(Box::new(swarm))
        })?,
        Effect::Delete { swarm_id, force } => {
            fold(client.delete(&swarm_id, force).await, |result| {
                Update::Deleted {
                    swarm_id: result.swarm_id,
                }
            })?
        }
    };
    Ok(Some(update))
}

/// Turn a call result into an update: success through `ok`, a daemon error
/// into [`Update::Failed`], a transport failure out to the caller.
fn fold<T>(result: CallResult<T>, ok: impl FnOnce(T) -> Update) -> Result<Update> {
    match result {
        Ok(value) => Ok(ok(value)),
        Err(CallError::Rpc(failure)) => Ok(Update::Failed(failure)),
        Err(CallError::Transport(error)) => Err(error),
    }
}

// ── Terminal shell ───────────────────────────────────────────────

fn init_terminal() -> Result<Term> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    TERMINAL_ACTIVE.store(true, Ordering::SeqCst);
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal(terminal: &mut Term) -> Result<()> {
    if !TERMINAL_ACTIVE.swap(false, Ordering::SeqCst) {
        return Ok(());
    }
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

/// Put the terminal back before anything else runs. Used by the panic hook,
/// where failing is not an option worth reporting — the process is going down
/// and a cooked terminal is the only thing that makes the message readable.
fn force_restore_terminal() {
    if TERMINAL_ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
    }
}

fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            force_restore_terminal();
            previous(info);
        }));
    });
}

// ── Shared plumbing ──────────────────────────────────────────────

/// Connect for a one-shot verb, refusing early if the daemon has no swarms.
async fn connect(config: &Config) -> Result<SwarmClient> {
    let client = SwarmClient::connect(config).await?;
    if !client.supports_swarms() {
        bail!(unsupported_message());
    }
    Ok(client)
}

fn unsupported_message() -> String {
    crate::t(
        "cli-swarm-unsupported",
        "This daemon has no swarm methods; it was built before swarms existed.",
    )
}

fn call_failed(error: CallError) -> anyhow::Error {
    if error.failure().is_some_and(RpcFailure::is_method_not_found) {
        return anyhow::Error::msg(unsupported_message());
    }
    let detail = error.to_string();
    anyhow::Error::msg(crate::ta(
        "cli-swarm-call-failed",
        &[("message", detail.as_str())],
        "The daemon refused the call.",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_api::jsonrpc::error_codes::{INVALID_PARAMS, METHOD_NOT_FOUND};

    #[test]
    fn a_daemon_without_swarms_is_named_as_such() {
        let error = call_failed(CallError::Rpc(RpcFailure {
            code: METHOD_NOT_FOUND,
            message: "Method not found".to_string(),
        }));
        assert_eq!(error.to_string(), unsupported_message());
    }

    #[test]
    fn any_other_refusal_carries_the_daemons_own_message() {
        let error = call_failed(CallError::Rpc(RpcFailure {
            code: INVALID_PARAMS,
            message: "swarm 'sw-9' not found".to_string(),
        }));
        assert!(
            error.to_string().contains("swarm 'sw-9' not found"),
            "got {error}"
        );
    }

    #[test]
    fn a_daemon_error_folds_into_an_update_while_a_dead_socket_does_not() {
        let folded = fold(
            CallResult::<Vec<zeroclaw_runtime::swarm::store::PersistedSwarm>>::Err(CallError::Rpc(
                RpcFailure {
                    code: INVALID_PARAMS,
                    message: "nope".to_string(),
                },
            )),
            Update::Swarms,
        );
        assert!(matches!(folded, Ok(Update::Failed(_))));

        let dead = fold(
            CallResult::<Vec<zeroclaw_runtime::swarm::store::PersistedSwarm>>::Err(
                CallError::Transport(anyhow::Error::msg("socket gone")),
            ),
            Update::Swarms,
        );
        assert!(dead.is_err());
    }

    #[test]
    fn restoring_a_terminal_that_was_never_raised_is_a_no_op() {
        assert!(!TERMINAL_ACTIVE.load(Ordering::SeqCst));
        force_restore_terminal();
        assert!(!TERMINAL_ACTIVE.load(Ordering::SeqCst));
    }
}
