//! Native process-boundary proof. The test executable doubles as a harmless
//! kernel stand-in. Each platform runs its real process-group or suspended
//! job/pipe implementation; a macOS result is not Windows evidence.

#[path = "support/owned_kernel.rs"]
mod owned_kernel;

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use zeroclaw_desktop::ownership::OwnedProcesses;

fn main() {
    owned_kernel::run_if_fixture();
    native_processes();
}

fn native_processes() {
    let root = std::env::temp_dir();
    let grace = Duration::from_secs(2);
    let fixture = owned_kernel::Fixture::new(&root);
    let owned = OwnedProcesses::default();
    owned.launch(&fixture.binary, 1).unwrap();
    let child_pid = fixture.pid("descendant.pid");
    let external_fixture = owned_kernel::Fixture::new(&root);
    let mut external = Command::new(&external_fixture.binary)
        .arg("--external")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert!(owned.quit(grace).is_empty());
    assert!(fixture.dir.join("graceful-stop").exists());
    owned_kernel::wait_stopped(child_pid);
    assert!(
        external.try_wait().unwrap().is_none(),
        "external process was terminated"
    );
    external.kill().unwrap();
    external.wait().unwrap();
    assert!(owned.launch(&fixture.binary, 1).is_err());
    println!("PASS graceful stop, descendant cleanup, external preservation, sealed admission");

    for explicit_no in [true, false] {
        let fixture = owned_kernel::Fixture::new(&root);
        let owned = OwnedProcesses::default();
        owned.launch(&fixture.binary, 1).unwrap();
        let pid = fixture.pid("supervisor.pid");
        if explicit_no {
            owned.keep_running();
        }
        drop(owned);
        std::thread::sleep(Duration::from_millis(250));
        assert!(
            owned_kernel::running(pid),
            "EOF/drop terminated keep-running work"
        );
        assert!(OwnedProcesses::default().quit(grace).is_empty());
        assert!(
            owned_kernel::running(pid),
            "new session adopted previous work"
        );
        std::fs::write(fixture.dir.join("stop"), b"test cleanup").unwrap();
        owned_kernel::wait_stopped(pid);
    }
    println!("PASS No, app-handle loss, EOF and previous-session preservation");

    let fixture = owned_kernel::Fixture::new(&root);
    let owned = OwnedProcesses::default();
    owned.launch(&fixture.binary, 2).unwrap();
    let child_pid = fixture.pid("descendant.pid");
    let started = Instant::now();
    assert!(owned.quit(Duration::from_millis(100)).is_empty());
    assert!(started.elapsed() < Duration::from_secs(6));
    assert!(!fixture.dir.join("graceful-stop").exists());
    owned_kernel::wait_stopped(child_pid);
    println!("PASS bounded force fallback reaches descendants");

    let fixture = owned_kernel::Fixture::new(&root);
    let owned = Arc::new(OwnedProcesses::default());
    let launching = Arc::clone(&owned);
    let binary = fixture.binary.clone();
    let launch = std::thread::spawn(move || launching.launch(&binary, 3));
    let child_pid = fixture.pid("descendant.pid");
    assert!(!owned.close_if_empty());
    assert!(owned.quit(grace).is_empty());
    assert!(launch.join().unwrap().is_err());
    owned_kernel::wait_stopped(child_pid);
    println!("PASS Quit during readiness retains and stops the owned job");

    for stop_owned in [true, false] {
        let fixture = owned_kernel::Fixture::new(&root);
        std::fs::write(fixture.dir.join("gate-probe"), b"hold probe").unwrap();
        let owned = Arc::new(OwnedProcesses::default());
        let launching = Arc::clone(&owned);
        let binary = fixture.binary.clone();
        let launch = std::thread::spawn(move || launching.launch(&binary, 1));
        fixture.wait_for("probe-started");
        assert!(!owned.close_if_empty(), "in-progress admission was lost");
        let gate = fixture.dir.join("gate-probe");
        if stop_owned {
            let release = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                std::fs::remove_file(gate).unwrap();
            });
            assert!(owned.quit(grace).is_empty());
            release.join().unwrap();
        } else {
            owned.keep_running();
            std::fs::remove_file(gate).unwrap();
        }
        assert!(launch.join().unwrap().is_err());
        assert!(!fixture.dir.join("supervisor.pid").exists());
    }
    let empty = OwnedProcesses::default();
    assert!(empty.close_if_empty());
    assert!(
        empty
            .launch(std::path::Path::new("must-not-be-probed"), 1)
            .is_err()
    );
    println!("PASS Yes/No during capability probe and atomic empty-registry exit");
}
