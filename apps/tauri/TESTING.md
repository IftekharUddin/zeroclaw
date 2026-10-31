# Desktop app — testing notes

## Startup flow

The desktop app is a thin shell over a running ZeroClaw **web gateway**. There
is no longer a macOS/Windows/Linux permission-setup wizard — the app goes
straight to the gateway, and first-time setup happens in the web Quickstart.

On launch:

1. A small **splash** window (`apps/tauri/splash/index.html`) appears and polls
   the gateway's `/health` (via the `get_health` IPC command) every ~1.2s.
2. Once the gateway is healthy, the splash calls the `open_dashboard` command,
   which pairs with the gateway (when pairing is required), creates the **main**
   window pointed at the gateway **root** (`http://127.0.0.1:42617/`), seeds the
   bearer token via an initialization script, and closes the splash.
3. The web app's fresh-install redirect (`FreshInstallRedirect` in
   `web/src/App.tsx`) sends first-time users — no agents yet, Quickstart never
   completed — to `/quickstart`. Returning users land on the dashboard.

> The app looks for a gateway on `127.0.0.1:42617`: it reuses a running daemon, or starts the discovered kernel with `zeroclaw service run-desktop-daemon --port 42617` (preferring a kernel bundled next to the app executable, then `PATH` and the common install dirs — see `src/daemon.rs::find_zeroclaw_binary`). Before launch, Desktop verifies that the kernel accepts this supervisor command. An externally installed kernel must therefore support the Desktop supervisor command; an older unsupported kernel produces an actionable startup error instead of falling back to an uncaptured daemon. The self-contained installer below bundles the matching kernel as a Tauri sidecar — the "full experience" distribution from architecture RFC fnd-001, D5.
>
> **Run `zeroclaw daemon`, not `zeroclaw gateway start`.** Both serve the
> dashboard on 42617, but only the daemon attaches the supervisor that powers
> in-place reload. After the Quickstart applies config it calls `/admin/reload`;
> a standalone `gateway start` has no supervisor and returns
> `503 "no daemon supervisor — running as standalone gateway"`, so the new agent
> won't go live until the process is restarted. The daemon hot-reloads instead.

The desktop supervisor writes combined daemon stdout and stderr to `<config-dir>/logs/zeroclaw-desktop-daemon.log`, where `<config-dir>` follows canonical config resolution precedence: `ZEROCLAW_CONFIG_DIR`, then `ZEROCLAW_DATA_DIR`, then deprecated `ZEROCLAW_WORKSPACE`, then Homebrew/default resolution. The capture is capped at 8 MiB and retains the newest tail when the cap is crossed.

## Self-contained build (bundled kernel)

The plain `cargo tauri build` produces an app that *finds* an installed
`zeroclaw`. To produce the zero-install artifact — double-click on a machine
with nothing pre-installed and get a running agent — bundle the kernel as a
sidecar:

```sh
# 1. Build the dashboard, then embed it in the staged kernel.
cargo web build
scripts/desktop/prepare-kernel.sh --features embedded-web
scripts/desktop/prepare-kernel.sh --target universal-apple-darwin --features embedded-web

# 2. Bundle with the sidecar overlay (adds bundle.externalBin).
cd apps/tauri && cargo tauri build --config tauri.bundled.conf.json
```

`ZEROCLAW_KERNEL_PATH` can reuse a prebuilt single-target kernel, but that
binary must already have been built with `--features embedded-web`; the staging
script cannot add embedded assets to an existing executable.

The overlay keeps the default config untouched, so `cargo tauri build`
without the staged kernel keeps working. Tauri places the sidecar next to the
app executable as `zeroclaw`, which is the first place
`find_zeroclaw_binary()` looks — so the bundled app starts its own daemon
from its own kernel.

To verify self-containment, launch on a machine (or shell) where `zeroclaw`
is not on `PATH` and not in `~/.cargo/bin`, then check the daemon's process
path points inside the app bundle:

```sh
pgrep -fl 'zeroclaw daemon'   # expect .../ZeroClaw.app/Contents/MacOS/zeroclaw
```

> Size note: the kernel dominates the artifact. A stripped release kernel is
> ~146 MB per arch (~55–65 MB compressed dmg); a universal (two-slice) kernel
> roughly doubles that. The unstripped dev kernel is ~228 MB — always let
> `prepare-kernel.sh` strip it.

## macOS (current target)

### Reset to fresh-install state
```sh
pkill -f 'target/debug/zeroclaw-desktop'
rm "$HOME/Library/Application Support/ai.zeroclawlabs.desktop/settings.json"
killall Dock                                   # if dock icon looks stale
bash dev/run-tauri-dev.sh
```

To exercise the full first-run path, also reset the gateway's config so the
Quickstart auto-launches (the gateway reports `quickstart_completed=false` and
an empty agents list via `GET /api/quickstart/state`).

For a real installed-bundle test:
```sh
cd apps/tauri && cargo tauri build
cp -R target/release/bundle/macos/ZeroClaw.app /Applications/
xattr -dr com.apple.quarantine /Applications/ZeroClaw.app
open /Applications/ZeroClaw.app
```

### What to verify
- With **no gateway running**: splash shows "Connecting to your ZeroClaw
  gateway…" and, after a few seconds, the "make sure the gateway is running"
  hint. The tray icon shows Disconnected.
- Start the daemon (`cargo run -p zeroclaw -- daemon`, or `zeroclaw daemon`):
  within ~1–2s the splash hands off — the dashboard window opens, splash closes.
- **First run** (fresh gateway config): the dashboard opens straight onto the
  **Quickstart**; completing it configures an agent and the gateway becomes
  usable. After completion, relaunching the app lands on the dashboard.
- **Returning run** (agent already configured): the dashboard opens on the
  normal dashboard, not the Quickstart.
- Close the dashboard window: the app keeps running in the tray. Clicking the
  tray icon, choosing **Show Dashboard** or **Agent Chat**, or launching the
  app again brings the same window back, restored if it was minimized and
  without reloading the page.
- Close the splash before the dashboard opens: the same routes reopen the
  splash, which shows any startup error again (such as a missing `zeroclaw`
  binary) and hands off to the dashboard once the gateway is healthy.
- Quit from the tray when this app instance started the daemon: the desktop
  asks whether to stop its background work. **Yes** attempts graceful shutdown,
  then forces remaining owned processes to stop after the bounded grace period.
  **No** quits the UI and leaves that work running. **Cancel**, Escape or closing
  the confirmation keeps the app open without changing the preference.
- **Don't ask again** starts unchecked. Check it with either Yes or No to remember
  that action across relaunches. **Ask Before Quitting Again** in the tray menu
  resets the preference. A fresh app session never adopts surviving processes,
  even when the saved choice is Yes.
- Start `zeroclaw daemon` yourself first, then launch and Quit the app: the app
  reused your daemon, so it keeps running after Quit.
- Inspect `<config-dir>/logs/zeroclaw-desktop-daemon.log` after startup to verify combined stdout/stderr capture; the file stays at or below 8 MiB and keeps the newest tail during continuous output.

### Process ownership

The app can stop only processes it launched in this run. Its retained OS
handles are the ownership authority. The single desktop-local preference in
`desktop-preferences.json` creates the Quit choice (`ask`, `stop_owned`, or
`keep_running`); it never stores process ownership. Quit reads that preference
when invoked. When there is no owned process or launch in progress, it exits
directly without changing the preference.

Yes first seals launch admission, then stops retained trees newest first, so
an owned gateway stops before its owned core. Each tree gets a 15-second grace
period. A launch already waiting for readiness remains owned and is stopped;
a capability probe already in progress must settle without starting a process.
Quit waits at most another 15 seconds for admitted launch calls to return.
Errors keep the confirmation open with a retry, No or Cancel choice. Failed
cleanup retains the exact handle in the same ownership registry. Cancel cannot
lose that capability, and a second Yes retries it. Unix signal authority is
retired when the supervisor is reaped; an error after that point remains an
error on retry rather than signaling a potentially reused group ID.

- **Unix:** Yes sends SIGTERM to the owned supervisor's process group, then
  SIGKILL to anything left after the grace period. The supervisor is not reaped
  until all signals are sent, reserving its PID and group ID against reuse.
- **Windows:** the supervisor is created suspended inside a retained Job
  Object before it can start descendants. Yes writes an explicit stop byte to
  its private inherited stdin pipe. The supervisor forwards the request over
  its daemon's private pipe, waits up to ten seconds for graceful daemon
  shutdown, then terminates any remaining inner-job processes and drains logs.
  Explicit shutdown suppresses update-driven daemon restart. After the outer
  grace period, the desktop terminates its retained outer job and waits up to
  five seconds for the supervisor to exit. Successful job termination requests
  force termination for every contained member; descendant finalization can
  finish asynchronously, so supervisor exit is not a job-wide completion
  barrier. It never discovers processes by PID or parent.

No seals admission and releases the handles without sending a stop request.
The outer Windows job deliberately does not kill on close, and pipe EOF means
keep running; it is not a shutdown request. The supervisor still owns its inner
job for descendant cleanup when the supervisor itself exits. A crash of the UI
also leaves work running. Windows graceful handling needs a matching kernel; an older supervisor-capable
kernel that does not consume the stop request reaches the bounded force fallback.
No state adopts a daemon reused at startup or left by
an earlier app session. Closing a dashboard window still hides it to the tray.
On macOS, the application menu, Command-Q and ordinary Dock Quit all enter the
same confirmation controller. The retained Apple-event target routes ordinary
Quit through Tauri's explicit exit request. Documented logout, restart,
shutdown and quit-all reasons call normal Cocoa termination without issuing
desktop process-stop requests. Force Quit and OS signals keep their existing behavior.

### Quit behavior proof

Run the native ownership fixtures on each supported OS:

```sh
cargo test --locked -p zeroclaw-desktop --lib --test capability_security --test owned_processes
# Windows only: actual supervisor -> daemon graceful shutdown and pipe EOF.
cargo test --locked --test component desktop_quit_windows
```

`owned_processes` launches harmless copies of its own test executable through
the production registry. It verifies graceful and forced cleanup, No, lost UI
handles, previous-session and external-process preservation, sealed admission,
Quit during readiness, and Yes/No during the capability probe. It writes only private fixture directories. The
Windows component test uses the actual kernel, temporary configuration and an
ephemeral loopback port. A result on one OS does not establish another OS's
job, signal or GUI behavior.

For native GUI and persistence proof without touching an existing daemon:

```sh
cargo run --locked -p zeroclaw-desktop --example quit_prompt -- /absolute/proof-directory --reset
```

The example uses the production Quit handlers, bundled confirmation, tray menu
and owned-process registry with a harmless kernel fixture. Its separate app
identifier is `ai.zeroclawlabs.desktop.quit-proof`; it does not probe the user's
gateway. The blank controls window keeps the app reachable after Cancel;
choose Quit from the native application or tray menu to exercise the installed
app's exact handler. The macOS Proof menu can send an ordinary Quit AppleEvent
through OS dispatch to the proof process. Record this separately from clicking
the Dock itself. Omit `--reset` on subsequent launches to verify persistence. When launched
without arguments, fixtures go in the OS temporary `zeroclaw-desktop-quit-proof`
directory. `current-fixture.json` identifies the fixture for observation and
cleanup: writing its private `stop` file ends it without touching other work.

Verify unchecked and checked Yes/No, Cancel, relaunch persistence and the tray
reset. Add `--stubborn` to exercise the bounded force path and attempt a native
confirmation-window close during cleanup: the window must stay open. A failed
preference save must restore enabled choices and permit Cancel and a later Quit.
No must leave the fixture alive after the UI exits, while cooperative Yes must write
its graceful-stop marker and terminate the descendant. Surviving fixtures have
a ten-minute ceiling; clean them up explicitly after proof. These GUI checks
remain distinct from native Windows process tests and signed installed-package
validation.

### Native command boundary

The Rust app still registers `take_screenshot` and `run_applescript`, but the
gateway-served main window receives no remote Tauri capability and cannot invoke
them. Exposing either command requires a separate, narrowly scoped approval and
ACL design.

## Linux / Windows

The app builds and runs the same splash → gateway → Quickstart flow. Bundle
targets are unchanged (`.deb`/`.AppImage` on Linux, `.exe`/`.msi` on Windows).
The window checks above apply on both. Linux tray icons report no clicks, so
use the tray menu there.
Screen capture and AppleScript capabilities remain macOS-only; the other
platforms register stubs that return an unsupported-platform error.

### How to build
```sh
cd apps/tauri
cargo tauri build          # native build on each platform
# Or cross-compile with the appropriate target + toolchain:
#   cargo build --release --target x86_64-unknown-linux-gnu
#   cargo build --release --target x86_64-pc-windows-msvc
```

## Native CI

`.github/workflows/desktop-check.yml` runs strict desktop Clippy and native
ownership/capability tests on macOS, Linux and Windows. Its Windows job also
runs the actual supervisor shutdown component tests. The workflow supports
manual dispatch. It does not automate the native GUI/persistence checklist.
