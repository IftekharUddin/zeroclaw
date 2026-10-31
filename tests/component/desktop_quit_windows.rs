//! Real Windows supervisor -> daemon shutdown boundary, isolated from user
//! configuration. Desktop's retained outer job is tested by owned_processes.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        // The real supervisor owns an inner kill-on-close job. Terminating our
        // retained child handle also closes that job if a test fails.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start() -> (tempfile::TempDir, Running, u16) {
    let config = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    std::fs::write(config.path().join("config.toml"), format!(
        "schema_version = 3\n[gateway]\nport = {port}\nhost = \"127.0.0.1\"\nrequire_pairing = false\ncheck_updates = false\n"
    )).unwrap();
    let log = std::fs::File::create(config.path().join("supervisor.stderr")).unwrap();
    let mut child = Running(
        Command::new(env!("CARGO_BIN_EXE_zeroclaw"))
            .arg("--config-dir")
            .arg(config.path())
            .args(["service", "run-desktop-daemon", "--port"])
            .arg(port.to_string())
            .env("ZEROCLAW_CONFIG_DIR", config.path())
            .env("RUST_LOG", "info")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
        let _ = tx.send(result);
    });
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(15))
            .unwrap()
            .unwrap()
            .trim(),
        "READY"
    );
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !client
        .get(format!("http://127.0.0.1:{port}/health"))
        .send()
        .is_ok_and(|response| response.status().is_success())
    {
        assert!(
            child.0.try_wait().unwrap().is_none() && Instant::now() < deadline,
            "daemon did not become healthy; logs: {:?}",
            std::fs::read_to_string(config.path().join("logs/zeroclaw-desktop-daemon.log"))
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    (config, child, port)
}

#[test]
fn desktop_quit_windows_explicit_byte_gracefully_stops_real_daemon_without_restart() {
    let (config, mut child, port) = start();
    child.0.stdin.as_mut().unwrap().write_all(&[0x03]).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success(), "supervisor exited with {status}");
            break;
        }
        assert!(Instant::now() < deadline, "desktop shutdown did not finish");
        std::thread::sleep(Duration::from_millis(50));
    }
    let log =
        std::fs::read_to_string(config.path().join("logs/zeroclaw-desktop-daemon.log")).unwrap();
    assert!(log.contains("Desktop requested graceful shutdown"), "{log}");
    assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err());
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", port)).is_err(),
        "daemon restarted after Quit"
    );
}

#[test]
fn desktop_quit_windows_pipe_eof_keeps_real_daemon_running() {
    let (_config, mut child, port) = start();
    drop(child.0.stdin.take());
    std::thread::sleep(Duration::from_millis(300));
    assert!(child.0.try_wait().unwrap().is_none());
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    assert!(
        client
            .get(format!("http://127.0.0.1:{port}/health"))
            .send()
            .unwrap()
            .status()
            .is_success()
    );
}
