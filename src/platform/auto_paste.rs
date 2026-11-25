use std::io::{Seek, SeekFrom, Write};
use std::os::fd::AsFd;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_keyboard::KeymapFormat;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

use crate::core::model::PasteMode;
use crate::platform::is_wayland_session;

// Linux evdev keycodes (input-event-codes.h)
const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_V: u32 = 47;

// Standard XKB modifier masks
const MOD_SHIFT: u32 = 1 << 0;
const MOD_CTRL: u32 = 1 << 2;

/// Minimal valid XKB v1 keymap defining LeftCtrl, LeftShift, and 'v'.
const MINIMAL_XKB_KEYMAP: &str = r#"xkb_keymap {
xkb_keycodes "evdev_minimal" {
    minimum = 8;
    maximum = 255;
    <LCTL> = 37;
    <LFSH> = 50;
    <AB04> = 55;
};
xkb_types "minimal" {
    virtual_modifiers NumLock;
    type "ONE_LEVEL" {
        modifiers = none;
        level_name[Level1] = "Any";
    };
    type "ALPHABETIC" {
        modifiers = Shift+Lock;
        map[Shift] = Level2;
        map[Lock] = Level2;
        level_name[Level1] = "Base";
        level_name[Level2] = "Caps";
    };
};
xkb_compatibility "minimal" {
    interpret.useModMapMods = AnyLevel;
    interpret.repeat = False;
    interpret Control_L+AnyOfOrNone(all) {
        action = SetMods(modifiers=Control);
    };
    interpret Shift_L+AnyOfOrNone(all) {
        action = SetMods(modifiers=Shift);
    };
};
xkb_symbols "minimal" {
    key <LCTL> { [ Control_L ] };
    key <LFSH> { [ Shift_L ] };
    key <AB04> { [ v, V ] };
    modifier_map Control { <LCTL> };
    modifier_map Shift { <LFSH> };
};
};
"#;

/// Triggers an OS-level paste shortcut into the currently focused window after `settle_delay`.
/// Runs best-effort fallback chain:
/// 1. Native Wayland `zwp_virtual_keyboard_manager_v1` (works on Niri, Sway, Hyprland, River, etc.)
/// 2. `wtype` (Wayland)
/// 3. `ydotool` (Wayland/X11 if ydotoold is running)
/// 4. `xdotool` (X11 / XWayland)
pub fn trigger_auto_paste(mode: PasteMode, settle_delay: Duration) -> Result<()> {
    if mode == PasteMode::CopyOnly {
        return Ok(());
    }
    if !settle_delay.is_zero() {
        std::thread::sleep(settle_delay);
    }

    let shift = mode == PasteMode::CopyAndPasteShift;

    if is_wayland_session() {
        if let Ok(()) = paste_via_wayland_virtual_keyboard(shift) {
            return Ok(());
        }
        if let Ok(()) = paste_via_wtype(shift) {
            return Ok(());
        }
        if let Ok(()) = paste_via_ydotool(shift) {
            return Ok(());
        }
    }

    if let Ok(()) = paste_via_xdotool(shift) {
        return Ok(());
    }

    bail!("no supported auto-paste mechanism succeeded (tried Wayland virtual keyboard, wtype, ydotool, xdotool)")
}

struct VkState;

impl Dispatch<WlRegistry, GlobalListContents> for VkState {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wayland_client::protocol::wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for VkState {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: wayland_client::protocol::wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for VkState {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardManagerV1,
        _: wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for VkState {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardV1,
        _: wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

fn paste_via_wayland_virtual_keyboard(shift: bool) -> Result<()> {
    let conn = Connection::connect_to_env().context("failed to connect to Wayland")?;
    let (globals, mut queue) = registry_queue_init::<VkState>(&conn)?;
    let qh = queue.handle();

    let seat: WlSeat = globals
        .bind(&qh, 1..=9, ())
        .map_err(|e| anyhow!("wl_seat unavailable: {e}"))?;
    let vk_manager: ZwpVirtualKeyboardManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| anyhow!("zwp_virtual_keyboard_manager_v1 unavailable: {e}"))?;

    let vk = vk_manager.create_virtual_keyboard(&seat, &qh, ());

    // Provide NUL-terminated XKB keymap through a temporary unlinked file descriptor rewound to 0.
    let mut keymap_bytes = MINIMAL_XKB_KEYMAP.as_bytes().to_vec();
    keymap_bytes.push(0);
    let mut keymap_file = create_temp_keymap_file(&keymap_bytes)?;
    keymap_file.flush()?;
    keymap_file.seek(SeekFrom::Start(0))?;
    vk.keymap(
        KeymapFormat::XkbV1 as u32,
        keymap_file.as_fd(),
        keymap_bytes.len() as u32,
    );
    queue.roundtrip(&mut VkState)?;

    let mods = if shift {
        MOD_CTRL | MOD_SHIFT
    } else {
        MOD_CTRL
    };

    vk.key(0, KEY_LEFTCTRL, 1);
    if shift {
        vk.key(1, KEY_LEFTSHIFT, 1);
    }
    vk.modifiers(mods, 0, 0, 0);
    vk.key(2, KEY_V, 1);
    queue.roundtrip(&mut VkState)?;

    std::thread::sleep(Duration::from_millis(30));

    vk.key(32, KEY_V, 0);
    if shift {
        vk.key(33, KEY_LEFTSHIFT, 0);
    }
    vk.key(34, KEY_LEFTCTRL, 0);
    vk.modifiers(0, 0, 0, 0);
    queue.roundtrip(&mut VkState)?;

    // Allow target clients (e.g. Electron/GTK/IME) to consume events before tearing down the virtual keyboard.
    std::thread::sleep(Duration::from_millis(30));

    vk.destroy();
    queue.roundtrip(&mut VkState)?;
    Ok(())
}

fn create_temp_keymap_file(bytes: &[u8]) -> Result<std::fs::File> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = dir.join(format!("clipflow-keymap-{}.xkb", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("failed to create keymap file {}", path.display()))?;
    // Unlink immediately so no leftover file remains on disk while fd stays valid.
    let _ = std::fs::remove_file(&path);
    file.write_all(bytes)?;
    Ok(file)
}

fn paste_via_wtype(shift: bool) -> Result<()> {
    let mut cmd = Command::new("wtype");
    cmd.args(["-M", "ctrl"]);
    if shift {
        cmd.args(["-M", "shift"]);
    }
    cmd.args(["-k", "v"]);
    if shift {
        cmd.args(["-m", "shift"]);
    }
    cmd.args(["-m", "ctrl"]);
    let status = cmd.status()?;
    if status.success() {
        Ok(())
    } else {
        bail!("wtype exited with {status}")
    }
}

fn paste_via_ydotool(shift: bool) -> Result<()> {
    let mut args = vec!["key", "29:1"];
    if shift {
        args.push("42:1");
    }
    args.extend(["47:1", "47:0"]);
    if shift {
        args.push("42:0");
    }
    args.push("29:0");
    let status = Command::new("ydotool").args(args).status()?;
    if status.success() {
        Ok(())
    } else {
        bail!("ydotool exited with {status}")
    }
}

fn paste_via_xdotool(shift: bool) -> Result<()> {
    let combo = if shift { "ctrl+shift+v" } else { "ctrl+v" };
    let status = Command::new("xdotool")
        .args(["key", "--clearmodifiers", combo])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        bail!("xdotool exited with {status}")
    }
}
