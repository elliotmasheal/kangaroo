//! Isolation from any already-installed ZeroClaw on this machine.
//!
//! The machine may carry an official ZeroClaw 0.8.7 install whose state
//! lives in `~/.zeroclaw`. This product (袋鼠 / Kangaroo) defaults to its own
//! data directory, its own gateway port, and its own binary name, and never
//! reads or writes the installed ZeroClaw's state. The default data root can
//! still be overridden per invocation (`--config-dir` at the CLI wiring
//! milestone); the constants here are only fallbacks.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use zeroclaw_config::schema::Config;

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

/// One containment check: `path` must resolve inside the product data root.
fn assert_under_root(root: &Path, label: &str, path: &Path) -> Result<()> {
    if path.starts_with(root) {
        Ok(())
    } else {
        bail!(
            "isolation violation: {label} ({}) escapes the product data root ({})",
            path.display(),
            root.display()
        );
    }
}

/// Hard-constraint scan: no `.zeroclaw` reference may survive anywhere in
/// the resolved configuration. This is a serialized-text scan, so it also
/// catches defaults and operator entries a field-by-field check would miss.
/// The serialized form is only held in memory and never printed.
pub fn scan_resolved_config_for_zeroclaw(config: &Config) -> Result<()> {
    let serialized =
        serde_json::to_string(config).context("isolation scan: config serialization failed")?;
    if serialized.contains(INSTALLED_ZEROCLAW_DIR_NAME) {
        bail!(
            "isolation violation: resolved configuration still references an installed ZeroClaw path ({INSTALLED_ZEROCLAW_DIR_NAME}); the product must only address its own data root"
        );
    }
    Ok(())
}

/// The startup assertion (wiring milestone hard constraint): after the
/// runtime config resolves, every path it names must land under the
/// product's own data root and nothing may reference an installed
/// ZeroClaw's `.zeroclaw`. A violation is a bug — some bootstrap default,
/// environment value, or operator entry still points at ZeroClaw state —
/// and must fail startup, never fall back.
pub fn assert_startup_isolation(config: &Config, root: &Path) -> Result<()> {
    for key in ["ZEROCLAW_CONFIG_DIR", "ZEROCLAW_DATA_DIR"] {
        if let Some(value) = std::env::var_os(key) {
            assert_under_root(root, key, &PathBuf::from(value))?;
        }
    }
    let install_root = config.install_root_dir();
    assert_under_root(root, "resolved config install root", &install_root)?;
    scan_resolved_config_for_zeroclaw(config)
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

    #[test]
    fn under_root_containment_accepts_inside_and_rejects_outside() {
        let base = std::env::temp_dir();
        let root = base.join("kangaroo-isolation-root-a");
        let outside = base.join("kangaroo-isolation-root-b");
        assert_under_root(&root, "probe", &root.join("config")).expect("inside root is contained");
        assert_under_root(&root, "probe", &root).expect("the root itself is contained");
        let error = assert_under_root(&root, "probe", &outside)
            .expect_err("a sibling path must be rejected");
        assert!(error.to_string().contains("isolation violation"));
    }

    #[test]
    fn scan_fails_when_any_resolved_field_references_the_installed_zeroclaw() {
        let mut config = Config::default();
        config.knowledge.db_path = "C:\\Users\\someone\\.zeroclaw\\knowledge.db".to_string();
        let error = scan_resolved_config_for_zeroclaw(&config)
            .expect_err("a .zeroclaw reference must fail the scan");
        assert!(
            error.to_string().contains(".zeroclaw"),
            "error must name the offending reference: {error}"
        );
    }

    #[test]
    fn scan_fails_on_a_tilde_zeroclaw_default_left_in_the_config() {
        let mut config = Config::default();
        config.plugins.plugins_dir = "~/.zeroclaw/plugins".to_string();
        assert!(scan_resolved_config_for_zeroclaw(&config).is_err());
    }
}
