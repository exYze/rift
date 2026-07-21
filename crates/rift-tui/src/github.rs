//! `rift github install` — the local-first GitHub integration. Instead of a
//! hosted GitHub App, rift ships a self-hosted Actions workflow: maintainers
//! comment `/rift <task>` on an issue or PR, a runner they control works the
//! task headless against THEIR model server, and the result comes back as a
//! PR plus a comment. This module only writes the workflow file; everything
//! that runs later is plain, auditable YAML (see docs/GITHUB.md).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// The workflow `rift github install` writes. Kept readable and commented on
/// purpose — users are expected to audit it before committing it.
const WORKFLOW_YML: &str = r#"# Rift GitHub integration — installed by `rift github install`.
#
# Comment `/rift <task>` on an issue or PR and a rift agent picks it up on
# your runner, works the task headless against your own model server, and
# opens a PR with whatever it changed. Local-first: nothing here talks to a
# hosted service — the runner reaches YOUR server (secrets.RIFT_HOST).
#
# Setup (details in docs/GITHUB.md of the rift repo):
#   * secret   RIFT_HOST  — model server URL, e.g. http://10.0.0.5:11434
#   * variable RIFT_MODEL — optional model override
#   * a runner that can reach RIFT_HOST — usually self-hosted on the same
#     network as your Ollama/vLLM box; switch `runs-on` to a GitHub-hosted
#     label only if the server is reachable from the internet
#   * runner tools: git, curl, gh, jq
#
# Security: the job runs ONLY for comments whose author has write/admin
# access to this repo (OWNER / MEMBER / COLLABORATOR). Everyone else's
# `/rift` comment is ignored by the `if:` gate below — do not remove it.

name: rift

on:
  issue_comment:
    types: [created]

permissions:
  contents: write        # push the result branch
  issues: write          # comment the outcome back on the issue/PR
  pull-requests: write   # open the result PR

jobs:
  rift:
    # The gate: a `/rift` prefix AND a write/admin comment author.
    if: >-
      startsWith(github.event.comment.body, '/rift') &&
      contains(fromJSON('["OWNER", "MEMBER", "COLLABORATOR"]'),
               github.event.comment.author_association)
    runs-on: self-hosted
    steps:
      - uses: actions/checkout@v4

      - name: Install rift
        run: |
          curl -fsSL https://raw.githubusercontent.com/exYze/rift/master/install.sh | sh
          # install.sh targets ~/.local/bin, which fresh runners don't
          # always have on PATH.
          echo "$HOME/.local/bin" >> "$GITHUB_PATH"

      - name: Run the task
        env:
          GH_TOKEN: ${{ github.token }}
          RIFT_HOST: ${{ secrets.RIFT_HOST }}
          RIFT_MODEL: ${{ vars.RIFT_MODEL }}
          COMMENT_BODY: ${{ github.event.comment.body }}
          ISSUE: ${{ github.event.issue.number }}
        run: |
          # The task = the comment minus its `/rift` prefix, plus the
          # issue/PR title and body as context (the issues API answers for
          # pull requests too, so one path covers both).
          request="${COMMENT_BODY#/rift}"
          title="$(gh api "repos/$GITHUB_REPOSITORY/issues/$ISSUE" -q .title)"
          body="$(gh api "repos/$GITHUB_REPOSITORY/issues/$ISSUE" -q .body)"
          TASK="$(printf '%s\n\nContext — issue/PR #%s: %s\n\n%s' "$request" "$ISSUE" "$title" "$body")"
          # Headless run. The JSON result (final reply, tool stats) lands in
          # RUNNER_TEMP so it never dirties the working tree's diff.
          rift -p "$TASK" --output-format json --approve > "$RUNNER_TEMP/rift-result.json"

      - name: Open a PR and report back
        env:
          GH_TOKEN: ${{ github.token }}
          ISSUE: ${{ github.event.issue.number }}
        run: |
          reply="$(jq -r '.reply // empty' "$RUNNER_TEMP/rift-result.json")"
          comment="$RUNNER_TEMP/rift-comment.md"
          if [ -n "$(git status --porcelain)" ]; then
            branch="rift/issue-$ISSUE"
            git config user.name "rift-agent"
            git config user.email "rift-agent@users.noreply.github.com"
            git checkout -b "$branch"
            git add -A
            git commit -m "rift: work issue #$ISSUE"
            # Force push: re-running `/rift` on the same issue replaces the
            # previous attempt's branch.
            git push -u origin "$branch" --force
            printf 'Automated changes for #%s by [rift](https://github.com/exYze/rift).\n\n%s\n' \
              "$ISSUE" "$reply" > "$comment"
            # A rerun already has an open PR for this branch — reuse it.
            pr="$(gh pr create --head "$branch" --title "rift: issue #$ISSUE" --body-file "$comment" \
              || gh pr view "$branch" --json url -q .url)"
            printf 'rift opened %s\n\n%s\n' "$pr" "$reply" > "$comment"
          else
            printf 'rift finished without changing any files.\n\n%s\n' "$reply" > "$comment"
          fi
          gh issue comment "$ISSUE" --body-file "$comment"
"#;

/// `rift github install`: write `.github/workflows/rift.yml` at the repo
/// root and print the setup steps. Refuses outside a git repo; an existing
/// file gets a y/N prompt on a terminal and a refusal otherwise.
pub fn install() -> Result<()> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("running git — is it installed?")?;
    if !out.status.success() {
        bail!("not a git repository — run `rift github install` inside the repo the workflow should live in");
    }
    let root = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    let dir = root.join(".github").join("workflows");
    let path = dir.join("rift.yml");
    if path.exists() {
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() {
            bail!(
                "{} already exists — refusing to overwrite in a non-interactive run (delete it first to reinstall)",
                path.display()
            );
        }
        if !confirm_overwrite(&path) {
            println!("kept the existing {}", path.display());
            return Ok(());
        }
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(&path, WORKFLOW_YML).with_context(|| format!("writing {}", path.display()))?;
    println!(
        "installed {}\n\n\
         next steps:\n\
         \x20 1. review it (it's short and commented), then commit and push:\n\
         \x20      git add .github/workflows/rift.yml && git commit -m \"add rift workflow\" && git push\n\
         \x20 2. set the RIFT_HOST secret to your model server URL — it must be reachable FROM THE RUNNER:\n\
         \x20      gh secret set RIFT_HOST --body http://<your-model-box>:11434\n\
         \x20    optionally pin a model:\n\
         \x20      gh variable set RIFT_MODEL --body <model>\n\
         \x20 3. register a self-hosted runner that can reach that server\n\
         \x20    (repo Settings → Actions → Runners), or edit `runs-on:` if your\n\
         \x20    model server is reachable from GitHub-hosted runners\n\
         \x20 4. trigger it: comment `/rift <task>` on any issue or PR —\n\
         \x20    it runs only for commenters with write access\n\n\
         the agent pushes a `rift/issue-<n>` branch, opens a PR, and comments\n\
         the result back on the issue. Docs: https://github.com/exYze/rift/blob/master/docs/GITHUB.md",
        path.display()
    );
    Ok(())
}

/// y/N overwrite prompt — same plain-stdin style as the startup trust
/// prompts in main.rs (this runs before any TUI exists).
fn confirm_overwrite(path: &Path) -> bool {
    use std::io::Write;
    eprint!("{} already exists. Overwrite it with the current template? [y/N] ", path.display());
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "y" | "Y" | "yes" | "YES")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The security gate and the headless invocation are the load-bearing
    /// parts of the template — pin them so an edit can't silently drop them.
    #[test]
    fn workflow_keeps_its_guardrails() {
        assert!(WORKFLOW_YML.contains("issue_comment"));
        assert!(WORKFLOW_YML.contains("startsWith(github.event.comment.body, '/rift')"));
        assert!(WORKFLOW_YML.contains("github.event.comment.author_association"));
        for assoc in ["OWNER", "MEMBER", "COLLABORATOR"] {
            assert!(WORKFLOW_YML.contains(assoc), "missing {assoc} in the association gate");
        }
        assert!(WORKFLOW_YML.contains("--output-format json --approve"));
        assert!(WORKFLOW_YML.contains("secrets.RIFT_HOST"));
        assert!(WORKFLOW_YML.contains("vars.RIFT_MODEL"));
        assert!(WORKFLOW_YML.contains("rift/issue-$ISSUE"));
    }
}
