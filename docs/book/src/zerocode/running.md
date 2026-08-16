# Running zerocode

## Local setup

On the same machine as the daemon, no extra configuration is needed:

<div class="os-tabs-src">

#### sh

```sh
zerocode
```

</div>

zerocode finds the daemon's local endpoint automatically: `<data_dir>/data/daemon.sock`
on Unix, `\\.\pipe\zeroclaw-<hash>` on Windows. If the daemon isn't running,
zerocode spawns an ephemeral one.

## Switching sessions

In the **Chat** and **Code** panes you can load or switch existing sessions without restarting zerocode:

- **Switch session** opens the session list (default chord: Ctrl+S; rebindable in the keymap).
- Use the list-navigation keys to move the selection (defaults: Up/Down).
- **Enter** switches to the highlighted session.
- **New session** starts fresh (default chord: Ctrl+N; rebindable).

The in-app help overlay shows your live key bindings for these actions.

Chat/Code sessions and ACP-backed sessions use different stores. If you use the ACP protocol directly, use `session/load` when you need transcript replay and `session/resume` when you only need the server-side session state restored. See the [ACP documentation](../channels/acp.md) for protocol-level details.

## Terminal text input

zerocode runs as a terminal UI in raw mode. It receives terminal key and paste
events, not native platform text-field events. On macOS, system text
replacements therefore work only when your terminal expands them before
zerocode receives the input.

### Copying text from the chat

By default zerocode captures mouse events so that scrolling, dragging the
scrollbar, and clickable controls work inside the TUI. While mouse capture is
on, the terminal hands mouse actions to zerocode instead of doing its own
click-and-drag text selection, so you can't highlight chat text to copy it with
a plain mouse drag.

Two ways to copy text out of the chat:

- **Keep mouse capture on (default):** use your terminal's selection override.
  In iTerm2 and most macOS terminals, hold **Option (⌥)** and drag to select,
  then copy as usual. Many terminals use a similar modifier.
- **Turn mouse capture off:** set `mouse_capture = false` under `[input]` in the
  zerocode config. The terminal's native select-to-copy then works everywhere,
  at the cost of in-app mouse scrolling and clickable controls. Keyboard
  scrolling still works.

```toml
[input]
# Release mouse events to the terminal so click-and-drag select-to-copy works.
# Default is true (zerocode captures the mouse for scroll/scrollbar/clicks).
mouse_capture = false
```

## CLI flags

| Flag | Description |
|------|-------------|
| `--connect <url>` | Connect to a remote daemon via WSS (e.g. `wss://host:9781`) |
| `--tls-skip-verify` | Skip TLS certificate verification. Required for self-signed certs |
| `--config-dir <path>` | Override the config directory |
