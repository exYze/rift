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
/// Result of a self-update, so each surface can present it its own way: the
/// CLI paints a colored banner, the in-TUI `/update` shows a plain line (it
/// renders as transcript text, where raw ANSI would be garbage).
pub enum UpdateOutcome {
    Updated { from: String, to: String, path: PathBuf },
    AlreadyCurrent { version: String },
}

impl UpdateOutcome {
    pub fn did_update(&self) -> bool {
        matches!(self, UpdateOutcome::Updated { .. })
    }

    /// Plain, ANSI-free line for the in-TUI `/update` transcript.
    pub fn plain(&self) -> String {
        match self {
            UpdateOutcome::Updated { from, to, .. } => format!("rift updated · v{from} → v{to}"),
            UpdateOutcome::AlreadyCurrent { version } => format!("rift is already up to date (v{version})"),
        }
    }

    /// Colored, multi-line banner for the terminal `rift update` command.
    pub fn cli_banner(&self) -> String {
        match self {
            UpdateOutcome::Updated { from, to, path } => format!(
                "\n  \x1b[38;5;44m\x1b[1m✦ rift\x1b[0m updated   \x1b[2mv{from}\x1b[0m \x1b[38;5;44m→\x1b[0m \x1b[1;38;5;48mv{to}\x1b[0m\n\
                 \x1b[2m  ↳ {}\x1b[0m\n\
                 \x1b[2m  restart rift or your editor to run the new version\x1b[0m\n",
                path.display()
            ),
            UpdateOutcome::AlreadyCurrent { version } => {
                format!("\n  \x1b[1;38;5;48m✓\x1b[0m rift is already on the latest release \x1b[2m(v{version})\x1b[0m\n")
            }
        }
    }
}

/// First `<stem>.old` / `<stem>.old-2` / … path next to `exe` that doesn't
/// exist yet. Renaming the running exe must never land on an existing file:
/// a stale `.old` can still be mapped by a process running that image, and
/// Windows fails a replacing rename onto a mapped file with Access Denied.
#[cfg_attr(not(windows), allow(dead_code))]
fn next_free_old_path(exe: &std::path::Path) -> PathBuf {
    let mut n = 1;
    loop {
        let candidate = if n == 1 {
            exe.with_extension("old")
        } else {
            exe.with_extension(format!("old-{n}"))
        };
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

/// Best-effort cleanup of `.old*` binaries parked by earlier updates. Ones
/// still locked (a process is running that image) simply stay for a later
/// sweep — failures here must never fail the update.
#[cfg_attr(not(windows), allow(dead_code))]
fn sweep_stale_old_binaries(exe: &std::path::Path) {
    let (Some(dir), Some(stem)) = (exe.parent(), exe.file_stem().and_then(|s| s.to_str()))
    else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let is_old = name
            .strip_prefix(stem)
            .and_then(|rest| rest.strip_prefix(".old"))
            .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with('-'));
        if is_old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

pub async fn self_update(current: &str) -> Result<UpdateOutcome> {
    let target = release_target()
        .with_context(|| format!("no prebuilt binary for {}-{}; update via cargo or the install script", std::env::consts::OS, std::env::consts::ARCH))?;

    let latest_tag = fetch_latest_tag().await.context("cannot reach GitHub to check the latest release")?;
    let latest = latest_tag.trim_start_matches('v').to_string();
    match (parse_version(&latest), parse_version(current)) {
        (Some(l), Some(c)) if l <= c => {
            return Ok(UpdateOutcome::AlreadyCurrent { version: current.to_string() })
        }
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
        sweep_stale_old_binaries(&exe);
        // Never rename ONTO an existing .old: a leftover from a previous
        // update can still be mapped by a process running the old image
        // (e.g. a rift --serve the editor started before that update), and
        // replacing a mapped file fails with Access Denied. Park the
        // running exe at the first free name instead; the sweep on a later
        // run picks the leftovers up once they unlock.
        let old = next_free_old_path(&exe);
        std::fs::rename(&exe, &old).context("cannot move the running executable aside")?;
    }
    if let Err(e) = std::fs::rename(&staged, &exe) {
        bail!("cannot replace {}: {e} (is it in a directory you can write to?)", exe.display());
    }
    Ok(UpdateOutcome::Updated { from: current.to_string(), to: latest, path: exe })
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

    #[test]
    fn old_path_never_targets_existing_files_and_sweep_clears_leftovers() {
        let dir = std::env::temp_dir().join(format!("rift-update-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("rift.exe");
        std::fs::write(&exe, "current").unwrap();

        // Nothing parked yet: plain .old.
        assert_eq!(next_free_old_path(&exe), dir.join("rift.old"));
        // Leftovers from earlier updates (possibly still mapped by a running
        // process) must never be rename targets — pick the next free name.
        std::fs::write(dir.join("rift.old"), "v1").unwrap();
        assert_eq!(next_free_old_path(&exe), dir.join("rift.old-2"));
        std::fs::write(dir.join("rift.old-2"), "v2").unwrap();
        assert_eq!(next_free_old_path(&exe), dir.join("rift.old-3"));

        // The sweep clears unlocked leftovers — and only ours: the running
        // exe and unrelated files stay.
        std::fs::write(dir.join("rifter.old"), "other tool").unwrap();
        sweep_stale_old_binaries(&exe);
        assert!(!dir.join("rift.old").exists());
        assert!(!dir.join("rift.old-2").exists());
        assert!(exe.exists());
        assert!(dir.join("rifter.old").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_outcome_renders_plain_and_banner() {
        let up = UpdateOutcome::Updated {
            from: "2.3.0".into(),
            to: "2.4.0".into(),
            path: PathBuf::from("/usr/local/bin/rift"),
        };
        assert!(up.did_update());
        // The in-TUI line is plain: versions present, no escape codes.
        assert_eq!(up.plain(), "rift updated · v2.3.0 → v2.4.0");
        assert!(!up.plain().contains('\x1b'));
        // The CLI banner is colored and shows both versions and the path.
        let banner = up.cli_banner();
        assert!(banner.contains('\x1b'));
        assert!(banner.contains("v2.3.0") && banner.contains("v2.4.0"));
        assert!(banner.contains("/usr/local/bin/rift"));

        let cur = UpdateOutcome::AlreadyCurrent { version: "2.4.0".into() };
        assert!(!cur.did_update());
        assert_eq!(cur.plain(), "rift is already up to date (v2.4.0)");
        assert!(!cur.plain().contains('\x1b'));
    }
}
