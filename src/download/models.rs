//! 下载相关的数据模型定义。
//!
//! 包含下载结果、下载模式、书籍元数据、下载计划、进度快照等核心数据结构。

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChapterRef {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DownloadResult {
    pub success: u32,
    pub failed: u32,
    pub canceled: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadMode {
    Resume,
    Full,
    FailedOnly,
    RangeIgnoreHistory,
}

pub enum RetryFailed {
    Never,
    Decide(Box<dyn FnMut(usize) -> bool + Send>),
}

pub struct DownloadFlowOptions {
    pub mode: DownloadMode,
    pub range: Option<ChapterRange>,
    pub retry_failed: RetryFailed,
    pub stage_callback: Option<Box<dyn FnMut(DownloadResult) + Send>>,
    pub book_name_asker: Option<BookNameAsker>,
    pub format_asker: Option<FormatAsker>,
}

pub type BookNameAsker =
    Box<dyn FnMut(&crate::book_parser::book_manager::BookManager) -> Option<String> + Send>;

pub type FormatAsker =
    Box<dyn FnMut(&crate::book_parser::book_manager::BookManager) -> Option<String> + Send>;

#[derive(Debug, Clone, Default)]
pub struct BookMeta {
    pub book_name: Option<String>,
    pub author: Option<String>,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub cover_url: Option<String>,
    pub detail_cover_url: Option<String>,
    pub finished: Option<bool>,
    pub chapter_count: Option<usize>,
    pub word_count: Option<usize>,
    pub score: Option<f32>,
    pub read_count: Option<String>,
    pub read_count_text: Option<String>,
    pub book_short_name: Option<String>,
    pub original_book_name: Option<String>,
    pub first_chapter_title: Option<String>,
    pub last_chapter_title: Option<String>,
    pub category: Option<String>,
    pub cover_primary_color: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BookNameOption {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct DownloadPlan {
    pub book_id: String,
    pub meta: BookMeta,
    pub chapters: Vec<ChapterRef>,
    pub _raw: Value,
}

#[derive(Debug, Clone, Copy)]
pub struct ChapterRange {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct ProgressSnapshot {
    pub group_done: usize,
    pub group_total: usize,
    pub saved_chapters: usize,
    pub chapter_total: usize,
    pub save_phase: SavePhase,
    pub comment_fetch: usize,
    pub comment_total: usize,
    pub comment_saved: usize,
    #[serde(default)]
    pub audiobook_generated: usize,
    #[serde(default)]
    pub audiobook_skipped: usize,
    #[serde(default)]
    pub audiobook_failed: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SavePhase {
    #[default]
    TextSave,
    Audiobook,
}

// ── 元数据合并工具函数 ──────────────────────────────────────────────────

/// Merge metadata with special handling for book_name: prefer hint (what user saw) over dir API
pub(crate) fn merge_meta_prefer_hint_name(dir_meta: BookMeta, hint_meta: BookMeta) -> BookMeta {
    BookMeta {
        // Special: prefer hint's book_name to maintain UI consistency
        book_name: hint_meta.book_name.or(dir_meta.book_name),
        // For all other fields, prefer directory (authoritative) over hint
        author: dir_meta.author.or(hint_meta.author),
        description: dir_meta.description.or(hint_meta.description),
        tags: if dir_meta.tags.is_empty() {
            hint_meta.tags
        } else {
            dir_meta.tags
        },
        cover_url: dir_meta.cover_url.or(hint_meta.cover_url),
        detail_cover_url: dir_meta.detail_cover_url.or(hint_meta.detail_cover_url),
        finished: dir_meta.finished.or(hint_meta.finished),
        chapter_count: dir_meta.chapter_count.or(hint_meta.chapter_count),
        word_count: dir_meta.word_count.or(hint_meta.word_count),
        score: dir_meta.score.or(hint_meta.score),
        read_count: dir_meta.read_count.or(hint_meta.read_count),
        read_count_text: dir_meta.read_count_text.or(hint_meta.read_count_text),
        book_short_name: dir_meta.book_short_name.or(hint_meta.book_short_name),
        original_book_name: dir_meta.original_book_name.or(hint_meta.original_book_name),
        first_chapter_title: dir_meta
            .first_chapter_title
            .or(hint_meta.first_chapter_title),
        last_chapter_title: dir_meta.last_chapter_title.or(hint_meta.last_chapter_title),
        category: dir_meta.category.or(hint_meta.category),
        cover_primary_color: dir_meta
            .cover_primary_color
            .or(hint_meta.cover_primary_color),
    }
}
