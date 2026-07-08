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

// ── Sidebar chat ────────────────────────────────────────────────────────────

class RiftChatProvider {
  constructor(context) {
    this.context = context;
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
      if (code !== 0 && code != null) {
        const tail = this.stderrTail.slice(-4).join('\n');
        this.post({ type: 'rift', ev: { event: 'warning', text: `rift exited (code ${code})${tail ? '\n' + tail : ''}` } });
      }
      this.postStatus();
    });
    this.postStatus();
  }

  onServerEvent(ev) {
    if (ev.event === 'ready') {
      this.model = ev.model;
    } else if (ev.event === 'done') {
      this.busy = false;
    }
    // Thinking/content deltas are high-volume; replaying them is what makes
    // the transcript reappear intact, so they are recorded like the rest.
    this.post({ type: 'rift', ev });
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
  const chat = new RiftChatProvider(context);
  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider('rift.chatView', chat, {
      webviewOptions: { retainContextWhenHidden: true },
    }),
    chat,
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
