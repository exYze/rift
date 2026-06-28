//! Cross-platform user-directory resolution.
//!
//! `HOME` is the primary source, but on Windows it is usually unset — the OS
//! exposes `USERPROFILE` instead. Without this fallback the session store, the
//! user config file, and user-level skills all silently break (or fail to
//! start) on a stock Windows box. The same `HOME` → `USERPROFILE` fallback
//! already lives in rift-tui's update-cache path; this is the shared version.

use std::path::PathBuf;

/// The user's home directory: `HOME`, else `USERPROFILE` (Windows). Returns
/// `None` only if neither is set to a non-empty value.
pub fn home_dir() -> Option<PathBuf> {
    for var in ["HOME", "USERPROFILE"] {
        if let Some(val) = std::env::var_os(var) {
            if !val.is_empty() {
                return Some(PathBuf::from(val));
            }
        }
    }
    None
}

/// The user-level config directory: `$XDG_CONFIG_HOME`, else `<home>/.config`.
pub fn config_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg));
        }
    }
    home_dir().map(|h| h.join(".config"))
}

/// The user-level data directory: `<home>/.local/share`. Mirrors the existing
/// session/update-cache layout (which never consulted `$XDG_DATA_HOME`), so the
/// only behavior change here is the `HOME` → `USERPROFILE` fallback inside
/// [`home_dir`].
pub fn data_dir() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".local/share"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_dirs_resolve_on_this_platform() {
        // CI runners set HOME (Unix) or USERPROFILE (Windows); either way these
        // must resolve. On a stock Windows runner HOME is unset, so this is the
        // test that actually exercises the USERPROFILE fallback.
        assert!(home_dir().is_some(), "home_dir must resolve via HOME or USERPROFILE");
        assert!(config_dir().is_some(), "config_dir must resolve");
        assert!(data_dir().is_some(), "data_dir must resolve");
    }
}
