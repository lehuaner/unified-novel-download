//! 聚合搜索：付费平台官方源 + 平台池资源站 → 归属判定。
//!
//! 当前仅注册晋江一个付费平台，但数据结构按"多付费平台 + 共享池"设计：
//! [`PaidPlatformDef`] 只是静态声明（平台 id / 官方源 / 池 id），
//! 新增平台不需要改动资源站代码。

pub mod classifier;

use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::official;
use crate::official::OfficialBook;
use crate::pool;
use crate::provider::{self, SearchOptions};

/// 池成员的搜索结果：`(成员 id, 搜索结果)`。
type MemberSearchResult = (&'static str, anyhow::Result<(Vec<Value>, bool)>);

use classifier::{Classification, FreeItem, MatchConfig};

/// 付费平台声明。
pub struct PaidPlatformDef {
    /// 逻辑平台 id（也是前端 `display_platform` 的取值）。
    pub id: &'static str,
    pub display_name: &'static str,
    /// 官方源 id（见 [`crate::official`]）。
    pub official_source: &'static str,
    /// 平台池 id（见 [`crate::pool`]）。
    pub pool_id: &'static str,
}

/// 已注册的付费平台。
///
/// 未来新增平台的接线：实现一个 `OfficialSource`，然后在这里加一行；
/// 若新平台的内容同样由共享池中的资源站提供，直接复用 `shared_pool_a`。
pub const PAID_PLATFORMS: &[PaidPlatformDef] = &[PaidPlatformDef {
    id: "jinjiang",
    display_name: "晋江",
    official_source: "jjwxc",
    pool_id: pool::SHARED_POOL_A.id,
}];

pub fn paid_platform(id: &str) -> Option<&'static PaidPlatformDef> {
    PAID_PLATFORMS.iter().find(|p| p.id == id)
}

/// 为聚合池中的资源站书补全官方元数据（按书名 + 作者匹配）。
///
/// 非池成员、官方源未启用或未达到匹配阈值时返回 `None`，调用方应保留资源站数据。
pub fn official_meta_for_pool_book(
    book_id: &str,
    book_name: &str,
    author: &str,
    timeout_secs: u64,
) -> Option<OfficialBook> {
    let name = book_name.trim();
    if name.is_empty() {
        return None;
    }
    let owner = provider::owner_of(book_id)?;
    let def = PAID_PLATFORMS.iter().find(|d| {
        pool::find(d.pool_id).is_some_and(|p| p.members.iter().any(|m| *m == owner.id()))
    })?;
    let src = official::find(def.official_source)?;
    let timeout = timeout_secs.max(3);
    let (books, _) = src.search(name, 1, timeout).ok()?;
    if books.is_empty() {
        return None;
    }
    let best = classifier::best_match(name, author.trim(), "", &books)?;
    if !classifier::accepted(best.score, best.author_eq, &MatchConfig::default()) {
        return None;
    }
    let mut hit = books.get(best.index)?.clone();
    // 搜索列表的简介可能被截断，补一次详情；失败则保留列表数据。
    if hit.intro_full.as_deref().unwrap_or("").trim().is_empty()
        && let Ok(detail) = src.fetch_detail(&hit.official_id, timeout)
    {
        hit = detail;
    }
    Some(hit)
}

/// 聚合搜索结果。
pub struct Outcome {
    /// 已标注 `display_platform` / `_provider` 的条目。
    pub items: Vec<Value>,
    pub warnings: Vec<String>,
    pub official_hits: usize,
}

/// 对指定付费平台执行一次聚合搜索。
///
/// - `free_items`: 本轮免费平台结果（用于"与免费平台重合则隐藏"）。
///   即使调用方不展示这些源，也会传进来供本函数做去重判定。
/// - 官方源失败不影响池搜索：池结果无法判定归属，按规则落入"其他"。
pub fn search(
    platform_id: &str,
    keyword: &str,
    page: u32,
    timeout_secs: u64,
    free_items: &[Value],
    cookie: Option<&str>,
    user_agent: Option<&str>,
) -> Outcome {
    let mut warnings: Vec<String> = Vec::new();
    let Some(def) = paid_platform(platform_id) else {
        return Outcome {
            items: Vec::new(),
            warnings: vec![format!("未知付费平台: {platform_id}")],
            official_hits: 0,
        };
    };
    let Some(pool_def) = pool::find(def.pool_id) else {
        return Outcome {
            items: Vec::new(),
            warnings: vec![format!(
                "付费平台 {} 引用了不存在的池 {}",
                def.id, def.pool_id
            )],
            official_hits: 0,
        };
    };

    // 本轮聚合的总预算：官方源与平台池**共享**同一个截止时间，
    // 不能「官方源耗时 + 池 10s」叠加，否则外层总会把整块取消。
    let started = Instant::now();
    let deadline = started + Duration::from_secs(timeout_secs.max(1));

    // ── 1. 官方源搜索（归属判定的基准）────────────────────────
    let mut officials = Vec::new();
    let official_src = official::find(def.official_source);
    let official_timeout = deadline
        .saturating_duration_since(Instant::now())
        .as_secs()
        .max(1);
    match official_src {
        Some(src) => match src.search(keyword, page, official_timeout) {
            Ok((books, _has_more)) => officials = books,
            Err(e) => warnings.push(format!("{}官网搜索失败: {e}", def.display_name)),
        },
        None => warnings.push(format!("{}官方源未启用", def.display_name)),
    }
    let official_hits = officials.len();

    // ── 2. 平台池调度（站点级熔断过滤）────────────────────────
    let mut available: Vec<&'static str> = Vec::new();
    for member in pool_def.members {
        if provider::find(member).is_none() {
            warnings.push(format!("资源站 {member} 未注册（feature 未启用）"));
            continue;
        }
        if pool::allow(member) {
            available.push(*member);
        } else {
            warnings.push(format!("{member} 处于熔断冷却中，本轮已跳过"));
        }
    }

    // 站点级并发：共享池中被多个平台引用的站点由调用方去重，这里只按池成员各查一次。
    // 用 channel + 截止时间收集：先返回的成员结果照常参与聚合，慢于预算的成员跳过；
    // 不再因为某个慢源而丢掉整轮已完成结果（此前 thread::scope 会等全部成员）。
    let collect_deadline = deadline;
    let remaining_secs = deadline
        .saturating_duration_since(Instant::now())
        .as_secs()
        .max(1);
    let (tx, rx) = mpsc::channel::<MemberSearchResult>();
    for &member in &available {
        let src = provider::find(member).expect("成员已在调度前校验");
        let mut opts = SearchOptions::new(keyword, page, remaining_secs);
        opts.cookie = cookie.map(str::to_string);
        opts.user_agent = user_agent.map(str::to_string);
        let tx = tx.clone();
        std::thread::spawn(move || {
            let r = src.search(&opts);
            let _ = tx.send((member, r));
        });
    }
    drop(tx);

    let mut results: Vec<MemberSearchResult> = Vec::new();
    loop {
        let remain = collect_deadline.saturating_duration_since(Instant::now());
        if remain.is_zero() {
            break;
        }
        match rx.recv_timeout(remain) {
            Ok(item) => results.push(item),
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    // 未在预算内返回的成员：记录一次失败并跳过；已完成成员的结果继续参与聚合。
    let responded: HashSet<&str> = results.iter().map(|(m, _)| *m).collect();
    for member in &available {
        if !responded.contains(*member) {
            let name = provider::find(member)
                .map(|p| p.display_name())
                .unwrap_or(member);
            pool::record(member, false);
            warnings.push(format!("{name} 搜索超时，已跳过（其他源结果照常返回）"));
        }
    }

    // ── 3. 归属判定 + 去重 + 其他归类 ─────────────────────────
    let cfg = MatchConfig::default();
    let free: Vec<FreeItem> = free_items
        .iter()
        .map(FreeItem::from_value)
        .filter(|f| !f.title.is_empty())
        .collect();

    // 分组：同一本官网书（official_id）合并多个下载源；非官网书按书名+作者去重。
    // 多源排序权重：只有同一本书命中多个源、且能取到章节数时，章节项才参与。
    const W_TITLE: f64 = 0.45;
    const W_AUTHOR: f64 = 0.25;
    const W_INTRO: f64 = 0.10;
    const W_CHAPTER: f64 = 0.20;

    struct BookGroup {
        rank: u8,
        best_score: f64,
        item: Value,
        /// 每个下载源的原始条目（book_id -> item），用于按综合分重新选出默认源。
        items: HashMap<String, Value>,
        sources: Vec<Value>,
        download_source: String,
        /// 官网章节数（多源排序用）。
        official_chapters: Option<u32>,
    }

    let mut groups: HashMap<String, BookGroup> = HashMap::new();
    let mut detail_budget: u32 = 8;
    let mut detail_cache: HashMap<String, OfficialBook> = HashMap::new();

    let ingest = |mut item: Value,
                  member: &'static str,
                  groups: &mut HashMap<String, BookGroup>,
                  detail_budget: &mut u32,
                  detail_cache: &mut HashMap<String, OfficialBook>| {
        let title = classifier::item_title(&item);
        let author = classifier::item_author(&item);
        let intro = classifier::item_intro(&item);
        if title.trim().is_empty() {
            return;
        }

        // 优先级：与免费平台重合 → 隐藏；否则尝试付费平台归属。
        let initial = classifier::classify(&title, &author, &intro, &officials, &free, &cfg);
        if matches!(initial, Classification::Hidden) {
            return;
        }
        let mut accepted = matches!(initial, Classification::Paid { .. });
        let best = classifier::best_match(&title, &author, &intro, &officials);

        // 命中或临界时拉官方详情：用于展示元数据（封面/简介/分类/字数…），
        // 也用于临界分数的复核。详情字段比搜索结果完整且标题无高亮空格。
        let mut official = best.and_then(|b| officials.get(b.index).cloned());
        if let (Some(off), Some(best)) = (official.as_mut(), best) {
            let needs_detail =
                off.intro_full.is_none() || off.cover_url.is_none() || off.category.is_none();
            if needs_detail && *detail_budget > 0 && (accepted || best.score >= 0.5) {
                let cached = detail_cache.get(&off.official_id).cloned();
                if let Some(detail) = cached {
                    merge_detail(off, &detail);
                } else if let Some(src) = official_src
                    && let Ok(detail) = src.fetch_detail(&off.official_id, timeout_secs)
                {
                    *detail_budget = detail_budget.saturating_sub(1);
                    detail_cache.insert(off.official_id.clone(), detail.clone());
                    merge_detail(off, &detail);
                }
            }
        }

        // 详情补全后复核临界匹配。
        let mut final_score = best.map(|b| b.score).unwrap_or(0.0);
        if !accepted && let Some(off) = official.as_ref() {
            let score = classifier::score_against(&title, &author, &intro, off);
            if classifier::accepted(score, classifier::authors_equal(&author, &off.author), &cfg) {
                accepted = true;
                final_score = score;
            }
        }

        if !accepted {
            item["display_platform"] = json!("other");
            item["_provider"] = json!(def.id);
            let key = format!(
                "other:{}|{}",
                classifier::normalize_title(&title),
                classifier::normalize_author(&author)
            );
            groups.entry(key).or_insert_with(|| BookGroup {
                rank: 1,
                best_score: 0.0,
                item: item.clone(),
                items: HashMap::new(),
                sources: Vec::new(),
                download_source: member.to_string(),
                official_chapters: None,
            });
            return;
        }

        // 卡片展示完整官网元数据；book_id / 下载路由仍指向资源站。
        if let Some(off) = official.as_ref() {
            apply_official(&mut item, off);
        }
        item["display_platform"] = json!(def.id);
        item["_provider"] = json!(def.id);

        let official_id = official
            .as_ref()
            .map(|o| o.official_id.clone())
            .unwrap_or_default();
        let official_chapters = official.as_ref().and_then(|o| o.chapter_count);
        let key = format!("official:{official_id}");
        let book_id = item
            .get("book_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let site_name = provider::find(member)
            .map(|p| p.display_name())
            .unwrap_or(member);
        // 匹配分量（书名 0.55 / 作者 0.30 / 简介 0.15），便于前端/调试比较下载源。
        let (title_sim, author_eq, intro_sim) = official
            .as_ref()
            .map(|off| {
                (
                    classifier::title_similarity(&title, &off.title),
                    classifier::authors_equal(&author, &off.author),
                    classifier::intro_similarity(&intro, off.intro_for_match()),
                )
            })
            .unwrap_or((0.0, false, 0.0));
        let source = json!({
            "site": member,
            "site_name": site_name,
            "book_id": book_id,
            "matched_score": (final_score * 100.0).round() / 100.0,
            "match_detail": {
                "title": (title_sim * 10000.0).round() / 10000.0,
                "author": author_eq,
                "intro": (intro_sim * 10000.0).round() / 10000.0,
            },
        });

        let entry = groups.entry(key).or_insert_with(|| BookGroup {
            rank: 0,
            best_score: -1.0,
            item: item.clone(),
            items: HashMap::new(),
            sources: Vec::new(),
            download_source: member.to_string(),
            official_chapters: None,
        });
        entry.official_chapters = entry.official_chapters.or(official_chapters);
        entry.items.insert(book_id.clone(), item.clone());
        let duplicate = entry.sources.iter().any(|s| {
            s.get("site").and_then(Value::as_str) == Some(member)
                && s.get("book_id").and_then(Value::as_str) == Some(book_id.as_str())
        });
        if !duplicate {
            entry.sources.push(source);
        }
        if final_score > entry.best_score {
            entry.best_score = final_score;
            entry.item = item;
            entry.download_source = member.to_string();
        }
    };

    for (member, joined) in results {
        match joined {
            Ok((items, _has_more)) => {
                pool::record(member, true);
                for item in items {
                    ingest(
                        item,
                        member,
                        &mut groups,
                        &mut detail_budget,
                        &mut detail_cache,
                    );
                }
            }
            Err(e) => {
                pool::record(member, false);
                warnings.push(format!("{member} 搜索失败: {e}"));
            }
        }
    }

    let mut items: Vec<(u8, f64, Value)> = groups
        .into_values()
        .map(|mut g| {
            if g.rank == 0 {
                // 同一本书命中多个下载源：加入章节数后再排序，选出默认源。
                if g.sources.len() >= 2
                    && let Some(official_ch) = g.official_chapters
                    && official_ch > 0
                {
                    let jobs: Vec<(String, String)> = g
                        .sources
                        .iter()
                        .map(|s| {
                            (
                                s.get("site")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                s.get("book_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                            )
                        })
                        .collect();
                    // 两个下载源的章节数并行获取（各自的详情/目录请求），避免串行叠加延迟。
                    let counts: Vec<Option<u32>> = std::thread::scope(|scope| {
                        let handles: Vec<_> = jobs
                            .into_iter()
                            .map(|(site, bid)| {
                                scope.spawn(move || {
                                    provider::find(&site)
                                        .and_then(|p| p.fetch_chapter_count(&bid, timeout_secs))
                                })
                            })
                            .collect();
                        handles
                            .into_iter()
                            .map(|h| h.join().unwrap_or(None))
                            .collect()
                    });
                    for (s, chapter) in g.sources.iter_mut().zip(counts) {
                        let chapter_sim = chapter
                            .filter(|c| *c > 0)
                            .map(|c| {
                                s["chapter_count"] = json!(c);
                                (c.min(official_ch) as f64) / (c.max(official_ch) as f64)
                            })
                            .unwrap_or(0.5);
                        let detail = s.get("match_detail").cloned().unwrap_or(Value::Null);
                        let title_sim = detail.get("title").and_then(Value::as_f64).unwrap_or(0.0);
                        let author_eq = detail
                            .get("author")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        let intro_sim = detail.get("intro").and_then(Value::as_f64).unwrap_or(0.0);
                        let total = W_TITLE * title_sim
                            + W_AUTHOR * f64::from(u8::from(author_eq))
                            + W_INTRO * intro_sim
                            + W_CHAPTER * chapter_sim;
                        s["match_detail"]["chapter"] =
                            json!((chapter_sim * 10000.0).round() / 10000.0);
                        s["matched_score"] = json!((total * 100.0).round() / 100.0);
                    }
                    g.sources.sort_by(|a, b| {
                        let sa = a
                            .get("matched_score")
                            .and_then(Value::as_f64)
                            .unwrap_or(0.0);
                        let sb = b
                            .get("matched_score")
                            .and_then(Value::as_f64)
                            .unwrap_or(0.0);
                        sb.total_cmp(&sa)
                    });
                    if let Some(best) = g.sources.first() {
                        let best_bid = best.get("book_id").and_then(Value::as_str).unwrap_or("");
                        let best_site = best.get("site").and_then(Value::as_str).unwrap_or("");
                        let best_score = best.get("matched_score").and_then(Value::as_f64);
                        if let Some(best_item) = g.items.remove(best_bid) {
                            g.item = best_item;
                            g.download_source = best_site.to_string();
                        }
                        if let Some(score) = best_score {
                            g.best_score = score;
                        }
                    }
                }
                g.sources.sort_by(|a, b| {
                    let sa = a
                        .get("matched_score")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0);
                    let sb = b
                        .get("matched_score")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0);
                    sb.total_cmp(&sa)
                });
                let count = g.sources.len();
                g.item["download_sources"] = Value::Array(g.sources);
                g.item["download_source"] = json!(g.download_source);
                g.item["source_count"] = json!(count);
                g.item["matched_score"] = json!((g.best_score * 100.0).round() / 100.0);
            }
            (g.rank, g.best_score, g.item)
        })
        .collect();
    // 付费平台条目在前，其次按匹配度降序。
    items.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.total_cmp(&a.1)));

    Outcome {
        items: items.into_iter().map(|(_, _, v)| v).collect(),
        warnings,
        official_hits,
    }
}
/// 用官方详情补全搜索结果元数据（详情字段优先，搜索结果元数据兜底）。
fn merge_detail(target: &mut OfficialBook, detail: &OfficialBook) {
    if !detail.title.trim().is_empty() {
        target.title = detail.title.clone();
    }
    if !detail.author.trim().is_empty() {
        target.author = detail.author.clone();
    }
    if !detail.intro_short.trim().is_empty() {
        target.intro_short = detail.intro_short.clone();
    }
    if detail.intro_full.is_some() {
        target.intro_full = detail.intro_full.clone();
    }
    if detail.tags.is_some() {
        target.tags = detail.tags.clone();
    }
    if detail.cover_url.is_some() {
        target.cover_url = detail.cover_url.clone();
    }
    if detail.category.is_some() {
        target.category = detail.category.clone();
    }
    if detail.word_count.is_some() {
        target.word_count = detail.word_count;
    }
    if detail.chapter_count.is_some() {
        target.chapter_count = detail.chapter_count;
    }
    if detail.finished.is_some() {
        target.finished = detail.finished;
    }
    if detail.score.is_some() {
        target.score = detail.score;
    }
}

/// 把「目标平台」的完整元数据覆盖到资源站条目上（用于卡片展示）。
///
/// 卡片展示官网的书名/作者/封面/简介/分类/标签/状态/评分/字数/章节数；
/// `book_id` 与下载路由保持资源站，章节与正文仍由下载源提供。
fn apply_official(item: &mut Value, o: &OfficialBook) {
    if !o.title.trim().is_empty() {
        item["title"] = json!(o.title.trim());
    }
    if !o.author.trim().is_empty() {
        item["author"] = json!(o.author.trim());
    }
    let intro = o.intro_for_match().trim();
    if !intro.is_empty() {
        // 卡片简介：截断长文，避免单条 payload 过大。
        let short: String = intro.chars().take(300).collect();
        item["description"] = json!(short);
    }
    if let Some(cover) = o.cover_url.as_deref().filter(|s| !s.trim().is_empty()) {
        item["cover_url"] = json!(cover.trim());
    } else {
        // 与官网对齐：判定为付费平台后，即使本轮详情预算用尽、官网封面暂缺，
        // 也不能回退下载源封面；前端会按 official_id 懒加载官网封面。
        item["cover_url"] = Value::Null;
    }
    if let Some(c) = o.category.as_deref().filter(|s| !s.trim().is_empty()) {
        item["category"] = json!(c.trim());
    }
    if let Some(tags) = o.tags.as_deref() {
        let list: Vec<&str> = tags
            .split([',', '，'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if !list.is_empty() {
            item["tags"] = json!(list);
        }
    }
    if let Some(w) = o.word_count {
        item["word_count"] = json!(w);
    }
    if let Some(c) = o.chapter_count {
        item["chapter_count"] = json!(c);
    }
    if let Some(f) = o.finished {
        item["finished"] = json!(f);
    }
    if let Some(s) = o.score {
        item["score"] = json!(s);
    }
    item["official_id"] = json!(o.official_id.clone());
    item["official_url"] = json!(o.url.clone());
    // 标记元数据来源，search.rs 的通用规范化据此跳过覆盖，避免下载源的
    // 封面/简介把官方数据盖回去。
    item["meta_source"] = json!("official");
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jinjiang_platform_is_registered_with_shared_pool() {
        let def = paid_platform("jinjiang").expect("晋江应已注册");
        assert_eq!(def.official_source, "jjwxc");
        assert_eq!(def.pool_id, "shared_pool_a");
        assert!(pool::find(def.pool_id).is_some());
    }

    #[test]
    fn unknown_platform_returns_warning() {
        let out = search("nope", "测试", 1, 5, &[], None, None);
        assert!(out.items.is_empty());
        assert!(!out.warnings.is_empty());
    }

    #[test]
    fn apply_official_overrides_only_display_fields() {
        let mut item = json!({
            "book_id": "zs:1",
            "title": "资源站标题",
            "author": "资源作者",
            "description": "资源简介",
            "cover_url": "https://zs/cover.jpg",
            "category": "资源分类",
            "word_count": 12345,
            "chapter_count": 67
        });
        let o = OfficialBook {
            official_id: "4449074".into(),
            title: "官方标题".into(),
            author: "星棘".into(),
            intro_short: "官方短简介".into(),
            intro_full: None,
            tags: Some("甜文,穿书".into()),
            cover_url: Some("https://jj/cover.jpg".into()),
            category: Some("原创-言情".into()),
            word_count: Some(369680),
            chapter_count: Some(99),
            finished: Some(true),
            score: Some(9.1),
            url: "https://jj/book".into(),
        };
        apply_official(&mut item, &o);

        // 下载标识保持资源站，其余展示字段完整来自官网
        assert_eq!(item["book_id"], "zs:1");
        assert_eq!(item["title"], "官方标题");
        assert_eq!(item["author"], "星棘");
        assert_eq!(item["word_count"], 369680);
        assert_eq!(item["chapter_count"], 99);
        assert_eq!(item["cover_url"], "https://jj/cover.jpg");
        assert_eq!(item["description"], "官方短简介");
        assert_eq!(item["category"], "原创-言情");
        assert_eq!(item["finished"], true);
        assert_eq!(item["score"], 9.1);
        assert_eq!(item["meta_source"], "official");
    }

    /// 真实端到端（非重放）：晋江官网 + 姿势吧 + 笔下文学。
    #[test]
    #[ignore = "真实网络请求：cargo test --bin unified-novel-downloader live_aggregate -- --ignored --nocapture"]
    fn live_aggregate_jinjiang() {
        let out = search("jinjiang", "救了病娇", 1, 20, &[], None, None);
        println!(
            "official_hits={} pool_items={} warnings={:?}",
            out.official_hits,
            out.items.len(),
            out.warnings
        );
        for item in &out.items {
            println!(
                "ITEM book_id={:?} title={:?} author={:?} platform={:?} cover={:?} category={:?} finished={:?} score={:?} desc_len={:?} sources={:?}",
                item.get("book_id").and_then(Value::as_str),
                item.get("title").and_then(Value::as_str),
                item.get("author").and_then(Value::as_str),
                item.get("display_platform").and_then(Value::as_str),
                item.get("cover_url").and_then(Value::as_str),
                item.get("category").and_then(Value::as_str),
                item.get("finished"),
                item.get("score"),
                item.get("description")
                    .and_then(Value::as_str)
                    .map(|s| s.chars().count()),
                item.get("download_sources")
            );
        }
        assert!(out.official_hits > 0, "晋江官网搜索应有结果");
        for item in &out.items {
            let dp = item
                .get("display_platform")
                .and_then(Value::as_str)
                .unwrap_or_default();
            assert!(
                dp == "jinjiang" || dp == "other",
                "池条目必须被标注为晋江或其他，实际={dp}"
            );
            if dp == "jinjiang" {
                // 卡片展示字段来自官方，下载标识仍来自资源站。
                let bid = item.get("book_id").and_then(Value::as_str).unwrap_or("");
                assert!(
                    bid.starts_with("zs:") || bid.starts_with("bx:"),
                    "book_id 应保持下载源前缀，实际={bid}"
                );
                let cover = item.get("cover_url").and_then(Value::as_str).unwrap_or("");
                assert!(
                    cover.contains("jjwxc.net"),
                    "晋江条目封面应来自官网，实际={cover}"
                );
                assert!(
                    item.get("description")
                        .and_then(Value::as_str)
                        .map(|s| !s.is_empty())
                        .unwrap_or(false),
                    "晋江条目应带官方简介"
                );
                assert!(
                    item.get("category").and_then(Value::as_str).is_some(),
                    "晋江条目应带官方分类"
                );
                let sources = item
                    .get("download_sources")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                assert!(!sources.is_empty(), "晋江条目应有至少一个下载源");
                assert_eq!(
                    item.get("source_count").and_then(Value::as_u64),
                    Some(sources.len() as u64),
                    "source_count 应与 download_sources 长度一致"
                );
                let best_id = sources[0]
                    .get("book_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                assert_eq!(bid, best_id, "book_id 应默认指向匹配度最高的下载源");
            }
        }
        // 至少一条应被判定为晋江（测试关键词是晋江作品）。
        assert!(
            out.items
                .iter()
                .any(|i| i.get("display_platform").and_then(Value::as_str) == Some("jinjiang")),
            "应至少有一条晋江归属结果，warnings={:?}",
            out.warnings
        );
    }

    /// 实网验证：聚合按预算返回「已完成成员」的结果，不再等所有池成员。
    /// 运行：`cargo test aggregate::tests::e2e_live_partial_results -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn e2e_live_partial_results() {
        let t = Instant::now();
        let o = crate::aggregate::search("jinjiang", "诡秘之主", 1, 10, &[], None, None);
        let elapsed = t.elapsed();
        println!(
            "[聚合] items={} official_hits={} elapsed={:.2}s warnings={:?}",
            o.items.len(),
            o.official_hits,
            elapsed.as_secs_f32(),
            o.warnings
        );
        assert!(
            elapsed <= Duration::from_secs(13),
            "聚合必须按预算返回，不能等待所有成员（实际 {elapsed:?}）"
        );
        assert!(
            !o.items.is_empty() || !o.warnings.is_empty(),
            "预算内至少应返回部分结果或明确的失败信息"
        );
    }
}
#[test]
fn apply_official_clears_download_source_cover_when_official_missing() {
    let mut item = json!({
        "book_id": "zs:1",
        "cover_url": "https://resource/cover.jpg"
    });
    let o = OfficialBook {
        official_id: "4449074".into(),
        title: "官方标题".into(),
        ..Default::default()
    };
    apply_official(&mut item, &o);
    assert_eq!(item["official_id"], "4449074");
    assert_eq!(
        item["cover_url"],
        Value::Null,
        "判定为付费平台后不得回退下载源封面"
    );
}
