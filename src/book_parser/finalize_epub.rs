//! EPUB 生成主逻辑。
//!
//! 将章节内容组装成 EPUB，处理封面、分卷、段评注入、内联图片等。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use regex::Regex;
use serde_json::Value;
use std::fs;
use std::sync::OnceLock;
use tracing::{info, warn};

// 编译一次复用的内联图片正则缓存
fn re_inline_img() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#"(?is)<img[^>]*?\bsrc\s*=\s*['\"]([^'\"]+)['\"][^>]*>"#).unwrap())
}

use super::book_manager::BookManager;
use super::epub_generator::EpubGenerator;
use super::html_utils::{
    clean_epub_body, decode_xhtml_attr_url, description_to_plain_text, escape_html,
    render_description_xhtml_fragment,
};
use super::image_utils::{ensure_cached_image, sha1_hex};
use super::segment_utils;

// ── EPUB 入口 ───────────────────────────────────────────────────

pub(super) fn finalize_epub(
    manager: &BookManager,
    chapters: &[Value],
    path: &Path,
    directory_raw: Option<&Value>,
    // 段评进度上报随官方 API 通道关闭，当前 EPUB 流程不再使用
    _reporter: Option<&mut crate::download::downloader::ProgressReporter>,
) -> anyhow::Result<()> {
    let description_meta = description_to_plain_text(&manager.description);

    // 将完结状态追加到标签字符串中（与 Python 版本行为一致）
    let tags = if manager.finished == Some(true) || manager.end {
        if manager.tags.is_empty() {
            "已完结".to_string()
        } else {
            format!("{}|已完结", manager.tags)
        }
    } else {
        manager.tags.clone()
    };

    let mut epub_gen = EpubGenerator::new(
        &manager.book_id,
        &manager.book_name,
        &manager.author,
        &tags,
        &description_meta,
        &manager.config,
    )?;

    info!(
        target: "segment",
        enable_segment_comments = manager.config.enable_segment_comments,
        novel_format = %manager.config.novel_format,
        top_n = manager.config.segment_comments_top_n,
        workers = manager.config.segment_comments_workers,
        download_comment_images = manager.config.download_comment_images,
        download_comment_avatars = manager.config.download_comment_avatars,
        directory_raw_present = directory_raw.is_some(),
        "segment comment pipeline start"
    );

    // 图片断点续传：先下载到该书临时目录 images/，生成时再从本地导入。
    let images_dir = manager.book_folder().join("images");
    fs::create_dir_all(&images_dir)?;

    let mut image_cache: HashMap<String, (PathBuf, &'static str, &'static str)> = HashMap::new();
    let mut resources_added: HashSet<String> = HashSet::new();

    // 简单的介绍页
    let intro_desc_html = render_description_xhtml_fragment(&manager.description);
    let intro_html = format!(
        "<p>书名：{}</p><p>作者：{}</p><p>标签：{}</p><p>简介：</p>{}",
        escape_html(&manager.book_name),
        escape_html(&manager.author),
        escape_html(&tags),
        intro_desc_html
    );
    let _ = epub_gen.add_aux_page_named("aux_00000.xhtml".to_string(), "简介", &intro_html, true);

    // #201: 分卷标题
    let known_chapter_ids: HashSet<String> = chapters
        .iter()
        .filter_map(|ch| ch.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .collect();

    let volumes: Vec<(String, Vec<String>)> = directory_raw
        .map(|raw| extract_volume_to_chapter_ids(raw, &known_chapter_ids))
        .unwrap_or_default();

    if directory_raw.is_some() {
        if volumes.is_empty() {
            warn!(
                target: "volume",
                known_chapters = known_chapter_ids.len(),
                "directory_raw present but no volumes extracted (check directory_raw shape / ids mapping)"
            );
        } else {
            let total_ch_refs: usize = volumes.iter().map(|(_, ids)| ids.len()).sum();
            info!(
                target: "volume",
                volumes = volumes.len(),
                known_chapters = known_chapter_ids.len(),
                total_chapter_refs = total_ch_refs,
                "volumes extracted from directory_raw"
            );

            for (i, (t, ids)) in volumes.iter().take(8).enumerate() {
                info!(
                    target: "volume",
                    idx = i,
                    title = %t,
                    chapters = ids.len(),
                    first_chapter_id = %ids.first().map(|s| s.as_str()).unwrap_or(""),
                    "volume detail"
                );
            }
            if volumes.len() > 8 {
                info!(target: "volume", more = volumes.len() - 8, "more volumes omitted");
            }
        }
    }

    // 快速查询：chapter_id -> 卷标题
    let mut volume_title_by_chapter_id: HashMap<String, String> = HashMap::new();
    let mut volume_order: Vec<String> = Vec::new();
    for (title, ids) in &volumes {
        let t = title.trim();
        if t.is_empty() || ids.is_empty() {
            continue;
        }
        if !volume_order.contains(&t.to_string()) {
            volume_order.push(t.to_string());
        }
        for id in ids {
            volume_title_by_chapter_id
                .entry(id.clone())
                .or_insert_with(|| t.to_string());
        }
    }

    // #204: 如果只有一个卷且卷名是系统自动生成的默认名称，跳过卷标题页，
    // 避免生成多余的 "第一卷 默认" 等页面。
    // 如果作者自定义了卷名（即使只有一卷），仍然保留。
    let skip_volume_pages = volume_order.len() == 1 && is_default_volume_name(&volume_order[0]);
    if skip_volume_pages {
        info!(
            target: "volume",
            single_volume = %volume_order[0],
            "skipping single volume with default name"
        );
    }
    let mut volume_file_by_title: HashMap<String, String> = HashMap::new();
    if !skip_volume_pages {
        for (i, t) in volume_order.iter().enumerate() {
            volume_file_by_title.insert(t.clone(), format!("aux_{:05}.xhtml", 1 + i));
        }
    }

    let item_versions_len = 0usize;

    info!(
        target: "segment",
        item_versions = item_versions_len,
        "item_version map prepared"
    );

    // ── 章节构建数据 ────────────────────────────────────────────

    #[derive(Debug)]
    struct ChapterBuild {
        chapter_id: String,
        title: String,
        raw_xhtml: String,
        seg_counts: serde_json::Map<String, Value>,
        // 段评依赖官方 API 通道，当前构建不编译，保留待第三方段评接口接入
        #[cfg(any())]
        per_para: Vec<(i32, tomato_novel_official_api::ReviewResponse)>,
    }

    let chapter_count = chapters.len();
    let mut builds: Vec<ChapterBuild> = Vec::with_capacity(chapter_count);

    for (ch_idx, ch) in chapters.iter().enumerate() {
        let chapter_id = ch.get("id").and_then(|v| v.as_str()).unwrap_or("0");
        let title = ch.get("title").and_then(|v| v.as_str()).unwrap_or("章节");
        let content_html = ch.get("content").and_then(|v| v.as_str()).unwrap_or("");

        info!(
            target: "segment",
            ch_idx,
            chapter_id = %chapter_id,
            title = %title,
            "segment comment scan chapter"
        );

        let rewritten = embed_inline_images_chapter_named(
            &mut epub_gen,
            &manager.config,
            chapter_id,
            content_html,
            &mut image_cache,
            &mut resources_added,
            &images_dir,
        )
        .unwrap_or_else(|_| content_html.to_string());

        let seg_counts = serde_json::Map::new();
        // 段评依赖官方 API 通道，当前构建不编译，保留待第三方段评接口接入
        #[cfg(any())]
        let mut per_para: Vec<(i32, tomato_novel_official_api::ReviewResponse)> = Vec::new();

        builds.push(ChapterBuild {
            chapter_id: chapter_id.to_string(),
            title: title.to_string(),
            raw_xhtml: rewritten,
            seg_counts,
            #[cfg(any())]
            per_para,
        });

        let _ = ch_idx;
    }

    // ── 段评页生成 + 正文组装 ───────────────────────────────────

    // 段评页映射：当前构建不启用段评，恒为空表（章节正文走 clean_epub_body 分支）
    let comment_page_for_chapter: HashMap<String, String> = HashMap::new();

    // #263: 在正文中增加可见目录页（table-of-contents.html），
    // 并放入 spine 前部，便于在不依赖阅读器侧边栏时快速跳转章节。
    let toc_entries: Vec<(String, String)> = builds
        .iter()
        .enumerate()
        .map(|(idx, b)| (format!("chapter_{:05}.xhtml", 1 + idx), b.title.clone()))
        .collect();
    let toc_volumes: Vec<String> = builds
        .iter()
        .map(|b| {
            volume_title_by_chapter_id
                .get(&b.chapter_id)
                .cloned()
                .unwrap_or_default()
        })
        .collect();
    let toc_html = build_inline_toc_html(&toc_entries, &toc_volumes);
    let _ = epub_gen.add_aux_page_named(
        "table-of-contents.html".to_string(),
        "目录",
        &toc_html,
        true,
    );

    // 按序插入分卷标题页和正文章节
    let mut inserted_volumes: HashSet<String> = HashSet::new();
    for (idx, b) in builds.iter().enumerate() {
        if let Some(vol) = volume_title_by_chapter_id.get(&b.chapter_id) {
            let vol_trim = vol.trim();
            if !vol_trim.is_empty()
                && inserted_volumes.insert(vol_trim.to_string())
                && let Some(file) = volume_file_by_title.get(vol_trim)
            {
                info!(
                    target: "volume",
                    title = %vol_trim,
                    file = %file,
                    before_chapter_id = %b.chapter_id,
                    before_chapter_index = idx,
                    "inserting volume title page"
                );
                let body = format!("<p class=\"no-indent\">{}</p>", escape_html(vol_trim));
                let _ = epub_gen.add_aux_page_named(file.clone(), vol_trim, &body, true);
            }
        }

        let comment_file = comment_page_for_chapter
            .get(&b.chapter_id)
            .map(|s| s.as_str())
            .unwrap_or("");

        let chapter_out = if !comment_file.is_empty() {
            segment_utils::inject_segment_links(&b.raw_xhtml, comment_file, &b.seg_counts)
        } else {
            clean_epub_body(&b.raw_xhtml)
        };
        epub_gen.add_chapter_named(
            format!("chapter_{:05}.xhtml", 1 + idx),
            &b.title,
            &chapter_out,
        );
    }

    epub_gen.generate(path, &manager.config)?;
    Ok(())
}

// ── 分卷提取 ────────────────────────────────────────────────────

/// #204: 判断卷名是否为番茄系统自动生成的默认名称。
/// 匹配：「默认」「第一卷」「第1卷」「第一卷 默认」「卷一」「Volume 1」等。
fn is_default_volume_name(name: &str) -> bool {
    let s = name.trim();
    if s.is_empty() {
        return true;
    }
    // 统一分隔符：将各种冒号、破折号、下划线统一为空格，再合并连续空格
    let normalized: String = s
        .chars()
        .map(|c| match c {
            '：' | ':' | '—' | '–' | '_' | '·' | '｜' | '|' => ' ',
            _ => c,
        })
        .collect();
    let normalized = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = normalized.to_lowercase();

    // 精确匹配常见默认名称
    let exact = [
        "默认",
        "默认卷",
        "默认分卷",
        "第一卷",
        "第1卷",
        "卷一",
        "卷1",
        "第一卷 默认",
        "第1卷 默认",
    ];
    for e in exact {
        if lower == e {
            return true;
        }
    }
    // "Volume 1" / "Vol 1" / "Vol.1" 等英文默认名
    let lower_ascii = lower.replace(['.', '_'], " ");
    let lower_ascii = lower_ascii.trim();
    if lower_ascii == "volume 1" || lower_ascii == "vol 1" || lower_ascii == "vol1" {
        return true;
    }
    false
}

/// #201: 从 `directory_raw` 递归提取"卷标题 -> 章节 id 列表"。
fn extract_volume_to_chapter_ids(
    directory_raw: &Value,
    known_chapter_ids: &HashSet<String>,
) -> Vec<(String, Vec<String>)> {
    fn pick_string_or_number(v: Option<&Value>) -> Option<String> {
        match v {
            Some(Value::String(s)) => {
                let t = s.trim();
                if t.is_empty() {
                    None
                } else {
                    Some(t.to_string())
                }
            }
            Some(Value::Number(n)) => Some(n.to_string()),
            _ => None,
        }
    }

    fn pick_volume_title(obj: &serde_json::Map<String, Value>) -> Option<String> {
        let candidates = [
            "volume_title",
            "volume_name",
            "section_title",
            "section_name",
            "group_title",
            "group_name",
        ];
        for k in candidates {
            if let Some(Value::String(s)) = obj.get(k) {
                let t = s.trim();
                if !t.is_empty() {
                    return Some(t.to_string());
                }
            }
        }
        None
    }

    fn pick_title(obj: &serde_json::Map<String, Value>) -> Option<String> {
        let candidates = [
            "volume_title",
            "volume_name",
            "catalog_name",
            "catalog_title",
            "section_title",
            "section_name",
            "group_title",
            "group_name",
            "title",
            "name",
        ];
        for k in candidates {
            if let Some(Value::String(s)) = obj.get(k) {
                let t = s.trim();
                if !t.is_empty() {
                    return Some(t.to_string());
                }
            }
        }
        None
    }

    fn looks_like_chapter_obj(obj: &serde_json::Map<String, Value>) -> Option<String> {
        let id = pick_string_or_number(
            obj.get("chapter_id")
                .or_else(|| obj.get("item_id"))
                .or_else(|| obj.get("catalog_id"))
                .or_else(|| obj.get("id")),
        )?;
        Some(id)
    }

    fn push_chapter(
        title: &str,
        chapter_id: &str,
        known_chapter_ids: &HashSet<String>,
        idx_by_title: &mut HashMap<String, usize>,
        out: &mut Vec<(String, Vec<String>)>,
    ) {
        let t = title.trim();
        if t.is_empty() {
            return;
        }
        if !known_chapter_ids.contains(chapter_id) {
            return;
        }
        let i = if let Some(i) = idx_by_title.get(t) {
            *i
        } else {
            let i = out.len();
            out.push((t.to_string(), Vec::new()));
            idx_by_title.insert(t.to_string(), i);
            i
        };

        let ids = &mut out[i].1;
        if ids.last().is_some_and(|last| last == chapter_id) {
            return;
        }
        if !ids.contains(&chapter_id.to_string()) {
            ids.push(chapter_id.to_string());
        }
    }

    fn visit(
        v: &Value,
        current_volume: Option<&str>,
        known_chapter_ids: &HashSet<String>,
        idx_by_title: &mut HashMap<String, usize>,
        out: &mut Vec<(String, Vec<String>)>,
    ) {
        match v {
            Value::Array(arr) => {
                for it in arr {
                    visit(it, current_volume, known_chapter_ids, idx_by_title, out);
                }
            }
            Value::Object(obj) => {
                let is_chapter = looks_like_chapter_obj(obj).is_some();

                if let Some(vol) = pick_volume_title(obj)
                    && let Some(id) = looks_like_chapter_obj(obj)
                {
                    push_chapter(&vol, &id, known_chapter_ids, idx_by_title, out);
                }

                if let Some(vol) = current_volume
                    && let Some(id) = looks_like_chapter_obj(obj)
                {
                    push_chapter(vol, &id, known_chapter_ids, idx_by_title, out);
                }

                let title_here = if is_chapter {
                    pick_volume_title(obj)
                } else {
                    pick_title(obj)
                };
                let next_volume = title_here.as_deref().or(current_volume);

                let child_keys = [
                    "catalog_data",
                    "item_data_list",
                    "items",
                    "item_list",
                    "children",
                    "child_list",
                    "sub_items",
                    "sub_item_list",
                    "chapter_list",
                    "chapters",
                    "chapter_ids",
                ];

                for k in child_keys {
                    if let Some(arr) = obj.get(k).and_then(Value::as_array) {
                        for child in arr {
                            if let (Some(vol), Some(id)) =
                                (next_volume, pick_string_or_number(Some(child)))
                            {
                                push_chapter(vol, &id, known_chapter_ids, idx_by_title, out);
                            } else {
                                visit(child, next_volume, known_chapter_ids, idx_by_title, out);
                            }
                        }
                    }
                }

                for (k, vv) in obj {
                    if child_keys.contains(&k.as_str()) {
                        continue;
                    }
                    visit(vv, next_volume, known_chapter_ids, idx_by_title, out);
                }
            }
            _ => {}
        }
    }

    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    let mut idx_by_title: HashMap<String, usize> = HashMap::new();

    if let Some(items) = directory_raw
        .get("item_data_list")
        .and_then(Value::as_array)
    {
        let mut current: Option<String> = None;
        for it in items {
            let Some(obj) = it.as_object() else {
                continue;
            };
            if let Some(vol) = pick_volume_title(obj) {
                current = Some(vol);
            }
            let Some(id) = looks_like_chapter_obj(obj) else {
                continue;
            };
            if let Some(vol) = current.as_deref() {
                push_chapter(vol, &id, known_chapter_ids, &mut idx_by_title, &mut out);
            }
        }
        if !out.is_empty() {
            return out;
        }
    }

    visit(
        directory_raw,
        None,
        known_chapter_ids,
        &mut idx_by_title,
        &mut out,
    );
    out
}

// ── 内联图片嵌入 ────────────────────────────────────────────────

fn embed_inline_images_chapter_named(
    epub: &mut EpubGenerator,
    cfg: &crate::base_system::context::Config,
    _chapter_id: &str,
    html: &str,
    cache: &mut HashMap<String, (PathBuf, &'static str, &'static str)>,
    resources_added: &mut HashSet<String>,
    images_dir: &Path,
) -> anyhow::Result<String> {
    let re_img = re_inline_img();

    let mut mapping: HashMap<String, String> = HashMap::new();
    for cap in re_img.captures_iter(html) {
        let src_raw = cap.get(1).map(|m| m.as_str()).unwrap_or("").trim();
        if src_raw.is_empty() {
            continue;
        }

        let decoded = decode_xhtml_attr_url(src_raw);
        let decoded = decoded.as_ref();
        if decoded.starts_with("images/") || decoded.starts_with("data:") {
            continue;
        }
        if !(decoded.starts_with("http://") || decoded.starts_with("https://")) {
            continue;
        }

        let normalized = if let Some((path, mime, ext)) = cache.get(decoded) {
            Some((path.clone(), *mime, *ext))
        } else {
            let fetched = ensure_cached_image(cfg, decoded, images_dir)?;
            if let Some((path, mime, ext)) = &fetched {
                cache.insert(decoded.to_string(), (path.clone(), *mime, *ext));
            }
            fetched
        };
        let Some((local_path, mime, ext)) = normalized else {
            continue;
        };

        let hash = sha1_hex(decoded);
        let resource_path = format!("images/{}{}", hash, ext);

        if !resources_added.contains(&resource_path)
            && let Ok(bytes) = fs::read(&local_path)
            && epub.add_resource_bytes(&resource_path, bytes, mime).is_ok()
        {
            resources_added.insert(resource_path.clone());
        }

        if resources_added.contains(&resource_path) {
            mapping.insert(src_raw.to_string(), resource_path.clone());
            mapping.insert(decoded.to_string(), resource_path);
        }
    }

    let rewritten = re_img
        .replace_all(html, |caps: &regex::Captures| {
            let whole = caps.get(0).map(|m| m.as_str()).unwrap_or("");
            let src_raw = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let decoded = decode_xhtml_attr_url(src_raw);

            if let Some(path) = mapping
                .get(src_raw)
                .or_else(|| mapping.get(decoded.as_ref()))
            {
                whole.replacen(src_raw, path, 1)
            } else {
                whole.to_string()
            }
        })
        .to_string();
    Ok(rewritten)
}

fn build_inline_toc_html(toc_entries: &[(String, String)], volumes: &[String]) -> String {
    let mut out = String::new();
    out.push_str("<nav epub:type=\"toc\" id=\"inline-toc\">\n");
    out.push_str("  <p class=\"no-indent\">点击章节标题可跳转到对应正文位置。</p>\n");

    let mut current_volume = String::new();
    let mut list_open = false;

    for ((file, title), vol_raw) in toc_entries.iter().zip(volumes.iter()) {
        let vol = Some(vol_raw.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .unwrap_or("");

        if vol != current_volume {
            if list_open {
                out.push_str("  </ol>\n");
            }
            if !vol.is_empty() {
                out.push_str(&format!("  <h3>{}</h3>\n", escape_html(vol)));
            }
            out.push_str("  <ol>\n");
            list_open = true;
            current_volume = vol.to_string();
        }

        out.push_str(&format!(
            "    <li><a href=\"{}\">{}</a></li>\n",
            escape_html(file),
            escape_html(title)
        ));
    }

    if list_open {
        out.push_str("  </ol>\n");
    } else {
        out.push_str("  <ol></ol>\n");
    }
    out.push_str("</nav>");
    out
}
