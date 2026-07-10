//! Update check + self-update against GitHub Releases.
//!
//! The startup check is deliberately quiet: at most one network call per
//! 24h (cached in ~/.local/share/rift/update-check.json), a short timeout,
//! and every failure path is a silent no-op — a local-first tool must never
//! nag or stall because the machine is offline. RIFT_NO_UPDATE_CHECK=1
//! disables it entirely.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

const REPO: &str = "exYze/rift";
const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

fn release_target() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        _ => return None,
    })
}

/// "v0.2.0" / "0.2.0" → (0, 2, 0). None if it doesn't look like semver.
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut parts = v.trim().trim_start_matches('v').splitn(3, '.');
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        // Tolerate suffixes like "1-rc1" on the patch component.
        parts.next()?.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()?,
    ))
}

fn http() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(4))
        .user_agent(concat!("rift/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()
}

#[derive(Deserialize)]
struct LatestRelease {
    tag_name: String,
}

async fn fetch_latest_tag() -> Option<String> {
    let resp = http()?
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .send()
        .await
        .ok()?;
    let release: LatestRelease = resp.error_for_status().ok()?.json().await.ok()?;
    Some(release.tag_name)
}

#[derive(Serialize, Deserialize, Default)]
struct CheckCache {
    checked_at: u64,
    latest: String,
}

fn cache_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).ok()?;
    Some(PathBuf::from(home).join(".local/share/rift/update-check.json"))
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Cache-only variant: the newer version string if the last check (done by
/// a TUI/headless run within 24h) recorded one. Never touches the network —
/// `--version` and headless nudges must not stall an offline machine.
pub fn cached_newer(current: &str) -> Option<String> {
    if std::env::var_os("RIFT_NO_UPDATE_CHECK").is_some() {
        return None;
    }
    let cached: CheckCache =
        serde_json::from_str(&std::fs::read_to_string(cache_path()?).ok()?).ok()?;
    (parse_version(&cached.latest)? > parse_version(current)?)
        .then(|| cached.latest.trim_start_matches('v').to_string())
}

/// Returns the newer version string if one is available. Silent on every
/// failure; hits the network at most once per 24h.
pub async fn check_for_update(current: &str) -> Option<String> {
    if std::env::var_os("RIFT_NO_UPDATE_CHECK").is_some() {
        return None;
    }
    let path = cache_path()?;
    let cached: Option<CheckCache> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());

    let latest = match cached {
        Some(c) if now_secs().saturating_sub(c.checked_at) < CHECK_INTERVAL_SECS => c.latest,
        _ => {
            let tag = fetch_latest_tag().await?;
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(
                &path,
                serde_json::to_vec(&CheckCache { checked_at: now_secs(), latest: tag.clone() })
                    .unwrap_or_default(),
            );
            tag
        }
    };

    (parse_version(&latest)? > parse_version(current)?).then(|| latest.trim_start_matches('v').to_string())
}

/// Download the latest release binary for this platform and replace the
/// running executable (write-then-rename: atomic, and a fresh inode so
/// macOS's signature cache never sees a modified file).
pub async fn self_update(current: &str) -> Result<String> {
    let target = release_target()
        .with_context(|| format!("no prebuilt binary for {}-{}; update via cargo or the install script", std::env::consts::OS, std::env::consts::ARCH))?;

    let latest_tag = fetch_latest_tag().await.context("cannot reach GitHub to check the latest release")?;
    let latest = latest_tag.trim_start_matches('v').to_string();
    match (parse_version(&latest), parse_version(current)) {
        (Some(l), Some(c)) if l <= c => return Ok(format!("already up to date (v{current})")),
        _ => {}
    }

    // No printing here — this runs inside the TUI too (the /update command).
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let url = format!("https://github.com/{REPO}/releases/latest/download/rift-{target}{suffix}");
    let bytes = http()
        .context("http client")?
        .get(&url)
        .timeout(Duration::from_secs(120))
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("download failed (release v{latest} may still be building — retry in a minute)"))?
        .bytes()
        .await?;

    let exe = std::env::current_exe().context("cannot locate the running executable")?;
    let staged = exe.with_extension("new");
    std::fs::write(&staged, &bytes).with_context(|| format!("cannot write {}", staged.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }
    // Windows can't overwrite a running exe, but it CAN rename it away.
    #[cfg(windows)]
    {
        let old = exe.with_extension("old");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(&exe, &old).context("cannot move the running executable aside")?;
    }
    if let Err(e) = std::fs::rename(&staged, &exe) {
        bail!("cannot replace {}: {e} (is it in a directory you can write to?)", exe.display());
    }
    Ok(format!("updated v{current} → v{latest} ({})", exe.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parsing_and_ordering() {
        assert_eq!(parse_version("v0.2.0"), Some((0, 2, 0)));
        assert_eq!(parse_version("1.10.3"), Some((1, 10, 3)));
        assert_eq!(parse_version("v1.0.0-rc1"), Some((1, 0, 0)));
        assert!(parse_version("v0.10.0") > parse_version("v0.9.9"));
        assert!(parse_version("not a version").is_none());
    }
}
