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

function readRiftConfig() {
  try {
    return JSON.parse(fs.readFileSync(riftConfigPath(), 'utf8'));
  } catch {
    return {};
  }
}

async function fetchJson(url, headers = {}) {
  const res = await fetch(url, { headers, signal: AbortSignal.timeout(4000) });
  if (!res.ok) throw new Error(`${res.status}`);
  return res.json();
}

/** Every model rift can currently reach: the default host's list (Ollama
 *  /api/tags, or /v1/models for OpenAI-style hosts like vLLM), plus each
 *  configured provider's /v1/models as "provider/model" entries — the same
 *  prefix routing rift itself uses. Unreachable servers are skipped. */
async function discoverModels() {
  const rc = readRiftConfig();
  const models = [];
  const host = (config().get('host') || rc.host || 'http://localhost:11434').replace(/\/+$/, '');
  try {
    if (host.endsWith('/v1')) {
      const d = await fetchJson(`${host}/models`);
      for (const m of d.data || []) models.push(m.id);
    } else {
      const d = await fetchJson(`${host}/api/tags`);
      for (const m of d.models || []) models.push(m.name);
    }
  } catch {
    /* default host down — provider entries below may still work */
  }
  for (const [name, p] of Object.entries(rc.providers || {})) {
    if (!p || !p.base_url) continue;
    try {
      const base = p.base_url.replace(/\/+$/, '');
      const key = p.api_key || (p.api_key_env ? process.env[p.api_key_env] : undefined);
      let ids = [];
      if (p.kind === 'anthropic') {
        const d = await fetchJson(`${base}/v1/models`, {
          'x-api-key': key || '',
          'anthropic-version': '2023-06-01',
        });
        ids = (d.data || []).map((m) => m.id);
      } else {
        const url = base.endsWith('/v1') ? `${base}/models` : `${base}/v1/models`;
        const d = await fetchJson(url, key ? { Authorization: `Bearer ${key}` } : {});
        ids = (d.data || []).map((m) => m.id);
      }
      for (const id of ids) models.push(`${name}/${id}`);
    } catch {
      /* provider unreachable — skip */
    }
  }
  return models;
}

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

/** Line-based Myers diff of a → b as an op list ('same' | 'del' | 'ins').
 *  Falls back to one whole-file change when the edit distance exceeds the
 *  cap — review still works, just as a single hunk. */
function diffOps(a, b) {
  const n = a.length;
  const m = b.length;
  if (!n) return b.map(() => 'ins');
  if (!m) return a.map(() => 'del');
  const maxD = Math.min(n + m, 2000);
  const offset = maxD;
  let v = new Array(2 * maxD + 1).fill(0);
  const trace = [];
  let found = -1;
  for (let d = 0; d <= maxD && found < 0; d++) {
    trace.push(v.slice());
    for (let k = -d; k <= d; k += 2) {
      let x;
      if (k === -d || (k !== d && v[offset + k - 1] < v[offset + k + 1])) x = v[offset + k + 1];
      else x = v[offset + k - 1] + 1;
      let y = x - k;
      while (x < n && y < m && a[x] === b[y]) { x++; y++; }
      v[offset + k] = x;
      if (x >= n && y >= m) {
        found = d;
        break;
      }
    }
  }
  if (found < 0) return [...a.map(() => 'del'), ...b.map(() => 'ins')];
  const ops = [];
  let x = n;
  let y = m;
  for (let d = found; d > 0; d--) {
    const vp = trace[d];
    const k = x - y;
    const prevK = k === -d || (k !== d && vp[offset + k - 1] < vp[offset + k + 1]) ? k + 1 : k - 1;
    const prevX = vp[offset + prevK];
    const prevY = prevX - prevK;
    while (x > prevX && y > prevY) { x--; y--; ops.push('same'); }
    if (x === prevX) { y--; ops.push('ins'); }
    else { x--; ops.push('del'); }
  }
  while (x > 0 && y > 0) { x--; y--; ops.push('same'); }
  while (x > 0) { x--; ops.push('del'); }
  while (y > 0) { y--; ops.push('ins'); }
  return ops.reverse();
}

/** Group old/new text into alternating unchanged/changed segments — each
 *  changed segment is one reviewable hunk. */
function diffSegments(oldText, newText) {
  const a = oldText.split('\n');
  const b = newText.split('\n');
  const segs = [];
  let ai = 0;
  let bi = 0;
  for (const op of diffOps(a, b)) {
    const last = segs[segs.length - 1];
    if (op === 'same') {
      if (last && last.same) last.lines.push(a[ai]);
      else segs.push({ same: true, lines: [a[ai]] });
      ai++; bi++;
    } else {
      let seg = last && !last.same ? last : null;
      if (!seg) {
        seg = { same: false, oldLines: [], newLines: [] };
        segs.push(seg);
      }
      if (op === 'del') { seg.oldLines.push(a[ai]); ai++; }
      else { seg.newLines.push(b[bi]); bi++; }
    }
  }
  return segs;
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
    const segments = diffSegments(ev.old, ev.new);
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
    this.stderrTail = [];
    this.stdoutBuf = '';
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
    <span id="header-buttons">
      <button id="btn-undo" data-tip="Undo — revert the file edits from the last turn (changes made via bash are not tracked)">↶</button>
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
      <span>max iterations</span><input id="set-iters" type="number" min="1" placeholder="25">
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
    if (this.view) {
      this.view.webview.postMessage({ type: 'status', text, running: !!this.proc, busy: this.busy });
    }
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
    this.write({ cmd: 'hello', edit_review: config().get('inlineDiffReview') !== false });
    this.postStatus();
  }

  onServerEvent(ev) {
    if (ev.event === 'ready') {
      this.model = ev.model;
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
        this.restart([]);
        break;
      case 'continueSession':
        this.restart(['--continue']);
        break;
      case 'refreshModels':
        this.postModels();
        break;
      case 'queryFiles':
        this.postFiles(m.query, m.token);
        break;
      case 'setModel': {
        config()
          .update('model', m.model || undefined, vscode.ConfigurationTarget.Global)
          .then(() => {
            // Respawn resuming the same session: the conversation continues
            // on the newly selected model (rift recomposes the system prompt).
            if (this.proc) this.restart(['--continue']);
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
        Promise.all([
          cfg.update('executablePath', m.executablePath || undefined, vscode.ConfigurationTarget.Global),
          cfg.update('host', m.host || undefined, vscode.ConfigurationTarget.Global),
          cfg.update('effort', m.effort || undefined, vscode.ConfigurationTarget.Global),
          cfg.update('numCtx', num(m.numCtx), vscode.ConfigurationTarget.Global),
          cfg.update('temperature', num(m.temperature), vscode.ConfigurationTarget.Global),
          cfg.update('maxIterations', num(m.maxIterations), vscode.ConfigurationTarget.Global),
        ]).then(() => {
          if (this.proc) this.restart(['--continue']);
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

  postModels() {
    discoverModels().then((models) => {
      if (this.view) {
        this.view.webview.postMessage({
          type: 'models',
          models,
          current: config().get('model') || '',
        });
      }
    });
  }

  restart(extraArgs) {
    this.reviewer.cancelAll('cancelled — session restarted');
    if (this.proc) {
      this.proc.kill();
      this.proc = null;
    }
    this.busy = false;
    this.log = [];
    this.post({ type: 'reset' }, false);
    this.ensureServer(extraArgs);
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
