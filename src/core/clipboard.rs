//! System clipboard integration — get text out of the TUI.
//!
//! The dashboard runs with mouse capture enabled (see [`crate::tui::init`]),
//! which disables the terminal's native click-drag selection. To let users copy
//! log text we write to the OS clipboard on demand by shelling out to the
//! platform clipboard utility — the same "shell out, stay unixy" pattern used by
//! [`crate::core::hosts`]. If no utility is found we fall back to an OSC 52
//! escape sequence, which routes the copy through the terminal itself (so it
//! also works over SSH on terminals that support it).
//!
//! No new crate dependency: the base64 needed for OSC 52 is a few lines below.
//!
//! # Author
//! Daniel Tamas <hello@danieltamas.com>

use std::io::Write;
use std::process::{Command, Stdio};

/// Copy `text` to the system clipboard.
///
/// Tries the platform clipboard utility first (`pbcopy` on macOS; `wl-copy`,
/// `xclip`, then `xsel` on Linux). If none is installed, falls back to emitting
/// an OSC 52 sequence on stdout so terminals that support it (iTerm2, kitty,
/// wezterm, foot, tmux with `set-clipboard on`, …) still capture the text.
///
/// On success returns the name of the mechanism used (for diagnostics); on
/// failure returns a message describing what to install.
pub fn copy_to_clipboard(text: &str) -> Result<&'static str, String> {
    // Ordered list of (binary, args) to try for this platform.
    #[cfg(target_os = "macos")]
    let candidates: &[(&str, &[&str])] = &[("pbcopy", &[])];
    #[cfg(not(target_os = "macos"))]
    let candidates: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];

    let mut missing = Vec::new();
    for (bin, args) in candidates {
        match pipe_to(bin, args, text) {
            Ok(()) => return Ok(bin),
            Err(PipeError::NotFound) => missing.push(*bin),
            Err(PipeError::Failed(e)) => return Err(format!("{bin}: {e}")),
        }
    }

    // No utility present — try routing through the terminal via OSC 52.
    if emit_osc52(text).is_ok() {
        return Ok("osc52");
    }

    if missing.is_empty() {
        Err("no clipboard tool available".to_string())
    } else {
        Err(format!("no clipboard tool found (install {})", missing.join(" or ")))
    }
}

enum PipeError {
    /// The binary isn't on `PATH`.
    NotFound,
    /// The binary exists but spawning/writing/exit failed.
    Failed(String),
}

/// Spawn `bin args`, write `text` to its stdin, and wait for it to exit.
/// stdout/stderr are sent to `/dev/null` so clipboard daemons that fork to hold
/// the selection (xclip, wl-copy) don't keep our terminal's fds open.
fn pipe_to(bin: &str, args: &[&str], text: &str) -> Result<(), PipeError> {
    let mut child = match Command::new(bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(PipeError::NotFound),
        Err(e) => return Err(PipeError::Failed(e.to_string())),
    };

    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(text.as_bytes()) {
            return Err(PipeError::Failed(e.to_string()));
        }
        // stdin dropped here → child sees EOF.
    }

    match child.wait() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(PipeError::Failed(format!("exited with {status}"))),
        Err(e) => Err(PipeError::Failed(e.to_string())),
    }
}

/// Emit an OSC 52 clipboard-set sequence on stdout. The terminal emulator reads
/// it and updates the system clipboard itself — works with no clipboard binary
/// and across SSH for terminals that implement OSC 52.
fn emit_osc52(text: &str) -> std::io::Result<()> {
    let seq = format!("\x1b]52;c;{}\x07", base64_encode(text.as_bytes()));
    let mut out = std::io::stdout();
    out.write_all(seq.as_bytes())?;
    out.flush()
}

/// Minimal standard base64 encoder (RFC 4648). Inlined to avoid a dependency for
/// the single place we need it (OSC 52 payloads).
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    // 4 output chars per 3 input bytes, rounded up.
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { ALPHABET[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[(n & 63) as usize] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::base64_encode;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_handles_binary_and_unicode() {
        assert_eq!(base64_encode(&[0xff, 0xfe, 0xfd]), "//79");
        // "✓" = E2 9C 93
        assert_eq!(base64_encode("✓".as_bytes()), "4pyT");
    }
}
