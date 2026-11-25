use std::collections::HashMap;
use std::io::Read;
use std::os::fd::AsFd;

use anyhow::{Context, Result, anyhow};
use async_channel::Sender;
use wayland_client::backend::ObjectId;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
};
use wl_clipboard_rs::copy::{ClipboardType, MimeType, Options, Seat, ServeRequests, Source};

use crate::core::dedup::{MAX_IMAGE_BYTES, MAX_TEXT_BYTES, is_sensitive_mime_list};
use crate::core::engine::ClipboardWriter;
use crate::core::model::{CapturedClipboard, CapturedContent, ClipPayload};

/// Writes text or PNG images to the Wayland regular clipboard using `wl-clipboard-rs`.
/// `prepare_copy` sets the selection synchronously before returning, while `serve()` runs on
/// a background thread until another client replaces the selection.
#[derive(Default)]
pub struct WaylandClipboardWriter;

impl WaylandClipboardWriter {
    pub fn new() -> Self {
        Self
    }
}

impl ClipboardWriter for WaylandClipboardWriter {
    fn write(&self, payload: &ClipPayload) -> Result<()> {
        let mut opts = Options::new();
        opts.clipboard(ClipboardType::Regular)
            .seat(Seat::All)
            .foreground(true)
            .serve_requests(ServeRequests::Unlimited);

        let prepared = match payload {
            ClipPayload::Text(text) => opts
                .prepare_copy(Source::Bytes(text.as_bytes().into()), MimeType::Text)
                .map_err(|e| anyhow!("wl-clipboard-rs prepare_copy(text) failed: {e}"))?,
            ClipPayload::Image { png_bytes, .. } => opts
                .prepare_copy(
                    Source::Bytes(png_bytes.clone().into_boxed_slice()),
                    MimeType::Specific("image/png".to_string()),
                )
                .map_err(|e| anyhow!("wl-clipboard-rs prepare_copy(image) failed: {e}"))?,
        };

        std::thread::Builder::new()
            .name("clipflow-wl-serve".to_string())
            .spawn(move || {
                let _ = prepared.serve();
            })
            .context("failed to spawn Wayland clipboard server thread")?;

        Ok(())
    }
}

struct WatcherState {
    tx: Sender<CapturedClipboard>,
    pending_mimes: HashMap<ObjectId, Vec<String>>,
    current_selection: Option<ZwlrDataControlOfferV1>,
    current_primary: Option<ZwlrDataControlOfferV1>,
    finished: bool,
}

impl WatcherState {
    fn handle_selection(
        &mut self,
        offer: Option<ZwlrDataControlOfferV1>,
        conn: &Connection,
    ) {
        if let Some(prev) = self.current_selection.take() {
            prev.destroy();
        }

        let Some(offer) = offer else {
            return;
        };

        let mimes = self
            .pending_mimes
            .remove(&offer.id())
            .unwrap_or_default();

        if mimes.is_empty() || is_sensitive_mime_list(&mimes) {
            self.current_selection = Some(offer);
            return;
        }

        let Some((chosen_mime, is_image)) = choose_mime(&mimes) else {
            self.current_selection = Some(offer);
            return;
        };

        let Ok((mut reader, writer)) = std::io::pipe() else {
            self.current_selection = Some(offer);
            return;
        };

        offer.receive(chosen_mime.clone(), writer.as_fd());
        drop(writer);
        let _ = conn.flush();

        // Keep offer alive until the next selection event per wlr-data-control spec.
        self.current_selection = Some(offer);

        let tx = self.tx.clone();
        let max_bytes = if is_image {
            MAX_IMAGE_BYTES
        } else {
            MAX_TEXT_BYTES
        };

        std::thread::Builder::new()
            .name("clipflow-offer-read".to_string())
            .spawn(move || {
                let mut buf = Vec::new();
                let mut limited = reader.by_ref().take((max_bytes + 1) as u64);
                if limited.read_to_end(&mut buf).is_err() || buf.is_empty() || buf.len() > max_bytes
                {
                    return;
                }

                let content = if is_image {
                    CapturedContent::ImageBytes {
                        mime: chosen_mime,
                        bytes: buf,
                    }
                } else {
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    CapturedContent::Text(text)
                };

                let _ = tx.send_blocking(CapturedClipboard {
                    mime_types: mimes,
                    content,
                });
            })
            .ok();
    }
}

fn choose_mime(mimes: &[String]) -> Option<(String, bool)> {
    const TEXT_PRIORITY: &[&str] = &[
        "text/plain;charset=utf-8",
        "UTF8_STRING",
        "text/plain",
    ];
    for candidate in TEXT_PRIORITY {
        if let Some(found) = mimes.iter().find(|m| m.eq_ignore_ascii_case(candidate)) {
            return Some((found.clone(), false));
        }
    }

    const IMAGE_PRIORITY: &[&str] = &[
        "image/png",
        "image/webp",
        "image/jpeg",
        "image/bmp",
        "image/gif",
    ];
    for candidate in IMAGE_PRIORITY {
        if let Some(found) = mimes.iter().find(|m| m.eq_ignore_ascii_case(candidate)) {
            return Some((found.clone(), true));
        }
    }

    None
}

impl Dispatch<WlRegistry, GlobalListContents> for WatcherState {
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

impl Dispatch<WlSeat, ()> for WatcherState {
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

impl Dispatch<ZwlrDataControlManagerV1, ()> for WatcherState {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlManagerV1,
        _: wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for WatcherState {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        conn: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                state.pending_mimes.insert(id.id(), Vec::new());
            }
            zwlr_data_control_device_v1::Event::Selection { id } => {
                state.handle_selection(id, conn);
            }
            zwlr_data_control_device_v1::Event::PrimarySelection { id } => {
                if let Some(prev) = state.current_primary.take() {
                    prev.destroy();
                }
                if let Some(offer) = id {
                    state.pending_mimes.remove(&offer.id());
                    state.current_primary = Some(offer);
                }
            }
            zwlr_data_control_device_v1::Event::Finished => {
                state.finished = true;
            }
            _ => {}
        }
    }

    event_created_child!(WatcherState, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for WatcherState {
    fn event(
        state: &mut Self,
        offer: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event
            && let Some(list) = state.pending_mimes.get_mut(&offer.id())
        {
            list.push(mime_type);
        }
    }
}

/// Runs a zero-polling, event-driven Wayland clipboard watcher using `zwlr_data_control_manager_v1`.
pub fn run_wayland_watcher(tx: Sender<CapturedClipboard>) -> Result<()> {
    let conn = Connection::connect_to_env().context("failed to connect to Wayland display")?;
    let (globals, mut queue) = registry_queue_init::<WatcherState>(&conn)?;
    let qh = queue.handle();

    let seat: WlSeat = globals
        .bind(&qh, 1..=9, ())
        .map_err(|e| anyhow!("failed to bind wl_seat: {e}"))?;
    let manager: ZwlrDataControlManagerV1 = globals
        .bind(&qh, 1..=2, ())
        .map_err(|e| anyhow!("compositor does not expose zwlr_data_control_manager_v1: {e}"))?;

    let _device = manager.get_data_device(&seat, &qh, ());
    let mut state = WatcherState {
        tx,
        pending_mimes: HashMap::new(),
        current_selection: None,
        current_primary: None,
        finished: false,
    };

    while !state.finished && !state.tx.is_closed() {
        queue.blocking_dispatch(&mut state)?;
    }

    Ok(())
}
