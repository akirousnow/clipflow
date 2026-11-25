use std::borrow::Cow;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use gpui::{
    App, Application, AssetSource, Bounds, SharedString, WindowBounds, WindowDecorations,
    WindowHandle, WindowKind, WindowOptions, point, prelude::*, px, size,
};

use clipflow::core::engine::{ClipboardEngine, IngestOutcome};
use clipflow::core::store::ClipStore;
use clipflow::platform::auto_paste::trigger_auto_paste;
use clipflow::platform::caret_position::{
    CaretTracker, PopupPlacement, schedule_niri_window_position,
};
use clipflow::platform::hotkey::spawn_global_shortcut_listener;
use clipflow::platform::ipc::{AppCommand, spawn_ipc_server, try_send_command};
use clipflow::platform::updater::{
    CURRENT_VERSION, check_for_update, configured_repo, perform_self_update,
};
use clipflow::platform::{default_clipboard_writer, spawn_clipboard_watcher};
use clipflow::ui::window::{ClipboardWindow, SharedUpdateState, bind_window_keys};

const POPUP_WIDTH: f64 = 500.0;
const POPUP_HEIGHT: f64 = 560.0;

struct FsAssets;

impl AssetSource for FsAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(Cow::Owned(bytes))),
            Err(_) => Ok(None),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                out.push(SharedString::from(entry.path().to_string_lossy().into_owned()));
            }
        }
        Ok(out)
    }
}

fn default_data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("clipflow")
}

enum CliAction {
    Run(Option<AppCommand>),
    CheckUpdate,
    SelfUpdate,
}

struct CliOptions {
    action: CliAction,
    start_hidden: bool,
    no_blur_dismiss: bool,
    hotkey: String,
    data_dir: PathBuf,
    seed_count: Option<usize>,
}

fn parse_cli() -> CliOptions {
    let mut args = std::env::args().skip(1);
    let mut action = CliAction::Run(None);
    let mut start_hidden = false;
    let mut no_blur_dismiss = false;
    let mut hotkey = "Super+Shift+V".to_string();
    let mut data_dir = default_data_dir();
    let mut seed_count = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "toggle" => action = CliAction::Run(Some(AppCommand::Toggle)),
            "show" => action = CliAction::Run(Some(AppCommand::Show)),
            "hide" => action = CliAction::Run(Some(AppCommand::Hide)),
            "quit" => action = CliAction::Run(Some(AppCommand::Quit)),
            "check-update" => action = CliAction::CheckUpdate,
            "update" => action = CliAction::SelfUpdate,
            "--version" | "-V" => {
                println!("clipflow {CURRENT_VERSION}");
                std::process::exit(0);
            }
            "--daemon" | "-d" => start_hidden = true,
            "--no-blur-dismiss" => no_blur_dismiss = true,
            "--hotkey" => {
                if let Some(val) = args.next() {
                    hotkey = val;
                }
            }
            "--data-dir" => {
                if let Some(val) = args.next() {
                    data_dir = PathBuf::from(val);
                }
            }
            "--seed" => {
                if let Some(val) = args.next() {
                    seed_count = val.parse().ok();
                }
            }
            "--help" | "-h" => {
                println!(
                    "clipflow {CURRENT_VERSION} — Minimalist Linux clipboard manager (GPUI + Rust)\n\n\
                     USAGE:\n  \
                       clipflow [COMMAND] [OPTIONS]\n\n\
                     COMMANDS:\n  \
                       toggle          Toggle the floating window at focused input position\n  \
                       show            Show the floating window\n  \
                       hide            Hide the floating window\n  \
                       quit            Stop the running clipflow daemon\n  \
                       check-update    Check GitHub Releases for a newer version\n  \
                       update          Download, verify SHA-256, replace binary, and restart daemon\n\n\
                     OPTIONS:\n  \
                       -d, --daemon           Start in background without opening the window\n  \
                       --hotkey <TRIGGER>     Preferred XDG GlobalShortcuts trigger (default: Super+Shift+V)\n  \
                       --data-dir <PATH>      Override SQLite & image storage directory\n  \
                       --no-blur-dismiss      Keep window open when focus leaves\n  \
                       --seed <N>             Seed N synthetic benchmark entries into the DB and exit\n  \
                       -V, --version          Print version and exit\n"
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }

    CliOptions {
        action,
        start_hidden,
        no_blur_dismiss,
        hotkey,
        data_dir,
        seed_count,
    }
}

fn open_floating_window(
    engine: Arc<ClipboardEngine>,
    available_update: SharedUpdateState,
    placement: Option<PopupPlacement>,
    dismiss_on_blur: bool,
    cx: &mut App,
) -> Result<WindowHandle<ClipboardWindow>> {
    let prof = std::env::var_os("CLIPFLOW_PROFILE").is_some();
    let t0 = Instant::now();

    let bounds = match placement {
        Some(pos) => Bounds::new(
            point(px(pos.x as f32), px(pos.y as f32)),
            size(px(POPUP_WIDTH as f32), px(POPUP_HEIGHT as f32)),
        ),
        None => Bounds::centered(
            None,
            size(px(POPUP_WIDTH as f32), px(POPUP_HEIGHT as f32)),
            cx,
        ),
    };

    if let Some(pos) = placement {
        schedule_niri_window_position(pos);
    }

    let handle = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: None,
            focus: true,
            show: true,
            kind: WindowKind::Floating,
            is_movable: true,
            is_resizable: false,
            is_minimizable: false,
            app_id: Some("clipflow".to_string()),
            window_decorations: Some(WindowDecorations::Client),
            ..Default::default()
        },
        move |window, cx| {
            cx.activate(true);
            cx.new(|cx| {
                ClipboardWindow::new(engine, available_update, dismiss_on_blur, window, cx)
            })
        },
    );
    if prof {
        eprintln!(
            "[profile] persistent open_window: {:.1} ms",
            t0.elapsed().as_secs_f64() * 1000.0
        );
    }
    handle
}

fn spawn_background_update_check(available_update: SharedUpdateState) {
    if std::env::var_os("CLIPFLOW_NO_UPDATE_CHECK").is_some() {
        return;
    }
    std::thread::Builder::new()
        .name("clipflow-update-check".to_string())
        .spawn(move || {
            // Wait briefly so startup stays completely idle, then re-check every 12 hours.
            std::thread::sleep(Duration::from_secs(15));
            loop {
                if let Ok(Some(info)) = check_for_update()
                    && let Ok(mut slot) = available_update.lock()
                {
                    *slot = Some(info);
                }
                std::thread::sleep(Duration::from_secs(12 * 3600));
            }
        })
        .ok();
}

fn main() -> Result<()> {
    let opts = parse_cli();

    if let Some(count) = opts.seed_count {
        let mut store = ClipStore::open(&opts.data_dir)?;
        let inserted = store.seed_benchmark_items(count)?;
        println!(
            "Seeded {inserted} entries into {}",
            opts.data_dir.join("clipflow.db").display()
        );
        return Ok(());
    }

    match opts.action {
        CliAction::CheckUpdate => {
            println!("Checking {} (current: v{CURRENT_VERSION})...", configured_repo());
            match check_for_update()? {
                Some(info) => {
                    println!(
                        "Update available: v{} -> v{}\nRelease: {}\nRun `clipflow update` to upgrade.",
                        info.current_version, info.latest_version, info.release_url
                    );
                }
                None => {
                    println!("clipflow v{CURRENT_VERSION} is already up to date.");
                }
            }
            return Ok(());
        }
        CliAction::SelfUpdate => {
            println!("Checking {} for updates...", configured_repo());
            let Some(info) = check_for_update()? else {
                println!("clipflow v{CURRENT_VERSION} is already up to date.");
                return Ok(());
            };
            println!(
                "Downloading {} ({})...",
                info.tag_name, info.asset_name
            );
            let updated_exe = perform_self_update(&info)?;
            println!(
                "Updated {} to v{} (SHA-256 verified).",
                updated_exe.display(),
                info.latest_version
            );
            if try_send_command(AppCommand::Quit).unwrap_or(false) {
                std::thread::sleep(Duration::from_millis(250));
                let _ = std::process::Command::new(&updated_exe)
                    .arg("--daemon")
                    .spawn();
                println!("Restarted clipflow daemon.");
            }
            return Ok(());
        }
        CliAction::Run(cmd_opt) => {
            if let Some(cmd) = cmd_opt {
                if try_send_command(cmd).unwrap_or(false) {
                    return Ok(());
                }
                if matches!(cmd, AppCommand::Hide | AppCommand::Quit) {
                    return Ok(());
                }
            } else if !opts.start_hidden && try_send_command(AppCommand::Toggle).unwrap_or(false) {
                return Ok(());
            }
        }
    }

    // Keep the patched Linux GPUI event loop alive when zero windows are open so Vulkan,
    // compiled Blade pipelines, and system fonts stay warm across instant toggle cycles.
    // SAFETY: called at startup on the main thread before spawning any worker threads.
    unsafe {
        std::env::set_var("GPUI_KEEP_ALIVE_NO_WINDOWS", "1");
    }

    let store = ClipStore::open(&opts.data_dir)?;
    let writer = default_clipboard_writer();
    let engine = Arc::new(ClipboardEngine::new(store, writer));
    let caret_tracker = CaretTracker::spawn();
    let available_update: SharedUpdateState = Arc::new(Mutex::new(None));
    spawn_background_update_check(available_update.clone());

    let (cmd_tx, cmd_rx) = async_channel::unbounded::<AppCommand>();
    if let Err(err) = spawn_ipc_server(cmd_tx.clone()) {
        eprintln!("[clipflow] IPC server notice: {err}");
    }
    spawn_global_shortcut_listener(opts.hotkey.clone(), cmd_tx);

    let (clip_tx, clip_rx) = async_channel::unbounded();
    spawn_clipboard_watcher(clip_tx);

    let (ui_refresh_tx, ui_refresh_rx) = async_channel::unbounded::<()>();
    {
        let engine = engine.clone();
        std::thread::Builder::new()
            .name("clipflow-ingest".to_string())
            .spawn(move || {
                while let Ok(capture) = clip_rx.recv_blocking() {
                    match engine.ingest(capture) {
                        Ok(IngestOutcome::Saved(_)) => {
                            let _ = ui_refresh_tx.try_send(());
                        }
                        Ok(_) => {}
                        Err(err) => {
                            eprintln!("[clipflow] ingest error: {err:#}");
                        }
                    }
                }
            })
            .expect("failed to spawn ingest worker");
    }

    let dismiss_on_blur = !opts.no_blur_dismiss;
    let start_hidden = opts.start_hidden;

    Application::new().with_assets(FsAssets).run(move |cx: &mut App| {
        bind_window_keys(cx);

        let active_window: Rc<RefCell<Option<WindowHandle<ClipboardWindow>>>> =
            Rc::new(RefCell::new(None));

        if !start_hidden {
            let placement = caret_tracker.resolve_placement(POPUP_WIDTH, POPUP_HEIGHT);
            if let Ok(handle) = open_floating_window(
                engine.clone(),
                available_update.clone(),
                placement,
                dismiss_on_blur,
                cx,
            ) {
                *active_window.borrow_mut() = Some(handle);
            }
        }

        // Handle IPC / GlobalShortcuts commands on GPUI's foreground executor.
        {
            let engine = engine.clone();
            let active_window = active_window.clone();
            let caret_tracker = caret_tracker.clone();
            let available_update = available_update.clone();
            cx.spawn(async move |cx| {
                while let Ok(cmd) = cmd_rx.recv().await {
                    let _ = cx.update(|cx| {
                        let existing = active_window.borrow().as_ref().copied();
                        let still_open = existing
                            .filter(|h| cx.windows().iter().any(|w| w.window_id() == h.window_id()));

                        match cmd {
                            AppCommand::Toggle => {
                                if let Some(handle) = still_open {
                                    let _ = handle.update(cx, |_, window, _| {
                                        window.remove_window();
                                    });
                                    *active_window.borrow_mut() = None;
                                } else {
                                    let placement = caret_tracker
                                        .resolve_placement(POPUP_WIDTH, POPUP_HEIGHT);
                                    if let Ok(handle) = open_floating_window(
                                        engine.clone(),
                                        available_update.clone(),
                                        placement,
                                        dismiss_on_blur,
                                        cx,
                                    ) {
                                        *active_window.borrow_mut() = Some(handle);
                                    }
                                }
                            }
                            AppCommand::Show => {
                                if let Some(handle) = still_open {
                                    let _ = handle.update(cx, |_, window, cx| {
                                        cx.activate(true);
                                        window.activate_window();
                                    });
                                } else {
                                    let placement = caret_tracker
                                        .resolve_placement(POPUP_WIDTH, POPUP_HEIGHT);
                                    if let Ok(handle) = open_floating_window(
                                        engine.clone(),
                                        available_update.clone(),
                                        placement,
                                        dismiss_on_blur,
                                        cx,
                                    ) {
                                        *active_window.borrow_mut() = Some(handle);
                                    }
                                }
                            }
                            AppCommand::Hide => {
                                if let Some(handle) = still_open {
                                    let _ = handle.update(cx, |_, window, _| {
                                        window.remove_window();
                                    });
                                    *active_window.borrow_mut() = None;
                                }
                            }
                            AppCommand::Activate { id, mode } => {
                                if let Ok(true) = engine.write_item_to_clipboard(id) {
                                    std::thread::spawn(move || {
                                        let _ = trigger_auto_paste(
                                            mode,
                                            Duration::from_millis(80),
                                        );
                                    });
                                }
                            }
                            AppCommand::Quit => {
                                cx.quit();
                            }
                        }
                    });
                }
            })
            .detach();
        }

        // Refresh the floating window smoothly if a new clip arrives while it is open.
        {
            let active_window = active_window.clone();
            cx.spawn(async move |cx| {
                while ui_refresh_rx.recv().await.is_ok() {
                    let _ = cx.update(|cx| {
                        if let Some(handle) = *active_window.borrow() {
                            let _ = handle.update(cx, |view, _, cx| {
                                view.notify_clipboard_updated(cx);
                            });
                        }
                    });
                }
            })
            .detach();
        }
    });

    Ok(())
}
