use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};

fn report_command() -> (tempfile::TempDir, Command) {
    let install = tempfile::tempdir().unwrap();
    std::fs::write(install.path().join("config.toml"), "locale = \"en\"\n").unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_zeroclaw"));
    command
        .env_clear()
        .env("ZEROCLAW_CONFIG_DIR", install.path())
        .env("ZEROCLAW_DATA_DIR", install.path().join("data"))
        .env("RUST_LOG", "off")
        .current_dir(install.path())
        .arg("--config-dir")
        .arg(install.path())
        .arg("markdown-help")
        .stdin(Stdio::null());
    (install, command)
}

#[test]
fn report_exits_with_sigpipe_when_stdout_reader_is_already_closed() {
    let (_install, mut command) = report_command();
    let (reader, writer) = std::io::pipe().unwrap();
    // Close before spawning so even a single write must observe the broken pipe.
    // A short report piped into `head` can instead fit before its reader exits.
    drop(reader);
    let output = command.stdout(writer).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.signal(), Some(libc::SIGPIPE), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(!stderr.contains("Broken pipe"), "{stderr}");
}

#[test]
fn report_succeeds_when_stdout_reader_stays_open() {
    let (_install, mut command) = report_command();
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("zeroclaw"));
}
