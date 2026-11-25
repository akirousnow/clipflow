use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use crate::core::dedup::{
    MAX_TEXT_BYTES, NormalizedImage, build_text_preview, hash_trimmed_text, normalize_image,
};
use crate::core::model::{
    ClipKind, ClipPayload, ClipSummary, PageCursor, PageRequest, PageResult, ViewTab,
};

pub fn now_epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Persistent SQLite (WAL) store for clipboard history, favorites, and content-addressed images.
pub struct ClipStore {
    conn: Connection,
    data_dir: PathBuf,
}

impl ClipStore {
    /// Opens or creates the database and asset directories inside `data_dir`.
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self> {
        let data_dir = data_dir.as_ref().to_path_buf();
        fs::create_dir_all(data_dir.join("images")).context("failed to create images dir")?;
        fs::create_dir_all(data_dir.join("thumbs")).context("failed to create thumbs dir")?;

        let db_path = data_dir.join("clipflow.db");
        let conn = Connection::open(&db_path)
            .with_context(|| format!("failed to open SQLite db at {}", db_path.display()))?;
        let store = Self { conn, data_dir };
        store.configure_and_migrate()?;
        Ok(store)
    }

    /// Opens an in-memory SQLite database backed by `assets_dir` for image files (ideal for tests).
    pub fn open_in_memory(assets_dir: impl AsRef<Path>) -> Result<Self> {
        let data_dir = assets_dir.as_ref().to_path_buf();
        fs::create_dir_all(data_dir.join("images"))?;
        fs::create_dir_all(data_dir.join("thumbs"))?;
        let conn = Connection::open_in_memory().context("failed to open in-memory SQLite db")?;
        let store = Self { conn, data_dir };
        store.configure_and_migrate()?;
        Ok(store)
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    fn configure_and_migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;
            PRAGMA busy_timeout = 3000;

            CREATE TABLE IF NOT EXISTS clips (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                content_hash TEXT NOT NULL UNIQUE,
                kind TEXT NOT NULL CHECK(kind IN ('text', 'image')),
                text_content TEXT,
                preview_text TEXT NOT NULL,
                search_text TEXT NOT NULL,
                char_count INTEGER NOT NULL DEFAULT 0,
                byte_size INTEGER NOT NULL DEFAULT 0,
                image_width INTEGER,
                image_height INTEGER,
                image_rel_path TEXT,
                thumb_rel_path TEXT,
                is_favorite INTEGER NOT NULL DEFAULT 0,
                created_at_ms INTEGER NOT NULL,
                last_copied_at_ms INTEGER NOT NULL,
                copy_count INTEGER NOT NULL DEFAULT 1
            );

            CREATE INDEX IF NOT EXISTS idx_clips_recent
                ON clips (last_copied_at_ms DESC, id DESC);

            CREATE INDEX IF NOT EXISTS idx_clips_fav_recent
                ON clips (is_favorite, last_copied_at_ms DESC, id DESC);
            ",
        )?;
        Ok(())
    }

    /// Inserts or deduplicates a text clip (ignoring leading/trailing whitespace for dedup).
    /// Returns `Ok(None)` if the text is empty/whitespace-only or exceeds `MAX_TEXT_BYTES`.
    pub fn upsert_text(&self, text: &str) -> Result<Option<ClipSummary>> {
        self.upsert_text_at(text, now_epoch_ms())
    }

    pub fn upsert_text_at(&self, text: &str, timestamp_ms: i64) -> Result<Option<ClipSummary>> {
        if text.len() > MAX_TEXT_BYTES {
            return Ok(None);
        }
        let Some(content_hash) = hash_trimmed_text(text) else {
            return Ok(None);
        };

        let preview_text = build_text_preview(text);
        let search_text = text.trim().to_lowercase();
        let char_count = text.chars().count() as i64;
        let byte_size = text.len() as i64;

        self.conn.execute(
            "
            INSERT INTO clips (
                content_hash, kind, text_content, preview_text, search_text,
                char_count, byte_size, is_favorite, created_at_ms, last_copied_at_ms, copy_count
            ) VALUES (?1, 'text', ?2, ?3, ?4, ?5, ?6, 0, ?7, ?7, 1)
            ON CONFLICT(content_hash) DO UPDATE SET
                text_content = excluded.text_content,
                preview_text = excluded.preview_text,
                search_text = excluded.search_text,
                char_count = excluded.char_count,
                byte_size = excluded.byte_size,
                last_copied_at_ms = excluded.last_copied_at_ms,
                copy_count = clips.copy_count + 1
            ",
            params![
                content_hash,
                text,
                preview_text,
                search_text,
                char_count,
                byte_size,
                timestamp_ms
            ],
        )?;

        self.get_summary_by_hash(&content_hash)
    }

    /// Decodes, deduplicates, saves content-addressed PNG + thumbnail, and upserts an image clip.
    pub fn upsert_image_bytes(&self, raw_bytes: &[u8]) -> Result<ClipSummary> {
        let normalized = normalize_image(raw_bytes)?;
        self.upsert_normalized_image_at(&normalized, now_epoch_ms())
    }

    pub fn upsert_normalized_image_at(
        &self,
        normalized: &NormalizedImage,
        timestamp_ms: i64,
    ) -> Result<ClipSummary> {
        let image_rel = format!("images/{}.png", normalized.content_hash);
        let thumb_rel = format!("thumbs/{}.png", normalized.content_hash);

        write_if_missing(&self.data_dir.join(&image_rel), &normalized.png_bytes)?;
        write_if_missing(&self.data_dir.join(&thumb_rel), &normalized.thumb_png_bytes)?;

        let byte_size = normalized.png_bytes.len() as i64;
        self.conn.execute(
            "
            INSERT INTO clips (
                content_hash, kind, text_content, preview_text, search_text,
                char_count, byte_size, image_width, image_height,
                image_rel_path, thumb_rel_path, is_favorite,
                created_at_ms, last_copied_at_ms, copy_count
            ) VALUES (?1, 'image', NULL, ?2, ?3, 0, ?4, ?5, ?6, ?7, ?8, 0, ?9, ?9, 1)
            ON CONFLICT(content_hash) DO UPDATE SET
                preview_text = excluded.preview_text,
                search_text = excluded.search_text,
                byte_size = excluded.byte_size,
                image_width = excluded.image_width,
                image_height = excluded.image_height,
                image_rel_path = excluded.image_rel_path,
                thumb_rel_path = excluded.thumb_rel_path,
                last_copied_at_ms = excluded.last_copied_at_ms,
                copy_count = clips.copy_count + 1
            ",
            params![
                normalized.content_hash,
                normalized.preview_text,
                normalized.search_text,
                byte_size,
                normalized.width as i64,
                normalized.height as i64,
                image_rel,
                thumb_rel,
                timestamp_ms
            ],
        )?;

        self.get_summary_by_hash(&normalized.content_hash)?
            .context("upserted image clip missing immediately after insert")
    }

    /// Queries a single paginated slice of lightweight summaries using keyset pagination.
    /// Does NOT load full `text_content` or full image bytes.
    pub fn query_page(&self, req: &PageRequest) -> Result<PageResult> {
        let limit = req.limit.clamp(1, 500);
        let tokens: Vec<String> = req
            .query
            .split_whitespace()
            .map(|t| format!("%{}%", escape_like(&t.to_lowercase())))
            .collect();

        let mut base_clauses: Vec<String> = Vec::new();
        let mut base_params: Vec<Value> = Vec::new();

        if req.tab == ViewTab::Favorites {
            base_clauses.push("is_favorite = 1".to_string());
        }
        for tok in &tokens {
            base_clauses.push("search_text LIKE ? ESCAPE '\\'".to_string());
            base_params.push(Value::Text(tok.clone()));
        }

        let base_where = if base_clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", base_clauses.join(" AND "))
        };

        let count_sql = format!("SELECT COUNT(*) FROM clips {base_where}");
        let total_count: i64 =
            self.conn
                .query_row(&count_sql, params_from_iter(base_params.iter()), |row| {
                    row.get(0)
                })?;

        let mut page_clauses = base_clauses;
        let mut page_params = base_params;
        if let Some(cursor) = req.cursor {
            page_clauses.push("(last_copied_at_ms < ? OR (last_copied_at_ms = ? AND id < ?))".to_string());
            page_params.push(Value::Integer(cursor.last_copied_at_ms));
            page_params.push(Value::Integer(cursor.last_copied_at_ms));
            page_params.push(Value::Integer(cursor.id));
        }
        let page_where = if page_clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", page_clauses.join(" AND "))
        };

        let fetch_limit = (limit + 1) as i64;
        page_params.push(Value::Integer(fetch_limit));

        let page_sql = format!(
            "
            SELECT
                id, content_hash, kind, preview_text, char_count, byte_size,
                image_width, image_height, thumb_rel_path, is_favorite,
                created_at_ms, last_copied_at_ms, copy_count
            FROM clips
            {page_where}
            ORDER BY last_copied_at_ms DESC, id DESC
            LIMIT ?
            "
        );

        let mut stmt = self.conn.prepare(&page_sql)?;
        let rows = stmt.query_map(params_from_iter(page_params.iter()), |row| {
            self.map_summary_row(row)
        })?;

        let mut items = Vec::with_capacity(limit + 1);
        for row in rows {
            items.push(row?);
        }

        let has_more = items.len() > limit;
        if has_more {
            items.truncate(limit);
        }
        let next_cursor = if has_more {
            items.last().map(|item| PageCursor {
                last_copied_at_ms: item.last_copied_at_ms,
                id: item.id,
            })
        } else {
            None
        };

        Ok(PageResult {
            items,
            next_cursor,
            has_more,
            total_count: total_count.max(0) as usize,
        })
    }

    /// Loads the full payload (full text or full PNG bytes) on demand for a single item.
    pub fn load_payload(&self, id: i64) -> Result<Option<(ClipPayload, String)>> {
        struct PayloadRow {
            content_hash: String,
            kind_str: String,
            text_content: Option<String>,
            width: Option<i64>,
            height: Option<i64>,
            image_rel_path: Option<String>,
        }

        let row: Option<PayloadRow> = self
            .conn
            .query_row(
                "
                SELECT content_hash, kind, text_content, image_width, image_height, image_rel_path
                FROM clips
                WHERE id = ?1
                ",
                params![id],
                |row| {
                    Ok(PayloadRow {
                        content_hash: row.get(0)?,
                        kind_str: row.get(1)?,
                        text_content: row.get(2)?,
                        width: row.get(3)?,
                        height: row.get(4)?,
                        image_rel_path: row.get(5)?,
                    })
                },
            )
            .optional()?;

        let Some(row) = row else {
            return Ok(None);
        };

        match ClipKind::from_db_str(&row.kind_str) {
            Some(ClipKind::Text) => {
                let text = row.text_content.unwrap_or_default();
                Ok(Some((ClipPayload::Text(text), row.content_hash)))
            }
            Some(ClipKind::Image) => {
                let rel = row
                    .image_rel_path
                    .context("image clip missing image_rel_path")?;
                let full_path = self.data_dir.join(&rel);
                let png_bytes = fs::read(&full_path)
                    .with_context(|| format!("failed to read image file {}", full_path.display()))?;
                Ok(Some((
                    ClipPayload::Image {
                        png_bytes,
                        width: row.width.unwrap_or(0) as u32,
                        height: row.height.unwrap_or(0) as u32,
                    },
                    row.content_hash,
                )))
            }
            None => bail!("unknown clip kind in database: {}", row.kind_str),
        }
    }

    /// Toggles `is_favorite` for an item and returns the new favorite state.
    pub fn toggle_favorite(&self, id: i64) -> Result<Option<bool>> {
        let changed = self.conn.execute(
            "UPDATE clips SET is_favorite = CASE WHEN is_favorite = 0 THEN 1 ELSE 0 END WHERE id = ?1",
            params![id],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        let is_fav: i64 = self.conn.query_row(
            "SELECT is_favorite FROM clips WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )?;
        Ok(Some(is_fav != 0))
    }

    /// Updates `last_copied_at_ms` and increments `copy_count` when reusing an existing history item.
    pub fn touch_item_at(&self, id: i64, timestamp_ms: i64) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE clips SET last_copied_at_ms = ?2, copy_count = copy_count + 1 WHERE id = ?1",
            params![id, timestamp_ms],
        )?;
        Ok(changed > 0)
    }

    /// Deletes an item from history and removes its disk image/thumbnail if present.
    pub fn delete_item(&self, id: i64) -> Result<bool> {
        let paths: Option<(Option<String>, Option<String>)> = self
            .conn
            .query_row(
                "SELECT image_rel_path, thumb_rel_path FROM clips WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        let Some((image_rel, thumb_rel)) = paths else {
            return Ok(false);
        };

        self.conn
            .execute("DELETE FROM clips WHERE id = ?1", params![id])?;

        if let Some(rel) = image_rel {
            let _ = fs::remove_file(self.data_dir.join(rel));
        }
        if let Some(rel) = thumb_rel {
            let _ = fs::remove_file(self.data_dir.join(rel));
        }
        Ok(true)
    }

    /// Returns total count of all clips and favorite clips.
    pub fn counts(&self) -> Result<(usize, usize)> {
        let total: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM clips", [], |row| row.get(0))?;
        let favs: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM clips WHERE is_favorite = 1",
            [],
            |row| row.get(0),
        )?;
        Ok((total.max(0) as usize, favs.max(0) as usize))
    }

    /// Bulk-seeds synthetic realistic entries inside a single transaction (useful for benchmarks/testing).
    pub fn seed_benchmark_items(&mut self, count: usize) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let base_ts = now_epoch_ms() - (count as i64 * 1000);
        let mut inserted = 0usize;

        for i in 0..count {
            let text = match i % 5 {
                0 => format!(
                    "https://github.com/zed-industries/zed/pull/{} — GPUI uniform_list virtualization note #{i}",
                    10000 + i
                ),
                1 => format!(
                    "SELECT id, preview_text FROM clips WHERE is_favorite = 1 ORDER BY last_copied_at_ms DESC LIMIT 40; -- query #{i}"
                ),
                2 => format!(
                    "Rust 剪贴板测试条目 #{i}：支持中英文混排、Emoji 🚀、忽略首尾空白去重与分页懒加载。"
                ),
                3 => format!(
                    "fn benchmark_case_{i}(x: usize) -> usize {{\n    // multi-line snippet\n    x.saturating_mul(2) + {i}\n}}"
                ),
                _ => format!(
                    "{{\"event\":\"clip_captured\",\"seq\":{i},\"tag\":\"stress-test\",\"payload\":\"{}\"}}",
                    "abcdefghij".repeat(20)
                ),
            };
            let Some(hash) = hash_trimmed_text(&text) else {
                continue;
            };
            let preview = build_text_preview(&text);
            let search = text.trim().to_lowercase();
            let char_count = text.chars().count() as i64;
            let byte_size = text.len() as i64;
            let is_fav = if i % 15 == 0 { 1 } else { 0 };
            let ts = base_ts + (i as i64 * 1000);

            tx.execute(
                "
                INSERT OR IGNORE INTO clips (
                    content_hash, kind, text_content, preview_text, search_text,
                    char_count, byte_size, is_favorite, created_at_ms, last_copied_at_ms, copy_count
                ) VALUES (?1, 'text', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, 1)
                ",
                params![hash, text, preview, search, char_count, byte_size, is_fav, ts],
            )?;
            inserted += 1;
        }
        tx.commit()?;
        Ok(inserted)
    }

    pub fn get_summary_by_hash(&self, content_hash: &str) -> Result<Option<ClipSummary>> {
        self.conn
            .query_row(
                "
                SELECT
                    id, content_hash, kind, preview_text, char_count, byte_size,
                    image_width, image_height, thumb_rel_path, is_favorite,
                    created_at_ms, last_copied_at_ms, copy_count
                FROM clips
                WHERE content_hash = ?1
                ",
                params![content_hash],
                |row| self.map_summary_row(row),
            )
            .optional()
            .map_err(Into::into)
    }

    fn map_summary_row(&self, row: &rusqlite::Row<'_>) -> rusqlite::Result<ClipSummary> {
        let kind_str: String = row.get(2)?;
        let kind = ClipKind::from_db_str(&kind_str).unwrap_or(ClipKind::Text);
        let thumb_rel: Option<String> = row.get(8)?;
        let thumb_path = thumb_rel.map(|rel| self.data_dir.join(rel));

        Ok(ClipSummary {
            id: row.get(0)?,
            content_hash: row.get(1)?,
            kind,
            preview_text: row.get(3)?,
            char_count: row.get::<_, i64>(4)?.max(0) as usize,
            byte_size: row.get::<_, i64>(5)?.max(0) as usize,
            image_width: row.get::<_, Option<i64>>(6)?.map(|v| v as u32),
            image_height: row.get::<_, Option<i64>>(7)?.map(|v| v as u32),
            thumb_path,
            is_favorite: row.get::<_, i64>(9)? != 0,
            created_at_ms: row.get(10)?,
            last_copied_at_ms: row.get(11)?,
            copy_count: row.get(12)?,
        })
    }
}

fn write_if_missing(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let tmp_path = path.with_extension("png.tmp");
    fs::write(&tmp_path, bytes)
        .with_context(|| format!("failed to write temp asset {}", tmp_path.display()))?;
    fs::rename(&tmp_path, path)
        .with_context(|| format!("failed to persist asset {}", path.display()))?;
    Ok(())
}

fn escape_like(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}
