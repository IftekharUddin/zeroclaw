//! Exercise both build modes with the real CLI and HTTP routes:
//! `cargo test --test component desktop_update`
//! `ZEROCLAW_DESKTOP_SIDECAR=1 cargo test --test component desktop_update`
//! Outbound update requests use a closed loopback proxy; no release is installed.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const DESKTOP_REFUSAL: &str =
    "This ZeroClaw was installed by ZeroClaw Desktop; update the desktop app to upgrade it.";
const FRENCH_REFUSAL: &str = "Mettez à jour l’application de bureau pour mettre à jour ZeroClaw.";

struct Install {
    _dir: tempfile::TempDir,
    binary: PathBuf,
    app: PathBuf,
    config: PathBuf,
}

impl Install {
    fn new(locale: &str, allow_self_upgrade: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join(if cfg!(windows) {
            "zeroclaw.exe"
        } else {
            "zeroclaw"
        });
        std::fs::copy(env!("CARGO_BIN_EXE_zeroclaw"), &binary).unwrap();
        let app = dir.path().join(if cfg!(windows) {
            "zeroclaw-desktop.exe"
        } else {
            "zeroclaw-desktop"
        });
        std::fs::write(&app, b"desktop package fixture").unwrap();
        let config = dir.path().join("config");
        std::fs::create_dir(&config).unwrap();
        std::fs::write(
            config.join("config.toml"),
            format!(
                "locale = {locale:?}\n[gateway]\nrequire_pairing = false\nallow_self_upgrade = {allow_self_upgrade}\ncheck_updates = false\n"
            ),
        )
        .unwrap();
        let catalog = config.join("data/ftl/fr");
        std::fs::create_dir_all(&catalog).unwrap();
        std::fs::write(
            catalog.join("cli.ftl"),
            format!("cli-update-desktop-bundled = {FRENCH_REFUSAL}\n"),
        )
        .unwrap();
        Self {
            _dir: dir,
            binary,
            app,
            config,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        command
            .arg("--config-dir")
            .arg(&self.config)
            .env("ZEROCLAW_CONFIG_DIR", &self.config)
            .env("ZEROCLAW_DESKTOP_SUPERVISED", "1")
            .env("HTTP_PROXY", "http://127.0.0.1:1")
            .env("HTTPS_PROXY", "http://127.0.0.1:1")
            .env("ALL_PROXY", "http://127.0.0.1:1")
            .env("NO_PROXY", "")
            .env("TERM", "dumb");
        command
    }

    fn cli_error(&self, args: &[&str]) -> String {
        let output = self.command().args(args).output().unwrap();
        assert!(!output.status.success());
        String::from_utf8(output.stderr).unwrap()
    }
}

fn built_as_sidecar() -> bool {
    option_env!("ZEROCLAW_DESKTOP_SIDECAR") == Some("1")
}

#[test]
fn desktop_update_cli_refuses_package_replacement_in_selected_locale() {
    for (locale, refusal) in [("en", DESKTOP_REFUSAL), ("fr", FRENCH_REFUSAL)] {
        let install = Install::new(locale, true);
        for args in [
            vec!["update", "--version", "0.0.0-desktop-test"],
            vec!["update", "--force", "--version", "0.0.0-desktop-test"],
        ] {
            let error = install.cli_error(&args);
            if built_as_sidecar() {
                assert!(error.contains(refusal), "{error}");
                assert!(!error.contains("failed to reach GitHub"), "{error}");
            } else {
                assert!(error.contains("failed to reach GitHub"), "{error}");
                assert!(!error.contains(refusal), "{error}");
            }
        }
        // A sidecar copied out of its package and a separately built kernel
        // both keep their existing update path, even under Desktop supervision.
        std::fs::remove_file(&install.app).unwrap();
        let error = install.cli_error(&["update", "--force", "--version", "0.0.0-desktop-test"]);
        assert!(error.contains("failed to reach GitHub"), "{error}");
        assert!(!error.contains(refusal), "{error}");
        assert!(!install.binary.with_extension("bak").exists());
    }
}

#[test]
fn desktop_update_read_only_check_remains_available_in_package() {
    let install = Install::new("en", true);
    let error = install.cli_error(&[
        "update",
        "--check",
        "--json",
        "--version",
        "0.0.0-desktop-test",
    ]);
    assert!(error.contains("failed to reach GitHub"), "{error}");
    assert!(!error.contains(DESKTOP_REFUSAL), "{error}");
}

struct Gateway(Child);

impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_gateway(install: &Install, port: u16, log: &Path) -> Gateway {
    Gateway(
        install
            .command()
            .args(["gateway", "start", "--host", "127.0.0.1", "--port"])
            .arg(port.to_string())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(log).unwrap())
            .spawn()
            .unwrap(),
    )
}

#[test]
fn desktop_update_http_enforces_current_ownership_and_config_gate() {
    for allow_self_upgrade in [false, true] {
        let install = Install::new("fr", allow_self_upgrade);
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let log = install.config.join("gateway.log");
        let mut gateway = start_gateway(&install, port, &log);
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let base = format!("http://127.0.0.1:{port}");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if client.get(format!("{base}/health")).send().is_ok() {
                break;
            }
            assert!(
                gateway.0.try_wait().unwrap().is_none() && Instant::now() < deadline,
                "gateway failed to start: {}",
                std::fs::read_to_string(&log).unwrap()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let status = || {
            client
                .get(format!("{base}/api/status"))
                .send()
                .unwrap()
                .error_for_status()
                .unwrap()
                .json::<serde_json::Value>()
                .unwrap()
        };
        assert_eq!(status()["desktop_bundled"], built_as_sidecar());
        let upgrade = || {
            client
                .post(format!("{base}/api/version/upgrade"))
                .json(&serde_json::json!({"version": "0.0.0-desktop-test"}))
                .send()
                .unwrap()
        };
        let response = upgrade();
        if !allow_self_upgrade || built_as_sidecar() {
            assert_eq!(response.status(), 403);
            let error = response.json::<serde_json::Value>().unwrap();
            if allow_self_upgrade {
                assert_eq!(error["error"], FRENCH_REFUSAL);
            } else {
                assert!(
                    error["error"]
                        .as_str()
                        .unwrap()
                        .contains("gateway.allow_self_upgrade")
                );
            }
        } else {
            assert_eq!(response.status(), 202);
        }
        // The gateway must re-resolve ownership, not retain the startup result.
        std::fs::remove_file(&install.app).unwrap();
        assert_eq!(status()["desktop_bundled"], false);
        if allow_self_upgrade && built_as_sidecar() {
            assert_eq!(upgrade().status(), 202);
        }
        if allow_self_upgrade {
            let deadline = Instant::now() + Duration::from_secs(25);
            loop {
                let progress = client
                    .get(format!("{base}/api/version/upgrade/status"))
                    .send()
                    .unwrap()
                    .json::<serde_json::Value>()
                    .unwrap();
                if progress["state"] == "failed" {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "upgrade child did not settle: {progress}"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        std::fs::write(&install.app, b"desktop package fixture").unwrap();
        assert_eq!(status()["desktop_bundled"], built_as_sidecar());
    }
}
