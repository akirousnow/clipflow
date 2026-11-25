use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};

use crate::core::dedup::{
    MAX_TEXT_BYTES, hash_trimmed_text, is_sensitive_mime_list, normalize_image,
};
use crate::core::model::{
    CapturedClipboard, CapturedContent, ClipPayload, ClipSummary, PageRequest, PageResult,
};
use crate::core::store::{ClipStore, now_epoch_ms};

/// Abstraction over writing clipboard content back to the operating system.
/// Decoupled from GPUI so the engine can be tested deterministically with a mock backend.
pub trait ClipboardWriter: Send + Sync {
    fn write(&self, payload: &ClipPayload) -> Result<()>;
}

/// Outcome of processing a captured clipboard event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestOutcome {
    Saved(ClipSummary),
    IgnoredSensitive,
    IgnoredEmpty,
    IgnoredTooLarge,
    SuppressedSelfWrite,
    /// Identical content re-emitted within milliseconds (e.g. XWayland clipboard bridge echo).
    CoalescedBridgeEcho,
}

#[derive(Debug, Clone)]
struct SelfWriteGuard {
    content_hash: String,
    expires_at: Instant,
}

/// Minimum interval (ms) between two identical hashes before treating the second as a fresh user copy.
/// Filters out immediate Wayland <-> XWayland bridge (e.g. xwayland-satellite) double selection offers.
const BRIDGE_ECHO_WINDOW_MS: i64 = 250;

/// Thread-safe application core coordinating SQLite persistence, deduplication,
/// sensitive-data filtering, and self-write suppression.
pub struct ClipboardEngine {
    store: Mutex<ClipStore>,
    writer: Arc<dyn ClipboardWriter>,
    self_write_guard: Mutex<Option<SelfWriteGuard>>,
    last_ingested: Mutex<Option<(String, i64)>>,
    self_write_ttl: Duration,
}

impl ClipboardEngine {
    pub fn new(store: ClipStore, writer: Arc<dyn ClipboardWriter>) -> Self {
        Self {
            store: Mutex::new(store),
            writer,
            self_write_guard: Mutex::new(None),
            last_ingested: Mutex::new(None),
            self_write_ttl: Duration::from_millis(2500),
        }
    }

    /// Processes a new clipboard event from the background OS listener.
    pub fn ingest(&self, capture: CapturedClipboard) -> Result<IngestOutcome> {
        self.ingest_at(capture, now_epoch_ms())
    }

    pub fn ingest_at(&self, capture: CapturedClipboard, timestamp_ms: i64) -> Result<IngestOutcome> {
        if is_sensitive_mime_list(&capture.mime_types) {
            return Ok(IngestOutcome::IgnoredSensitive);
        }

        match capture.content {
            CapturedContent::Text(text) => {
                if text.len() > MAX_TEXT_BYTES {
                    return Ok(IngestOutcome::IgnoredTooLarge);
                }
                let Some(hash) = hash_trimmed_text(&text) else {
                    return Ok(IngestOutcome::IgnoredEmpty);
                };
                if self.check_and_consume_self_write(&hash) {
                    return Ok(IngestOutcome::SuppressedSelfWrite);
                }
                if self.is_bridge_echo(&hash, timestamp_ms) {
                    return Ok(IngestOutcome::CoalescedBridgeEcho);
                }

                let store = self
                    .store
                    .lock()
                    .map_err(|_| anyhow!("store mutex poisoned"))?;
                match store.upsert_text_at(&text, timestamp_ms)? {
                    Some(summary) => {
                        self.record_ingested(&hash, timestamp_ms);
                        Ok(IngestOutcome::Saved(summary))
                    }
                    None => Ok(IngestOutcome::IgnoredEmpty),
                }
            }
            CapturedContent::ImageBytes { bytes, .. } => {
                if bytes.is_empty() {
                    return Ok(IngestOutcome::IgnoredEmpty);
                }
                if bytes.len() > crate::core::dedup::MAX_IMAGE_BYTES {
                    return Ok(IngestOutcome::IgnoredTooLarge);
                }
                let normalized = normalize_image(&bytes)?;
                if self.check_and_consume_self_write(&normalized.content_hash) {
                    return Ok(IngestOutcome::SuppressedSelfWrite);
                }
                if self.is_bridge_echo(&normalized.content_hash, timestamp_ms) {
                    return Ok(IngestOutcome::CoalescedBridgeEcho);
                }

                let store = self
                    .store
                    .lock()
                    .map_err(|_| anyhow!("store mutex poisoned"))?;
                let summary = store.upsert_normalized_image_at(&normalized, timestamp_ms)?;
                self.record_ingested(&normalized.content_hash, timestamp_ms);
                Ok(IngestOutcome::Saved(summary))
            }
        }
    }

    /// Loads an item's full payload, arms self-write suppression, writes it to the OS clipboard,
    /// and bumps its recency in SQLite.
    pub fn write_item_to_clipboard(&self, id: i64) -> Result<bool> {
        self.write_item_to_clipboard_at(id, now_epoch_ms())
    }

    pub fn write_item_to_clipboard_at(&self, id: i64, timestamp_ms: i64) -> Result<bool> {
        let loaded = {
            let store = self
                .store
                .lock()
                .map_err(|_| anyhow!("store mutex poisoned"))?;
            store.load_payload(id)?
        };

        let Some((payload, content_hash)) = loaded else {
            return Ok(false);
        };

        self.arm_self_write(&content_hash);
        if let Err(err) = self.writer.write(&payload) {
            self.clear_self_write();
            return Err(err).context("failed to write selected clip to system clipboard");
        }

        let store = self
            .store
            .lock()
            .map_err(|_| anyhow!("store mutex poisoned"))?;
        store.touch_item_at(id, timestamp_ms)?;
        Ok(true)
    }

    pub fn query_page(&self, req: &PageRequest) -> Result<PageResult> {
        let store = self
            .store
            .lock()
            .map_err(|_| anyhow!("store mutex poisoned"))?;
        store.query_page(req)
    }

    pub fn toggle_favorite(&self, id: i64) -> Result<Option<bool>> {
        let store = self
            .store
            .lock()
            .map_err(|_| anyhow!("store mutex poisoned"))?;
        store.toggle_favorite(id)
    }

    pub fn delete_item(&self, id: i64) -> Result<bool> {
        let store = self
            .store
            .lock()
            .map_err(|_| anyhow!("store mutex poisoned"))?;
        store.delete_item(id)
    }

    pub fn counts(&self) -> Result<(usize, usize)> {
        let store = self
            .store
            .lock()
            .map_err(|_| anyhow!("store mutex poisoned"))?;
        store.counts()
    }

    pub fn seed_benchmark_items(&self, count: usize) -> Result<usize> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| anyhow!("store mutex poisoned"))?;
        store.seed_benchmark_items(count)
    }

    fn is_bridge_echo(&self, content_hash: &str, timestamp_ms: i64) -> bool {
        let Ok(last) = self.last_ingested.lock() else {
            return false;
        };
        match last.as_ref() {
            Some((prev_hash, prev_ts)) => {
                prev_hash == content_hash
                    && timestamp_ms.saturating_sub(*prev_ts) < BRIDGE_ECHO_WINDOW_MS
            }
            None => false,
        }
    }

    fn record_ingested(&self, content_hash: &str, timestamp_ms: i64) {
        if let Ok(mut last) = self.last_ingested.lock() {
            *last = Some((content_hash.to_string(), timestamp_ms));
        }
    }

    fn arm_self_write(&self, content_hash: &str) {
        if let Ok(mut guard) = self.self_write_guard.lock() {
            *guard = Some(SelfWriteGuard {
                content_hash: content_hash.to_string(),
                expires_at: Instant::now() + self.self_write_ttl,
            });
        }
    }

    fn clear_self_write(&self) {
        if let Ok(mut guard) = self.self_write_guard.lock() {
            *guard = None;
        }
    }

    fn check_and_consume_self_write(&self, content_hash: &str) -> bool {
        let Ok(mut guard) = self.self_write_guard.lock() else {
            return false;
        };
        let now = Instant::now();
        match guard.as_ref() {
            Some(active) if active.expires_at > now && active.content_hash == content_hash => {
                // Keep guard alive briefly? On Wayland a single wl-copy can emit 1 selection event,
                // or occasionally 2 (offer + re-offer), so we leave it valid until expires_at
                // or until a DIFFERENT hash arrives.
                true
            }
            Some(active) if active.expires_at <= now || active.content_hash != content_hash => {
                *guard = None;
                false
            }
            _ => false,
        }
    }
}
