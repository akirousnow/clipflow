use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde_json::Value;

const SCREEN_MARGIN: f64 = 12.0;
const CARET_GAP: f64 = 8.0;
/// Maximum age for an AT-SPI2 caret/input rect before we treat it as stale and fall back.
const CARET_FRESHNESS: Duration = Duration::from_secs(30);

/// Logical screen rectangle (top-left origin, in logical pixels).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Logical size of the currently focused output.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputRect {
    pub width: f64,
    pub height: f64,
}

/// Computed top-left screen coordinates for the floating ClipFlow window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PopupPlacement {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy)]
struct CachedCaret {
    rect: ScreenRect,
    updated_at: Instant,
}

/// Background tracker that listens to AT-SPI2 focus/caret events with 0% idle CPU and caches
/// the latest focused input / caret rectangle in memory for zero-latency popup placement.
#[derive(Clone, Default)]
pub struct CaretTracker {
    latest: Arc<Mutex<Option<CachedCaret>>>,
}

impl CaretTracker {
    pub fn spawn() -> Self {
        let tracker = Self::default();
        let latest = tracker.latest.clone();
        std::thread::Builder::new()
            .name("clipflow-atspi".to_string())
            .spawn(move || {
                smol::block_on(async move {
                    if let Err(err) = run_atspi_listener(latest).await {
                        eprintln!("[clipflow] AT-SPI2 caret tracker inactive: {err}");
                    }
                });
            })
            .ok();
        tracker
    }

    /// Returns the most recent fresh caret/input rectangle if available.
    pub fn recent_caret(&self) -> Option<ScreenRect> {
        let guard = self.latest.lock().ok()?;
        let cached = (*guard)?;
        if cached.updated_at.elapsed() <= CARET_FRESHNESS {
            Some(cached.rect)
        } else {
            None
        }
    }

    /// Captures the desired popup placement right BEFORE opening the ClipFlow window
    /// (while the target application still has focus).
    pub fn resolve_placement(&self, popup_w: f64, popup_h: f64) -> Option<PopupPlacement> {
        let output = query_niri_focused_output().unwrap_or(OutputRect {
            width: 1920.0,
            height: 1080.0,
        });
        let focused_win = query_niri_focused_window_rect();
        let caret = self
            .recent_caret()
            .filter(|rect| is_rect_plausible(*rect, output, focused_win));

        Some(compute_popup_placement(
            popup_w,
            popup_h,
            output,
            caret,
            focused_win,
        ))
    }
}

/// Pure placement calculator:
/// 1. Places popup directly below the focused caret/input box (flipping above if near bottom edge).
/// 2. Falls back to centering over the focused application window tile.
/// 3. Falls back to centering on the focused output.
///
/// Always clamps cleanly inside the output bounds with `SCREEN_MARGIN`.
pub fn compute_popup_placement(
    popup_w: f64,
    popup_h: f64,
    output: OutputRect,
    caret: Option<ScreenRect>,
    focused_window: Option<ScreenRect>,
) -> PopupPlacement {
    let max_x = (output.width - popup_w - SCREEN_MARGIN).max(SCREEN_MARGIN);
    let max_y = (output.height - popup_h - SCREEN_MARGIN).max(SCREEN_MARGIN);

    let (raw_x, raw_y) = if let Some(c) = caret {
        let below_y = c.y + c.height + CARET_GAP;
        let y = if below_y + popup_h + SCREEN_MARGIN <= output.height {
            below_y
        } else {
            let above_y = c.y - popup_h - CARET_GAP;
            if above_y >= SCREEN_MARGIN {
                above_y
            } else {
                below_y
            }
        };
        (c.x, y)
    } else if let Some(w) = focused_window {
        (
            w.x + (w.width - popup_w) * 0.5,
            w.y + (w.height - popup_h) * 0.5,
        )
    } else {
        (
            (output.width - popup_w) * 0.5,
            (output.height - popup_h) * 0.5,
        )
    };

    PopupPlacement {
        x: raw_x.clamp(SCREEN_MARGIN, max_x).round(),
        y: raw_y.clamp(SCREEN_MARGIN, max_y).round(),
    }
}

fn is_rect_plausible(
    rect: ScreenRect,
    output: OutputRect,
    focused_win: Option<ScreenRect>,
) -> bool {
    if rect.width < 0.0 || rect.height < 0.0 {
        return false;
    }
    // Reject degenerate (0,0,0,0) coordinates that some toolkits report for unmapped widgets.
    if rect.x == 0.0 && rect.y == 0.0 && rect.width == 0.0 && rect.height == 0.0 {
        return false;
    }
    if rect.x < -50.0 || rect.y < -50.0 || rect.x > output.width + 50.0 || rect.y > output.height + 50.0 {
        return false;
    }
    if let Some(win) = focused_win {
        let pad = 40.0;
        let in_x = rect.x >= win.x - pad && rect.x <= win.x + win.width + pad;
        let in_y = rect.y >= win.y - pad && rect.y <= win.y + win.height + pad;
        if !in_x || !in_y {
            return false;
        }
    }
    true
}

/// Moves the newly opened floating `clipflow` window on Niri to `(placement.x, placement.y)`
/// via direct `$NIRI_SOCKET` IPC as soon as Niri registers the window (~4–15 ms).
pub fn schedule_niri_window_position(placement: PopupPlacement) {
    if std::env::var_os("NIRI_SOCKET").is_none() {
        return;
    }
    std::thread::Builder::new()
        .name("clipflow-position".to_string())
        .spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(250);
            while Instant::now() < deadline {
                if let Some(win_id) = find_niri_clipflow_window_id() {
                    let req = serde_json::json!({
                        "Action": {
                            "MoveFloatingWindow": {
                                "id": win_id,
                                "x": { "SetFixed": placement.x },
                                "y": { "SetFixed": placement.y }
                            }
                        }
                    });
                    let _ = send_niri_request(&req.to_string());
                    return;
                }
                std::thread::sleep(Duration::from_millis(3));
            }
        })
        .ok();
}

fn send_niri_request(request_line: &str) -> Option<Value> {
    let sock_path = std::env::var_os("NIRI_SOCKET")?;
    let mut stream = UnixStream::connect(sock_path).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(120)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(120)));
    writeln!(stream, "{request_line}").ok()?;
    stream.flush().ok()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

fn query_niri_focused_output() -> Option<OutputRect> {
    let resp = send_niri_request("\"FocusedOutput\"")?;
    let logical = resp.get("Ok")?.get("FocusedOutput")?.get("logical")?;
    let width = logical.get("width")?.as_f64()?;
    let height = logical.get("height")?.as_f64()?;
    Some(OutputRect { width, height })
}

fn query_niri_focused_window_rect() -> Option<ScreenRect> {
    let resp = send_niri_request("\"FocusedWindow\"")?;
    let win = resp.get("Ok")?.get("FocusedWindow")?;
    if win.is_null() {
        return None;
    }
    // Avoid using ClipFlow itself if already focused.
    if win.get("app_id").and_then(Value::as_str) == Some("clipflow") {
        return None;
    }
    let layout = win.get("layout")?;
    let win_size = layout.get("window_size")?.as_array()?;
    let width = win_size.first()?.as_f64()?;
    let height = win_size.get(1)?.as_f64()?;

    let (tile_x, tile_y) = layout
        .get("tile_pos_in_workspace_view")
        .and_then(Value::as_array)
        .and_then(|a| Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?)))
        .unwrap_or((0.0, 0.0));
    let (off_x, off_y) = layout
        .get("window_offset_in_tile")
        .and_then(Value::as_array)
        .and_then(|a| Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?)))
        .unwrap_or((0.0, 0.0));

    Some(ScreenRect {
        x: tile_x + off_x,
        y: tile_y + off_y,
        width,
        height,
    })
}

fn find_niri_clipflow_window_id() -> Option<u64> {
    let resp = send_niri_request("\"Windows\"")?;
    let windows = resp.get("Ok")?.get("Windows")?.as_array()?;
    for win in windows {
        if win.get("app_id").and_then(Value::as_str) == Some("clipflow") {
            return win.get("id")?.as_u64();
        }
    }
    None
}

async fn run_atspi_listener(latest: Arc<Mutex<Option<CachedCaret>>>) -> anyhow::Result<()> {
    let session = zbus::Connection::session().await?;
    let bus_proxy = zbus::Proxy::new(
        &session,
        "org.a11y.Bus",
        "/org/a11y/bus",
        "org.a11y.Bus",
    )
    .await?;
    let address: String = bus_proxy.call("GetAddress", &()).await?;

    let a11y_conn = zbus::connection::Builder::address(address.as_str())?
        .build()
        .await?;

    // Register AT-SPI2 event listeners for caret movement and widget focus changes.
    let registry = zbus::Proxy::new(
        &a11y_conn,
        "org.a11y.atspi.Registry",
        "/org/a11y/atspi/registry",
        "org.a11y.atspi.Registry",
    )
    .await?;
    let _ = registry
        .call::<_, _, ()>("RegisterEvent", &("object:text-caret-moved",))
        .await;
    let _ = registry
        .call::<_, _, ()>("RegisterEvent", &("object:state-changed:focused",))
        .await;

    // Add D-Bus match rules so the accessibility daemon delivers those signals to us.
    let dbus_proxy = zbus::fdo::DBusProxy::new(&a11y_conn).await?;
    let _ = dbus_proxy
        .add_match_rule(
            zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .interface("org.a11y.atspi.Event.Object")?
                .member("TextCaretMoved")?
                .build(),
        )
        .await;
    let _ = dbus_proxy
        .add_match_rule(
            zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .interface("org.a11y.atspi.Event.Object")?
                .member("StateChanged")?
                .build(),
        )
        .await;

    let mut stream = zbus::MessageStream::from(&a11y_conn);
    while let Some(Ok(msg)) = stream.next().await {
        let header = msg.header();
        if header.message_type() != zbus::message::Type::Signal {
            continue;
        }
        let Some(member) = header.member() else {
            continue;
        };
        let member_str = member.as_str();
        if member_str != "TextCaretMoved" && member_str != "StateChanged" {
            continue;
        }
        let (Some(sender), Some(path)) = (header.sender(), header.path()) else {
            continue;
        };

        // AT-SPI2 Object event signature: (siiv(so)) -> (detail, detail1, detail2, any_data, app)
        let Ok((detail, detail1, _detail2, _any, _app)) =
            msg.body()
                .deserialize::<(String, i32, i32, zbus::zvariant::OwnedValue, (String, zbus::zvariant::OwnedObjectPath))>()
        else {
            continue;
        };

        if member_str == "StateChanged" && (detail != "focused" || detail1 != 1) {
            continue;
        }

        let sender_str = sender.to_string();
        let path_str = path.to_string();
        let rect = if member_str == "TextCaretMoved" {
            query_atspi_caret_rect(&a11y_conn, &sender_str, &path_str, detail1)
                .await
                .or(query_atspi_component_rect(&a11y_conn, &sender_str, &path_str).await)
        } else {
            query_atspi_focused_text_or_component(&a11y_conn, &sender_str, &path_str).await
        };

        if let Some(rect) = rect
            && let Ok(mut slot) = latest.lock()
        {
            *slot = Some(CachedCaret {
                rect,
                updated_at: Instant::now(),
            });
        }
    }

    Ok(())
}

async fn query_atspi_focused_text_or_component(
    conn: &zbus::Connection,
    dest: &str,
    path: &str,
) -> Option<ScreenRect> {
    if let Ok(text_proxy) = zbus::Proxy::new(conn, dest, path, "org.a11y.atspi.Text").await
        && let Ok(offset) = text_proxy.get_property::<i32>("CaretOffset").await
        && let Some(rect) = query_atspi_caret_rect(conn, dest, path, offset).await
    {
        return Some(rect);
    }
    query_atspi_component_rect(conn, dest, path).await
}

async fn query_atspi_caret_rect(
    conn: &zbus::Connection,
    dest: &str,
    path: &str,
    offset: i32,
) -> Option<ScreenRect> {
    let proxy = zbus::Proxy::new(conn, dest, path, "org.a11y.atspi.Text")
        .await
        .ok()?;
    let safe_offset = offset.max(0);
    // coord_type 0 = ATSPI_COORD_TYPE_SCREEN
    let (x, y, w, h): (i32, i32, i32, i32) = proxy
        .call("GetCharacterExtents", &(safe_offset, 0u32))
        .await
        .ok()?;
    if x == 0 && y == 0 && w == 0 && h == 0 {
        return None;
    }
    Some(ScreenRect {
        x: x as f64,
        y: y as f64,
        width: w.max(1) as f64,
        height: h.max(16) as f64,
    })
}

async fn query_atspi_component_rect(
    conn: &zbus::Connection,
    dest: &str,
    path: &str,
) -> Option<ScreenRect> {
    let proxy = zbus::Proxy::new(conn, dest, path, "org.a11y.atspi.Component")
        .await
        .ok()?;
    let (x, y, w, h): (i32, i32, i32, i32) = proxy.call("GetExtents", &(0u32,)).await.ok()?;
    if w <= 0 || h <= 0 || (x == 0 && y == 0 && w > 1800 && h > 900) {
        return None;
    }
    Some(ScreenRect {
        x: x as f64,
        y: y as f64,
        width: w as f64,
        height: h as f64,
    })
}
