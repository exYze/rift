//! System-clipboard access: native CLI tools first (pbcopy/wl-copy/xclip/
//! clip.exe — reliable where they exist), OSC 52 escape as the fallback for
//! terminals over ssh or without a clipboard tool. Reading back the other
//! way (Ctrl+V, right-click) uses the same tools' paste side.

use std::process::Stdio;

use tokio::io::AsyncWriteExt;

/// Try the platform clipboard tools; returns the tool used on success.
pub async fn copy_via_tool(text: &str) -> Option<&'static str> {
    let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else if cfg!(windows) {
        &[("clip", &[])]
    } else {
        &[
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["-ib"]),
        ]
    };
    // clip.exe interprets stdin in the OEM code page, mangling any non-ASCII
    // (✓, ❯, box drawing…) — but it honors a UTF-16 BOM, so feed it UTF-16LE.
    let payload: Vec<u8> = if cfg!(windows) {
        let mut bytes = vec![0xFF, 0xFE];
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes
    } else {
        text.as_bytes().to_vec()
    };
    for (tool, args) in candidates {
        let Ok(mut child) = tokio::process::Command::new(tool)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            if stdin.write_all(&payload).await.is_err() {
                continue;
            }
            drop(stdin);
        }
        if matches!(child.wait().await, Ok(s) if s.success()) {
            return Some(tool);
        }
    }
    None
}

/// Read text off the system clipboard — the paste side of `copy_via_tool`.
///
/// Sync on purpose: every caller already runs on a blocking thread, next to
/// `clipboard_image_data_url`, which shells out the same way.
///
/// `Some(text)` means a clipboard tool ran (an empty string = nothing text-ish
/// on the clipboard); `None` means no tool was available at all — worth
/// telling the user apart, since the fix for one is "copy something" and for
/// the other "install xclip".
pub fn paste_via_tool() -> Option<String> {
    #[cfg(target_os = "macos")]
    let candidates: &[(&str, &[&str])] = &[("pbpaste", &[])];
    // Windows has no `paste.exe` counterpart to `clip.exe`; PowerShell is the
    // only always-present way in. `Get-Clipboard` writes in the console code
    // page, which mangles anything non-ASCII, so force UTF-8 on the way out.
    #[cfg(windows)]
    let candidates: &[(&str, &[&str])] = &[(
        "powershell",
        &[
            "-NoProfile",
            "-NonInteractive",
            // Get-Clipboard needs a single-threaded apartment.
            "-STA",
            "-Command",
            "[Console]::OutputEncoding=[Text.Encoding]::UTF8; Get-Clipboard -Raw",
        ],
    )];
    #[cfg(all(unix, not(target_os = "macos")))]
    let candidates: &[(&str, &[&str])] = &[
        ("wl-paste", &["--no-newline"]),
        ("xclip", &["-selection", "clipboard", "-o"]),
        ("xsel", &["-ob"]),
    ];

    // A tool that runs but fails is usually just an empty clipboard (wl-paste
    // exits 1 on "Nothing is copied"), so remember that we found one and keep
    // trying the rest before deciding nothing is installed.
    let mut found_tool = false;
    for (tool, args) in candidates {
        let Ok(out) = std::process::Command::new(tool)
            .args(*args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
        else {
            continue;
        };
        found_tool = true;
        if !out.status.success() {
            continue;
        }
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        // PowerShell terminates its output with a newline the clipboard never
        // held; the unix tools hand back the bytes verbatim.
        let text = if cfg!(windows) {
            let t = text.strip_suffix('\n').unwrap_or(&text);
            t.strip_suffix('\r').unwrap_or(t).to_string()
        } else {
            text
        };
        // A leading BOM is an encoding marker, not something anyone meant to
        // paste — and it is invisible, so it would silently ride into a
        // prompt (or a source file) and break whatever reads it later.
        // clip.exe leaves one behind on everything `/copy` writes.
        let text = text.strip_prefix('\u{feff}').map(str::to_string).unwrap_or(text);
        return Some(text);
    }
    found_tool.then(String::new)
}

/// OSC 52 sequence that asks the terminal itself to set the clipboard.
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn osc52_wraps_base64() {
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
    }

    /// Ignored by default: it overwrites whatever the developer has copied,
    /// and headless CI has no clipboard to talk to. Run it deliberately
    /// (`cargo test -p rift-tui -- --ignored`) after touching either tool
    /// list — it is what catches a wrong flag or a mangled encoding.
    #[tokio::test]
    #[ignore = "reads and overwrites the real system clipboard"]
    async fn copy_and_paste_round_trip() {
        // Non-ASCII and a newline: the two things the platform tools get
        // wrong (clip.exe's code page, PowerShell's trailing newline).
        let text = "rift ✓ line one\nline two";
        let tool = copy_via_tool(text).await.expect("no clipboard tool to copy with");
        let back = paste_via_tool().expect("no clipboard tool to paste with");
        assert_eq!(back.replace("\r\n", "\n"), text, "round trip via {tool}");
    }
}
