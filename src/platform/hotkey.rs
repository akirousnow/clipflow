use async_channel::Sender;
use futures_util::StreamExt;

use crate::platform::ipc::AppCommand;

/// Registers a global toggle shortcut via XDG Desktop Portal (`org.freedesktop.portal.GlobalShortcuts`).
/// Works seamlessly on Wayland compositors whose portal backend supports GlobalShortcuts (KDE, GNOME, Hyprland, etc.).
/// On compositors without portal GlobalShortcuts (e.g. minimal wlroots/Niri setups), this logs cleanly and
/// users can bind `clipflow toggle` directly in their compositor config.
pub fn spawn_global_shortcut_listener(preferred_trigger: String, tx: Sender<AppCommand>) {
    std::thread::Builder::new()
        .name("clipflow-hotkey".to_string())
        .spawn(move || {
            smol::block_on(async move {
                if let Err(err) = run_portal_shortcuts(&preferred_trigger, tx).await {
                    eprintln!(
                        "[clipflow] XDG GlobalShortcuts portal unavailable ({err}); \
                         use `clipflow toggle` in your compositor keybindings."
                    );
                }
            });
        })
        .ok();
}

async fn run_portal_shortcuts(preferred_trigger: &str, tx: Sender<AppCommand>) -> ashpd::Result<()> {
    use ashpd::desktop::global_shortcuts::{
        BindShortcutsOptions, GlobalShortcuts, NewShortcut,
    };
    use ashpd::desktop::CreateSessionOptions;

    let portal = GlobalShortcuts::new().await?;
    let session = portal.create_session(CreateSessionOptions::default()).await?;

    let shortcut = NewShortcut::new("toggle", "Toggle ClipFlow clipboard history")
        .preferred_trigger(preferred_trigger);

    let _bound = portal
        .bind_shortcuts(&session, &[shortcut], None, BindShortcutsOptions::default())
        .await?
        .response()?;

    let mut activated = portal.receive_activated().await?;
    while let Some(event) = activated.next().await {
        if event.shortcut_id() == "toggle" && tx.send(AppCommand::Toggle).await.is_err() {
            break;
        }
    }

    Ok(())
}
