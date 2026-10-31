//! Validation-only fixture: place in zeroclaw-rpc-client/tests, never in the
//! publication candidate. The workflow must verify the pinned release ZIP's
//! SHA256 before setting ZEROCLAW_LEGACY_DAEMON_EXE to its extracted binary.
#![cfg(windows)]

use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use zeroclaw_rpc_client::{ClientError, ConnectOptions, Method, RpcClient, endpoint};

const RELEASE: &str = "0.8.7";
const ZIP_SHA256: &str = "46ede57760dfccca71ed94f135e149b103dff12a76a63fd8d13c2a5fe097bca7";

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("zc-release-{tag}-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&path).expect("create uniquely owned fixture directory");
        std::fs::create_dir(path.join("data")).unwrap();
        std::fs::write(path.join("config.toml"),
            "schema_version = 3\n[gateway]\nhost = \"127.0.0.1\"\nport = 0\nrequire_pairing = true\n").unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct OwnedDaemon(Child);
impl Drop for OwnedDaemon {
    fn drop(&mut self) {
        // Only this fixture's owned OS child handle; never kill by name/PID lookup.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn isolated_command(binary: &Path, scratch: &Path) -> Command {
    let mut cmd = Command::new(binary);
    cmd.env_clear().current_dir(scratch).stdin(Stdio::null());
    for key in ["SystemRoot", "WINDIR"] {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
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
        cmd.env(key, scratch);
    }
    cmd.env("LANG", "en_US.UTF-8");
    cmd
}

async fn exercise_released_daemon(explicit: bool) {
    let binary = PathBuf::from(
        std::env::var_os("ZEROCLAW_LEGACY_DAEMON_EXE")
            .expect("set the path to the checksum-verified v0.8.7 Windows release binary"),
    );
    assert!(binary.is_file(), "released binary must exist");
    assert_eq!(
        std::env::var("ZEROCLAW_LEGACY_RELEASE_SHA256").unwrap(),
        ZIP_SHA256,
        "workflow must record the verified archive digest"
    );
    let scratch = Scratch::new(if explicit { "override" } else { "default" });
    let version = isolated_command(&binary, &scratch.0)
        .arg("--version")
        .output()
        .unwrap();
    assert!(version.status.success(), "released binary --version failed");
    assert!(
        String::from_utf8_lossy(&version.stdout).contains(RELEASE),
        "wrong released binary version"
    );

    let data = scratch.0.join("data");
    let defaults = endpoint::client_endpoints_with(None, &data);
    let override_name = format!(
        r"\\.\pipe\zc-release-override-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let endpoints = if explicit {
        endpoint::client_endpoints_with(Some(&override_name), &data)
    } else {
        defaults.clone()
    };
    if explicit {
        assert!(endpoints.legacy.is_none());
    } else {
        assert!(endpoints.legacy.is_some());
    }

    let log = std::fs::File::create(scratch.0.join("daemon.log")).unwrap();
    let mut command = isolated_command(&binary, &scratch.0);
    command
        .args(["--config-dir"])
        .arg(&scratch.0)
        .args(["daemon", "--host", "127.0.0.1", "--port", "0"])
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log));
    if explicit {
        command.env("ZEROCLAW_SOCKET", &override_name);
    }
    let mut daemon = OwnedDaemon(
        command
            .spawn()
            .expect("start checksum-verified released daemon"),
    );
    let child_pid = daemon.0.id();
    let deadline = Instant::now() + Duration::from_secs(60);
    let client = loop {
        assert!(
            daemon.0.try_wait().unwrap().is_none(),
            "released daemon exited during startup; do not print its raw logs (they may contain pairing material)"
        );
        let options = ConnectOptions {
            handshake_timeout: Some(Duration::from_secs(5)),
            ..ConnectOptions::default()
        };
        match RpcClient::connect_local_endpoints(&endpoints, options).await {
            Ok(client) => break client,
            Err(ClientError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                assert!(
                    Instant::now() < deadline,
                    "released daemon was not found under the computed endpoint; distinguish startup failure from the documented cross-toolchain legacy-hash limitation before interpreting this result"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) => panic!("released daemon connection was refused: {error}"),
        }
    };
    assert_eq!(client.handshake().server_version, RELEASE);
    assert_eq!(
        client.handshake().server_pid,
        child_pid,
        "client must reach this fixture's released daemon"
    );
    let status = tokio::time::timeout(
        Duration::from_secs(10),
        client.request(Method::Status, json!({})),
    )
    .await
    .expect("bounded released daemon status reply")
    .expect("real released daemon status reply");
    assert_eq!(status["server_version"], RELEASE);
    assert_eq!(
        status["protocol_version"],
        client.handshake().protocol_version
    );
    assert_eq!(
        std::fs::canonicalize(
            status["config_dir"]
                .as_str()
                .expect("status config directory")
        )
        .unwrap(),
        std::fs::canonicalize(&scratch.0).unwrap(),
        "status must identify the isolated fixture config"
    );
    let reached = status["local_ipc_endpoint"]
        .as_str()
        .expect("status endpoint");
    let expected = if explicit {
        &endpoints.primary
    } else {
        endpoints.legacy.as_ref().unwrap()
    };
    assert_eq!(Path::new(reached), expected);
    if !explicit {
        match tokio::net::windows::named_pipe::ClientOptions::new().open(&defaults.primary) {
            Err(error) => assert_eq!(
                error.kind(),
                std::io::ErrorKind::NotFound,
                "the release uses only the legacy default name"
            ),
            Ok(_) => panic!("an unintended second daemon is serving the stable name"),
        }
    }
    println!(
        "released_daemon_evidence={}",
        json!({
            "release": RELEASE, "archive_sha256": ZIP_SHA256,
            "mode": if explicit { "explicit_override" } else { "missing_primary_legacy_fallback" },
            "owned_pid": child_pid, "reported_pid": client.handshake().server_pid,
            "protocol_version": client.handshake().protocol_version,
            "status_reached_owned_config": true, "reported_endpoint_matches": true
        })
    );
    drop(client);
    // Drop order closes the owned process before removing its isolated state.
    drop(daemon);
    drop(scratch);
}

#[tokio::test]
async fn released_v0_8_7_daemon_is_reached_through_missing_primary_legacy_fallback() {
    exercise_released_daemon(false).await;
}

#[tokio::test]
async fn released_v0_8_7_daemon_works_with_an_explicit_endpoint() {
    exercise_released_daemon(true).await;
}
