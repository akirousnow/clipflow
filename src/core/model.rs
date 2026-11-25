use std::path::PathBuf;

/// Distinguishes text and image clipboard items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipKind {
    Text,
    Image,
}

impl ClipKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Image => "image",
        }
    }

    pub fn from_db_str(value: &str) -> Option<Self> {
        match value {
            "text" => Some(Self::Text),
            "image" => Some(Self::Image),
            _ => None,
        }
    }
}

/// Active tab in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewTab {
    #[default]
    All,
    Favorites,
}

/// Lightweight row projection returned by paginated list & search queries.
/// Full text/image payloads are NOT loaded here so opening and scrolling stay fast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipSummary {
    pub id: i64,
    pub content_hash: String,
    pub kind: ClipKind,
    pub preview_text: String,
    pub char_count: usize,
    pub byte_size: usize,
    pub image_width: Option<u32>,
    pub image_height: Option<u32>,
    pub thumb_path: Option<PathBuf>,
    pub is_favorite: bool,
    pub created_at_ms: i64,
    pub last_copied_at_ms: i64,
    pub copy_count: i64,
}

/// Full clipboard payload fetched on demand when writing back to the system clipboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipPayload {
    Text(String),
    Image {
        png_bytes: Vec<u8>,
        width: u32,
        height: u32,
    },
}

/// Keyset pagination cursor ordered by `(last_copied_at_ms DESC, id DESC)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageCursor {
    pub last_copied_at_ms: i64,
    pub id: i64,
}

/// Request for a single page of clipboard summaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRequest {
    pub tab: ViewTab,
    pub query: String,
    pub cursor: Option<PageCursor>,
    pub limit: usize,
}

/// Paginated response from SQLite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageResult {
    pub items: Vec<ClipSummary>,
    pub next_cursor: Option<PageCursor>,
    pub has_more: bool,
    pub total_count: usize,
}

/// Raw capture emitted by a clipboard backend before deduplication and filtering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedClipboard {
    pub mime_types: Vec<String>,
    pub content: CapturedContent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapturedContent {
    Text(String),
    ImageBytes {
        mime: String,
        bytes: Vec<u8>,
    },
}

/// How an activated item should be applied to the system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteMode {
    /// Write to clipboard and send Ctrl+V to the previously focused app.
    CopyAndPaste,
    /// Write to clipboard and send Ctrl+Shift+V (useful for Linux terminals).
    CopyAndPasteShift,
    /// Only write to clipboard without simulating paste.
    CopyOnly,
}
