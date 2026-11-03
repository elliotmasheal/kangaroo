//! Isolation from any already-installed ZeroClaw on this machine.
//!
//! The machine may carry an official ZeroClaw 0.8.7 install whose state
//! lives in `~/.zeroclaw`. This product (袋鼠 / Kangaroo) defaults to its own
//! data directory, its own gateway port, and its own binary name, and never
//! reads or writes the installed ZeroClaw's state. The default data root can
//! still be overridden per invocation (`--config-dir` at the CLI wiring
//! milestone); the constants here are only fallbacks.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// The product's data directory name: `%USERPROFILE%\.kangaroo`. This MUST
/// never be the installed ZeroClaw's `.zeroclaw`.
pub const DEFAULT_DATA_DIR_NAME: &str = ".kangaroo";

/// The product's default gateway port: 42618. The installed ZeroClaw binds
/// 42617 by default; one step above it is memorable and collision-free, and
/// it remains config-overridable at the wiring milestone.
pub const DEFAULT_GATEWAY_PORT: u16 = 42_618;

/// The product binary name; distinct from the installed `zeroclaw`,
/// `zerocode`, and `zerorelay` binaries.
pub const BINARY_NAME: &str = "kangaroo";

/// The known data directory of an installed ZeroClaw; the default data root
/// must never resolve to it.
const INSTALLED_ZEROCLAW_DIR_NAME: &str = ".zeroclaw";

/// Home directory without extra dependencies: `USERPROFILE` on Windows,
/// `HOME` elsewhere. Returns `None` when neither is usable.
fn home_dir() -> Option<PathBuf> {
    for key in ["USERPROFILE", "HOME"] {
        if let Some(value) = std::env::var_os(key) {
            let candidate = PathBuf::from(value);
            if !candidate.as_os_str().is_empty() {
                return Some(candidate);
            }
        }
    }
    None
}

/// The product's default data root: `<home>/.kangaroo`. Never the installed
/// ZeroClaw's `~/.zeroclaw` — see [`isolation_holds`].
pub fn default_data_root() -> Result<PathBuf> {
    let home = home_dir().context("no usable home directory (USERPROFILE/HOME)")?;
    Ok(home.join(DEFAULT_DATA_DIR_NAME))
}

/// The verifiable isolation invariant (M1 gate): the default data root is
/// not the installed ZeroClaw data directory, and does not live inside it.
pub fn isolation_holds() -> bool {
    let Some(home) = home_dir() else {
        return false;
    };
    let installed = home.join(INSTALLED_ZEROCLAW_DIR_NAME);
    match default_data_root() {
        Ok(root) => root != installed && !root.starts_with(&installed),
        Err(_) => false,
    }
}

/// Test-time helper for wiring roots under a base directory.
pub fn data_root_under(base: &Path) -> PathBuf {
    base.join(DEFAULT_DATA_DIR_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_data_root_is_kangaroo_and_never_the_installed_zeroclaw_dir() {
        assert!(
            isolation_holds(),
            "default data root must not be ~/.zeroclaw"
        );
        let root = default_data_root().expect("home is available in test environments");
        assert_eq!(
            root.file_name().and_then(|name| name.to_str()),
            Some(DEFAULT_DATA_DIR_NAME)
        );
        assert_ne!(
            root.file_name().and_then(|name| name.to_str()),
            Some(INSTALLED_ZEROCLAW_DIR_NAME)
        );
    }

    #[test]
    fn gateway_port_avoids_the_installed_default() {
        assert_eq!(DEFAULT_GATEWAY_PORT, 42_618);
        assert_ne!(DEFAULT_GATEWAY_PORT, 42_617);
    }

    #[test]
    fn binary_name_does_not_collide_with_installed_binaries() {
        assert_eq!(BINARY_NAME, "kangaroo");
        assert_ne!(BINARY_NAME, "zeroclaw");
        assert_ne!(BINARY_NAME, "zerocode");
        assert_ne!(BINARY_NAME, "zerorelay");
    }
}
