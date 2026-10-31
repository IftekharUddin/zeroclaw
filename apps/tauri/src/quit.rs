//! Explicit Quit owns a local preference and an in-flight UI decision only.
//! Process ownership always comes from `OwnedProcesses`; the store never holds
//! PIDs, handles, or a snapshot of whether this app owns background work.

use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, Runtime, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use tauri_plugin_store::StoreExt;

use crate::ownership::{QUIT_GRACE, SharedOwnedProcesses};

pub const QUIT_WINDOW: &str = "quit-confirmation";
const PREFERENCES: &str = "desktop-preferences.json";
const QUIT_CHOICE: &str = "quit_choice";
// Serialize creation/recovery so concurrent explicit Quit events cannot both
// build the same native confirmation label. This owns no preference or policy.
static PROMPT_BUILD: Mutex<()> = Mutex::new(());

/// The single persisted choice. There is no separate remember flag.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum QuitChoice {
    #[default]
    Ask,
    StopOwned,
    KeepRunning,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuitAnswer {
    StopOwned,
    KeepRunning,
}

impl From<QuitAnswer> for QuitChoice {
    fn from(answer: QuitAnswer) -> Self {
        match answer {
            QuitAnswer::StopOwned => Self::StopOwned,
            QuitAnswer::KeepRunning => Self::KeepRunning,
        }
    }
}

#[derive(Debug, Default)]
enum Phase {
    #[default]
    Idle,
    Resolving,
    Prompt {
        error: Option<String>,
    },
    Stopping,
    ExitAuthorized,
}

/// Creates the fact that an explicit Quit is in progress. It does not cache
/// either the stored preference or the process registry.
#[derive(Debug, Default)]
pub struct QuitController(Mutex<Phase>);

pub type SharedQuitController = Arc<QuitController>;

/// Cocoa's predefined Quit calls terminate directly, bypassing ExitRequested.
/// Keep the framework's default menu but route its Quit through our explicit
/// app-exit handler, just like tray Quit. Resolve the predefined item's current
/// title from the framework rather than depending on a translated label/index.
#[cfg(target_os = "macos")]
pub fn application_menu<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<tauri::menu::Menu<R>> {
    use tauri::menu::{Menu, MenuItemBuilder, PredefinedMenuItem};

    let menu = Menu::default(app)?;
    let quit_title = PredefinedMenuItem::quit(app, None)?.text()?;
    for item in menu.items()? {
        if let Some(submenu) = item.as_submenu() {
            for (index, item) in submenu.items()?.into_iter().enumerate() {
                if let Some(predefined) = item.as_predefined_menuitem()
                    && predefined.text()? == quit_title
                {
                    submenu.remove(&item)?;
                    submenu.insert(&crate::tray::menu::reset_quit_item(app)?, index)?;
                    let quit = MenuItemBuilder::with_id("quit", &quit_title)
                        .accelerator("CmdOrCtrl+Q")
                        .build(app)?;
                    submenu.insert(&quit, index + 1)?;
                }
            }
        }
    }
    Ok(menu)
}

/// Shared by the installed app and the isolated native UI proof harness.
pub fn handle_window_event<R: Runtime>(window: &tauri::Window<R>, event: &tauri::WindowEvent) {
    crate::window::on_window_event(window, event);
    if window.label() == QUIT_WINDOW {
        let controller = window.state::<SharedQuitController>();
        match event {
            tauri::WindowEvent::CloseRequested { api, .. } => {
                if !controller.allow_close() {
                    api.prevent_close();
                }
            }
            tauri::WindowEvent::Destroyed => {
                controller.dismiss();
            }
            _ => {}
        }
    }
}

pub fn handle_run_event<R: Runtime>(app: &AppHandle<R>, event: tauri::RunEvent) {
    if let tauri::RunEvent::ExitRequested { code, api, .. } = event {
        if crate::window::keep_running_on_exit_request(code) {
            api.prevent_exit();
        } else if !app.state::<SharedQuitController>().exit_authorized() {
            api.prevent_exit();
            request(app);
        }
    }
}

impl QuitController {
    pub fn exit_authorized(&self) -> bool {
        matches!(
            *self.0.lock().unwrap_or_else(PoisonError::into_inner),
            Phase::ExitAuthorized
        )
    }

    pub fn dismiss(&self) {
        let mut phase = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(*phase, Phase::Prompt { .. }) {
            *phase = Phase::Idle;
        }
    }

    /// Native close and Cancel share this atomic decision. Once an answer is
    /// accepted, closing the window must not interrupt its save/cleanup or
    /// remove the only surface that can report a failure and offer retry.
    fn allow_close(&self) -> bool {
        let mut phase = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        match *phase {
            Phase::Resolving | Phase::Stopping => false,
            Phase::Prompt { .. } => {
                *phase = Phase::Idle;
                true
            }
            Phase::Idle | Phase::ExitAuthorized => true,
        }
    }

    fn needs_prompt(&self) -> bool {
        matches!(
            *self.0.lock().unwrap_or_else(PoisonError::into_inner),
            Phase::Prompt { .. }
        )
    }

    fn begin(&self) -> bool {
        let mut phase = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if !matches!(*phase, Phase::Idle) {
            return false;
        }
        *phase = Phase::Resolving;
        true
    }

    fn set(&self, next: Phase) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = next;
    }
}

fn choice<R: Runtime>(app: &AppHandle<R>) -> Result<QuitChoice, String> {
    let store = app
        .store_builder(PREFERENCES)
        .disable_auto_save()
        .build()
        .map_err(|error| format!("Couldn't read the desktop Quit preference: {error}"))?;
    store.get(QUIT_CHOICE).map_or(Ok(QuitChoice::Ask), |value| {
        serde_json::from_value(value)
            .map_err(|error| format!("Couldn't read the desktop Quit preference: {error}"))
    })
}

fn save_choice<R: Runtime>(app: &AppHandle<R>, choice: QuitChoice) -> Result<(), String> {
    let store = app
        .store_builder(PREFERENCES)
        .disable_auto_save()
        .build()
        .map_err(|error| format!("Couldn't open desktop preferences: {error}"))?;
    let previous = store.get(QUIT_CHOICE);
    store.set(
        QUIT_CHOICE,
        serde_json::to_value(choice).map_err(|error| error.to_string())?,
    );
    if let Err(error) = store.save() {
        // A failed save must not silently become a remembered choice in this
        // session. Keep the prompt open and let the user retry without saving.
        match previous {
            Some(value) => store.set(QUIT_CHOICE, value),
            None => {
                store.delete(QUIT_CHOICE);
            }
        }
        return Err(format!(
            "Couldn't save the desktop Quit preference: {error}"
        ));
    }
    Ok(())
}

/// Discoverable tray control; it changes only the preference, never processes.
pub fn reset_preference<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    save_choice(app, QuitChoice::Ask)
}

/// Called for explicit exit requests. Window-close requests are handled by
/// `window::keep_running_on_exit_request` before reaching this function.
pub fn request<R: Runtime>(app: &AppHandle<R>) {
    let controller = app.state::<SharedQuitController>();
    if !controller.begin() {
        if controller.needs_prompt() {
            // Recover even if a prior native window was destroyed while an
            // asynchronous action was completing. Never build on the Windows
            // menu/event callback thread.
            let app = app.clone();
            std::thread::spawn(move || show_or_report(&app));
        }
        return;
    }
    let app = app.clone();
    // Building a webview on the menu/event callback thread deadlocks on Windows.
    std::thread::spawn(move || {
        let controller = app.state::<SharedQuitController>();
        let owned = app.state::<SharedOwnedProcesses>();
        if owned.close_if_empty() {
            authorize_exit(&app);
            return;
        }
        match choice(&app) {
            Ok(QuitChoice::StopOwned) => finish_or_prompt(&app, QuitAnswer::StopOwned),
            Ok(QuitChoice::KeepRunning) => finish_or_prompt(&app, QuitAnswer::KeepRunning),
            Ok(QuitChoice::Ask) => {
                controller.set(Phase::Prompt { error: None });
                show_or_report(&app);
            }
            Err(error) => {
                controller.set(Phase::Prompt { error: Some(error) });
                show_or_report(&app);
            }
        }
    });
}

fn authorize_exit<R: Runtime>(app: &AppHandle<R>) {
    app.state::<SharedQuitController>()
        .set(Phase::ExitAuthorized);
    app.exit(0);
}

fn finish<R: Runtime>(app: &AppHandle<R>, answer: QuitAnswer) -> Result<(), String> {
    let owned = app.state::<SharedOwnedProcesses>();
    match answer {
        QuitAnswer::StopOwned => {
            let errors = owned.quit(QUIT_GRACE);
            if !errors.is_empty() {
                return Err(format!(
                    "Some background work could not be confirmed stopped: {}. Choose No to quit only the desktop, or Cancel to keep it open.",
                    errors
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("; ")
                ));
            }
        }
        QuitAnswer::KeepRunning => owned.keep_running(),
    }
    authorize_exit(app);
    Ok(())
}

fn finish_or_prompt<R: Runtime>(app: &AppHandle<R>, answer: QuitAnswer) {
    let controller = app.state::<SharedQuitController>();
    controller.set(Phase::Stopping);
    if let Err(error) = finish(app, answer) {
        controller.set(Phase::Prompt { error: Some(error) });
        show_or_report(app);
    }
}

fn show_or_report<R: Runtime>(app: &AppHandle<R>) {
    if let Err(error) = show_prompt(app) {
        app.state::<SharedQuitController>().dismiss();
        eprintln!("ZeroClaw Desktop: showing the Quit confirmation failed: {error}");
    }
}

fn show_prompt<R: Runtime>(app: &AppHandle<R>) -> Result<(), tauri::Error> {
    let _build = PROMPT_BUILD.lock().unwrap_or_else(PoisonError::into_inner);
    if !app.state::<SharedQuitController>().needs_prompt() {
        return Ok(());
    }
    if let Some(prompt) = app.get_webview_window(QUIT_WINDOW) {
        crate::window::reveal(&prompt);
        return Ok(());
    }
    WebviewWindowBuilder::new(app, QUIT_WINDOW, WebviewUrl::App("quit.html".into()))
        .title("Quit ZeroClaw")
        .inner_size(480.0, 350.0)
        .center()
        .resizable(false)
        .on_navigation(local_prompt_url)
        .build()?;
    Ok(())
}

fn local_prompt_url(url: &tauri::Url) -> bool {
    matches!(
        (url.scheme(), url.host_str()),
        ("tauri", Some("localhost")) | ("http" | "https", Some("tauri.localhost"))
    ) && url.path() == "/quit.html"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
}

/// App commands can otherwise be called by remote dashboard content. Restrict
/// this new authority to this exact bundled window and deny navigation away.
fn require_local_prompt(window: &WebviewWindow) -> Result<(), String> {
    if window.label() != QUIT_WINDOW
        || !local_prompt_url(&window.url().map_err(|error| error.to_string())?)
    {
        return Err("Quit choices are only available in the desktop confirmation.".into());
    }
    Ok(())
}

#[tauri::command]
pub fn quit_prompt_error(window: WebviewWindow) -> Result<Option<String>, String> {
    require_local_prompt(&window)?;
    let controller = window.state::<SharedQuitController>();
    let phase = controller.0.lock().unwrap_or_else(PoisonError::into_inner);
    Ok(match &*phase {
        Phase::Prompt { error } => error.clone(),
        _ => None,
    })
}

#[tauri::command]
pub fn cancel_quit(window: WebviewWindow) -> Result<(), String> {
    require_local_prompt(&window)?;
    if !window.state::<SharedQuitController>().allow_close() {
        return Err("A Quit action is already in progress.".into());
    }
    window.close().map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn confirm_quit(
    window: WebviewWindow,
    answer: QuitAnswer,
    remember: bool,
) -> Result<(), String> {
    require_local_prompt(&window)?;
    let app = window.app_handle().clone();
    {
        let controller = app.state::<SharedQuitController>();
        let mut phase = controller.0.lock().unwrap_or_else(PoisonError::into_inner);
        if !matches!(*phase, Phase::Prompt { .. }) {
            return Err("A Quit action is already in progress.".into());
        }
        *phase = Phase::Stopping;
    }
    let task_app = app.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        if remember {
            save_choice(&task_app, answer.into())?;
        }
        finish(&task_app, answer)
    })
    .await
    .map_err(|error| format!("Couldn't complete the Quit action: {error}"))
    .and_then(|result| result);
    if let Err(error) = &result {
        app.state::<SharedQuitController>().set(Phase::Prompt {
            error: Some(error.clone()),
        });
        // A native destruction can bypass CloseRequested. Restore the real
        // surface as well as its phase so retry, No and Cancel remain reachable.
        show_or_report(&app);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_bundled_confirmation_origin_is_allowed() {
        for url in [
            "tauri://localhost/quit.html",
            "http://tauri.localhost/quit.html",
            "https://tauri.localhost/quit.html",
        ] {
            assert!(local_prompt_url(&url.parse().unwrap()), "{url}");
        }
        for url in [
            "http://127.0.0.1:42617/quit.html",
            "https://example.test/quit.html",
            "tauri://localhost/index.html",
            "http://tauri.localhost:8000/quit.html",
            "http://user@tauri.localhost/quit.html",
        ] {
            assert!(!local_prompt_url(&url.parse().unwrap()), "{url}");
        }
    }

    #[test]
    fn duplicate_quit_during_preference_resolution_cannot_offer_a_choice() {
        let controller = QuitController::default();
        assert!(controller.begin());
        assert!(!controller.begin());
        assert!(!controller.needs_prompt());
        assert!(!controller.allow_close());
        // Only the worker that resolves Ask (or its read error) opens a prompt.
        controller.set(Phase::Prompt { error: None });
        assert!(controller.needs_prompt());
    }

    #[test]
    fn native_close_during_stop_does_not_strand_a_failed_quit() {
        let controller = QuitController::default();
        assert!(controller.begin());
        controller.set(Phase::Stopping);
        // Native CloseRequested and the Cancel command both use this gate.
        assert!(!controller.allow_close());
        assert!(!controller.needs_prompt());
        controller.set(Phase::Prompt {
            error: Some("cleanup failed".into()),
        });
        assert!(
            controller.needs_prompt(),
            "failure must request a recoverable surface"
        );
        assert!(
            !controller.begin(),
            "re-entry must reveal/rebuild the pending prompt"
        );
        assert!(
            controller.allow_close(),
            "Cancel must become available after failure"
        );
        assert!(
            controller.begin(),
            "a later explicit Quit must remain actionable"
        );
    }

    #[test]
    fn cancel_reopens_admission_but_cannot_interrupt_cleanup() {
        let controller = QuitController::default();
        assert!(controller.begin());
        assert!(!controller.begin());
        assert!(!controller.needs_prompt());
        assert!(!controller.allow_close());
        controller.set(Phase::Prompt { error: None });
        controller.dismiss();
        assert!(controller.begin());
        controller.set(Phase::Stopping);
        controller.dismiss();
        assert!(!controller.begin());
        assert!(!controller.exit_authorized());
        controller.set(Phase::ExitAuthorized);
        assert!(controller.exit_authorized());
    }
}
