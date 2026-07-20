//! `/release-notes`: the newest CHANGELOG entry, embedded at compile time so
//! the running binary always shows exactly its own version's notes (no file
//! to find, works from `rift update`'s replaced binary too).

/// The whole changelog, baked into the binary.
const CHANGELOG: &str = include_str!("../../../CHANGELOG.md");

/// (heading, body lines) for the newest release — the first `## v…` section,
/// up to the next one. A pending `## Unreleased` section is skipped: it
/// describes work this binary's version doesn't claim yet. The heading is
/// returned without the `## ` marker (e.g. "v2.1.0 — 2026-07-10"); trailing
/// blank lines are trimmed.
pub fn latest() -> (String, Vec<String>) {
    latest_from(CHANGELOG)
}

fn latest_from(text: &str) -> (String, Vec<String>) {
    let mut lines = text.lines();
    let heading = lines
        .by_ref()
        .find_map(|l| l.strip_prefix("## ").filter(|h| h.starts_with('v')).map(str::to_string))
        .unwrap_or_else(|| format!("v{}", env!("CARGO_PKG_VERSION")));
    let mut body: Vec<String> = Vec::new();
    for line in lines {
        if line.starts_with("## ") {
            break; // next release — stop
        }
        body.push(line.to_string());
    }
    while body.last().is_some_and(|l| l.trim().is_empty()) {
        body.pop();
    }
    (heading.trim().to_string(), body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_only_the_newest_section() {
        let sample = "# Changelog\n\nintro\n\n## v2.1.0 — 2026-07-10\n\n- **A**: did a thing\n- **B**: did another\n\n## v2.0.0 — 2026-07-09\n\n- old stuff\n";
        let (heading, body) = latest_from(sample);
        assert_eq!(heading, "v2.1.0 — 2026-07-10");
        assert_eq!(body.first().unwrap(), "");
        assert!(body.iter().any(|l| l.contains("did a thing")));
        // The previous release must not leak in.
        assert!(!body.iter().any(|l| l.contains("old stuff")));
        // Trailing blank lines trimmed.
        assert!(!body.last().unwrap().is_empty());
    }

    #[test]
    fn unreleased_section_is_skipped() {
        let sample = "# Changelog\n\n## Unreleased\n\n- pending\n\n## v2.1.0 — 2026-07-10\n\n- shipped\n";
        let (heading, body) = latest_from(sample);
        assert_eq!(heading, "v2.1.0 — 2026-07-10");
        assert!(body.iter().any(|l| l.contains("shipped")));
        assert!(!body.iter().any(|l| l.contains("pending")));
    }

    #[test]
    fn the_real_changelog_parses_and_names_a_version() {
        let (heading, body) = latest();
        assert!(heading.starts_with('v'), "heading was {heading:?}");
        assert!(!body.is_empty());
    }
}
