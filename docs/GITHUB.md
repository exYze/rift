# GitHub integration

Comment `/rift <task>` on an issue or PR and a rift agent works the task,
pushes a branch, opens a PR, and comments the result back — all on
infrastructure you control. Where other tools route this through a hosted
GitHub App, rift's version is a single self-hosted Actions workflow you
install, read, and own: your runner, your model server, your token.

## Install

```sh
cd your-repo
rift github install     # writes .github/workflows/rift.yml
git add .github/workflows/rift.yml && git commit -m "add rift workflow" && git push
```

The command refuses outside a git repo, and asks before overwriting an
existing `rift.yml` (in a non-interactive shell it refuses instead).

## Setup

The workflow needs three things:

1. **A runner that can reach your model server.** That usually means a
   [self-hosted runner](https://docs.github.com/en/actions/hosting-your-own-runners)
   on the same network as your Ollama/vLLM box (repo Settings → Actions →
   Runners). The template's `runs-on: self-hosted` assumes this. If your
   model server is reachable from the internet, you can switch it to
   `ubuntu-latest` instead — but think twice before exposing an inference
   server that far.
2. **The `RIFT_HOST` secret** — the model server URL *as seen from the
   runner*:

   ```sh
   gh secret set RIFT_HOST --body http://10.0.0.5:11434
   ```

   Optionally pin a model with a repo variable (otherwise rift's default
   applies):

   ```sh
   gh variable set RIFT_MODEL --body qwen3.6:35b
   ```

3. **Runner tools**: `git`, `curl`, `gh`, and `jq` on the runner's PATH.
   GitHub-hosted images have all four; a minimal self-hosted box may need
   `gh` and `jq` installed.

## Usage

Comment on any issue or PR:

```
/rift fix the flaky test in tests/session.rs — it assumes the files land in mtime order
```

The workflow:

- installs the latest rift release via `install.sh`,
- builds the task from your comment (minus the `/rift` prefix) plus the
  issue/PR title and body as context,
- runs it headless (`rift -p … --output-format json`) against `RIFT_HOST`,
- if the working tree changed: pushes a `rift/issue-<n>` branch, opens a PR,
  and comments the PR link plus the agent's summary on the issue,
- if nothing changed: comments the agent's reply alone (so `/rift why does
  X happen?` works as a question, too).

Re-running `/rift` on the same issue force-pushes the branch and reuses the
open PR.

## Security notes

- **Only maintainers can trigger it.** The job's `if:` gate checks
  `github.event.comment.author_association` against `OWNER` / `MEMBER` /
  `COLLABORATOR` — a drive-by `/rift` comment from anyone without write
  access never starts the job. Don't remove that gate: the agent runs with
  a repo-writable token, on your runner.
- The workflow uses the ephemeral `GITHUB_TOKEN` with explicitly scoped
  `permissions:` (contents/issues/pull-requests write) — no PAT to mint or
  rotate.
- The agent runs headless with no approval prompts, so its guardrails are
  the ones any headless rift run has (built-in bash deny list, plus
  whatever a `.rift.json` in the repo tightens). Treat the runner like you
  would any CI box that runs repo code.
- Everything that executes is in the committed YAML — audit it once,
  and diff it like any other code when it changes.
