//! JSON 提取与容错解析工具。

#![allow(dead_code)]

use serde_json::Value;

pub type JsonMap = serde_json::Map<String, Value>;

pub fn collect_maps(raw: &Value) -> Vec<&JsonMap> {
    let mut maps = Vec::new();
    if let Some(map) = raw.as_object() {
        maps.push(map);
        if let Some(info) = map.get("book_info").and_then(|v| v.as_object()) {
            maps.push(info);
        }
        if let Some(info) = map.get("bookInfo").and_then(|v| v.as_object()) {
            maps.push(info);
        }
        if let Some(info) = map.get("book_data").and_then(|v| v.as_object()) {
            maps.push(info);
        }
        if let Some(info) = map.get("data").and_then(|v| v.as_object()) {
            maps.push(info);
        }
        if let Some(info) = map.get("meta").and_then(|v| v.as_object()) {
            maps.push(info);
        }
    }
    maps
}

pub fn pick_string(map: &JsonMap, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(val) = map.get(*key) {
            if let Some(s) = val.as_str() {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            } else if let Some(n) = val.as_i64() {
                return Some(n.to_string());
            } else if let Some(n) = val.as_u64() {
                return Some(n.to_string());
            } else if let Some(n) = val.as_f64()
                && n.is_finite()
            {
                return Some(n.to_string());
            }
        }
    }
    None
}

pub fn pick_tags(map: &JsonMap) -> Vec<String> {
    let candidates = [
        "tags",
        "book_tags",
        "tag",
        "category",
        "categories",
        "classify_tags",
    ];
    for key in candidates {
        if let Some(val) = map.get(key) {
            let out = tags_from_value(val);
            if !out.is_empty() {
                return out;
            }
        }
    }
    Vec::new()
}

pub fn pick_tags_opt(map: &JsonMap) -> Option<Vec<String>> {
    let tags = pick_tags(map);
    if tags.is_empty() { None } else { Some(tags) }
}

pub fn pick_description(map: &JsonMap) -> Option<String> {
    let candidates = [
        "book_intro",
        "book_desc",
        "book_description",
        "desc",
        "description",
        "abstract",
        "intro",
        "content_desc",
        "brief",
        "summary",
    ];
    pick_string(map, &candidates)
}

pub fn pick_cover(map: &JsonMap) -> Option<String> {
    // 仅接受绝对 http(s) URL：番茄/七猫等源的相对封面（如 novel-pic/xxx、thumb_uri）
    // 前端与 /api/search-cover 代理都无法直接取回；返回相对值反而会“挡在”
    // 真正可用的绝对封面字段之前，导致列表看不到封面而预览（走 detail 接口）能看到。
    let candidates = [
        "cover",
        "cover_url",
        "pic_url",
        "thumb_url",
        "thumb",
        "coverUrl",
        "picUrl",
        "book_cover",
        "bookCover",
        "book_cover_url",
        "image",
        "image_url",
        "imageUrl",
        "imageUri",
        "image_uri",
        // 侧车 FQSearchResponse.BookItem 的绝对封面字段（camelCase）+ 蛇形变体
        "detailPageThumbUrl",
        "detail_page_thumb_url",
        "horizThumbUrl",
        "horiz_thumb_url",
        "expandThumbUrl",
        "expand_thumb_url",
    ];
    for key in candidates {
        if let Some(val) = map.get(key)
            && let Some(url) = val.as_str()
        {
            let trimmed = url.trim();
            if is_absolute_url(trimmed) {
                return Some(trimmed.to_string());
            }
        }
    }
    // 数组型字段（常见于 番茄 fqsearch、部分 书旗 API）：取首元素的 url/uri 或字符串本身。
    for key in [
        "image_list",
        "images",
        "cover_list",
        "covers",
        "picture_list",
    ] {
        if let Some(arr) = map.get(key).and_then(|v| v.as_array()) {
            for entry in arr {
                if let Some(s) = entry.as_str() {
                    let t = s.trim();
                    if is_absolute_url(t) {
                        return Some(t.to_string());
                    }
                }
                if let Some(obj) = entry.as_object() {
                    for sub in ["url", "uri", "image_url", "imageUrl"] {
                        if let Some(s) = obj.get(sub).and_then(|v| v.as_str()) {
                            let t = s.trim();
                            if is_absolute_url(t) {
                                return Some(t.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

fn is_absolute_url(s: &str) -> bool {
    !s.is_empty() && (s.starts_with("http://") || s.starts_with("https://"))
}

/// 把番茄/抖音图床的签名 HEIC 封面 URL 转换为公网免签的 JPEG 镜像。
///
/// 移动端搜索接口返回的 `thumb_url`/`detail_page_thumb_url` 等均为
/// `.../<dir>/<hash>~tplv-....heic?...x-signature=...`：把 `.heic` 换成 `.jpeg` 会破坏
/// 签名（403），而 `image` crate 无 HEIC 解码能力，导致 `/api/search-cover` 只能 302 回
/// 原始 heic，浏览器（Chrome/Edge）无法渲染 → 列表显示占位文字（详情页因走 web 抓取拿到
/// 非 heic 才正常）。所幸同一 `<dir>/<hash>` 在公网 `p6-novel.byteimg.com` 上有免签镜像：
/// `https://p6-novel.byteimg.com/<dir>/<hash>~tplv-shrink:360:0.jpeg` 直接返回 image/jpeg。
/// 非 heic 的 URL 原样返回。
pub fn to_public_jpeg_cover(url: &str) -> String {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    // 只处理 heic/heif；其余格式（jpeg/png/webp）交给上层原样使用。
    let lower = trimmed.to_ascii_lowercase();
    if !(lower.contains(".heic") || lower.contains(".heif")) {
        return trimmed.to_string();
    }
    // 去掉 query（? 之后），取 path 段。
    let path_part = trimmed.split('?').next().unwrap_or("");
    // 取 host 之后、`~tplv` 之前的 `<dir>/<hash>` 片段。
    let after_host = match path_part.find("://") {
        Some(i) => {
            let rest = &path_part[i + 3..];
            match rest.find('/') {
                Some(slash) => &rest[slash + 1..],
                None => return trimmed.to_string(),
            }
        }
        None => return trimmed.to_string(),
    };
    let base = match after_host.find('~') {
        Some(t) => &after_host[..t],
        None => after_host,
    };
    // base 形如 novel-pic/<hash> 或纯 <hash>；仅当空时放弃转换
    // （无目录前缀的纯 hash 在公网 byteimg 同样有免签 JPEG 镜像）。
    if base.is_empty() {
        return trimmed.to_string();
    }
    format!("https://p6-novel.byteimg.com/{base}~tplv-shrink:360:0.jpeg")
}

pub fn pick_detail_cover(map: &JsonMap) -> Option<String> {
    let candidates = [
        "detail_page_thumb_url",
        "detail_thumb",
        "detail_cover",
        "detail_cover_url",
    ];
    for key in candidates {
        if let Some(val) = map.get(key)
            && let Some(s) = val.as_str()
        {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

pub fn pick_finished(map: &JsonMap) -> Option<bool> {
    // Stronger signals first.
    if let Some(ts) = pick_string(map, &["creation_latest_finish_time", "finish_time"])
        && let Ok(n) = ts.trim().parse::<i64>()
        && n > 0
    {
        return Some(true);
    }

    // update_status: 1=still updating; 0 often means not updating (finished or stopped) so treat as unknown.
    if let Some(s) = pick_string(map, &["update_status", "updateStatus"])
        && let Ok(n) = s.trim().parse::<i64>()
        && n == 1
    {
        return Some(false);
    }

    let candidates = [
        "is_finish",
        "is_finished",
        "finish_status",
        "finishstate",
        "finish_state",
        "is_end",
        "isEnd",
        "finish",
        "finished",
        "book_status",
        "status",
        "serial_status",
        "serialStatus",
        "finishStatus",
    ];

    let parse_num = |key: &str, n: i64| -> Option<bool> {
        match key {
            // These fields are typically enums: 1=连载, 2=完结.
            "status" | "book_status" | "serial_status" | "serialStatus" | "finish_status"
            | "finishStatus" => match n {
                2 => Some(true),
                // Many APIs use 0/1 as non-terminal states or always "1"; do not treat them as definitive.
                0 | 1 => None,
                _ => None,
            },
            // Binary flags.
            _ => match n {
                1 | 2 => Some(true),
                0 => Some(false),
                _ => None,
            },
        }
    };

    for key in candidates {
        if let Some(val) = map.get(key) {
            if let Some(b) = val.as_bool() {
                return Some(b);
            }
            if let Some(n) = val.as_i64()
                && let Some(b) = parse_num(key, n)
            {
                return Some(b);
            }
            if let Some(s) = val.as_str() {
                let trimmed = s.trim();
                if let Ok(n) = trimmed.parse::<i64>()
                    && let Some(b) = parse_num(key, n)
                {
                    return Some(b);
                }
                let lower = trimmed.to_ascii_lowercase();
                if ["true", "yes", "finished", "end", "completed", "serial_end"]
                    .contains(&lower.as_str())
                {
                    return Some(true);
                }
                if [
                    "false",
                    "no",
                    "ongoing",
                    "serialize",
                    "serializing",
                    "serial",
                ]
                .contains(&lower.as_str())
                {
                    return Some(false);
                }

                // String enums for status-like keys.
                if matches!(
                    key,
                    "status"
                        | "book_status"
                        | "serial_status"
                        | "serialStatus"
                        | "finish_status"
                        | "finishStatus"
                ) && lower == "2"
                {
                    return Some(true);
                    // Treat 0/1 as unknown for these keys.
                }
            }
        }
    }

    None
}

pub fn pick_chapter_count(map: &JsonMap) -> Option<usize> {
    let candidates = [
        "item_cnt",
        "book_item_cnt",
        "chapter_num",
        "chapter_count",
        "chapter_total_cnt",
        "serial_count",
        "content_chapter_number",
        "content_count",
        "total_chapter_count",
    ];
    for key in candidates {
        if let Some(val) = map.get(key) {
            if let Some(n) = val.as_u64() {
                return Some(n as usize);
            }
            if let Some(s) = val.as_str()
                && let Ok(n) = s.parse::<usize>()
            {
                return Some(n);
            }
        }
    }
    None
}

pub fn pick_word_count(map: &JsonMap) -> Option<usize> {
    let candidates = ["word_number", "word_count", "word_cnt", "words"];
    for key in candidates {
        if let Some(val) = map.get(key) {
            if let Some(n) = val.as_u64() {
                return Some(n as usize);
            }
            if let Some(s) = val.as_str()
                && let Ok(n) = s.parse::<usize>()
            {
                return Some(n);
            }
        }
    }
    None
}

pub fn pick_score(map: &JsonMap) -> Option<f32> {
    let candidates = ["score", "book_score", "rating"];
    for key in candidates {
        if let Some(val) = map.get(key) {
            if let Some(n) = val.as_f64() {
                return Some(n as f32);
            }
            if let Some(s) = val.as_str()
                && let Ok(n) = s.parse::<f32>()
            {
                return Some(n);
            }
        }
    }
    None
}

pub fn pick_read_count(map: &JsonMap) -> Option<String> {
    let candidates = ["read_count", "read_count_all", "readcnt", "pv"];
    for key in candidates {
        if let Some(val) = map.get(key) {
            if let Some(s) = val.as_str() {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            } else if let Some(n) = val.as_u64() {
                return Some(n.to_string());
            }
        }
    }
    None
}

pub fn pick_read_count_text(map: &JsonMap) -> Option<String> {
    let candidates = ["read_cnt_text", "read_count_text"];
    for key in candidates {
        if let Some(val) = map.get(key)
            && let Some(s) = val.as_str()
        {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

pub fn pick_book_short_name(map: &JsonMap) -> Option<String> {
    let candidates = ["book_short_name", "short_name", "short_title"];
    for key in candidates {
        if let Some(val) = map.get(key)
            && let Some(s) = val.as_str()
        {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

pub fn pick_original_book_name(map: &JsonMap) -> Option<String> {
    let candidates = ["original_book_name", "origin_title", "original_title"];
    for key in candidates {
        if let Some(val) = map.get(key)
            && let Some(s) = val.as_str()
        {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

pub fn pick_first_chapter_title(map: &JsonMap) -> Option<String> {
    let candidates = [
        "first_chapter_title",
        "first_catalog_title",
        "firstItemTitle",
    ];
    for key in candidates {
        if let Some(val) = map.get(key)
            && let Some(s) = val.as_str()
        {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

pub fn pick_last_chapter_title(map: &JsonMap) -> Option<String> {
    let candidates = [
        "last_chapter_title",
        "latest_catalog_title",
        "lastItemTitle",
    ];
    for key in candidates {
        if let Some(val) = map.get(key)
            && let Some(s) = val.as_str()
        {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

pub fn pick_category(map: &JsonMap) -> Option<String> {
    let candidates = ["category", "category_name", "book_category", "classify"];
    for key in candidates {
        if let Some(val) = map.get(key)
            && let Some(s) = val.as_str()
        {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

pub fn pick_cover_primary_color(map: &JsonMap) -> Option<String> {
    let candidates = ["cover_primary_color", "primary_color", "cover_color"];
    for key in candidates {
        if let Some(val) = map.get(key)
            && let Some(s) = val.as_str()
        {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

pub fn tags_from_value(value: &Value) -> Vec<String> {
    match value {
        Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect(),
        Value::String(s) => s
            .split(['|', ',', ';', ' '])
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .map(|p| p.to_string())
            .collect(),
        _ => Vec::new(),
    }
}
