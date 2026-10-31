//! Launch the shipped CLI against real local transports. The scripted peer
//! speaks NDJSON; it is not evidence of compatibility with a released daemon.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use zeroclaw_api::rpc_endpoint;

const PREVIOUS_VERSION: &str = "0.0.0-previous";
const REFUSAL: &str = "fixture credential refused";

#[cfg(unix)]
struct Listener(tokio::net::UnixListener);
#[cfg(windows)]
struct Listener(tokio::net::windows::named_pipe::NamedPipeServer);

impl Listener {
    fn bind(path: &Path) -> Self {
        #[cfg(unix)]
        return Self(tokio::net::UnixListener::bind(path).expect("bind private socket"));
        #[cfg(windows)]
        return Self(
            tokio::net::windows::named_pipe::ServerOptions::new()
                .first_pipe_instance(true)
                .create(path)
                .expect("bind private named pipe"),
        );
    }

    async fn assert_unused(self) {
        #[cfg(unix)]
        let connection = self.0.accept();
        #[cfg(windows)]
        let connection = self.0.connect();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), connection)
                .await
                .is_err(),
            "a rejected or explicit endpoint must not dial another daemon"
        );
    }

    // Zerocode is the documented standalone-client exemption in clippy.toml;
    // its transport fixture must not pull in the daemon attribution crate.
    #[allow(clippy::disallowed_methods)]
    fn respond(self, refusal: bool) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            #[cfg(unix)]
            let (stream, _) = self.0.accept().await.expect("accept CLI");
            #[cfg(windows)]
            let stream = {
                self.0.connect().await.expect("accept CLI");
                self.0
            };
            let (read, mut write) = tokio::io::split(stream);
            let mut lines = BufReader::new(read).lines();
            let request = lines.next_line().await.unwrap().unwrap();
            let request: Value = serde_json::from_str(&request).unwrap();
            assert_eq!(request["method"], "initialize");
            // Do not retain or print initialize.env, even though this child
            // receives only the isolated environment constructed below.
            let response = if refusal {
                json!({"jsonrpc": "2.0", "id": request["id"],
                    "error": {"code": -32010, "message": REFUSAL}})
            } else {
                json!({"jsonrpc": "2.0", "id": request["id"], "result": {
                    "protocol_version": 1, "server_version": PREVIOUS_VERSION,
                    "commands": []}})
            };
            write
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
            // The real client must close the refused connection.
            assert!(lines.next_line().await.unwrap().is_none());
        })
    }
}

fn binaries() -> &'static Path {
    static BINARIES: OnceLock<tempfile::TempDir> = OnceLock::new();
    BINARIES
        .get_or_init(|| {
            let dir = tempfile::tempdir().expect("private executable directory");
            let cli_name = format!("zerocode{}", std::env::consts::EXE_SUFFIX);
            std::fs::copy(env!("CARGO_BIN_EXE_zerocode"), dir.path().join(cli_name))
                .expect("copy shipped CLI beside harmless daemon sentinel");
            // The production launcher locates a sibling zeroclaw executable.
            // This tiny fixture records an attempted spawn, then exits. It cannot
            // run a daemon or access the user's configuration.
            let source = dir.path().join("sentinel.rs");
            std::fs::write(
                &source,
                r#"
fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    let index = args.iter().position(|arg| arg == "--config-dir").unwrap();
    let marker = std::path::Path::new(&args[index + 1]).join("unexpected-daemon-spawn");
    std::fs::write(marker, b"spawned").unwrap();
    std::process::exit(27);
}
"#,
            )
            .unwrap();
            let compiler = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
            let output = std::process::Command::new(compiler)
                .arg(&source)
                .arg("-o")
                .arg(
                    dir.path()
                        .join(format!("zeroclaw{}", std::env::consts::EXE_SUFFIX)),
                )
                .output()
                .expect("compile harmless spawn sentinel with the test toolchain");
            assert!(
                output.status.success(),
                "sentinel compile: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            dir
        })
        .path()
}

struct Fixture {
    dir: tempfile::TempDir,
    endpoints: rpc_endpoint::ClientEndpoints,
}

impl Fixture {
    fn new() -> Self {
        // Keep Unix-domain socket paths below the macOS limit.
        #[cfg(unix)]
        let dir = tempfile::Builder::new()
            .prefix("zc-migrate-")
            .tempdir_in("/tmp")
            .unwrap();
        #[cfg(windows)]
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(dir.path().join("zerocode-config.toml"), "locale = \"en\"\n").unwrap();
        let endpoints = rpc_endpoint::client_endpoints_with(None, &data);
        Self { dir, endpoints }
    }

    fn alternate_endpoint(&self) -> PathBuf {
        #[cfg(unix)]
        return self.dir.path().join("override.sock");
        #[cfg(windows)]
        return rpc_endpoint::resolve_endpoint_with(None, &self.dir.path().join("override"));
    }

    async fn run(&self, socket_override: Option<&Path>) -> String {
        let mut command = tokio::process::Command::new(
            binaries().join(format!("zerocode{}", std::env::consts::EXE_SUFFIX)),
        );
        command.env_clear();
        // Windows requires its system directory for normal process startup.
        #[cfg(windows)]
        for key in ["SystemRoot", "WINDIR"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        for key in [
            "HOME",
            "USERPROFILE",
            "APPDATA",
            "LOCALAPPDATA",
            "TMP",
            "TEMP",
            "TMPDIR",
        ] {
            command.env(key, self.dir.path());
        }
        command.env("LANG", "en_US.UTF-8");
        if let Some(endpoint) = socket_override {
            // Exercise the production override parser, including trimming.
            command.env("ZEROCLAW_SOCKET", format!("  {}  ", endpoint.display()));
        }
        command
            .arg("--config-dir")
            .arg(self.dir.path())
            .current_dir(self.dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = command.spawn().expect("launch shipped zerocode");
        let output = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
            .await
            .expect("CLI must report refusal promptly")
            .expect("collect CLI output");
        let stderr = String::from_utf8(output.stderr).expect("UTF-8 startup diagnostic");
        assert!(!output.status.success(), "refusal must fail the command");
        assert!(
            !self.dir.path().join("unexpected-daemon-spawn").exists(),
            "refusal must not spawn another daemon; CLI stderr: {stderr}"
        );
        stderr
    }
}

async fn peer_finished(peer: tokio::task::JoinHandle<()>) {
    tokio::time::timeout(Duration::from_secs(5), peer)
        .await
        .expect("refused connection closes")
        .expect("scripted peer completed");
}

#[tokio::test]
async fn cli_primary_version_refusal_does_not_spawn_or_fall_back() {
    let fixture = Fixture::new();
    let peer = Listener::bind(&fixture.endpoints.primary).respond(false);
    let legacy = fixture.endpoints.legacy.as_deref().map(Listener::bind);
    let stderr = fixture.run(None).await;
    assert!(stderr.contains(PREVIOUS_VERSION), "{stderr}");
    assert!(stderr.contains(env!("CARGO_PKG_VERSION")), "{stderr}");
    peer_finished(peer).await;
    if let Some(legacy) = legacy {
        legacy.assert_unused().await;
    }
}

#[tokio::test]
async fn cli_primary_handshake_refusal_does_not_spawn_or_fall_back() {
    let fixture = Fixture::new();
    let peer = Listener::bind(&fixture.endpoints.primary).respond(true);
    let legacy = fixture.endpoints.legacy.as_deref().map(Listener::bind);
    let stderr = fixture.run(None).await;
    assert!(stderr.contains(REFUSAL), "{stderr}");
    peer_finished(peer).await;
    if let Some(legacy) = legacy {
        legacy.assert_unused().await;
    }
}

#[tokio::test]
async fn cli_explicit_endpoint_refusal_never_dials_defaults_or_spawns() {
    let fixture = Fixture::new();
    let endpoint = fixture.alternate_endpoint();
    let peer = Listener::bind(&endpoint).respond(true);
    let primary = Listener::bind(&fixture.endpoints.primary);
    let legacy = fixture.endpoints.legacy.as_deref().map(Listener::bind);
    let stderr = fixture.run(Some(&endpoint)).await;
    assert!(stderr.contains(REFUSAL), "{stderr}");
    peer_finished(peer).await;
    primary.assert_unused().await;
    if let Some(legacy) = legacy {
        legacy.assert_unused().await;
    }
}

#[cfg(windows)]
#[tokio::test]
async fn cli_legacy_version_refusal_is_reported_without_spawning() {
    let fixture = Fixture::new();
    let legacy = fixture
        .endpoints
        .legacy
        .as_deref()
        .expect("Windows legacy endpoint");
    let peer = Listener::bind(legacy).respond(false);
    let stderr = fixture.run(None).await;
    assert!(stderr.contains(PREVIOUS_VERSION), "{stderr}");
    peer_finished(peer).await;
}

#[cfg(windows)]
#[tokio::test]
async fn cli_legacy_handshake_refusal_is_reported_without_spawning() {
    let fixture = Fixture::new();
    let legacy = fixture
        .endpoints
        .legacy
        .as_deref()
        .expect("Windows legacy endpoint");
    let peer = Listener::bind(legacy).respond(true);
    let stderr = fixture.run(None).await;
    assert!(stderr.contains(REFUSAL), "{stderr}");
    peer_finished(peer).await;
}
