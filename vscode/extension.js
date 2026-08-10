// Rift for VS Code — a sidebar chat (webview) backed by `rift --serve`, a
// JSON-lines protocol over stdio, plus the original integrated-terminal
// launcher. The chat view lives in its own activity-bar container, so VS
// Code lets you drag it to the secondary sidebar and keep the editor,
// explorer, and chat visible at once.
const vscode = require('vscode');
const cp = require('child_process');
const crypto = require('crypto');
const fs = require('fs');
const os = require('os');
const path = require('path');

function config() {
  return vscode.workspace.getConfiguration('rift');
}

/** rift's own user config (~/.config/rift/config.json, honoring
 *  XDG_CONFIG_HOME the way rift does) — read for defaults and the provider
 *  map so model discovery covers every server rift can reach. */
function riftConfigPath() {
  const base =
    process.env.XDG_CONFIG_HOME && process.env.XDG_CONFIG_HOME.length
      ? process.env.XDG_CONFIG_HOME
      : path.join(os.homedir(), '.config');
  return path.join(base, 'rift', 'config.json');
}

// ── Custom system prompt ────────────────────────────────────────────────────
// The settings panel's "system prompt" field reads/writes rift's own custom
// prompt file (~/.config/rift/prompts/custom.md) — the same file the TUI's
// /system save manages — so both frontends share one source of truth. The
// file is a prompt target with `match: *` frontmatter: it replaces rift's
// built-in prompt for every model; rift picks it up on (re)start.

function riftCustomPromptPath() {
  return path.join(path.dirname(riftConfigPath()), 'prompts', 'custom.md');
}

/** The saved custom prompt's body, frontmatter stripped ('' if none). */
function readCustomPrompt() {
  let text;
  try {
    text = fs.readFileSync(riftCustomPromptPath(), 'utf8');
  } catch {
    return '';
  }
  // Same shape rift parses: `---` frontmatter up to the next `\n---` line.
  const fm = text.match(/^---[^]*?\n---[^\n]*\n?/);
  return (fm ? text.slice(fm[0].length) : text).trim();
}

/** Write the custom prompt (empty/blank body deletes it — back to rift's
 *  built-in per-model prompts). */
function writeCustomPrompt(body) {
  const p = riftCustomPromptPath();
  if (!body || !body.trim()) {
    try {
      fs.unlinkSync(p);
    } catch {
      /* nothing saved */
    }
    return;
  }
  fs.mkdirSync(path.dirname(p), { recursive: true });
  fs.writeFileSync(p, `---\nfamily: custom\nmatch: *\n---\n${body.trim()}\n`);
}
// Model discovery and provider routing live in rift now (the `list_models`
// serve command) — one source of truth instead of a JS re-implementation
// reading rift's config and probing servers itself.

function workspaceRoot() {
  const folders = vscode.workspace.workspaceFolders;
  return folders && folders.length > 0 ? folders[0].uri : undefined;
}

/** Config-driven argv for launching rift (shared by chat server + terminal). */
function riftArgs() {
  const cfg = config();
  const args = [];
  const host = cfg.get('host');
  if (host) args.push('--host', host);
  const model = cfg.get('model');
  if (model) args.push('--model', model);
  const effort = cfg.get('effort');
  if (effort) args.push('--effort', effort);
  const numCtx = cfg.get('numCtx');
  if (typeof numCtx === 'number') args.push('--num-ctx', String(numCtx));
  const temp = cfg.get('temperature');
  if (typeof temp === 'number') args.push('--temp', String(temp));
  const iters = cfg.get('maxIterations');
  if (typeof iters === 'number') args.push('--max-iterations', String(iters));
  return args;
}

// ── Inline diff review ──────────────────────────────────────────────────────
// rift (with the edit_review capability negotiated at spawn) emits every
// proposed write/edit as {path, old, new} BEFORE touching disk. We show it
// as a native VS Code diff between two virtual documents and let the user
// accept/reject individual hunks via CodeLens; the assembled result goes
// back as an edit_decision and only then does rift write the file.

/** Reviewable segments for an edit_review event. rift ships its own
 *  authoritative hunking on the event (`segments`, rift ≥ 2.6.3) — use it
 *  verbatim. An older rift without it degrades to one whole-file hunk:
 *  review still works, just without per-hunk granularity. */
function reviewSegments(ev) {
  if (Array.isArray(ev.segments)) {
    return ev.segments.map((s) =>
      s.same
        ? { same: true, lines: s.lines || [] }
        : { same: false, oldLines: s.old || [], newLines: s.new || [] }
    );
  }
  return [{ same: false, oldLines: ev.old.split('\n'), newLines: ev.new.split('\n') }];
}

/** The right-hand document: unchanged text plus each hunk in its currently
 *  accepted (new) or rejected (old) form — so rejecting a hunk visibly
 *  removes its highlight from the diff. Also returns each hunk's line. */
function reviewPreview(r) {
  const lines = [];
  const hunkAt = [];
  let h = 0;
  for (const seg of r.segments) {
    if (seg.same) {
      lines.push(...seg.lines);
      continue;
    }
    hunkAt[h] = lines.length;
    lines.push(...(r.accepted[h] ? seg.newLines : seg.oldLines));
    h++;
  }
  return { text: lines.join('\n'), hunkAt };
}

const REVIEW_SCHEME = 'rift-review';

/** Command argument → review id: editor-title buttons pass the resource
 *  Uri, CodeLens/chat pass the id itself. */
function reviewIdFromArg(arg) {
  if (arg && typeof arg === 'object' && arg.scheme) {
    return arg.scheme === REVIEW_SCHEME ? arg.path.split('/')[1] : undefined;
  }
  return arg;
}

class DiffReviewer {
  constructor() {
    /** Pending reviews by String(id): {id, tool, rel, old, segments,
     *  accepted[], left, right, send, notify, done}. */
    this.reviews = new Map();
    this.contentEmitter = new vscode.EventEmitter();
    this.lensEmitter = new vscode.EventEmitter();
    /** TextDocumentContentProvider */
    this.onDidChange = this.contentEmitter.event;
    /** CodeLensProvider */
    this.onDidChangeCodeLenses = this.lensEmitter.event;
  }

  /** Track a new edit_review event. `send` writes an edit_decision command
   *  to rift; `notify` updates the chat card when the review resolves. */
  register(ev, send, notify) {
    const rel = vscode.workspace.asRelativePath(ev.path, false);
    const segments = reviewSegments(ev);
    // The basename keeps its extension so the diff gets real syntax
    // highlighting; the id makes the pair unique.
    const name = path.basename(ev.path);
    const left = vscode.Uri.from({ scheme: REVIEW_SCHEME, path: `/${ev.id}/orig/${name}` });
    const right = vscode.Uri.from({ scheme: REVIEW_SCHEME, path: `/${ev.id}/proposed/${name}` });
    const r = {
      id: ev.id,
      tool: ev.tool,
      rel,
      old: ev.old,
      segments,
      accepted: segments.filter((s) => !s.same).map(() => true),
      left,
      right,
      send,
      notify,
      done: false,
    };
    this.reviews.set(String(ev.id), r);
    const added = segments.reduce((t, s) => t + (s.same ? 0 : s.newLines.length), 0);
    const removed = segments.reduce((t, s) => t + (s.same ? 0 : s.oldLines.length), 0);
    return { hunks: r.accepted.length, added, removed };
  }

  get(id) {
    return this.reviews.get(String(id));
  }

  async show(id) {
    const r = this.get(id);
    if (!r) return;
    const n = r.accepted.length;
    await vscode.commands.executeCommand(
      'vscode.diff',
      r.left,
      r.right,
      `rift: ${r.rel} (${n} hunk${n === 1 ? '' : 's'})`,
      { preview: false }
    );
  }

  provideTextDocumentContent(uri) {
    const [, id, side] = uri.path.split('/');
    const r = this.reviews.get(id);
    if (!r) return '(review closed)';
    return side === 'orig' ? r.old : reviewPreview(r).text;
  }

  provideCodeLenses(doc) {
    const [, id, side] = doc.uri.path.split('/');
    if (side !== 'proposed') return [];
    const r = this.reviews.get(id);
    if (!r || r.done) return [];
    const { hunkAt } = reviewPreview(r);
    const total = r.accepted.length;
    const kept = r.accepted.filter(Boolean).length;
    const top = new vscode.Range(0, 0, 0, 0);
    const lenses = [
      new vscode.CodeLens(top, {
        title: kept
          ? `✔ rift: Apply ${kept}/${total} hunk${total === 1 ? '' : 's'}`
          : 'rift: nothing accepted — Apply rejects the edit',
        command: 'rift.reviewApply',
        arguments: [r.id],
        tooltip: 'Write the accepted hunks to the file; the agent continues from the result',
      }),
      new vscode.CodeLens(top, {
        title: '✘ Reject all',
        command: 'rift.reviewReject',
        arguments: [r.id],
        tooltip: 'Apply nothing — the agent is told the edit was denied',
      }),
    ];
    r.accepted.forEach((on, i) => {
      const line = Math.min(hunkAt[i] || 0, Math.max(doc.lineCount - 1, 0));
      lenses.push(
        new vscode.CodeLens(new vscode.Range(line, 0, line, 0), {
          title: on
            ? `✓ hunk ${i + 1}/${total} — click to reject`
            : `✗ hunk ${i + 1}/${total} rejected — click to restore`,
          command: 'rift.reviewToggle',
          arguments: [r.id, i],
        })
      );
    });
    return lenses;
  }

  toggle(id, i) {
    const r = this.get(id);
    if (!r || r.done) return;
    r.accepted[i] = !r.accepted[i];
    this.contentEmitter.fire(r.right);
    this.lensEmitter.fire();
  }

  apply(id) {
    const r = this.get(id);
    if (!r || r.done) return;
    const kept = r.accepted.filter(Boolean).length;
    if (!kept) return this.reject(id); // nothing accepted = a rejection
    r.send({ cmd: 'edit_decision', id: r.id, apply: true, content: reviewPreview(r).text });
    this.finish(r, `applied ${kept}/${r.accepted.length} hunk${r.accepted.length === 1 ? '' : 's'}`, true);
  }

  reject(id) {
    const r = this.get(id);
    if (!r || r.done) return;
    r.send({ cmd: 'edit_decision', id: r.id, apply: false });
    this.finish(r, 'rejected', false);
  }

  finish(r, verdict, applied) {
    r.done = true;
    this.reviews.delete(String(r.id));
    r.notify({ verdict, applied });
    this.lensEmitter.fire();
    this.closeTabs(r);
  }

  /** Close every diff tab showing this review. */
  async closeTabs(r) {
    const target = r.right.toString();
    for (const group of vscode.window.tabGroups.all) {
      for (const tab of group.tabs) {
        if (tab.input && tab.input.modified && tab.input.modified.toString() === target) {
          try {
            await vscode.window.tabGroups.close(tab);
          } catch {
            /* already gone */
          }
        }
      }
    }
  }

  /** Resolve one pending review without sending a decision — the turn was
   *  cancelled/finished, so rift already denied or abandoned the edit. */
  cancelOne(id, reason) {
    const r = this.get(id);
    if (!r) return;
    r.done = true;
    this.reviews.delete(String(r.id));
    r.notify({ verdict: reason, applied: false });
    this.closeTabs(r);
    this.lensEmitter.fire();
  }

  /** rift exited / session restarted: resolve every pending card, close tabs.
   *  No decision is sent — rift saw the channel drop and denied the edit. */
  cancelAll(reason) {
    for (const r of [...this.reviews.values()]) this.cancelOne(r.id, reason);
  }

  /** The review in the active tab — lets the editor-title ✓/✗ buttons work
   *  without arguments. */
  activeReviewId() {
    const tab = vscode.window.tabGroups.activeTabGroup && vscode.window.tabGroups.activeTabGroup.activeTab;
    const uri =
      tab && tab.input && tab.input.modified
        ? tab.input.modified
        : vscode.window.activeTextEditor && vscode.window.activeTextEditor.document.uri;
    if (!uri || uri.scheme !== REVIEW_SCHEME) return undefined;
    return uri.path.split('/')[1];
  }
}

// ── Sidebar chat ────────────────────────────────────────────────────────────

class RiftChatProvider {
  constructor(context, reviewer) {
    this.context = context;
    this.reviewer = reviewer;
    this.view = null;
    this.proc = null;
    this.busy = false;
    this.model = '';
    /** Replay log so the transcript survives the view being disposed (e.g.
     *  when dragged between primary and secondary sidebars). */
    this.log = [];
    /** Path of the session rift is currently in (from the `ready` event).
     *  Persisted per-workspace so a reload reopens the same chat instead of
     *  starting blank, and reused across model/setting restarts. */
    this.sessionPath = null;
    /** Set when a past-chats list was requested before rift was running —
     *  the request is sent once the `ready` handshake arrives. */
    this.pendingListSessions = false;
    /** Commands advertised by this rift's `ready` event — the feature-
     *  detection surface for list_models/set_model vs older binaries. */
    this.commands = [];
    this.stderrTail = [];
    this.stdoutBuf = '';
    /** Context-window occupancy from rift's `context` events: estimated
     *  tokens in the conversation vs the working num_ctx. 0/0 = unknown. */
    this.ctxUsed = 0;
    this.ctxLimit = 0;
    /** Approval mode as rift last reported it (ready / approval_changed):
     *  true = it pauses for approval, false = auto-approve. Mirrors rift's
     *  own naming; the `rift.autoApprove` setting is its inverse. Null
     *  until a process reports one. */
    this.approve = null;
  }

  resolveWebviewView(view) {
    this.view = view;
    view.webview.options = {
      enableScripts: true,
      localResourceRoots: [vscode.Uri.joinPath(this.context.extensionUri, 'media')],
    };
    view.webview.html = this.html(view.webview);
    view.webview.onDidReceiveMessage((m) => this.onWebviewMessage(m));
    view.onDidDispose(() => {
      if (this.view === view) this.view = null;
    });
  }

  html(webview) {
    const media = (f) =>
      webview.asWebviewUri(vscode.Uri.joinPath(this.context.extensionUri, 'media', f));
    const nonce = crypto.randomBytes(16).toString('base64');
    return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy"
      content="default-src 'none'; style-src ${webview.cspSource}; script-src 'nonce-${nonce}';">
<link rel="stylesheet" href="${media('chat.css')}">
</head>
<body>
  <div id="header">
    <span id="status">rift</span>
    <span id="ctx" class="hidden"></span>
    <span id="header-buttons">
      <button id="btn-undo" data-tip="Undo — revert the file edits from the last turn (changes made via bash are not tracked)">↶</button>
      <button id="btn-history" data-tip="Past chats — reopen a previous conversation">🕘</button>
      <button id="btn-new" data-tip="New session — clear this conversation and start fresh">＋</button>
      <button id="btn-continue" data-tip="Continue — restart rift and resume your last saved session">↻</button>
    </span>
  </div>
  <div id="messages"></div>
  <div id="settings" class="hidden">
    <div id="settings-head">
      <span>Settings</span>
      <button id="set-close" data-tip="Close without saving">✕</button>
    </div>
    <label data-tip="Path to the rift executable — leave empty if 'rift' is on your PATH">
      <span>rift binary</span><input id="set-bin" placeholder="rift">
    </label>
    <label data-tip="The model server rift talks to: an Ollama URL, or an OpenAI-style one ending in /v1 (vLLM). Empty = rift's own config">
      <span>server URL</span><input id="set-host" placeholder="http://localhost:11434 or http://host:8000/v1">
    </label>
    <label data-tip="Tokens of context requested per call (--num-ctx). On Ollama this sizes the server's KV cache">
      <span>context window</span><input id="set-numctx" type="number" min="1024" step="1024" placeholder="32768">
    </label>
    <label data-tip="Sampling temperature (--temp). Low values keep tool calling reliable">
      <span>temperature</span><input id="set-temp" type="number" min="0" max="2" step="0.1" placeholder="0.2">
    </label>
    <label data-tip="Max agent-loop iterations (tool calls) per turn (--max-iterations)">
      <span>max iterations</span><input id="set-iters" type="number" min="1" placeholder="40">
    </label>
    <label data-tip="How much thinking models reason before answering (--effort)">
      <span>reasoning effort</span>
      <select id="set-effort">
        <option value="">model default</option>
        <option value="minimal">minimal</option>
        <option value="low">low</option>
        <option value="medium">medium</option>
        <option value="high">high</option>
        <option value="xhigh">xhigh</option>
        <option value="max">max</option>
      </select>
    </label>
    <label class="check" data-tip="Auto-accept edits and shell commands instead of approving each one (the TUI's /yolo). Applied edits still show as diffs in the chat, and rift's permission rules still hold: 'deny' rules refuse and 'ask' rules prompt even here">
      <span>auto-approve</span>
      <span class="check-row"><input id="set-autoapprove" type="checkbox"><em>apply the agent's edits without asking</em></span>
    </label>
    <label class="stack" data-tip="Your own system prompt — replaces rift's built-in prompt for every model, every session. Shared with the TUI (/system save); stored in ~/.config/rift/prompts/custom.md. {cwd} and {shell} placeholders are filled at startup. Empty = rift's built-in per-model prompts">
      <span>system prompt</span>
      <textarea id="set-sysprompt" rows="6"
        placeholder="Empty = rift's built-in per-model prompts"></textarea>
    </label>
    <div class="row">
      <button id="set-save" data-tip="Save — applies immediately; a running session restarts and resumes">Save</button>
      <button id="set-config" class="secondary" data-tip="Providers (vLLM, cloud APIs), permissions, hooks and more live in rift's own config">Edit rift config file…</button>
    </div>
    <p class="settings-note">Empty fields fall back to rift's own config file, then its built-in defaults.</p>
  </div>
  <div id="composer">
    <div id="mention-popup" class="hidden"></div>
    <textarea id="input" rows="1"
      placeholder="Ask rift… (@file attaches, Enter sends, Shift+Enter newline)"></textarea>
    <button id="btn-send" data-tip="Send message (Enter)">➤</button>
    <button id="btn-stop" data-tip="Stop — cancel the turn in progress" class="hidden">■</button>
  </div>
  <div id="footer">
    <select id="model-select" data-tip="Model — switching keeps the current conversation"></select>
    <span>
      <button id="btn-approve" data-tip="Approval mode"><span class="ap-icon">🔒</span><span class="ap-label">approve</span></button>
      <button id="btn-refresh" data-tip="Refresh the model list from your servers">⟳</button>
      <button id="btn-settings" data-tip="Settings — binary path, server URL, reasoning effort">⚙</button>
    </span>
  </div>
  <script nonce="${nonce}" src="${media('highlight.min.js')}"></script>
  <script nonce="${nonce}" src="${media('chat.js')}"></script>
</body>
</html>`;
  }

  post(msg, record = true) {
    if (record) this.log.push(msg);
    if (this.view) this.view.webview.postMessage(msg);
  }

  postStatus() {
    const text = this.proc
      ? `${this.model || 'starting…'}${this.busy ? ' · working' : ''}`
      : 'not running — send a message to start';
    // Status is derived state, not history: never recorded for replay.
    // ctxUsed/ctxLimit drive the header's context gauge pill.
    if (this.view) {
      this.view.webview.postMessage({
        type: 'status',
        text,
        running: !!this.proc,
        busy: this.busy,
        ctxUsed: this.proc ? this.ctxUsed : 0,
        ctxLimit: this.proc ? this.ctxLimit : 0,
        skills: this.skills || [],
        // Exactly two states, and the setting is the single source of
        // truth: the toggle is applied at the `hello` handshake and again
        // on every change, so what the button says is what rift is doing.
        autoApprove: config().get('autoApprove') === true,
      });
    }
  }

  /** Turn auto-approve on/off: persist the choice and push it to the
   *  running process (no restart — it is one flag rift flips live).
   *  `on` omitted = toggle. Returns the state actually adopted.
   *
   *  Two states, no third: if the running rift is too old to switch, the
   *  change is refused outright rather than left pending, because a button
   *  claiming "auto" while rift still prompts is worse than no button. */
  setAutoApprove(on) {
    const cfg = config();
    const current = cfg.get('autoApprove') === true;
    const next = on === undefined ? !current : !!on;
    if (next === current) return current;
    if (this.proc && !this.commands.includes('set_approval')) {
      this.post({
        type: 'rift',
        ev: {
          event: 'warning',
          text: 'this rift is too old to switch approval mode — update rift (or start a new session) to use auto-approve',
        },
      });
      return current;
    }
    cfg.update('autoApprove', next, vscode.ConfigurationTarget.Global).then(() => {
      // rift's flag is the inverse: approve=true means "keep asking".
      if (this.proc) this.write({ cmd: 'set_approval', approve: !next });
      this.postStatus();
      this.postSettings();
    });
    return next;
  }

  ensureServer(extraArgs = []) {
    if (this.proc) return;
    const cfg = config();
    const bin = cfg.get('executablePath') || 'rift';
    const cwdUri = workspaceRoot();
    let proc;
    try {
      proc = cp.spawn(bin, ['--serve', ...riftArgs(), ...extraArgs], {
        cwd: cwdUri ? cwdUri.fsPath : undefined,
        stdio: ['pipe', 'pipe', 'pipe'],
      });
    } catch (e) {
      this.post({ type: 'rift', ev: { event: 'warning', text: `failed to launch ${bin}: ${e.message}` } });
      return;
    }
    this.proc = proc;
    this.stderrTail = [];
    this.stdoutBuf = '';
    proc.stdout.setEncoding('utf8');
    proc.stdout.on('data', (chunk) => {
      this.stdoutBuf += chunk;
      let nl;
      while ((nl = this.stdoutBuf.indexOf('\n')) >= 0) {
        const line = this.stdoutBuf.slice(0, nl);
        this.stdoutBuf = this.stdoutBuf.slice(nl + 1);
        if (!line.trim()) continue;
        let ev;
        try {
          ev = JSON.parse(line);
        } catch {
          continue;
        }
        this.onServerEvent(ev);
      }
    });
    proc.stderr.setEncoding('utf8');
    proc.stderr.on('data', (chunk) => {
      for (const l of chunk.split('\n')) {
        if (l.trim()) this.stderrTail.push(l);
      }
      this.stderrTail = this.stderrTail.slice(-15);
    });
    proc.on('error', (e) => {
      this.proc = null;
      this.busy = false;
      this.post({
        type: 'rift',
        ev: { event: 'warning', text: `could not start '${bin}': ${e.message} — set rift.executablePath` },
      });
      this.postStatus();
    });
    proc.on('exit', (code) => {
      this.proc = null;
      this.busy = false;
      this.commands = []; // capability set died with the process
      this.approve = null; // unknown again until the next ready
      this.reviewer.cancelAll('cancelled — rift exited');
      if (code !== 0 && code != null) {
        const tail = this.stderrTail.slice(-4).join('\n');
        this.post({ type: 'rift', ev: { event: 'warning', text: `rift exited (code ${code})${tail ? '\n' + tail : ''}` } });
      }
      this.postStatus();
    });
    // Capability handshake: opt into edit_review events so write/edit
    // proposals arrive as native diffs instead of plain approval prompts.
    // rift.inlineDiffReview=false skips the opt-in — approvals fall back to
    // the classic in-chat prompt with the diff as colored text.
    // `approve` rides along so the mode is set before anything can prompt
    // (older rifts ignore the field; `ready` then reconciles it).
    this.write({
      cmd: 'hello',
      edit_review: config().get('inlineDiffReview') !== false,
      approve: !(config().get('autoApprove') === true),
    });
    this.postStatus();
  }

  onServerEvent(ev) {
    if (ev.event === 'ready') {
      this.model = ev.model;
      if (ev.num_ctx) this.ctxLimit = ev.num_ctx;
      // Remember which session we're in so a reload reopens it, and so
      // model/setting restarts stay on the same chat instead of jumping.
      if (ev.session) {
        this.sessionPath = ev.session;
        this.context.workspaceState.update('rift.lastSession', ev.session);
      }
      // A history request that arrived before rift was up: answer it now.
      if (this.pendingListSessions) {
        this.pendingListSessions = false;
        this.write({ cmd: 'list_sessions' });
      }
      // Skills + plugin commands, completable in the webview as /skill:<name>.
      this.skills = ev.skills || [];
      this.commands = ev.commands || [];
      // Approval mode: the setting is the source of truth and already rode
      // in on `hello`. Reconcile anyway in case rift started before the
      // handshake landed — and if this build can't switch at all, say so
      // once and drop the setting, so the button never claims a mode rift
      // isn't actually in.
      this.approve = typeof ev.approve === 'boolean' ? ev.approve : null;
      const wantApprove = !(config().get('autoApprove') === true);
      if (this.commands.includes('set_approval')) {
        if (this.approve !== wantApprove) this.write({ cmd: 'set_approval', approve: wantApprove });
      } else if (!wantApprove) {
        config().update('autoApprove', false, vscode.ConfigurationTarget.Global);
        this.post({
          type: 'rift',
          ev: {
            event: 'warning',
            text: 'auto-approve turned off: this rift build does not support it — update rift to use it',
          },
        });
      }
      // rift owns model discovery: populate the dropdown from the running
      // process (provider routing included), not from HTTP probes of our own.
      if (this.commands.includes('list_models')) this.write({ cmd: 'list_models' });
      // This extension speaks serve protocol v1 (docs/SERVE.md). An absent
      // protocol_version means a pre-1.10 rift — same v1 wire shapes.
      if (ev.protocol_version && ev.protocol_version > 1) {
        vscode.window.showWarningMessage(
          `rift speaks serve protocol v${ev.protocol_version}; this extension expects v1 — update the extension`
        );
      }
    } else if (ev.event === 'done') {
      this.busy = false;
    } else if (ev.event === 'edit_review') {
      // Full file contents ride on this event — route it to the diff
      // reviewer and give the webview a slim card instead of the raw event.
      this.onEditReview(ev);
      return;
    } else if (ev.event === 'edit_review_closed') {
      // The turn ended/cancelled before a decision — resolve the card so a
      // stale Apply can't claim success for an edit that never happened.
      this.reviewer.cancelOne(ev.id, 'cancelled — the turn ended before a decision');
      return;
    } else if (ev.event === 'context') {
      // Derived state for the status line (and the webview gauge) — not
      // transcript history, so don't record/forward the raw event.
      this.ctxUsed = ev.used;
      this.ctxLimit = ev.limit;
      this.postStatus();
      return;
    } else if (ev.event === 'sessions') {
      // Answer to a list_sessions request — a native quick-pick, not
      // transcript, so handle it here and don't record/forward it.
      this.showSessionPicker(ev.items || []);
      return;
    } else if (ev.event === 'models') {
      // Answer to list_models: rift's reachable models, provider prefixes
      // included. Dropdown state, not transcript — don't record/forward.
      if (this.view) {
        this.view.webview.postMessage({ type: 'models', models: ev.models || [], current: ev.current || this.model });
      }
      return;
    } else if (ev.event === 'approval_changed') {
      // Ack for set_approval. Announced in the transcript because it
      // changes what happens to your files without further confirmation.
      this.approve = ev.approve;
      this.post({
        type: 'rift',
        ev: {
          event: 'info',
          text: ev.approve
            ? 'approval mode ON — rift asks before edits and shell commands'
            : "auto-approve ON — rift applies edits and runs commands without asking (deny/ask permission rules still hold)",
        },
      });
      this.postStatus();
      return;
    } else if (ev.event === 'model_changed') {
      // A live set_model landed: same chat, new model. The context event
      // that follows refreshes the gauge for any num_ctx change.
      this.model = ev.model;
      this.post({ type: 'rift', ev: { event: 'info', text: `switched to ${ev.model}${ev.note ? ' ' + ev.note : ''}` } });
      this.postStatus();
      return;
    }
    // Thinking/content deltas are high-volume; replaying them is what makes
    // the transcript reappear intact, so they are recorded like the rest.
    this.post({ type: 'rift', ev });
    this.postStatus();
  }

  onEditReview(ev) {
    const stats = this.reviewer.register(
      ev,
      (cmd) => this.write(cmd),
      (res) => this.post({ type: 'editReviewDone', id: ev.id, verdict: res.verdict, applied: res.applied })
    );
    this.post({
      type: 'editReview',
      id: ev.id,
      tool: ev.tool,
      path: vscode.workspace.asRelativePath(ev.path, false),
      hunks: stats.hunks,
      added: stats.added,
      removed: stats.removed,
    });
    this.reviewer.show(ev.id);
    this.postStatus();
  }

  write(cmd) {
    if (this.proc && this.proc.stdin.writable) {
      this.proc.stdin.write(JSON.stringify(cmd) + '\n');
    }
  }

  onWebviewMessage(m) {
    switch (m.type) {
      case 'ready': {
        // Fresh webview (first open, or re-created after a sidebar move):
        // replay everything it missed.
        for (const msg of this.log) this.view?.webview.postMessage(msg);
        this.postStatus();
        this.postSettings();
        this.postModels();
        // Reopen the last chat for this workspace so a VS Code reload or a
        // plugin update doesn't drop the conversation. Only when nothing is
        // running and nothing has replayed yet (a genuine cold open), and
        // only if this workspace has a saved session to return to.
        if (!this.proc && this.log.length === 0) {
          const last = this.context.workspaceState.get('rift.lastSession');
          if (last) {
            this.sessionPath = last;
            this.ensureServer(['--resume', last]);
          }
        }
        break;
      }
      case 'send': {
        this.ensureServer();
        this.post({ type: 'userEcho', text: m.text });
        this.busy = true;
        this.write({ cmd: 'prompt', text: m.text });
        this.postStatus();
        break;
      }
      case 'answer':
        this.write({ cmd: 'answer', id: m.id, text: m.text });
        break;
      case 'reviewAction': {
        // Buttons on the chat review card mirror the diff view's actions.
        if (!this.reviewer.get(m.id)) {
          this.post({ type: 'rift', ev: { event: 'warning', text: 'that edit review is no longer pending' } }, false);
          break;
        }
        if (m.action === 'open') this.reviewer.show(m.id);
        else if (m.action === 'apply') this.reviewer.apply(m.id);
        else if (m.action === 'reject') this.reviewer.reject(m.id);
        break;
      }
      case 'cancel':
        this.write({ cmd: 'cancel' });
        break;
      case 'undo':
        if (this.proc) this.write({ cmd: 'undo' });
        else this.post({ type: 'rift', ev: { event: 'warning', text: 'nothing to undo — rift is not running' } });
        break;
      case 'insertCode': {
        // The webview has focus when the button is clicked, so
        // activeTextEditor may be undefined — fall back to any visible one.
        const editor = vscode.window.activeTextEditor || vscode.window.visibleTextEditors[0];
        if (!editor) {
          vscode.window.showWarningMessage('rift: no open editor to insert into');
          break;
        }
        editor.edit((b) => {
          for (const sel of editor.selections) b.replace(sel, m.code);
        });
        break;
      }
      case 'newSession':
        this.sessionPath = null;
        this.restart([]);
        break;
      case 'continueSession':
        this.restart(['--continue']);
        break;
      case 'history':
        this.requestSessions();
        break;
      case 'refreshModels':
        this.postModels(true);
        break;
      case 'toggleAutoApprove':
        this.setAutoApprove(m.on);
        break;
      case 'queryFiles':
        this.postFiles(m.query, m.token);
        break;
      case 'setModel': {
        config()
          .update('model', m.model || undefined, vscode.ConfigurationTarget.Global)
          .then(() => {
            // Live switch on the running process — same conversation, no
            // respawn (rift preflights the target and swaps the client).
            // Only an older rift without set_model needs the restart path.
            if (this.proc && m.model && this.commands.includes('set_model')) {
              this.write({ cmd: 'set_model', model: m.model });
            } else if (this.proc) {
              this.restart(this.continueArgs());
            }
            this.postSettings();
          });
        break;
      }
      case 'saveSettings': {
        const cfg = config();
        // Number fields arrive as input strings; empty/garbage clears back
        // to rift's own default.
        const num = (v) => {
          const n = Number(v);
          return v !== '' && v != null && isFinite(n) ? n : undefined;
        };
        // The system prompt lives in rift's own prompts dir, not VS Code
        // settings — write it first so the restart below picks it up. Only
        // when the webview sent the field: an absent value (stale webview
        // JS) must not delete a prompt saved elsewhere.
        if (typeof m.systemPrompt === 'string') {
          try {
            writeCustomPrompt(m.systemPrompt);
          } catch (e) {
            vscode.window.showWarningMessage(`rift: could not save the system prompt: ${e.message}`);
          }
        }
        Promise.all([
          cfg.update('executablePath', m.executablePath || undefined, vscode.ConfigurationTarget.Global),
          cfg.update('host', m.host || undefined, vscode.ConfigurationTarget.Global),
          cfg.update('effort', m.effort || undefined, vscode.ConfigurationTarget.Global),
          cfg.update('numCtx', num(m.numCtx), vscode.ConfigurationTarget.Global),
          cfg.update('temperature', num(m.temperature), vscode.ConfigurationTarget.Global),
          cfg.update('maxIterations', num(m.maxIterations), vscode.ConfigurationTarget.Global),
          cfg.update('autoApprove', m.autoApprove === true, vscode.ConfigurationTarget.Global),
        ]).then(() => {
          if (this.proc) this.restart(this.continueArgs());
          this.postSettings();
          this.postModels();
        });
        break;
      }
      case 'openConfig': {
        const p = riftConfigPath();
        if (!fs.existsSync(p)) {
          fs.mkdirSync(path.dirname(p), { recursive: true });
          fs.writeFileSync(
            p,
            JSON.stringify(
              { providers: { vllm: { base_url: 'http://your-vllm-host:8000/v1' } } },
              null,
              2
            ) + '\n'
          );
        }
        vscode.window.showTextDocument(vscode.Uri.file(p));
        break;
      }
    }
  }

  postSettings() {
    const cfg = config();
    if (this.view) {
      this.view.webview.postMessage({
        type: 'settings',
        executablePath: cfg.get('executablePath') || '',
        host: cfg.get('host') || '',
        model: cfg.get('model') || '',
        effort: cfg.get('effort') || '',
        numCtx: cfg.get('numCtx') ?? '',
        temperature: cfg.get('temperature') ?? '',
        maxIterations: cfg.get('maxIterations') ?? '',
        systemPrompt: readCustomPrompt(),
        autoApprove: cfg.get('autoApprove') === true,
      });
    }
  }

  /** Workspace paths for @-mention completion: every file findFiles returns
   *  plus each ancestor directory, cached briefly so a burst of keystrokes
   *  runs one scan. */
  async fileIndex() {
    const now = Date.now();
    if (this.fileEntries && now - this.fileEntriesAt < 15000) return this.fileEntries;
    const exclude =
      '**/{node_modules,.git,target,dist,build,out,.next,__pycache__,.venv,venv,vendor}/**';
    let uris = [];
    try {
      uris = await vscode.workspace.findFiles('**/*', exclude, 5000);
    } catch {
      /* no workspace open */
    }
    const dirs = new Set();
    const entries = [];
    for (const u of uris) {
      const rel = vscode.workspace.asRelativePath(u, false);
      entries.push({ path: rel, dir: false });
      for (let i = rel.lastIndexOf('/'); i > 0; i = rel.lastIndexOf('/', i - 1)) {
        dirs.add(rel.slice(0, i));
      }
    }
    for (const d of dirs) entries.push({ path: d, dir: true });
    this.fileEntries = entries;
    this.fileEntriesAt = now;
    return entries;
  }

  async postFiles(query, token) {
    const entries = await this.fileIndex();
    const q = (query || '').toLowerCase();
    const scored = [];
    for (const e of entries) {
      const p = e.path.toLowerCase();
      const base = p.slice(p.lastIndexOf('/') + 1);
      let s = 0;
      if (!q) s = 1;
      else if (base === q) s = 6;
      else if (base.startsWith(q)) s = 5;
      else if (base.includes(q)) s = 4;
      else if (p.startsWith(q)) s = 3;
      else if (p.includes(q)) s = 2;
      if (s) scored.push({ e, s, depth: e.path.split('/').length });
    }
    scored.sort(
      (a, b) =>
        b.s - a.s || a.depth - b.depth || b.e.dir - a.e.dir || a.e.path.localeCompare(b.e.path)
    );
    if (this.view) {
      this.view.webview.postMessage({
        type: 'files',
        token,
        results: scored.slice(0, 50).map(({ e }) => e),
      });
    }
  }

  /** Populate the model dropdown. rift answers list_models with everything
   *  it can reach (default host + configured providers). Until a process is
   *  running the dropdown just shows the configured model; `spawn` (the
   *  explicit ⟳ button) starts rift, whose `ready` then requests the full
   *  list. A passive webview open never spawns a process. */
  postModels(spawn = false) {
    if (this.proc && this.commands.includes('list_models')) {
      this.write({ cmd: 'list_models' });
      return;
    }
    if (this.view) {
      const current = this.model || config().get('model') || '';
      this.view.webview.postMessage({
        type: 'models',
        models: current ? [current] : [],
        current,
      });
    }
    if (!this.proc && spawn) this.ensureServer();
  }

  restart(extraArgs) {
    this.reviewer.cancelAll('cancelled — session restarted');
    if (this.proc) {
      this.proc.kill();
      this.proc = null;
    }
    this.busy = false;
    this.ctxUsed = 0;
    this.ctxLimit = 0;
    this.log = [];
    this.post({ type: 'reset' }, false);
    this.ensureServer(extraArgs);
  }

  /** Args to relaunch on the SAME chat (model/setting changes must not jump
   *  to a different session). Falls back to the latest saved one if we don't
   *  yet know our path. */
  continueArgs() {
    return this.sessionPath ? ['--resume', this.sessionPath] : ['--continue'];
  }

  /** Ask rift for the list of past chats. Needs a running process to answer;
   *  if none is up yet, start one and send the request on `ready`. */
  requestSessions() {
    if (this.proc) {
      this.write({ cmd: 'list_sessions' });
    } else {
      this.pendingListSessions = true;
      this.ensureServer();
    }
  }

  /** Native quick-pick of past chats; picking one reopens it by resuming
   *  that session file. */
  async showSessionPicker(items) {
    if (!items.length) {
      vscode.window.showInformationMessage('rift: no past chats saved yet');
      return;
    }
    const when = (s) => {
      if (!s) return '';
      const diff = Date.now() / 1000 - s;
      if (diff < 3600) return `${Math.max(1, Math.round(diff / 60))}m ago`;
      if (diff < 86400) return `${Math.round(diff / 3600)}h ago`;
      if (diff < 86400 * 7) return `${Math.round(diff / 86400)}d ago`;
      return new Date(s * 1000).toLocaleDateString();
    };
    const picks = items.map((it) => ({
      label: it.title || '(untitled chat)',
      description: `${when(it.saved_at)} · ${it.turns} turn${it.turns === 1 ? '' : 's'}`,
      detail: `${it.model || ''}${it.cwd ? ' · ' + it.cwd : ''}`,
      path: it.path,
      current: it.path === this.sessionPath,
    }));
    const pick = await vscode.window.showQuickPick(picks, {
      placeHolder: 'Reopen a past chat',
      matchOnDescription: true,
      matchOnDetail: true,
    });
    if (!pick || pick.current) return;
    this.sessionPath = pick.path;
    this.context.workspaceState.update('rift.lastSession', pick.path);
    this.restart(['--resume', pick.path]);
  }

  /** Append text to the chat input (Add File/Selection to Prompt). */
  insert(text) {
    this.post({ type: 'insert', text }, false);
  }

  dispose() {
    this.reviewer.cancelAll('cancelled');
    if (this.proc) this.proc.kill();
    this.proc = null;
  }
}

// ── Integrated-terminal launcher (the original integration) ────────────────

let riftTerminal = null;

function quote(arg) {
  return /^[A-Za-z0-9_@%+=:,.\/-]+$/.test(arg) ? arg : `'${arg.replace(/'/g, `'\\''`)}'`;
}

function openRiftTerminal(flags = [], fresh = false) {
  if (riftTerminal && !fresh) {
    riftTerminal.show();
    return riftTerminal;
  }
  if (riftTerminal && fresh) {
    riftTerminal.dispose();
    riftTerminal = null;
  }
  const bin = config().get('executablePath') || 'rift';
  const command = [bin, ...riftArgs(), ...flags].map(quote).join(' ');
  const term = vscode.window.createTerminal({ name: 'rift', cwd: workspaceRoot() });
  term.sendText(command, true);
  term.show();
  riftTerminal = term;
  return term;
}

// ── Activation ──────────────────────────────────────────────────────────────

function mentionPath(uri) {
  return vscode.workspace.asRelativePath(uri, false);
}

function activate(context) {
  const reviewer = new DiffReviewer();
  const chat = new RiftChatProvider(context, reviewer);
  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider('rift.chatView', chat, {
      webviewOptions: { retainContextWhenHidden: true },
    }),
    chat,
    vscode.workspace.registerTextDocumentContentProvider(REVIEW_SCHEME, reviewer),
    vscode.languages.registerCodeLensProvider({ scheme: REVIEW_SCHEME }, reviewer),
    vscode.commands.registerCommand('rift.reviewToggle', (id, i) => reviewer.toggle(id, i)),
    // Title-bar invocations pass the editor's resource Uri; CodeLens and
    // chat-card invocations pass the review id; bare palette calls pass
    // nothing — resolve all three.
    vscode.commands.registerCommand('rift.reviewApply', (arg) =>
      reviewer.apply(reviewIdFromArg(arg) ?? reviewer.activeReviewId())
    ),
    vscode.commands.registerCommand('rift.reviewReject', (arg) =>
      reviewer.reject(reviewIdFromArg(arg) ?? reviewer.activeReviewId())
    ),
    vscode.window.onDidCloseTerminal((t) => {
      if (t === riftTerminal) riftTerminal = null;
    })
  );

  const addToChat = (text) => {
    vscode.commands.executeCommand('rift.chatView.focus').then(() => chat.insert(text));
  };

  context.subscriptions.push(
    vscode.commands.registerCommand('rift.openChat', () => {
      vscode.commands.executeCommand('rift.chatView.focus');
    }),
    vscode.commands.registerCommand('rift.toggleAutoApprove', () => {
      const on = chat.setAutoApprove();
      vscode.window.showInformationMessage(
        on
          ? 'rift: auto-approve ON — edits and commands apply without asking'
          : 'rift: auto-approve off — rift asks before edits and commands'
      );
    }),
    vscode.commands.registerCommand('rift.open', () => {
      openRiftTerminal();
    }),
    vscode.commands.registerCommand('rift.newSession', () => {
      openRiftTerminal([], true);
    }),
    vscode.commands.registerCommand('rift.continueSession', () => {
      openRiftTerminal(['--continue'], true);
    }),
    vscode.commands.registerCommand('rift.addFileToPrompt', (uri) => {
      const target = uri || vscode.window.activeTextEditor?.document.uri;
      if (!target) {
        vscode.window.showWarningMessage('rift: no file to add — open a file first');
        return;
      }
      addToChat(`@${mentionPath(target)} `);
    }),
    vscode.commands.registerCommand('rift.addSelectionToPrompt', () => {
      const editor = vscode.window.activeTextEditor;
      if (!editor) {
        vscode.window.showWarningMessage('rift: no active editor');
        return;
      }
      const rel = mentionPath(editor.document.uri);
      const sel = editor.selection;
      addToChat(
        sel.isEmpty ? `@${rel} ` : `@${rel} (lines ${sel.start.line + 1}-${sel.end.line + 1}) `
      );
    })
  );

  const status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 100);
  status.text = '$(terminal) rift';
  status.tooltip = 'Open the rift chat sidebar';
  status.command = 'rift.openChat';
  status.show();
  context.subscriptions.push(status);
}

function deactivate() {}

module.exports = { activate, deactivate };
