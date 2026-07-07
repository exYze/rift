// Rift for VS Code — runs the real rift TUI in the integrated terminal, so
// the full feature set (approval prompts, sessions, skills, MCP, sub-agents,
// swarm, /commands) works exactly as it does standalone. The extension adds
// the editor-side glue: launch commands, keybindings, and @file injection.
const vscode = require('vscode');

/** The terminal hosting rift, if we launched one and it is still alive. */
let riftTerminal = null;
/** Set while the terminal is younger than STARTUP_MS — sendText issued
 *  before the TUI enters raw mode would be eaten by the shell prompt. */
let launchedAt = 0;
const STARTUP_MS = 1500;

function config() {
  return vscode.workspace.getConfiguration('rift');
}

/** Shell-quote one argument (POSIX-ish; PowerShell accepts the same for
 *  simple flag values). Only quotes when needed so the command stays legible. */
function quote(arg) {
  return /^[A-Za-z0-9_@%+=:,.\/-]+$/.test(arg) ? arg : `'${arg.replace(/'/g, `'\\''`)}'`;
}

/** Build the rift command line from settings plus per-launch flags. */
function riftCommand(flags) {
  const cfg = config();
  const parts = [quote(cfg.get('executablePath') || 'rift')];
  const host = cfg.get('host');
  if (host) parts.push('--host', quote(host));
  const model = cfg.get('model');
  if (model) parts.push('--model', quote(model));
  for (const extra of cfg.get('extraArgs') || []) parts.push(quote(extra));
  parts.push(...flags);
  return parts.join(' ');
}

function workspaceRoot() {
  const folders = vscode.workspace.workspaceFolders;
  return folders && folders.length > 0 ? folders[0].uri : undefined;
}

/** Reuse the live rift terminal, or launch a new one with the given flags.
 *  `fresh` forces a new terminal (new/continued sessions must not type into
 *  an already-running rift). */
function openRift(flags = [], fresh = false) {
  if (riftTerminal && !fresh) {
    riftTerminal.show();
    return riftTerminal;
  }
  if (riftTerminal && fresh) {
    riftTerminal.dispose();
    riftTerminal = null;
  }
  const term = vscode.window.createTerminal({ name: 'rift', cwd: workspaceRoot() });
  term.sendText(riftCommand(flags), true);
  term.show();
  riftTerminal = term;
  launchedAt = Date.now();
  return term;
}

/** Type text into rift's input (no submit), waiting out TUI startup first. */
function typeIntoRift(text) {
  const term = openRift();
  const wait = Math.max(0, launchedAt + STARTUP_MS - Date.now());
  setTimeout(() => term.sendText(text, false), wait);
}

/** Workspace-relative path for @-mentions; falls back to the full path for
 *  files outside the workspace. */
function mentionPath(uri) {
  return vscode.workspace.asRelativePath(uri, false);
}

function activate(context) {
  // A terminal closed by the user (or a rift /quit) must not be reused.
  context.subscriptions.push(
    vscode.window.onDidCloseTerminal((t) => {
      if (t === riftTerminal) riftTerminal = null;
    })
  );

  context.subscriptions.push(
    vscode.commands.registerCommand('rift.open', () => {
      openRift();
    }),

    vscode.commands.registerCommand('rift.newSession', () => {
      openRift([], true);
    }),

    vscode.commands.registerCommand('rift.continueSession', () => {
      openRift(['--continue'], true);
    }),

    // Explorer context menu passes the clicked uri; from the palette or
    // editor context menu, fall back to the active editor's file.
    vscode.commands.registerCommand('rift.addFileToPrompt', (uri) => {
      const target = uri || vscode.window.activeTextEditor?.document.uri;
      if (!target) {
        vscode.window.showWarningMessage('rift: no file to add — open a file first');
        return;
      }
      typeIntoRift(`@${mentionPath(target)} `);
    }),

    vscode.commands.registerCommand('rift.addSelectionToPrompt', () => {
      const editor = vscode.window.activeTextEditor;
      if (!editor) {
        vscode.window.showWarningMessage('rift: no active editor');
        return;
      }
      const rel = mentionPath(editor.document.uri);
      const sel = editor.selection;
      if (sel.isEmpty) {
        typeIntoRift(`@${rel} `);
      } else {
        // The mention attaches the file; the line range rides along as plain
        // text so the model knows where to look.
        typeIntoRift(`@${rel} (lines ${sel.start.line + 1}-${sel.end.line + 1}) `);
      }
    })
  );

  // One-click entry point in the status bar.
  const status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 100);
  status.text = '$(terminal) rift';
  status.tooltip = 'Open rift (reuses the running session if there is one)';
  status.command = 'rift.open';
  status.show();
  context.subscriptions.push(status);
}

function deactivate() {}

module.exports = { activate, deactivate };
