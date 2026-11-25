# ClipFlow

Minimalist Linux clipboard manager built with **Rust** and **GPUI**.

## Highlights

- **Text & Images**: Captures plain text and images (`image/png`, `image/webp`, `image/jpeg`, `image/bmp`, `image/gif`) via event-driven Wayland `zwlr_data_control_manager_v1` (zero polling, 0.00% idle CPU), with X11 fallback.
- **Deduplication & Favorites**:
  - Ignores leading/trailing whitespace when deduplicating text (`blake3` hash); pixel-canonical deduplication for images across encodings.
  - Re-copying moves the existing entry to the top while preserving its favorite status.
  - Infinite history retention in SQLite (WAL mode) with content-addressed full PNGs + thumbnails on disk.
- **Instant Startup (~18 ms) & Lazy Loading**:
  - Single-process persistent GPUI daemon keeps Vulkan context, cached Blade shader pipelines, and system fonts warm while consuming 0.00% idle CPU when hidden.
  - Opening the popup loads only the first page (40 lightweight summaries) via keyset cursor pagination; scrolling near the bottom automatically fetches the next page; typing filters directly against SQLite.
- **Focused Input Positioning**:
  - Tracks the active caret / focused input box via AT-SPI2 (zero-polling D-Bus listener) and anchors the floating window directly below the caret (flipping above near the bottom edge and clamping inside screen bounds).
  - Falls back to centering inside the focused application window tile via Niri IPC (`$NIRI_SOCKET`), and finally monitor center.
- **Safety**:
  - Self-write suppression prevents copy-back loops when selecting an item from history; coalesces rapid Wayland/XWayland bridge echoes.
  - Skips sensitive/transient clipboard markers (`x-kde-passwordManagerHint`, `org.nspasteboard.ConcealedType`, `ExcludeClipboardContentFromMonitorProcessing`, etc.).
- **Online Self-Update**:
  - Checks GitHub Releases (`akirousnow/clipflow`), verifies `SHA256SUMS.txt` with SHA-256, atomically replaces the executable, and restarts the daemon (`clipflow check-update`, `clipflow update`, or one-click footer update badge when a newer version is available).
- **Minimalist Native UX**:
  - Automatically follows system Light/Dark theme via XDG Desktop Portal `Settings`.
  - Frameless floating window with top filter bar and `All` / `Favorites` tab switcher.
  - Selecting an item writes to the clipboard, closes the window, and auto-pastes into the previously focused app via native Wayland `zwp_virtual_keyboard_manager_v1` (with `wtype` / `ydotool` / `xdotool` fallback).

## Keyboard Shortcuts

| Key | Action |
| --- | --- |
| `↑` / `↓` or `Ctrl+P` / `Ctrl+N` or `Ctrl+K` / `Ctrl+J` | Move selection |
| `Enter` | Copy & auto-paste (`Ctrl+V`) into previous app |
| `Shift+Enter` | Copy only (no auto-paste) |
| `Ctrl+Enter` | Copy & terminal paste (`Ctrl+Shift+V`) |
| `Ctrl+S` | Toggle favorite (`★`) |
| `Tab` | Switch between `All` and `Favorites` tabs |
| `Ctrl+D` | Delete selected entry |
| `Ctrl+U` | Clear search input |
| `Esc` | Clear search (if active) or close floating window |

## Usage

```bash
# Build release binary
cargo build --release

# Start background daemon (no window on startup)
./target/release/clipflow --daemon

# Toggle the floating window via single-instance Unix socket (~18 ms)
./target/release/clipflow toggle

# Check for updates / upgrade in place from GitHub Releases
./target/release/clipflow check-update
./target/release/clipflow update
```

### Compositor Configuration Examples

**Niri** (`~/.config/niri/config.kdl`):
```kdl
spawn-at-startup "/path/to/clipflow" "--daemon"

binds {
    Mod+V { spawn "/path/to/clipflow" "toggle"; }
}

window-rule {
    match app-id="^clipflow$"
    open-floating true
    default-column-width { fixed 500; }
    default-window-height { fixed 560; }
}
```

**Hyprland** (`~/.config/hypr/hyprland.conf`):
```ini
exec-once = /path/to/clipflow --daemon
bind = SUPER, V, exec, /path/to/clipflow toggle
```

**Sway** (`~/.config/sway/config`):
```ini
exec /path/to/clipflow --daemon
bindsym $mod+v exec /path/to/clipflow toggle
```

On desktop environments with XDG Desktop Portal `GlobalShortcuts` (KDE Plasma, GNOME, Hyprland), `clipflow` also automatically registers a `toggle` global shortcut (default preferred trigger: `Super+Shift+V`, override with `--hotkey`).
