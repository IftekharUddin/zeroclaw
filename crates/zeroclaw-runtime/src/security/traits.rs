//! Sandbox trait for pluggable OS-level isolation.

use async_trait::async_trait;
use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;

#[async_trait]
pub trait Sandbox: Send + Sync {
    fn wrap_command(&self, cmd: &mut Command) -> std::io::Result<()>;

    /// Wrap a runtime-built shell command with its pre-resolution identity.
    ///
    /// Host sandboxes retain the resolved program in `cmd`. A container
    /// sandbox may instead use `shell_program` inside its image, without
    /// changing how its own host launcher is resolved. `None` means that the
    /// runtime does not expose a direct shell program.
    fn wrap_shell_command(
        &self,
        cmd: &mut Command,
        _shell_program: Option<&OsStr>,
    ) -> std::io::Result<()> {
        self.wrap_command(cmd)
    }

    /// Report a retained construction failure without probing backend availability.
    fn check_initialization(&self) -> std::io::Result<()> {
        Ok(())
    }

    /// Whether successful wrapping protects this read-only ordinary argument.
    ///
    /// Only actual grants overlapping this target matter. A writable descendant
    /// remains valid when the caller's current policy authorizes that subtree.
    /// Pass-through wrappers default closed; a backend name is not proof.
    fn protects_read_only_argument(
        &self,
        _resolved_argument: &Path,
        _write_subtree_allowed: &dyn Fn(&Path) -> bool,
    ) -> bool {
        false
    }

    fn is_available(&self) -> bool;

    /// Return the human-readable name of this sandbox backend.
    /// Used in logs and diagnostics to identify which isolation strategy is
    /// active (e.g., `"firejail"`, `"bubblewrap"`, `"none"`).
    fn name(&self) -> &str;

    /// Return a brief description of the isolation guarantees this sandbox provides.
    /// Displayed in status output and health checks so operators can verify
    /// the active security posture.
    fn description(&self) -> &str;

    /// Return a reason when this sandbox cannot preserve coding CLI semantics.
    ///
    /// Coding CLI tools need a writable validated working directory and selected
    /// environment values to reach the actual CLI process. Container-style or
    /// replacing wrappers that cannot preserve those semantics must fail closed
    /// instead of spawning a misleading partial sandbox.
    fn coding_cli_unsupported_reason(&self) -> Option<&'static str> {
        None
    }
}

/// Check only the writable intersection with the newly admitted argument.
/// Disjoint grants do not affect it; authorized writable children remain valid.
#[cfg(any(
    test,
    target_os = "macos",
    all(target_os = "linux", feature = "sandbox-landlock")
))]
pub(super) fn write_grant_preserves_argument(
    argument: &Path,
    grant: &Path,
    write_subtree_allowed: &dyn Fn(&Path) -> bool,
) -> bool {
    if argument.starts_with(grant) {
        write_subtree_allowed(argument)
    } else if grant.starts_with(argument) {
        write_subtree_allowed(grant)
    } else {
        true
    }
}

#[derive(Debug, Clone, Default)]
pub struct NoopSandbox;

impl Sandbox for NoopSandbox {
    fn wrap_command(&self, _cmd: &mut Command) -> std::io::Result<()> {
        // Pass through unchanged
        Ok(())
    }

    fn is_available(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "none"
    }

    fn description(&self) -> &str {
        "No sandboxing (application-layer security only)"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_grants_are_checked_only_at_the_argument_intersection() {
        let writable = Path::new("/shared/cache");
        let allowed = |root: &Path| root.starts_with(writable);
        assert!(write_grant_preserves_argument(
            Path::new("/shared"),
            writable,
            &allowed
        ));
        assert!(write_grant_preserves_argument(
            Path::new("/protected/file"),
            Path::new("/tmp"),
            &|_| false
        ));
        assert!(!write_grant_preserves_argument(
            Path::new("/tmp/shared/file"),
            Path::new("/tmp"),
            &|_| false
        ));
        assert!(!write_grant_preserves_argument(
            Path::new("/data/shared/docs/file"),
            Path::new("/data"),
            &|_| false
        ));
    }

    #[test]
    fn noop_sandbox_name() {
        assert_eq!(NoopSandbox.name(), "none");
    }

    #[test]
    fn noop_sandbox_is_always_available() {
        assert!(NoopSandbox.is_available());
        assert!(!NoopSandbox.protects_read_only_argument(Path::new("/shared"), &|_| true));
        assert!(NoopSandbox.check_initialization().is_ok());
    }

    #[test]
    fn noop_sandbox_wrap_command_is_noop() {
        let mut cmd = Command::new("echo");
        cmd.arg("test");
        let original_program = cmd.get_program().to_string_lossy().to_string();
        let original_args: Vec<String> = cmd
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();

        let sandbox = NoopSandbox;
        assert!(sandbox.wrap_command(&mut cmd).is_ok());

        // Command should be unchanged
        assert_eq!(cmd.get_program().to_string_lossy(), original_program);
        assert_eq!(
            cmd.get_args()
                .map(|s| s.to_string_lossy().to_string())
                .collect::<Vec<_>>(),
            original_args
        );
    }
}
