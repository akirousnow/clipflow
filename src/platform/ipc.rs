use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use async_channel::Sender;

use crate::core::model::PasteMode;

/// Commands delivered over the single-instance Unix socket or global shortcut portal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppCommand {
    Toggle,
    Show,
    Hide,
    Quit,
    /// Sent by the transient GPUI popup to the persistent daemon when an item is chosen.
    Activate { id: i64, mode: PasteMode },
}

impl AppCommand {
    pub fn to_wire(self) -> String {
        match self {
            Self::Toggle => "toggle".to_string(),
            Self::Show => "show".to_string(),
            Self::Hide => "hide".to_string(),
            Self::Quit => "quit".to_string(),
            Self::Activate { id, mode } => {
                let mode_str = match mode {
                    PasteMode::CopyAndPaste => "paste",
                    PasteMode::CopyAndPasteShift => "paste_shift",
                    PasteMode::CopyOnly => "copy",
                };
                format!("activate {id} {mode_str}")
            }
        }
    }

    pub fn from_wire(s: &str) -> Option<Self> {
        let trimmed = s.trim();
        match trimmed {
            "toggle" => Some(Self::Toggle),
            "show" => Some(Self::Show),
            "hide" => Some(Self::Hide),
            "quit" => Some(Self::Quit),
            _ => {
                let mut parts = trimmed.split_whitespace();
                if parts.next()? != "activate" {
                    return None;
                }
                let id: i64 = parts.next()?.parse().ok()?;
                let mode = match parts.next()? {
                    "paste" => PasteMode::CopyAndPaste,
                    "paste_shift" => PasteMode::CopyAndPasteShift,
                    "copy" => PasteMode::CopyOnly,
                    _ => return None,
                };
                Some(Self::Activate { id, mode })
            }
        }
    }
}

pub fn socket_path() -> PathBuf {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime).join("clipflow.sock")
    } else {
        let user = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
        std::env::temp_dir().join(format!("clipflow-{user}.sock"))
    }
}

/// Attempts to forward a command to an already-running `clipflow` daemon.
/// Returns `Ok(true)` if an existing daemon accepted the command, `Ok(false)` if none is running.
pub fn try_send_command(cmd: AppCommand) -> Result<bool> {
    let path = socket_path();
    let Ok(mut stream) = UnixStream::connect(&path) else {
        return Ok(false);
    };
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    writeln!(stream, "{}", cmd.to_wire())?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    let _ = reader.read_line(&mut reply);
    Ok(reply.trim() == "ok")
}

/// Starts the single-instance Unix socket listener on a background thread.
pub fn spawn_ipc_server(tx: Sender<AppCommand>) -> Result<()> {
    let path = socket_path();
    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            anyhow::bail!(
                "another clipflow instance is already listening at {}",
                path.display()
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    let listener = UnixListener::bind(&path)
        .with_context(|| format!("failed to bind IPC socket at {}", path.display()))?;

    std::thread::Builder::new()
        .name("clipflow-ipc".to_string())
        .spawn(move || {
            for incoming in listener.incoming() {
                let Ok(mut stream) = incoming else {
                    continue;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                let mut line = String::new();
                let mut reader = BufReader::new(&stream);
                if reader.read_line(&mut line).is_ok()
                    && let Some(cmd) = AppCommand::from_wire(&line)
                {
                    let _ = tx.send_blocking(cmd);
                    let _ = writeln!(stream, "ok");
                }
            }
        })
        .context("failed to spawn IPC server thread")?;

    Ok(())
}
