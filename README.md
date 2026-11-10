# Conversation cost interface evidence

Initial multi-call and delegate sequence: `3974b62f7d9e44b4059bcb660eb617cb6def2095`.

The daemon-restart/reopen, mobile partial, and tiny web screenshots were repeated after the final production change at `1a2e91f962`. Implementation head `a678bce2a35e46930b8a0113d1399e31c5a0f9d2` adds only further authorization regressions after that production revision.

The screenshots use the actual built dashboard and ZeroCode connected to a local ZeroClaw daemon, a temporary real ledger/session store, and a scripted provider. No paid provider calls or real credentials were used. ZeroCode runs in a 120 by 38 PTY rendered by xterm; web screenshots use 1280 by 900 or 390 by 844 viewports. Captions describe the observed step.

- `web-multi.png` / `tui-multi.png`: two parent calls and a synchronous delegate total $0.0060.
- `*-background-running.png`: parent finished, detached delegate pending; total $0.0100.
- `*-background-finished.png`: polling while idle observes the child's additional $0.0020 exactly once.
- `web-restarted.png` / `tui-reopened.png`: daemon restarted, conversations reopened; both still show $0.0120.
- `tui-native-code.png`: native Code uses the same read API and shows its own $0.0020 total.
- `web-partial-mobile.png` / `tui-partial.png`: a successful reply without provider usage preserves a gap and shows the known amount as incomplete. The web screenshot follows a reload.
- `tui-unavailable.png`: new conversation has no recorded estimate.
- `web-tiny.png` / `tui-tiny.png`: one reported input token is positive spending below $0.0001.

## Reproduce the scripted calls

Use a clean temporary directory `/tmp/conversation-cost-demo`, place `config.toml` there, and create its `workspace` subdirectory. The configuration binds only to loopback, disables pairing for this fixture, and allows only the delegate tool. Do not apply it to an existing installation.

1. Run `python3 provider.py` (loopback port 42992).
2. Build the implementation with `cargo web build` and `cargo build -p zeroclaw --features gateway -p zerocode`.
3. Run `target/debug/zeroclaw --config-dir /tmp/conversation-cost-demo daemon`.
4. Open the dashboard at `http://127.0.0.1:42991/agent/demo`, or run `target/debug/zerocode --config-dir /tmp/conversation-cost-demo` and open Chat with agent `demo`.
5. In a new chat send `multi call fixture web 1`, then `background fixture web 1` (use `tui` instead of `web` in ZeroCode). The provider issues a tool call only once per marker. Use a fresh trailing number for another run.
6. Observe $0.0060, then $0.0100, then $0.0120 after the background worker's eight-second delay. Restart the daemon and reopen the same conversations.
7. For missing/tiny usage, write `{"usage":"missing"}` or `{"usage":"tiny"}` to `/tmp/conversation-cost-demo/provider-mode.json` before a plain prompt. Restore `{"usage":"normal"}` afterwards. Tiny usage should be tested in a new conversation.

The estimates describe the fixture's recorded usage, not an invoice. These screenshots do not claim behavior for unobserved or unpersisted charges.

## Local CI limitation

`./dev/ci.sh all` passed formatting and workspace correctness Clippy after a temporary local image supplied its missing compiler/tools and limited Cargo to one build job. It then failed the provider-dispatch gate on two calls in the existing `safety_net_loop_recovery.rs` test fixture. The unchanged dispatch classifier reports the same two lines on upstream `8512b711`; compare `dispatch-gate-base.txt` and `dispatch-gate-feature.txt`. The full Docker pipeline is not claimed as passing. Repository CI files were not changed.
