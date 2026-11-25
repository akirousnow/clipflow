pub mod auto_paste;
pub mod caret_position;
pub mod hotkey;
pub mod ipc;
pub mod updater;
pub mod wayland_clip;
pub mod x11_clip;

use std::sync::Arc;

use async_channel::Sender;

use crate::core::engine::ClipboardWriter;
use crate::core::model::CapturedClipboard;

/// Returns `true` when running inside a Wayland session.
pub fn is_wayland_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
}

/// Constructs the appropriate Linux clipboard writer for the current display server.
pub fn default_clipboard_writer() -> Arc<dyn ClipboardWriter> {
    if is_wayland_session() {
        Arc::new(wayland_clip::WaylandClipboardWriter::new())
    } else {
        Arc::new(x11_clip::X11ClipboardWriter::new())
    }
}

/// Spawns the background clipboard listener thread, pushing captured events into `tx`.
pub fn spawn_clipboard_watcher(tx: Sender<CapturedClipboard>) {
    std::thread::Builder::new()
        .name("clipflow-watcher".to_string())
        .spawn(move || {
            if is_wayland_session() {
                if let Err(err) = wayland_clip::run_wayland_watcher(tx.clone()) {
                    eprintln!("[clipflow] Wayland watcher exited with error: {err:#}");
                }
            } else if let Err(err) = x11_clip::run_x11_watcher(tx) {
                eprintln!("[clipflow] X11 watcher exited with error: {err:#}");
            }
        })
        .expect("failed to spawn clipboard watcher thread");
}
