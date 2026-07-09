//! Granular permission rules: `Tool(pattern)` entries in three lists —
//! allow / ask / deny — evaluated at tool time with precedence
//! **deny > ask > allow > the session's approval mode**.
//!
//! ```json
//! {
//!   "permissions": {
//!     "allow": ["Bash(git status *)", "Edit(src/**)"],
//!     "ask":   ["Bash(git push *)"],
//!     "deny":  ["Read(~/.ssh/**)", "Bash(docker push *)", "Fetch(*://*.internal/*)"]
//!   }
//! }
//! ```
//!
//! Rule anatomy: a tool name, optionally with a glob in parentheses scoping
//! it to that tool's *specifier* — the shell command for `Bash`, the file
//! path for file tools, the URL for `Fetch`. A bare tool name (`Edit`)
//! matches every use of the tool. Tool names are case-insensitive, and two
//! act as families the way users think about them:
//!   - `Edit(...)`  covers the edit AND write tools (both mutate files)
//!   - `Read(...)`  covers read, ls, grep, glob and outline (all read files)
//!
//! `Write(...)` alone scopes to just the write tool.
//!
//! Semantics per list:
//!   - **deny**: the action is refused outright — even in /yolo mode, even
//!     headless. Denied reads/fetches error before touching the resource.
//!   - **ask**: the action always prompts, even when approval mode is off —
//!     the way to keep /yolo fast but gate the few things that matter. In a
//!     run with no interactive user, an ask rule denies (it demands a human).
//!   - **allow**: the action skips the approval prompt when approval mode is
//!     on. Loaded from the USER config only — a project `.rift.json` can add
//!     deny and ask rules (tighten) but never allow (loosen).
//!
//! Path patterns match the file's cwd-relative path and its absolute path
//! (both forward-slashed), with `~/` expanded to the home directory; `*`
//! stays within a directory, `**` crosses. Bash patterns keep the flat glob
//! semantics of the legacy `bash_allow`/`bash_deny` lists (which still load,
//! folded in as `Bash(...)` rules).

use std::path::Path;

use crate::config::Permissions;

/// What a matching rule says to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask,
    Deny,
}

/// One parsed `Tool(pattern)` rule.
#[derive(Debug, Clone)]
pub struct Rule {
    /// Lowercased tool name ("bash", "edit", ...).
    tool: String,
    /// The raw pattern between the parens; None = bare tool name.
    pattern: Option<String>,
    /// Compiled matcher for `pattern`.
    matcher: Option<globset::GlobMatcher>,
    /// The rule as written, for display and error messages.
    pub raw: String,
    /// Built-in rules (the hard bash deny list) are enforced but hidden from
    /// the user-rule listings.
    builtin: bool,
}

/// Tools whose specifier is a filesystem path (matched rel + abs with
/// path-aware globbing); everything else matches flat (bash commands, URLs).
fn is_path_tool(tool: &str) -> bool {
    matches!(tool, "edit" | "write" | "read" | "ls" | "grep" | "glob" | "outline")
}

/// Does a rule written for `rule_tool` govern an invocation of `tool`?
/// Families mirror how users think: `Edit` covers both mutating file tools,
/// `Read` covers every file-reading tool.
fn rule_applies(rule_tool: &str, tool: &str) -> bool {
    match rule_tool {
        "edit" => matches!(tool, "edit" | "write"),
        "read" => matches!(tool, "read" | "ls" | "grep" | "glob" | "outline"),
        other => other == tool,
    }
}

impl Rule {
    /// Parse `"Tool(pattern)"` or `"Tool"`. Returns None for empty/garbage
    /// (unbalanced parens, empty tool name) — callers surface a warning.
    pub fn parse(raw: &str) -> Option<Rule> {
        let raw_trim = raw.trim();
        if raw_trim.is_empty() {
            return None;
        }
        let (tool, pattern) = match raw_trim.find('(') {
            Some(open) => {
                if !raw_trim.ends_with(')') || open == 0 {
                    return None;
                }
                let pat = raw_trim[open + 1..raw_trim.len() - 1].trim();
                if pat.is_empty() {
                    return None;
                }
                (raw_trim[..open].trim(), Some(pat.to_string()))
            }
            None => (raw_trim, None),
        };
        if tool.is_empty() || !tool.chars().all(|c| c.is_alphanumeric() || c == '_') {
            return None;
        }
        let tool = tool.to_ascii_lowercase();
        let matcher = match &pattern {
            Some(pat) => {
                // A pattern whose parens don't balance is a mis-parse
                // (`Bash(git *) extra)` would otherwise become a rule that
                // silently never matches) — reject it so the caller warns.
                let mut depth = 0i32;
                for c in pat.chars() {
                    depth += match c {
                        '(' => 1,
                        ')' => -1,
                        _ => 0,
                    };
                    if depth < 0 {
                        return None;
                    }
                }
                if depth != 0 {
                    return None;
                }
                let expanded = expand_home(pat);
                let glob = globset::GlobBuilder::new(&expanded)
                    // Path tools get real path semantics (`*` stays in one
                    // directory, `**` crosses); flat specifiers (commands,
                    // URLs) keep the legacy any-match `*`. Paths compare
                    // case-insensitively where the filesystem does — a rule
                    // must not be evadable by `SECRETS/key.pem`.
                    .literal_separator(is_path_tool(&tool))
                    .case_insensitive(is_path_tool(&tool) && cfg!(any(windows, target_os = "macos")))
                    .build()
                    .ok()?;
                Some(glob.compile_matcher())
            }
            None => None,
        };
        Some(Rule { tool, pattern, matcher, raw: raw_trim.to_string(), builtin: false })
    }

    /// Does this rule match an invocation of `tool` with `specifier`
    /// candidates (a path is offered both relative and absolute)?
    fn matches(&self, tool: &str, candidates: &[String]) -> bool {
        if !rule_applies(&self.tool, tool) {
            return false;
        }
        let Some(m) = &self.matcher else { return true }; // bare tool name
        candidates.iter().any(|c| {
            m.is_match(c.as_str())
                // "git push *" should also cover the bare "git push".
                || self.pattern.as_deref().and_then(|p| p.strip_suffix(" *")) == Some(c.as_str())
        })
    }
}

/// `~/` at the start of a pattern → the home directory, forward-slashed.
fn expand_home(pat: &str) -> String {
    if let Some(rest) = pat.strip_prefix("~/") {
        if let Some(home) = crate::paths::home_dir() {
            return format!("{}/{rest}", home.display().to_string().replace('\\', "/"));
        }
    }
    pat.to_string()
}

/// Resolve `.`/`..` components and drop Windows verbatim (`\\?\`) prefixes
/// WITHOUT touching the filesystem — the shape rules are written against.
/// `foo/../secrets/key.pem` must match `secrets/**`, not slip past it.
fn lexical_normalize(p: &Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                // Popping past the root is left as-is (can't resolve it
                // lexically); popping a normal component folds the `..`.
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::Prefix(prefix) => out.push(strip_verbatim(prefix.as_os_str())),
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `\\?\C:\x` → `C:\x`, `\\?\UNC\server\share` → `\\server\share`.
fn strip_verbatim(prefix: &std::ffi::OsStr) -> std::path::PathBuf {
    let s = prefix.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        std::path::PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        std::path::PathBuf::from(rest.to_string())
    } else {
        std::path::PathBuf::from(&*s)
    }
}

/// The canonical shape of a path for rule matching: symlinks resolved when
/// the file exists (a link into `~/.ssh` must not evade `Read(~/.ssh/**)`),
/// verbatim prefixes stripped, `.`/`..` folded. A path that doesn't exist
/// yet (a new file being written) canonicalizes its nearest existing
/// ancestor and re-attaches the rest — otherwise a symlinked cwd (macOS
/// /var → /private/var) would strand new files outside the relative rules.
fn canonical_for_match(p: &Path) -> std::path::PathBuf {
    let norm = lexical_normalize(p);
    let mut existing = norm.as_path();
    let mut tail: Vec<std::ffi::OsString> = vec![];
    loop {
        if let Ok(c) = std::fs::canonicalize(existing) {
            let mut out = lexical_normalize(&c);
            for name in tail.iter().rev() {
                out.push(name);
            }
            return out;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
                tail.push(name.to_os_string());
                existing = parent;
            }
            _ => return norm,
        }
    }
}

/// The candidate strings a path is matched under: cwd-relative (when under
/// cwd) and absolute, both normalized and forward-slashed.
pub fn path_candidates(path: &Path, cwd: &Path) -> Vec<String> {
    let path = canonical_for_match(path);
    let cwd = canonical_for_match(cwd);
    let mut out = Vec::with_capacity(2);
    if let Ok(rel) = path.strip_prefix(&cwd) {
        let rel = rel.display().to_string().replace('\\', "/");
        if !rel.is_empty() {
            out.push(rel);
        }
    }
    out.push(path.display().to_string().replace('\\', "/"));
    out
}

/// The compiled rule lists a `ToolCtx` consults. Rebuilt whole on startup
/// and /config reload; grown one rule at a time by "always allow".
#[derive(Debug, Default, Clone)]
pub struct RuleSet {
    allow: Vec<Rule>,
    ask: Vec<Rule>,
    deny: Vec<Rule>,
}

impl RuleSet {
    /// Compile from config. Legacy `bash_allow`/`bash_deny` glob lists fold
    /// in as `Bash(...)` rules; `builtin_deny` is the hard-coded shell deny
    /// list. Unparseable rules are skipped and reported in `warnings`.
    pub fn compile(perms: &Permissions, builtin_deny: &[&str], warnings: &mut Vec<String>) -> RuleSet {
        let mut set = RuleSet::default();
        let mut load = |dst: &mut Vec<Rule>, raw: &str, builtin: bool| match Rule::parse(raw) {
            Some(mut r) => {
                r.builtin = builtin;
                dst.push(r);
            }
            // Built-ins that don't compile as globs (the fork-bomb pattern's
            // stray `{`) were silently skipped by the old glob-set builder
            // too — only user-written rules deserve a warning.
            None if builtin => {}
            None => warnings.push(format!("ignoring malformed permission rule: {raw:?}")),
        };
        for pat in builtin_deny {
            load(&mut set.deny, &format!("Bash({pat})"), true);
        }
        for pat in &perms.bash_deny {
            load(&mut set.deny, &format!("Bash({pat})"), false);
        }
        for pat in &perms.bash_allow {
            load(&mut set.allow, &format!("Bash({pat})"), false);
        }
        for raw in &perms.deny {
            load(&mut set.deny, raw, false);
        }
        for raw in &perms.ask {
            load(&mut set.ask, raw, false);
        }
        for raw in &perms.allow {
            load(&mut set.allow, raw, false);
        }
        set
    }

    /// Add one allow rule at runtime (the "always allow" approval choice).
    pub fn add_allow(&mut self, raw: &str) {
        if self.allow.iter().any(|r| r.raw == raw) {
            return;
        }
        if let Some(rule) = Rule::parse(raw) {
            self.allow.push(rule);
        }
    }

    /// Deny > ask > allow. Returns the verdict and the text of the rule that
    /// decided it; None = no rule speaks, fall through to the approval mode.
    pub fn decide(&self, tool: &str, candidates: &[String]) -> Option<(Decision, String)> {
        for (list, decision) in
            [(&self.deny, Decision::Deny), (&self.ask, Decision::Ask), (&self.allow, Decision::Allow)]
        {
            if let Some(rule) = list.iter().find(|r| r.matches(tool, candidates)) {
                return Some((decision, rule.raw.clone()));
            }
        }
        None
    }

    /// Deny/ask matching for one bash segment (any-segment semantics — the
    /// caller splits the command).
    pub fn bash_deny_match(&self, segment: &str) -> Option<String> {
        let c = [segment.to_string()];
        self.deny.iter().find(|r| r.matches("bash", &c)).map(|r| r.raw.clone())
    }

    pub fn bash_ask_match(&self, segment: &str) -> Option<String> {
        let c = [segment.to_string()];
        self.ask.iter().find(|r| r.matches("bash", &c)).map(|r| r.raw.clone())
    }

    /// Is this one bash segment allow-listed? (The caller requires ALL
    /// segments to pass — `git status && curl evil` must still prompt.)
    pub fn bash_allow_match(&self, segment: &str) -> bool {
        let c = [segment.to_string()];
        self.allow.iter().any(|r| r.matches("bash", &c))
    }

    /// Are there any allow rules that could cover bash at all? (Fast path:
    /// skip segment splitting when the answer can only be "prompt".)
    pub fn has_bash_allow(&self) -> bool {
        self.allow.iter().any(|r| rule_applies(&r.tool, "bash"))
    }

    /// Convenience for file walkers: is reading `path` denied for `tool`?
    /// (Only deny matters on the read side — reads never prompt.)
    pub fn read_denied(&self, tool: &str, path: &Path, cwd: &Path) -> bool {
        matches!(self.decide(tool, &path_candidates(path, cwd)), Some((Decision::Deny, _)))
    }

    /// User-visible rules per list (built-ins excluded), for /permissions.
    pub fn user_rules(&self) -> (Vec<String>, Vec<String>, Vec<String>) {
        let show = |list: &[Rule]| list.iter().filter(|r| !r.builtin).map(|r| r.raw.clone()).collect();
        (show(&self.allow), show(&self.ask), show(&self.deny))
    }
}

/// Match candidates for a URL: as written, plus normalized — scheme and
/// host lowercased, default port stripped, bare authority given a trailing
/// slash — so `HTTPS://X.INTERNAL:443/a` can't evade `*://*.internal/*`.
pub fn url_candidates(url: &str) -> Vec<String> {
    let mut out = vec![url.to_string()];
    if let Some(norm) = normalize_url(url) {
        if !out.contains(&norm) {
            out.push(norm);
        }
    }
    out
}

fn normalize_url(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let (authority, path) = match rest.find(['/', '?', '#']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let (userinfo, hostport) = match authority.rfind('@') {
        Some(i) => (&authority[..=i], &authority[i + 1..]),
        None => ("", authority),
    };
    let (host, port) = match hostport.rfind(':') {
        // Only a trailing all-digit suffix is a port (an IPv6 literal's
        // colons sit inside `[...]`).
        Some(i)
            if !hostport[i..].contains(']')
                && !hostport[i + 1..].is_empty()
                && hostport[i + 1..].chars().all(|c| c.is_ascii_digit()) =>
        {
            (&hostport[..i], &hostport[i + 1..])
        }
        _ => (hostport, ""),
    };
    let host = host.to_ascii_lowercase();
    let default_port = matches!((scheme.as_str(), port), ("http", "80") | ("https", "443"));
    let port_part = if port.is_empty() || default_port { String::new() } else { format!(":{port}") };
    let path_part = if path.is_empty() { "/" } else { path };
    Some(format!("{scheme}://{userinfo}{host}{port_part}{path_part}"))
}

/// The persistent "always allow" rule offered for a file mutation: scoped to
/// the file's top-level directory (`Edit(crates/**)`) so one grant covers a
/// work area without covering the world; a file at the project root gets
/// its own rule (`Edit(Cargo.toml)`). A file OUTSIDE the cwd scopes to its
/// own directory (`Edit(C:/Users/x/notes/**)`) — never the drive root
/// (`Edit(C:/**)` would exempt everything from approval with one click).
/// Uses the `Edit` family so it covers write too.
pub fn suggest_edit_rule(path: &Path, cwd: &Path) -> String {
    let path = canonical_for_match(path);
    let cwd = canonical_for_match(cwd);
    if let Ok(rel) = path.strip_prefix(&cwd) {
        let rel = rel.display().to_string().replace('\\', "/");
        return match rel.split('/').next() {
            Some(top) if top != rel => format!("Edit({top}/**)"),
            _ => format!("Edit({rel})"),
        };
    }
    match path.parent().filter(|p| p.parent().is_some()) {
        Some(dir) => format!("Edit({}/**)", dir.display().to_string().replace('\\', "/")),
        // No usable parent (file at a filesystem root): just this file.
        None => format!("Edit({})", path.display().to_string().replace('\\', "/")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn perms(json: &str) -> Permissions {
        serde_json::from_str(json).unwrap()
    }

    fn compile(json: &str) -> RuleSet {
        let mut w = vec![];
        let set = RuleSet::compile(&perms(json), &[], &mut w);
        assert!(w.is_empty(), "unexpected warnings: {w:?}");
        set
    }

    #[test]
    fn parses_rules() {
        let r = Rule::parse("Bash(git commit *)").unwrap();
        assert_eq!(r.tool, "bash");
        assert_eq!(r.pattern.as_deref(), Some("git commit *"));
        let r = Rule::parse("Edit").unwrap();
        assert_eq!(r.tool, "edit");
        assert!(r.pattern.is_none());
        // Malformed shapes are rejected, not misread.
        assert!(Rule::parse("").is_none());
        assert!(Rule::parse("(src/**)").is_none());
        assert!(Rule::parse("Edit(src/**").is_none());
        assert!(Rule::parse("Edit()").is_none());
        assert!(Rule::parse("Edit(a) extra").is_none());
    }

    #[test]
    fn precedence_deny_over_ask_over_allow() {
        let set = compile(
            r#"{"allow": ["Bash(git *)"], "ask": ["Bash(git push *)"], "deny": ["Bash(git push --force *)"]}"#,
        );
        let d = |cmd: &str| set.decide("bash", &[cmd.to_string()]).map(|(d, _)| d);
        assert_eq!(d("git status"), Some(Decision::Allow));
        assert_eq!(d("git push origin main"), Some(Decision::Ask));
        assert_eq!(d("git push --force origin main"), Some(Decision::Deny));
        assert_eq!(d("cargo build"), None);
    }

    #[test]
    fn edit_family_covers_write_and_read_family_covers_readers() {
        let set = compile(r#"{"deny": ["Edit(src/**)", "Read(secrets/**)"]}"#);
        let candidates = vec!["src/main.rs".to_string()];
        assert!(set.decide("edit", &candidates).is_some());
        assert!(set.decide("write", &candidates).is_some());
        let secret = vec!["secrets/key.pem".to_string()];
        for tool in ["read", "ls", "grep", "glob", "outline"] {
            assert_eq!(set.decide(tool, &secret), Some((Decision::Deny, "Read(secrets/**)".into())));
        }
        // Write rules scope to write only.
        let set = compile(r#"{"deny": ["Write(src/**)"]}"#);
        assert!(set.decide("write", &candidates).is_some());
        assert!(set.decide("edit", &candidates).is_none());
    }

    #[test]
    fn path_globs_are_separator_aware() {
        let set = compile(r#"{"allow": ["Edit(src/**)", "Edit(*.md)"]}"#);
        let hit = |p: &str| set.decide("edit", &[p.to_string()]).is_some();
        assert!(hit("src/main.rs"));
        assert!(hit("src/deep/nested/mod.rs"));
        assert!(hit("README.md"));
        assert!(!hit("docs/guide.md")); // *.md does not cross directories
        assert!(!hit("srcs/main.rs"));
    }

    #[test]
    fn bare_tool_rule_matches_everything() {
        let set = compile(r#"{"ask": ["Fetch"]}"#);
        assert_eq!(
            set.decide("fetch", &["https://example.com".to_string()]),
            Some((Decision::Ask, "Fetch".into()))
        );
    }

    /// A scratch cwd with `src/main.rs` and `secrets/key.pem` inside it.
    fn scratch_cwd(tag: &str) -> PathBuf {
        let cwd = std::env::temp_dir().join(format!("rift-permtest-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(cwd.join("src")).unwrap();
        std::fs::create_dir_all(cwd.join("secrets")).unwrap();
        std::fs::write(cwd.join("src").join("main.rs"), "x").unwrap();
        std::fs::write(cwd.join("secrets").join("key.pem"), "k").unwrap();
        cwd
    }

    #[test]
    fn path_candidates_relative_and_absolute() {
        let cwd = scratch_cwd("cand");
        let c = path_candidates(&cwd.join("src").join("main.rs"), &cwd);
        assert!(c.contains(&"src/main.rs".to_string()), "candidates: {c:?}");
        assert!(c.iter().any(|s| s.ends_with("/src/main.rs") && s != "src/main.rs"), "candidates: {c:?}");
        // A file that doesn't exist yet (a new write) still gets its
        // relative candidate, even when the cwd resolves through a symlink.
        let c = path_candidates(&cwd.join("src").join("new.rs"), &cwd);
        assert!(c.contains(&"src/new.rs".to_string()), "candidates: {c:?}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn path_matching_survives_traversal_and_verbatim_tricks() {
        let cwd = scratch_cwd("tricks");
        let set = compile(r#"{"deny": ["Read(secrets/**)"]}"#);
        // `..` must resolve before matching — foo/../secrets IS secrets
        // (foo doesn't even exist; the fold is lexical).
        let c = path_candidates(&cwd.join("foo").join("..").join("secrets").join("key.pem"), &cwd);
        assert!(set.decide("read", &c).is_some(), "traversal evaded: {c:?}");
        // Windows verbatim prefix must not evade the rule.
        #[cfg(windows)]
        {
            let verbatim = PathBuf::from(format!(r"\\?\{}", cwd.join("secrets").join("key.pem").display()));
            let c = path_candidates(&verbatim, &cwd);
            assert!(set.decide("read", &c).is_some(), "verbatim evaded: {c:?}");
        }
        // Case tricks on case-insensitive filesystems.
        if cfg!(any(windows, target_os = "macos")) {
            let c = path_candidates(&cwd.join("SECRETS").join("Key.PEM"), &cwd);
            assert!(set.decide("read", &c).is_some(), "case evaded: {c:?}");
        }
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn url_candidates_normalize_scheme_host_and_port() {
        let set = compile(r#"{"deny": ["Fetch(*://*.internal/*)"]}"#);
        for url in
            ["https://X.INTERNAL/a", "HTTPS://build.internal:443/a", "http://x.internal:80/", "https://x.internal"]
        {
            let c = url_candidates(url);
            assert!(set.decide("fetch", &c).is_some(), "{url} evaded via {c:?}");
        }
        // Non-default ports survive; IPv6 literals don't lose their colons.
        assert!(url_candidates("http://host:8080/a").contains(&"http://host:8080/a".to_string()));
        assert!(url_candidates("http://[::1]:11434/x").contains(&"http://[::1]:11434/x".to_string()));
    }

    #[test]
    fn legacy_bash_lists_fold_in() {
        let mut w = vec![];
        let set = RuleSet::compile(
            &perms(r#"{"bash_allow": ["git status"], "bash_deny": ["docker push *"]}"#),
            &["sudo *"],
            &mut w,
        );
        assert!(set.bash_allow_match("git status"));
        assert_eq!(set.bash_deny_match("docker push registry"), Some("Bash(docker push *)".into()));
        assert_eq!(set.bash_deny_match("sudo whoami"), Some("Bash(sudo *)".into()));
        // Built-ins are enforced but not listed.
        let (_, _, deny) = set.user_rules();
        assert_eq!(deny, vec!["Bash(docker push *)".to_string()]);
    }

    #[test]
    fn bash_prefix_pattern_covers_bare_command() {
        let set = compile(r#"{"allow": ["Bash(git push *)"]}"#);
        assert!(set.bash_allow_match("git push"));
        assert!(set.bash_allow_match("git push origin main"));
        assert!(!set.bash_allow_match("git pushx"));
    }

    #[test]
    fn suggest_edit_rule_scopes_to_top_dir() {
        let cwd = scratch_cwd("suggest");
        assert_eq!(suggest_edit_rule(&cwd.join("src").join("main.rs"), &cwd), "Edit(src/**)");
        assert_eq!(suggest_edit_rule(&cwd.join("Cargo.toml"), &cwd), "Edit(Cargo.toml)");
        // Outside the cwd the suggestion scopes to the file's own directory —
        // never a drive-wide Edit(C:/**) that would exempt everything.
        let outside = std::env::temp_dir().join("rift-permtest-outside").join("notes").join("todo.md");
        let rule = suggest_edit_rule(&outside, &cwd);
        assert!(rule.ends_with("notes/**)"), "got: {rule}");
        assert!(!rule.contains(":/**") && rule != "Edit(/**)", "too broad: {rule}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn parse_rejects_unbalanced_pattern_parens() {
        // A trailing-garbage rule must be rejected loudly, not become a rule
        // that silently never matches.
        assert!(Rule::parse("Bash(git push *) (oops)").is_none());
        // Balanced parens inside a pattern stay legal.
        assert!(Rule::parse("Bash(echo (hi))").is_some());
    }

    #[test]
    fn malformed_rules_warn_and_are_skipped() {
        let mut w = vec![];
        let set = RuleSet::compile(&perms(r#"{"deny": ["Edit(", "Bash(rm *)"]}"#), &[], &mut w);
        assert_eq!(w.len(), 1);
        assert!(set.bash_deny_match("rm -rf x").is_some());
    }
}
