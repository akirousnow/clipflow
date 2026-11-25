use std::collections::HashSet;
use std::io::Cursor;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use clipflow::core::dedup::MAX_TEXT_BYTES;
use clipflow::core::engine::{ClipboardEngine, ClipboardWriter, IngestOutcome};
use clipflow::core::model::{
    CapturedClipboard, CapturedContent, ClipKind, ClipPayload, PageRequest, ViewTab,
};
use clipflow::core::store::ClipStore;
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use tempfile::TempDir;

#[derive(Default)]
struct MockClipboardWriter {
    written: Mutex<Vec<ClipPayload>>,
}

impl MockClipboardWriter {
    fn written_count(&self) -> usize {
        self.written.lock().unwrap().len()
    }

    fn last_written(&self) -> Option<ClipPayload> {
        self.written.lock().unwrap().last().cloned()
    }
}

impl ClipboardWriter for MockClipboardWriter {
    fn write(&self, payload: &ClipPayload) -> Result<()> {
        self.written.lock().unwrap().push(payload.clone());
        Ok(())
    }
}

fn make_test_image(format: ImageFormat, color: [u8; 4]) -> Vec<u8> {
    let mut img = RgbaImage::new(32, 24);
    for px in img.pixels_mut() {
        *px = Rgba(color);
    }
    let dyn_img = DynamicImage::ImageRgba8(img);
    let mut buf = Cursor::new(Vec::new());
    dyn_img.write_to(&mut buf, format).unwrap();
    buf.into_inner()
}

fn text_capture(text: &str) -> CapturedClipboard {
    CapturedClipboard {
        mime_types: vec!["text/plain;charset=utf-8".to_string()],
        content: CapturedContent::Text(text.to_string()),
    }
}

#[test]
fn trimmed_whitespace_dedup_bumps_recency_and_preserves_favorite() {
    let dir = TempDir::new().unwrap();
    let store = ClipStore::open_in_memory(dir.path()).unwrap();
    let writer = Arc::new(MockClipboardWriter::default());
    let engine = ClipboardEngine::new(store, writer);

    let first = match engine.ingest_at(text_capture("  hello rust  "), 1_000).unwrap() {
        IngestOutcome::Saved(item) => item,
        other => panic!("expected Saved, got {other:?}"),
    };
    engine.ingest_at(text_capture("second item"), 2_000).unwrap();

    // Favorite the first item
    assert_eq!(engine.toggle_favorite(first.id).unwrap(), Some(true));

    // Copy the same logical text with different leading/trailing whitespace
    let updated = match engine
        .ingest_at(text_capture("\n\thello rust\r\n"), 3_000)
        .unwrap()
    {
        IngestOutcome::Saved(item) => item,
        other => panic!("expected Saved, got {other:?}"),
    };

    assert_eq!(updated.id, first.id);
    assert!(updated.is_favorite, "favorite flag must survive deduplication");
    assert_eq!(updated.copy_count, 2);
    assert_eq!(updated.last_copied_at_ms, 3_000);

    let page = engine
        .query_page(&PageRequest {
            tab: ViewTab::All,
            query: String::new(),
            cursor: None,
            limit: 10,
        })
        .unwrap();
    assert_eq!(page.total_count, 2);
    assert_eq!(page.items[0].id, first.id, "deduped item must move to top");
    assert!(page.items[0].is_favorite);
}

#[test]
fn empty_whitespace_and_oversized_text_are_ignored() {
    let dir = TempDir::new().unwrap();
    let store = ClipStore::open_in_memory(dir.path()).unwrap();
    let engine = ClipboardEngine::new(store, Arc::new(MockClipboardWriter::default()));

    assert_eq!(
        engine.ingest_at(text_capture("   \n\t  "), 1_000).unwrap(),
        IngestOutcome::IgnoredEmpty
    );

    let huge = "a".repeat(MAX_TEXT_BYTES + 1);
    assert_eq!(
        engine.ingest_at(text_capture(&huge), 2_000).unwrap(),
        IngestOutcome::IgnoredTooLarge
    );

    assert_eq!(engine.counts().unwrap(), (0, 0));
}

#[test]
fn sensitive_mime_markers_are_never_persisted() {
    let dir = TempDir::new().unwrap();
    let store = ClipStore::open_in_memory(dir.path()).unwrap();
    let engine = ClipboardEngine::new(store, Arc::new(MockClipboardWriter::default()));

    let sensitive_cases = [
        vec![
            "text/plain;charset=utf-8".to_string(),
            "x-kde-passwordManagerHint".to_string(),
        ],
        vec![
            "text/plain".to_string(),
            "org.nspasteboard.ConcealedType".to_string(),
        ],
        vec![
            "text/plain".to_string(),
            "ExcludeClipboardContentFromMonitorProcessing".to_string(),
        ],
    ];

    for (idx, mimes) in sensitive_cases.into_iter().enumerate() {
        let outcome = engine
            .ingest_at(
                CapturedClipboard {
                    mime_types: mimes,
                    content: CapturedContent::Text(format!("super-secret-{idx}")),
                },
                1_000 + idx as i64,
            )
            .unwrap();
        assert_eq!(outcome, IngestOutcome::IgnoredSensitive);
    }

    assert_eq!(engine.counts().unwrap(), (0, 0));
}

#[test]
fn self_write_suppresses_echo_loop_without_blocking_new_copies() {
    let dir = TempDir::new().unwrap();
    let store = ClipStore::open_in_memory(dir.path()).unwrap();
    let writer = Arc::new(MockClipboardWriter::default());
    let engine = ClipboardEngine::new(store, writer.clone());

    let item = match engine.ingest_at(text_capture("echo target"), 1_000).unwrap() {
        IngestOutcome::Saved(item) => item,
        other => panic!("unexpected {other:?}"),
    };

    // Write back to system clipboard
    assert!(engine.write_item_to_clipboard_at(item.id, 2_000).unwrap());
    assert_eq!(writer.written_count(), 1);
    assert_eq!(
        writer.last_written(),
        Some(ClipPayload::Text("echo target".to_string()))
    );

    // Simulated OS watcher echo of the same content must be suppressed
    assert_eq!(
        engine.ingest_at(text_capture("echo target"), 2_010).unwrap(),
        IngestOutcome::SuppressedSelfWrite
    );

    // A genuinely different copy immediately afterward must still be saved
    assert!(matches!(
        engine.ingest_at(text_capture("fresh copy"), 2_020).unwrap(),
        IngestOutcome::Saved(_)
    ));

    assert_eq!(engine.counts().unwrap(), (2, 0));
}

#[test]
fn image_dedup_across_encodings_thumbnails_and_cleanup() {
    let dir = TempDir::new().unwrap();
    let store = ClipStore::open_in_memory(dir.path()).unwrap();
    let writer = Arc::new(MockClipboardWriter::default());
    let engine = ClipboardEngine::new(store, writer.clone());

    let png = make_test_image(ImageFormat::Png, [20, 120, 240, 255]);
    let bmp = make_test_image(ImageFormat::Bmp, [20, 120, 240, 255]);

    let first = match engine
        .ingest_at(
            CapturedClipboard {
                mime_types: vec!["image/png".to_string()],
                content: CapturedContent::ImageBytes {
                    mime: "image/png".to_string(),
                    bytes: png,
                },
            },
            1_000,
        )
        .unwrap()
    {
        IngestOutcome::Saved(item) => item,
        other => panic!("unexpected {other:?}"),
    };

    assert_eq!(first.kind, ClipKind::Image);
    assert_eq!(first.image_width, Some(32));
    assert_eq!(first.image_height, Some(24));
    let thumb = first.thumb_path.clone().expect("thumbnail path expected");
    assert!(thumb.exists(), "thumbnail file must exist on disk");

    engine.toggle_favorite(first.id).unwrap();

    // Same pixels encoded as BMP must deduplicate to the same item and preserve favorite
    let second = match engine
        .ingest_at(
            CapturedClipboard {
                mime_types: vec!["image/bmp".to_string()],
                content: CapturedContent::ImageBytes {
                    mime: "image/bmp".to_string(),
                    bytes: bmp,
                },
            },
            2_000,
        )
        .unwrap()
    {
        IngestOutcome::Saved(item) => item,
        other => panic!("unexpected {other:?}"),
    };

    assert_eq!(second.id, first.id);
    assert!(second.is_favorite);
    assert_eq!(second.copy_count, 2);

    // Activating image loads full PNG bytes and writes to backend
    assert!(engine.write_item_to_clipboard_at(first.id, 3_000).unwrap());
    assert!(matches!(
        writer.last_written(),
        Some(ClipPayload::Image { width: 32, height: 24, .. })
    ));

    // Deleting removes both row and thumbnail file
    assert!(engine.delete_item(first.id).unwrap());
    assert!(!thumb.exists(), "deleted image thumbnail should be removed");
    assert_eq!(engine.counts().unwrap(), (0, 0));
}

#[test]
fn infinite_retention_lazy_keyset_pagination_and_db_search() {
    let dir = TempDir::new().unwrap();
    let mut store = ClipStore::open_in_memory(dir.path()).unwrap();
    store.seed_benchmark_items(1_250).unwrap();
    let engine = ClipboardEngine::new(store, Arc::new(MockClipboardWriter::default()));

    let (all_count, fav_count) = engine.counts().unwrap();
    assert_eq!(all_count, 1_250, "all items must be retained without eviction");
    assert!(fav_count > 50);

    // Walk every single page of 40 items using keyset cursor pagination — zero skips, zero duplicates
    let mut seen = HashSet::new();
    let mut cursor = None;
    let mut pages = 0usize;
    let mut prev_ts = i64::MAX;

    loop {
        let page = engine
            .query_page(&PageRequest {
                tab: ViewTab::All,
                query: String::new(),
                cursor,
                limit: 40,
            })
            .unwrap();
        assert_eq!(page.total_count, 1_250);
        assert!(page.items.len() <= 40);
        for item in &page.items {
            assert!(item.last_copied_at_ms <= prev_ts);
            prev_ts = item.last_copied_at_ms;
            assert!(seen.insert(item.id), "duplicate item across pages");
        }
        pages += 1;
        if !page.has_more {
            assert!(page.next_cursor.is_none());
            break;
        }
        cursor = page.next_cursor;
    }

    assert_eq!(seen.len(), 1_250);
    assert_eq!(pages, 32); // ceil(1250 / 40) = 32

    // DB-backed CJK & multi-token substring search (#42 matches #42, #422, #427; #42： matches uniquely)
    let cjk_page = engine
        .query_page(&PageRequest {
            tab: ViewTab::All,
            query: "剪贴板 #42：".to_string(),
            cursor: None,
            limit: 40,
        })
        .unwrap();
    assert_eq!(cjk_page.total_count, 1);
    assert!(cjk_page.items[0].preview_text.contains("#42："));

    // Favorites tab only returns favorited rows
    let fav_page = engine
        .query_page(&PageRequest {
            tab: ViewTab::Favorites,
            query: String::new(),
            cursor: None,
            limit: 40,
        })
        .unwrap();
    assert_eq!(fav_page.total_count, fav_count);
    assert!(fav_page.items.iter().all(|i| i.is_favorite));
}

#[test]
fn persistence_survives_restart() {
    let dir = TempDir::new().unwrap();
    let png = make_test_image(ImageFormat::Png, [255, 80, 40, 255]);

    let (text_id, img_id) = {
        let store = ClipStore::open(dir.path()).unwrap();
        let text_item = store.upsert_text_at("persist across restart", 1_000).unwrap().unwrap();
        let img_item = store.upsert_image_bytes(&png).unwrap();
        store.toggle_favorite(text_item.id).unwrap();
        (text_item.id, img_item.id)
    };

    // Reopen from disk as after an app restart
    let reopened = ClipStore::open(dir.path()).unwrap();
    assert_eq!(reopened.counts().unwrap(), (2, 1));

    let page = reopened
        .query_page(&PageRequest {
            tab: ViewTab::Favorites,
            query: String::new(),
            cursor: None,
            limit: 10,
        })
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, text_id);
    assert_eq!(page.items[0].preview_text, "persist across restart");

    let (img_payload, _) = reopened.load_payload(img_id).unwrap().unwrap();
    assert!(matches!(
        img_payload,
        ClipPayload::Image { width: 32, height: 24, .. }
    ));
}

#[test]
fn rapid_xwayland_bridge_echo_is_coalesced() {
    let dir = TempDir::new().unwrap();
    let store = ClipStore::open_in_memory(dir.path()).unwrap();
    let engine = ClipboardEngine::new(store, Arc::new(MockClipboardWriter::default()));

    let first = match engine.ingest_at(text_capture("bridge echo"), 10_000).unwrap() {
        IngestOutcome::Saved(s) => s,
        other => panic!("expected Saved, got {other:?}"),
    };
    assert_eq!(first.copy_count, 1);

    // Immediate echo 25ms later (e.g. xwayland-satellite sync) is coalesced cleanly
    assert_eq!(
        engine.ingest_at(text_capture("bridge echo"), 10_025).unwrap(),
        IngestOutcome::CoalescedBridgeEcho
    );

    // Intentional re-copy 500ms later is counted normally
    let second = match engine.ingest_at(text_capture("bridge echo"), 10_500).unwrap() {
        IngestOutcome::Saved(s) => s,
        other => panic!("expected Saved, got {other:?}"),
    };
    assert_eq!(second.copy_count, 2);
}

#[test]
fn caret_popup_placement_anchors_below_flips_above_and_clamps() {
    use clipflow::platform::caret_position::{
        OutputRect, ScreenRect, compute_popup_placement,
    };

    let output = OutputRect {
        width: 2560.0,
        height: 1440.0,
    };

    // Normal caret near upper-left -> placed directly below caret
    let pos = compute_popup_placement(
        500.0,
        560.0,
        output,
        Some(ScreenRect {
            x: 320.0,
            y: 200.0,
            width: 2.0,
            height: 20.0,
        }),
        None,
    );
    assert_eq!(pos.x, 320.0);
    assert_eq!(pos.y, 228.0); // 200 + 20 + 8 gap

    // Caret near bottom of screen -> flips cleanly above caret
    let flipped = compute_popup_placement(
        500.0,
        560.0,
        output,
        Some(ScreenRect {
            x: 600.0,
            y: 1300.0,
            width: 2.0,
            height: 20.0,
        }),
        None,
    );
    assert_eq!(flipped.x, 600.0);
    assert_eq!(flipped.y, 732.0); // 1300 - 560 - 8 gap

    // Caret near right edge -> clamped inside output with 12px margin
    let clamped = compute_popup_placement(
        500.0,
        560.0,
        output,
        Some(ScreenRect {
            x: 2450.0,
            y: 100.0,
            width: 2.0,
            height: 20.0,
        }),
        None,
    );
    assert_eq!(clamped.x, 2048.0); // 2560 - 500 - 12

    // No AT-SPI2 caret -> falls back to centering inside the focused window tile
    let win_fallback = compute_popup_placement(
        500.0,
        560.0,
        output,
        None,
        Some(ScreenRect {
            x: 1280.0,
            y: 0.0,
            width: 1280.0,
            height: 1440.0,
        }),
    );
    assert_eq!(win_fallback.x, 1670.0); // 1280 + (1280 - 500) / 2
    assert_eq!(win_fallback.y, 440.0);
}

#[test]
fn updater_semver_and_sha256_verification() {
    use clipflow::platform::updater::{find_expected_sha256, is_newer_version, sha256_hex};

    assert!(is_newer_version("0.1.0", "v0.1.1"));
    assert!(is_newer_version("0.1.0", "0.2.0"));
    assert!(is_newer_version("0.9.9", "v1.0.0"));
    assert!(!is_newer_version("0.1.0", "v0.1.0"));
    assert!(!is_newer_version("0.2.0", "v0.1.9"));

    let digest = sha256_hex(b"clipflow-release-payload");
    let sums = format!(
        "0000000000000000000000000000000000000000000000000000000000000000  other.tar.gz\n\
         {digest}  clipflow-linux-x86_64.tar.gz\n"
    );
    assert_eq!(
        find_expected_sha256(&sums, "clipflow-linux-x86_64.tar.gz").as_deref(),
        Some(digest.as_str())
    );
    assert_eq!(find_expected_sha256(&sums, "missing.tar.gz"), None);
}


