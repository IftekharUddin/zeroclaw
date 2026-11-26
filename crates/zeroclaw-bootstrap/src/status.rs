//! Bootstrap status: what this host is, and what is already installed.
//!
//! Status never changes anything and never runs an install. Its one judgement
//! is the recommendation, and an existing binary whose identity cannot be
//! verified produces a repair recommendation rather than a silent replacement
//! or an execution.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use zeroclaw_dist::DistTarget;

use crate::error::BootstrapError;
use crate::fetch::sha256_file;
use crate::plan::HostEnv;
use crate::target;

/// What the launcher found where the binary would be installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinaryState {
    /// Nothing at the install path.
    Absent,
    /// A file exists and reported a version.
    Verified {
        /// Path of the existing binary.
        path: PathBuf,
        /// SHA-256 of the file on disk.
        digest: String,
        /// Version it reported.
        version: String,
    },
    /// A file exists but its identity could not be established. It is never
    /// executed further and never overwritten without a fresh approval.
    Unverifiable {
        /// Path of the existing file.
        path: PathBuf,
        /// SHA-256 of the file on disk, when it could be read.
        digest: Option<String>,
        /// Why the version could not be established.
        reason: String,
    },
}

/// The single next step in the entry-point route, as a stable machine-readable
/// token. Either the instance is present and the harness moves on to configure
/// it, or it must be installed first. The token is derived from the
/// [`Recommendation`]; it is never a second source of truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextAction {
    /// ZeroClaw is installed and verified: connect to the control server with
    /// `handoff` and configure the instance there.
    Configure,
    /// ZeroClaw is absent or unverifiable: `plan`, then `install`, then
    /// `handoff` — installation always needs a human-approved plan digest.
    Install,
}

impl NextAction {
    /// Stable lowercase token a harness can branch on without parsing prose.
    pub fn token(self) -> &'static str {
        match self {
            Self::Configure => "configure",
            Self::Install => "install",
        }
    }
}

/// What the launcher suggests the operator do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recommendation {
    /// No binary present; run `plan`.
    PlanInstall,
    /// A verified binary is present; `handoff` can proceed.
    ReadyForHandoff,
    /// A binary is present but unverifiable; run `plan` and review a repair.
    PlanRepair,
}

impl Recommendation {
    /// Label for status output. Each variant names the explicit two-branch
    /// entry route and its `configure` destination, so a reader (human or
    /// harness) sees not only the current state but the next step to reach the
    /// control surface.
    pub fn label(&self) -> &'static str {
        match self {
            Self::PlanInstall => {
                "ZeroClaw is not installed — run `plan`, then `install`, then `handoff` \
                 to install and configure this instance"
            }
            Self::ReadyForHandoff => {
                "ZeroClaw is installed — run `handoff` to connect to the control server \
                 and configure this instance"
            }
            Self::PlanRepair => {
                "a file exists at the install path but its version could not be verified — \
                 run `plan`, then `install`, then `handoff` to repair and configure this \
                 instance; nothing was executed or replaced"
            }
        }
    }

    /// The route's next step as a machine-readable token. A verified install is
    /// ready to configure; an absent or unverifiable one must be installed (or
    /// repaired) first. This is resolved from the recommendation, not stored.
    pub fn next_action(&self) -> NextAction {
        match self {
            Self::ReadyForHandoff => NextAction::Configure,
            Self::PlanInstall | Self::PlanRepair => NextAction::Install,
        }
    }
}

/// The full status report.
#[derive(Debug, Clone)]
pub struct BootstrapStatus {
    /// Triple this launcher was built for.
    pub host_triple: String,
    /// Registry entry, when the host is a published target.
    pub target: Option<&'static DistTarget>,
    /// Refusal text when the host is not a published target.
    pub unsupported: Option<String>,
    /// Directory the binary would be installed into, when derivable.
    pub install_dir: Option<PathBuf>,
    /// What was found there.
    pub binary: BinaryState,
    /// What to do next.
    pub recommendation: Recommendation,
}

/// How long a candidate binary may take to answer `--version`.
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the probe checks whether the candidate has exited.
const VERSION_PROBE_POLL: Duration = Duration::from_millis(20);

/// How long output the candidate already wrote may take to arrive once it has
/// exited. Only a descendant still holding the pipe open can exceed this.
const VERSION_OUTPUT_GRACE: Duration = Duration::from_secs(1);

/// The most `--version` output the probe keeps. Only the first line is ever
/// read; anything past this is drained and discarded.
const VERSION_OUTPUT_LIMIT: u64 = 64 * 1024;

/// Probes a candidate binary for its version by running `--version`.
///
/// A file that is not executable, exits non-zero, does not exit within the
/// probe timeout, or prints something that is not a ZeroClaw version banner
/// is reported as unverifiable. A candidate that overruns the timeout is
/// killed and reaped, so a hung binary cannot block `status` or `handoff`.
/// There is no fallback to trusting the file name.
pub fn probe_version(path: &Path) -> Result<String, String> {
    probe_version_within(path, VERSION_PROBE_TIMEOUT)
}

/// [`probe_version`] with the timeout as a parameter, so the bound itself can
/// be exercised without waiting out the production value.
fn probe_version_within(path: &Path, timeout: Duration) -> Result<String, String> {
    let mut child = Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("could not execute it: {err}"))?;

    // Stdout is collected on its own thread: the candidate can never stall on
    // a full pipe, and this thread never blocks on a read it cannot bound.
    let Some(mut stdout) = child.stdout.take() else {
        kill_and_reap(&mut child);
        return Err("could not capture `--version` output".to_string());
    };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let read = stdout
            .by_ref()
            .take(VERSION_OUTPUT_LIMIT)
            .read_to_end(&mut kept);
        let _ = tx.send(read.map(|_| kept));
        // Keep the pipe drained so a verbose candidate still exits on its own
        // terms; nothing past the limit is retained.
        let _ = std::io::copy(&mut stdout, &mut std::io::sink());
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(VERSION_PROBE_POLL),
            Ok(None) => {
                kill_and_reap(&mut child);
                return Err(format!("`--version` did not exit within {timeout:?}"));
            }
            Err(err) => {
                kill_and_reap(&mut child);
                return Err(format!("could not wait for `--version`: {err}"));
            }
        }
    };
    if !status.success() {
        return Err(format!("`--version` exited with {status}"));
    }

    let stdout = rx
        .recv_timeout(VERSION_OUTPUT_GRACE)
        .map_err(|_| "`--version` exited but left its output open".to_string())?
        .map_err(|err| format!("could not read `--version` output: {err}"))?;
    let stdout = String::from_utf8_lossy(&stdout);
    parse_version_banner(&stdout).ok_or_else(|| {
        format!(
            "`--version` printed no recognisable version banner (first line: {:?})",
            stdout.lines().next().unwrap_or("").trim()
        )
    })
}

/// Stops a probe process that overran its bound and collects its exit status
/// so it does not linger as a zombie.
fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Extracts the version from a `zeroclaw <version>` banner.
fn parse_version_banner(stdout: &str) -> Option<String> {
    let first = stdout.lines().next()?.trim();
    let mut fields = first.split_whitespace();
    let name = fields.next()?;
    if name != "zeroclaw" && name != "zeroclaw.exe" {
        return None;
    }
    let version = fields.next()?;
    if !version.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some(version.to_string())
}

/// Inspects a candidate install path without executing anything it cannot
/// first identify as a file.
pub fn inspect_binary(path: &Path) -> BinaryState {
    if !path.exists() {
        return BinaryState::Absent;
    }
    let digest = sha256_file(path).ok();
    match probe_version(path) {
        Ok(version) => BinaryState::Verified {
            path: path.to_path_buf(),
            digest: digest.unwrap_or_default(),
            version,
        },
        Err(reason) => BinaryState::Unverifiable {
            path: path.to_path_buf(),
            digest,
            reason,
        },
    }
}

/// Builds the status report for a host.
pub fn status(env: &HostEnv, host_triple: &str) -> BootstrapStatus {
    let (target, unsupported) = match target::resolve(host_triple) {
        Ok(target) => (Some(target), None),
        Err(err) => (None, Some(err.to_string())),
    };

    let install_dir = target.and_then(|t| env.install_dir(t.family).ok());
    let binary = match (target, install_dir.as_ref()) {
        (Some(target), Some(dir)) => inspect_binary(&dir.join(target.binary_name)),
        _ => BinaryState::Absent,
    };

    let recommendation = match &binary {
        BinaryState::Absent => Recommendation::PlanInstall,
        BinaryState::Verified { .. } => Recommendation::ReadyForHandoff,
        BinaryState::Unverifiable { .. } => Recommendation::PlanRepair,
    };

    BootstrapStatus {
        host_triple: host_triple.to_string(),
        target,
        unsupported,
        install_dir,
        binary,
        recommendation,
    }
}

impl BootstrapStatus {
    /// Human-readable status.
    pub fn render(&self) -> String {
        let mut out = String::from("Bootstrap status\n");
        out.push_str(&format!("  host target       {}\n", self.host_triple));
        match self.target {
            Some(target) => out.push_str(&format!(
                "  published         yes ({}, {})\n  artifact          {}\n",
                target::family_label(target.family),
                target::tier_label(target.tier),
                target::asset_name(target),
            )),
            None => out.push_str("  published         no\n"),
        }
        if let Some(reason) = &self.unsupported {
            out.push_str(&format!("  refusal           {reason}\n"));
        }
        match &self.install_dir {
            Some(dir) => out.push_str(&format!("  install dir       {}\n", dir.display())),
            None => out.push_str("  install dir       not derivable on this host\n"),
        }
        match &self.binary {
            BinaryState::Absent => out.push_str("  existing binary   none\n"),
            BinaryState::Verified {
                path,
                digest,
                version,
            } => out.push_str(&format!(
                "  existing binary   {}\n  version           {version} (verified)\n  \
                 sha256            {digest}\n",
                path.display()
            )),
            BinaryState::Unverifiable {
                path,
                digest,
                reason,
            } => out.push_str(&format!(
                "  existing binary   {}\n  version           UNVERIFIABLE — {reason}\n  \
                 sha256            {}\n",
                path.display(),
                digest.as_deref().unwrap_or("unreadable"),
            )),
        }
        out.push_str(&format!(
            "\n  next action       {}\n  {}\n",
            self.recommendation.next_action().token(),
            self.recommendation.label()
        ));
        out
    }
}

/// Reads the version of an already-installed binary, refusing when it cannot
/// be established. Used by `handoff` before it trusts an executable.
pub fn require_verified_version(path: &Path) -> Result<(String, String), BootstrapError> {
    match inspect_binary(path) {
        BinaryState::Verified {
            version, digest, ..
        } => Ok((version, digest)),
        BinaryState::Absent => Err(BootstrapError::HandoffTargetUnusable {
            path: path.to_path_buf(),
            reason: "no file at the install path".to_string(),
        }),
        BinaryState::Unverifiable { reason, .. } => Err(BootstrapError::HandoffTargetUnusable {
            path: path.to_path_buf(),
            reason,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_version_banner() {
        assert_eq!(
            parse_version_banner("zeroclaw 0.8.4\n").as_deref(),
            Some("0.8.4")
        );
        assert_eq!(
            parse_version_banner("zeroclaw.exe 1.0.0-rc.1\nextra\n").as_deref(),
            Some("1.0.0-rc.1")
        );
    }

    #[test]
    fn refuses_a_banner_from_another_program() {
        assert!(parse_version_banner("git version 2.4.0").is_none());
        assert!(parse_version_banner("zeroclaw").is_none());
        assert!(parse_version_banner("zeroclaw notaversion").is_none());
        assert!(parse_version_banner("").is_none());
    }

    #[test]
    fn absent_path_is_absent() {
        assert_eq!(
            inspect_binary(Path::new("/nonexistent/zeroclaw-bootstrap-test")),
            BinaryState::Absent
        );
    }

    #[test]
    fn the_next_action_routes_installed_to_configure_and_absent_to_install() {
        assert_eq!(
            Recommendation::ReadyForHandoff.next_action(),
            NextAction::Configure
        );
        assert_eq!(
            Recommendation::ReadyForHandoff.next_action().token(),
            "configure"
        );
        // Absent and unverifiable both install (or repair) before configuring.
        assert_eq!(
            Recommendation::PlanInstall.next_action(),
            NextAction::Install
        );
        assert_eq!(
            Recommendation::PlanRepair.next_action(),
            NextAction::Install
        );
        assert_eq!(Recommendation::PlanInstall.next_action().token(), "install");
    }

    #[test]
    fn every_label_names_configure_and_its_route() {
        // Installed: the destination is named and reached through `handoff`.
        let installed = Recommendation::ReadyForHandoff.label();
        assert!(installed.contains("installed"));
        assert!(installed.contains("handoff"));
        assert!(installed.contains("configure"));

        // Absent: the full plan -> install -> handoff -> configure route.
        let absent = Recommendation::PlanInstall.label();
        assert!(absent.contains("not installed"));
        assert!(absent.contains("plan"));
        assert!(absent.contains("install"));
        assert!(absent.contains("handoff"));
        assert!(absent.contains("configure"));
    }

    /// Serialises the tests that write a script and then run it. A process
    /// forked by one test would otherwise briefly inherit the other's
    /// still-open script, and executing that script fails with ETXTBSY.
    #[cfg(unix)]
    static SCRIPT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[cfg(unix)]
    fn write_script(dir: &Path, contents: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join("zeroclaw");
        std::fs::write(&path, contents).expect("fixture script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("fixture script mode");
        path
    }

    #[cfg(unix)]
    #[test]
    fn a_candidate_that_never_exits_is_unverifiable_within_the_timeout() {
        let _guard = SCRIPT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("temp dir");
        // Far longer than the test could tolerate if the probe were unbounded.
        let path = write_script(dir.path(), "#!/bin/sh\nexec sleep 600\n");

        let started = Instant::now();
        let outcome = probe_version_within(&path, Duration::from_millis(300));
        let elapsed = started.elapsed();

        let reason = outcome.expect_err("a hung candidate must not be verified");
        assert!(
            reason.contains("did not exit within"),
            "reason was: {reason}"
        );
        assert!(
            elapsed >= Duration::from_millis(300),
            "the probe gave up after {elapsed:?}, before its timeout"
        );
        assert!(
            elapsed < Duration::from_secs(60),
            "the probe took {elapsed:?}; it must give up at its timeout, not wait for the candidate"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_candidate_that_exits_but_leaves_its_output_open_is_unverifiable() {
        let _guard = SCRIPT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("temp dir");
        // The banner is valid and the exit is clean, but a background child
        // keeps the pipe open, so the output never reaches end-of-file.
        let path = write_script(
            dir.path(),
            "#!/bin/sh\necho 'zeroclaw 0.8.4'\nsleep 20 &\nexit 0\n",
        );

        let started = Instant::now();
        let outcome = probe_version_within(&path, Duration::from_secs(30));
        let elapsed = started.elapsed();

        let reason = outcome.expect_err("unterminated output must not be verified");
        assert!(
            reason.contains("left its output open"),
            "reason was: {reason}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "the probe took {elapsed:?}; it must not wait for the pipe to close"
        );
    }
}
