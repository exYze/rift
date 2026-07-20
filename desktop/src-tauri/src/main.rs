// Rift desktop — a thin native shell over `rift --serve`. Each tab in the UI
// owns one serve process; this backend only spawns them, pumps their
// line-JSON, and provides the few things a webview can't do itself (folder
// picker, settings file, workspace file index for @-mentions). All agent
// logic stays in rift — the desktop app is a consumer of the serve protocol
// (docs/SERVE.md), exactly like the VS Code extension.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

struct Proc {
    child: Child,
    stdin: ChildStdin,
    /// Spawn generation for this tab id. A restart reuses the id, and the
    /// old process's reader thread may outlive it — the generation check
    /// keeps that thread from reaping (or worse, waiting on) the new child.
    gen: u64,
}

#[derive(Default)]
struct Procs(Mutex<HashMap<String, Proc>>);

static NEXT_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[derive(Clone, Serialize)]
struct RiftLine {
    tab: String,
    line: String,
}

#[derive(Clone, Serialize)]
struct RiftExit {
    tab: String,
    code: Option<i32>,
    stderr: String,
}

/// Spawn `rift --serve` for a tab, killing any previous process on the same
/// tab id (restart semantics — the UI reuses ids for "new session" etc.).
#[tauri::command]
fn start_rift(
    app: AppHandle,
    procs: State<'_, Procs>,
    tab: String,
    dir: String,
    bin: String,
    args: Vec<String>,
) -> Result<(), String> {
    stop_rift(procs.clone(), tab.clone());

    let mut cmd = Command::new(if bin.is_empty() { "rift".into() } else { bin });
    cmd.arg("--serve")
        .args(&args)
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // The app itself has no console; without this, every child would flash
    // (or permanently open) a conhost window on Windows.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not start rift ({e}) — set the rift binary path in Settings"))?;
    let stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let stderr = child.stderr.take().ok_or("no stderr")?;

    // stderr is diagnostics only (per the protocol) — keep a short tail to
    // show if the process dies.
    let tail: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let tail = tail.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut t = tail.lock().unwrap();
                t.push(line);
                let excess = t.len().saturating_sub(15);
                if excess > 0 {
                    t.drain(..excess);
                }
            }
        });
    }

    let gen = NEXT_GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    {
        let app = app.clone();
        let tab_id = tab.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if line.trim().is_empty() {
                    continue;
                }
                let _ = app.emit("rift-event", RiftLine { tab: tab_id.clone(), line });
            }
            // stdout closed — the process is gone (or going). Take our entry
            // out of the map (only ours: a restart may have replaced it),
            // release the lock, then reap outside it.
            let mine = {
                let procs = app.state::<Procs>();
                let mut map = procs.0.lock().unwrap();
                match map.get(&tab_id) {
                    Some(p) if p.gen == gen => map.remove(&tab_id),
                    _ => None,
                }
            };
            let Some(mut p) = mine else { return }; // replaced/stopped: not ours to report
            let code = p.child.wait().ok().and_then(|s| s.code());
            let _ = app.emit(
                "rift-exit",
                RiftExit { tab: tab_id, code, stderr: tail.lock().unwrap().join("\n") },
            );
        });
    }

    procs.0.lock().unwrap().insert(tab, Proc { child, stdin, gen });
    Ok(())
}

/// Write one command line (already-serialized JSON) to a tab's rift.
#[tauri::command]
fn send_rift(procs: State<'_, Procs>, tab: String, line: String) -> Result<(), String> {
    let mut map = procs.0.lock().unwrap();
    let proc = map.get_mut(&tab).ok_or("rift is not running for this tab")?;
    writeln!(proc.stdin, "{line}").map_err(|e| e.to_string())
}

#[tauri::command]
fn stop_rift(procs: State<'_, Procs>, tab: String) {
    if let Some(p) = procs.0.lock().unwrap().remove(&tab) {
        // Closing stdin asks rift to exit cleanly (saving the session);
        // kill is the backstop for a wedged process. Wait afterwards so the
        // child is reaped — its reader thread saw the map entry vanish and
        // won't touch it.
        let Proc { mut child, stdin, .. } = p;
        drop(stdin);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            let _ = child.kill();
            let _ = child.wait();
        });
    }
}

/// Native folder picker. Runs on the main thread (macOS requires it; the
/// modal block of the event loop is the normal dialog experience).
#[tauri::command]
async fn pick_folder(app: AppHandle) -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = app.run_on_main_thread(move || {
        let picked = rfd::FileDialog::new().pick_folder();
        let _ = tx.send(picked);
    });
    rx.recv()
        .ok()
        .flatten()
        .map(|p| p.to_string_lossy().to_string())
}

#[derive(Clone, Serialize)]
struct FileEntry {
    path: String,
    dir: bool,
}

const SKIP_DIRS: &[&str] = &[
    "node_modules", ".git", "target", "dist", "build", "out", ".next",
    "__pycache__", ".venv", "venv", "vendor",
];

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>, cap: usize) {
    if out.len() >= cap {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if out.len() >= cap {
            return;
        }
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if SKIP_DIRS.contains(&name.as_ref()) || name.starts_with('.') {
                continue;
            }
            walk(root, &path, out, cap);
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
}

/// Workspace paths for @-mention completion: files plus their ancestor
/// directories, scored the same way as the VS Code extension so the two
/// integrations feel identical.
#[tauri::command]
fn query_files(dir: String, query: String) -> Vec<FileEntry> {
    let root = PathBuf::from(&dir);
    let mut files = Vec::new();
    walk(&root, &root, &mut files, 5000);

    let mut dirs = std::collections::BTreeSet::new();
    for f in &files {
        let mut idx = f.rfind('/');
        while let Some(i) = idx {
            dirs.insert(f[..i].to_string());
            idx = f[..i].rfind('/');
        }
    }
    let entries: Vec<FileEntry> = files
        .into_iter()
        .map(|path| FileEntry { path, dir: false })
        .chain(dirs.into_iter().map(|path| FileEntry { path, dir: true }))
        .collect();

    let q = query.to_lowercase();
    let mut scored: Vec<(i32, usize, &FileEntry)> = entries
        .iter()
        .filter_map(|e| {
            let p = e.path.to_lowercase();
            let base = p.rsplit('/').next().unwrap_or(&p);
            let s = if q.is_empty() {
                1
            } else if base == q {
                6
            } else if base.starts_with(&q) {
                5
            } else if base.contains(&q) {
                4
            } else if p.starts_with(&q) {
                3
            } else if p.contains(&q) {
                2
            } else {
                0
            };
            let depth = e.path.matches('/').count();
            (s > 0).then_some((s, depth, e))
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(a.1.cmp(&b.1))
            .then(b.2.dir.cmp(&a.2.dir))
            .then(a.2.path.cmp(&b.2.path))
    });
    scored.into_iter().take(50).map(|(_, _, e)| e.clone()).collect()
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("settings.json"))
}

/// Settings are one JSON blob owned by the frontend (binary path, host,
/// model, recent projects, theme …) — the backend just persists it.
#[tauri::command]
fn load_settings(app: AppHandle) -> Result<serde_json::Value, String> {
    let path = settings_path(&app)?;
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).map_err(|e| e.to_string()),
        Err(_) => Ok(serde_json::json!({})),
    }
}

#[tauri::command]
fn save_settings(app: AppHandle, settings: serde_json::Value) -> Result<(), String> {
    let path = settings_path(&app)?;
    std::fs::write(path, serde_json::to_string_pretty(&settings).unwrap())
        .map_err(|e| e.to_string())
}

fn main() {
    tauri::Builder::default()
        .manage(Procs::default())
        .invoke_handler(tauri::generate_handler![
            start_rift,
            send_rift,
            stop_rift,
            pick_folder,
            query_files,
            load_settings,
            save_settings,
        ])
        .setup(|app| {
            // CI smoke test: prove the window + webview come up, then exit.
            if std::env::var("RIFT_DESKTOP_SMOKE").is_ok() {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(3));
                    handle.exit(0);
                });
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window closes every rift (stdin EOF → clean exit,
            // sessions saved) — same contract as the protocol documents.
            if let tauri::WindowEvent::Destroyed = event {
                let procs = window.app_handle().state::<Procs>();
                let mut map = procs.0.lock().unwrap();
                for (_, p) in map.drain() {
                    drop(p.stdin);
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running rift desktop");
}
