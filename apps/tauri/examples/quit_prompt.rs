//! Manual native UI proof with production Quit handlers and bundled HTML.
//! Uses a separate app identifier/store and a harmless kernel fixture. It never
//! probes the user's gateway or launches the real daemon.

#[path = "../tests/support/owned_kernel.rs"]
pub mod owned_kernel;

use tauri::Manager;
use zeroclaw_desktop::{ownership::SharedOwnedProcesses, quit, state, tray};

fn main() {
    owned_kernel::run_if_fixture();
    let args: Vec<_> = std::env::args().skip(1).collect();
    let proof_dir = args.first().map_or_else(
        || std::env::temp_dir().join("zeroclaw-desktop-quit-proof"),
        std::path::PathBuf::from,
    );
    assert!(proof_dir.is_absolute(), "proof directory must be absolute");
    let reset = args.iter().any(|arg| arg == "--reset");
    let fixture = owned_kernel::Fixture::new(&proof_dir);
    let owned = SharedOwnedProcesses::default();
    let mode = if args.iter().any(|arg| arg == "--stubborn") {
        2
    } else {
        1
    };
    owned
        .launch(&fixture.binary, mode)
        .expect("start harmless owned kernel");
    let pid = fixture.pid("supervisor.pid");
    std::fs::write(
        proof_dir.join("current-fixture.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "directory": fixture.dir, "pid": pid, "running": owned_kernel::running(pid)
        }))
        .unwrap(),
    )
    .unwrap();
    // No must leave work alive after UI exit. The fixture has a ten-minute
    // ceiling; writing its private `stop` file also cleans up after proof.
    std::mem::forget(fixture);
    let mut context = tauri::generate_context!();
    context.config_mut().identifier = "ai.zeroclawlabs.desktop.quit-proof".into();
    context.config_mut().app.windows.clear();
    tauri::Builder::default()
        .plugin(tauri_plugin_store::Builder::default().build())
        .manage(owned)
        .manage(state::shared_state())
        .manage(quit::SharedQuitController::default())
        .on_window_event(quit::handle_window_event)
        .on_menu_event(|app, event| {
            #[cfg(target_os = "macos")]
            if event.id.as_ref() == "proof-native-quit" {
                // A native OS-event boundary probe, not evidence that the Dock
                // itself was clicked. It reaches the installed production hook.
                use objc2_foundation::{NSAppleEventDescriptor, NSAppleEventSendOptions};
                let target = NSAppleEventDescriptor::descriptorWithProcessIdentifier(
                    i32::try_from(std::process::id()).expect("own PID fits pid_t"),
                );
                let quit = NSAppleEventDescriptor::appleEventWithEventClass_eventID_targetDescriptor_returnID_transactionID(
                    u32::from_be_bytes(*b"aevt"), u32::from_be_bytes(*b"quit"), Some(&target), -1, 0,
                );
                quit.sendEventWithOptions_timeout_error(NSAppleEventSendOptions::NoReply, 1.0)
                    .expect("send native Quit event to this proof process");
                return;
            }
            tray::events::handle_menu_event(app, event);
        })
        .invoke_handler(tauri::generate_handler![
            quit::quit_prompt_error,
            quit::cancel_quit,
            quit::confirm_quit
        ])
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            {
                use tauri::menu::{MenuItemBuilder, SubmenuBuilder};
                let menu = quit::application_menu(app.handle())?;
                let native_quit = MenuItemBuilder::with_id("proof-native-quit", "Send Native Quit Event")
                    .build(app)?;
                menu.append(&SubmenuBuilder::new(app, "Proof").item(&native_quit).build()?)?;
                app.set_menu(menu)?;
            }
            #[cfg(target_os = "macos")]
            zeroclaw_desktop::macos::quit::install(app.handle())?;
            println!(
                "QUIT_PROOF_STORE={}",
                app.path()
                    .app_data_dir()?
                    .join("desktop-preferences.json")
                    .display()
            );
            if reset {
                quit::reset_preference(app.handle()).map_err(std::io::Error::other)?;
            }
            tauri::WebviewWindowBuilder::new(
                app,
                "proof-controls",
                tauri::WebviewUrl::External("about:blank".parse()?),
            )
            .title("Quit Proof Controls")
            .inner_size(360.0, 120.0)
            .build()?;
            tray::setup_tray(app)?;
            Ok(())
        })
        .build(context)
        .expect("build isolated desktop proof app")
        .run(quit::handle_run_event);
}
