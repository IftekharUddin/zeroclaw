//! macOS-specific native integrations.

#[cfg(target_os = "macos")]
pub mod permissions;

#[cfg(target_os = "macos")]
pub mod quit;
