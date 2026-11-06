//! 资源站条目 → 付费平台归属判定。
//!
//! 全部为纯函数，便于单元测试与阈值校准（不依赖网络/全局状态）。

use serde_json::Value;

use crate::official::OfficialBook;

/// 匹配阈值（经验初值，待真实数据校准）。
#[derive(Debug, Clone, Copy)]
pub struct MatchConfig {
    /// 直接采纳阈值。
    pub accept: f64,
    /// 作者完全一致时的放宽采纳阈值（简介常被资源站改写）。
    pub accept_with_author: f64,
    /// 与免费平台判定为"同一本书"的隐藏阈值。
    pub hide_dup: f64,
}

impl Default for MatchConfig {
    fn default() -> Self {
        Self {
            accept: 0.80,
            accept_with_author: 0.65,
            hide_dup: 0.90,
        }
    }
}

/// 判定结果。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Classification {
    /// 判定属于该付费平台（score 为匹配度）。
    Paid { score: f64 },
    /// 与免费平台重合，隐藏。
    Hidden,
    /// 无法归类，显示"其他"。
    Other,
}

/// 免费平台结果的最小投影（用于重合判定）。
#[derive(Debug, Clone, Default)]
pub struct FreeItem {
    pub title: String,
    pub author: String,
}

impl FreeItem {
    pub fn from_value(v: &Value) -> Self {
        Self {
            title: item_title(v),
            author: item_author(v),
        }
    }
}

/// 书名归一化：只保留字母/数字/汉字，统一小写。
pub fn normalize_title(input: &str) -> String {
    input
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

/// 作者归一化：只保留字母/数字/汉字，统一小写。
pub fn normalize_author(input: &str) -> String {
    input
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

fn bigrams(s: &str) -> Vec<char> {
    // 以字符 bigram 计数需要成对字符串；这里返回扁平序列，用滑窗比较。
    s.chars().collect()
}

/// 字符 bigram Dice 相似度（0.0~1.0）。
///
/// 对短书名（2~20 字）比编辑距离更宽容，且无需额外依赖。
pub fn similarity(a: &str, b: &str) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    if a == b {
        return 1.0;
    }
    let ca = bigrams(a);
    let cb = bigrams(b);
    if ca.len() < 2 || cb.len() < 2 {
        return 0.0;
    }
    let mut map: std::collections::HashMap<(char, char), i32> = std::collections::HashMap::new();
    for w in ca.windows(2) {
        *map.entry((w[0], w[1])).or_insert(0) += 1;
    }
    let mut inter = 0i32;
    for w in cb.windows(2) {
        if let Some(count) = map.get_mut(&(w[0], w[1]))
            && *count > 0
        {
            *count -= 1;
            inter += 1;
        }
    }
    let total = (ca.len() - 1 + cb.len() - 1) as f64;
    if total <= 0.0 {
        return 0.0;
    }
    (2.0 * inter as f64) / total
}

fn title_sim(a: &str, b: &str) -> f64 {
    similarity(&normalize_title(a), &normalize_title(b))
}

/// 作者是否一致（归一化后完全相等）。
pub fn authors_equal(a: &str, b: &str) -> bool {
    let x = normalize_author(a);
    let y = normalize_author(b);
    !x.is_empty() && x == y
}

fn author_eq(a: &str, b: &str) -> bool {
    authors_equal(a, b)
}

fn intro_sim(a: &str, b: &str) -> f64 {
    let na = normalize_title(a).chars().take(80).collect::<String>();
    let nb = normalize_title(b).chars().take(80).collect::<String>();
    if na.is_empty() || nb.is_empty() {
        // 简介缺失时折中，避免把缺字段当成"不像"。
        return 0.5;
    }
    similarity(&na, &nb)
}

/// 资源站条目与官方条目的匹配度。
pub fn score_against(
    item_title: &str,
    item_author: &str,
    item_intro: &str,
    official: &OfficialBook,
) -> f64 {
    0.55 * title_sim(item_title, &official.title)
        + 0.30
            * if author_eq(item_author, &official.author) {
                1.0
            } else {
                0.0
            }
        + 0.15 * intro_sim(item_intro, official.intro_for_match())
}

/// 书名相似度（已归一化）。
pub fn title_similarity(a: &str, b: &str) -> f64 {
    similarity(&normalize_title(a), &normalize_title(b))
}

/// 简介相似度（已归一化并截断前 80 字符）。
pub fn intro_similarity(a: &str, b: &str) -> f64 {
    intro_sim(a, b)
}
/// 最佳官方候选。
#[derive(Debug, Clone, Copy)]
pub struct BestMatch {
    pub index: usize,
    pub score: f64,
    pub author_eq: bool,
}

pub fn best_match(
    item_title: &str,
    item_author: &str,
    item_intro: &str,
    officials: &[OfficialBook],
) -> Option<BestMatch> {
    officials
        .iter()
        .enumerate()
        .map(|(index, o)| BestMatch {
            index,
            score: score_against(item_title, item_author, item_intro, o),
            author_eq: author_eq(item_author, &o.author),
        })
        .max_by(|a, b| a.score.total_cmp(&b.score))
}

/// 是否达到采纳阈值。
pub fn accepted(score: f64, author_eq: bool, cfg: &MatchConfig) -> bool {
    score >= cfg.accept || (score >= cfg.accept_with_author && author_eq)
}

/// 是否与免费平台结果重合（书名高相似且作者一致）。
pub fn free_duplicate(
    item_title: &str,
    item_author: &str,
    free: &[FreeItem],
    cfg: &MatchConfig,
) -> bool {
    free.iter().any(|f| {
        title_sim(item_title, &f.title) >= cfg.hide_dup && author_eq(item_author, &f.author)
    })
}

/// 完整判定：**与免费平台重合优先隐藏**（用户明确要求不重复展示免费平台已有的书），
/// 其次才是付费平台归属，最后归"其他"。
pub fn classify(
    item_title: &str,
    item_author: &str,
    item_intro: &str,
    officials: &[OfficialBook],
    free: &[FreeItem],
    cfg: &MatchConfig,
) -> Classification {
    if free_duplicate(item_title, item_author, free, cfg) {
        return Classification::Hidden;
    }
    if let Some(best) = best_match(item_title, item_author, item_intro, officials)
        && accepted(best.score, best.author_eq, cfg)
    {
        return Classification::Paid { score: best.score };
    }
    Classification::Other
}

// ── JSON 字段提取（各源字段名不完全一致）──────────────────────

pub fn item_title(v: &Value) -> String {
    ["title", "book_title", "book_name", "novelname", "name"]
        .iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .unwrap_or_default()
        .trim()
        .to_string()
}

pub fn item_author(v: &Value) -> String {
    ["author", "authorname", "author_name", "writer"]
        .iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .unwrap_or_default()
        .trim()
        .to_string()
}

pub fn item_intro(v: &Value) -> String {
    for key in [
        "description",
        "intro",
        "novelintroshort",
        "novelintro",
        "desc",
    ] {
        if let Some(s) = v.get(key).and_then(Value::as_str)
            && !s.trim().is_empty()
        {
            return s.trim().to_string();
        }
    }
    if let Some(s) = v.get("raw").and_then(Value::as_str)
        && !s.trim().is_empty()
    {
        return s.trim().to_string();
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn official(title: &str, author: &str, intro: &str) -> OfficialBook {
        OfficialBook {
            official_id: "1".into(),
            title: title.into(),
            author: author.into(),
            intro_short: intro.into(),
            intro_full: None,
            tags: None,
            cover_url: None,
            category: None,
            word_count: None,
            chapter_count: None,
            finished: None,
            score: None,
            url: String::new(),
        }
    }

    #[test]
    fn normalize_strips_punctuation_and_brackets_content_chars() {
        assert_eq!(
            normalize_title("《救了病娇后他总想杀我[穿书]》"),
            "救了病娇后他总想杀我穿书"
        );
        assert_eq!(normalize_author("墨 香 铜 臭"), "墨香铜臭");
        assert!(similarity("救了病娇", "救了病娇") > 0.99);
    }

    #[test]
    fn classifies_jinjiang_when_title_author_intro_match() {
        let officials = vec![official(
            "救了病娇后他总想杀我[穿书]",
            "星棘",
            "病娇残忍嗜血但很乖，白稚穿书了……",
        )];
        let cls = classify(
            "救了病娇后他总想杀我",
            "星棘",
            "病娇残忍嗜血但很乖，白稚穿书了",
            &officials,
            &[],
            &MatchConfig::default(),
        );
        assert!(matches!(cls, Classification::Paid { score } if score >= 0.80));
    }

    #[test]
    fn classifies_hidden_when_duplicates_free_platform() {
        let free = vec![FreeItem {
            title: "救了病娇后他总想杀我".into(),
            author: "星棘".into(),
        }];
        let cls = classify(
            "救了病娇后他总想杀我",
            "星棘",
            "随便的简介",
            &[],
            &free,
            &MatchConfig::default(),
        );
        assert_eq!(cls, Classification::Hidden);
    }

    #[test]
    fn classifies_other_when_no_claim() {
        let cls = classify(
            "完全无关的书",
            "某某",
            "无关简介",
            &[official("另一本", "另一个人", "另一个简介")],
            &[],
            &MatchConfig::default(),
        );
        assert_eq!(cls, Classification::Other);
    }

    #[test]
    fn free_duplicate_hides_even_if_paid_claim_matches() {
        // 用户要求：与免费平台重合就隐藏，不重复展示。
        let officials = vec![official("同名书", "同一作者", "简介")];
        let free = vec![FreeItem {
            title: "同名书".into(),
            author: "同一作者".into(),
        }];
        let cls = classify(
            "同名书",
            "同一作者",
            "简介",
            &officials,
            &free,
            &MatchConfig::default(),
        );
        assert_eq!(cls, Classification::Hidden);
    }

    #[test]
    fn strong_author_lowers_threshold() {
        let officials = vec![official("重生之我在末世当大佬", "星棘", "")];
        let cfg = MatchConfig::default();
        // 资源站去掉了书名前缀，书名相似度不足以达到 0.80，
        // 但作者一致 + 简介缺失折中后可过 0.65。
        let cls = classify("我在末世当大佬", "星棘", "", &officials, &[], &cfg);
        assert!(matches!(cls, Classification::Paid { .. }));
    }

    #[test]
    fn item_extractors_cover_common_keys() {
        let v = json!({"title":"A","author":"B","description":"C"});
        assert_eq!(item_title(&v), "A");
        assert_eq!(item_author(&v), "B");
        assert_eq!(item_intro(&v), "C");
        let v2 = json!({"book_id":"zs:1","raw":"简介文本"});
        assert_eq!(item_intro(&v2), "简介文本");
    }
}
