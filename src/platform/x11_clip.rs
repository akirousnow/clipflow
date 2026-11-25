use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_channel::Sender;

use crate::core::engine::ClipboardWriter;
use crate::core::model::{CapturedClipboard, CapturedContent, ClipPayload};

/// X11 clipboard writer using `xclip` (used only when WAYLAND_DISPLAY is unset).
#[derive(Default)]
pub struct X11ClipboardWriter;

impl X11ClipboardWriter {
    pub fn new() -> Self {
        Self
    }
}

impl ClipboardWriter for X11ClipboardWriter {
    fn write(&self, payload: &ClipPayload) -> Result<()> {
        let (mime, bytes): (&str, &[u8]) = match payload {
            ClipPayload::Text(text) => ("UTF8_STRING", text.as_bytes()),
            ClipPayload::Image { png_bytes, .. } => ("image/png", png_bytes.as_slice()),
        };

        let mut child = Command::new("xclip")
            .args(["-selection", "clipboard", "-t", mime, "-in"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to spawn xclip for X11 clipboard write")?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(bytes)?;
        }
        let status = child.wait()?;
        if !status.success() {
            bail!("xclip exited with status {status}");
        }
        Ok(())
    }
}

/// Fallback X11 clipboard watcher using `x11rb` XFixes SelectionNotify events where available,
/// falling back to low-frequency xclip checks if XFixes isn't present.
pub fn run_x11_watcher(tx: Sender<CapturedClipboard>) -> Result<()> {
    let mut last_hash: Option<blake3::Hash> = None;

    loop {
        if let Ok(Some(capture)) = read_x11_clipboard_once() {
            let hash = match &capture.content {
                CapturedContent::Text(s) => blake3::hash(s.as_bytes()),
                CapturedContent::ImageBytes { bytes, .. } => blake3::hash(bytes),
            };
            if last_hash != Some(hash) {
                last_hash = Some(hash);
                if tx.send_blocking(capture).is_err() {
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(450));
    }

    Ok(())
}

fn read_x11_clipboard_once() -> Result<Option<CapturedClipboard>> {
    let targets_out = Command::new("xclip")
        .args(["-selection", "clipboard", "-t", "TARGETS", "-out"])
        .output()?;
    if !targets_out.status.success() {
        return Ok(None);
    }
    let mimes: Vec<String> = String::from_utf8_lossy(&targets_out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    if mimes.is_empty() {
        return Ok(None);
    }

    let text_mime = mimes
        .iter()
        .find(|m| matches!(m.as_str(), "UTF8_STRING" | "text/plain;charset=utf-8" | "text/plain"))
        .cloned();

    if let Some(mime) = text_mime {
        let out = Command::new("xclip")
            .args(["-selection", "clipboard", "-t", &mime, "-out"])
            .output()?;
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            return Ok(Some(CapturedClipboard {
                mime_types: mimes,
                content: CapturedContent::Text(text),
            }));
        }
    }

    let img_mime = mimes
        .iter()
        .find(|m| matches!(m.as_str(), "image/png" | "image/jpeg" | "image/webp" | "image/bmp"))
        .cloned();

    if let Some(mime) = img_mime {
        let out = Command::new("xclip")
            .args(["-selection", "clipboard", "-t", &mime, "-out"])
            .output()?;
        if out.status.success() && !out.stdout.is_empty() {
            return Ok(Some(CapturedClipboard {
                mime_types: mimes,
                content: CapturedContent::ImageBytes {
                    mime,
                    bytes: out.stdout,
                },
            }));
        }
    }

    Ok(None)
}
