# 晋江聚合搜索架构方案（付费平台池版 v2.1 · 已实施）

> 状态：**v2.1 已实施并完成真实端到端验证**
> v2 相对 v1 的变更：
> 1. **平台池语义修正**——平台池是"付费平台背后的资源站集合"，不是全局资源站池；池成员可被多个付费平台**共享**（详见 §3、§5）。
> 2. **基于 `examples/晋江搜索_解析结果` 的真实抓包重写晋江源实现方案**，并对关键接口做了非重放的真实验证（详见 §6）。
> 3. 晋江官网源从 `BookProvider` 中剥离，改为独立的 `OfficialSource` trait（官方源只提供搜索与元数据，不参与下载）。
>
> v2.1 变更：按实施结果补充实网踩坑（GBK 传参、混排编码、分页参数）与验证记录（§6.5、§11）。
>
> v2.2 变更：卡片展示改为「目标平台元数据 + 下载源内容」混合策略（§7.4）；补充晋江封面 Referer 坑（§6.4）。

---

## 1. 背景与目标

### 1.1 现状

当前软件已集成三个免费平台（番茄、七猫、书旗），另有两个**资源整合站**（非小说平台，聚合转载资源）：

| 资源站 | 代码位置 | 前缀 | 现状 |
|---|---|---|---|
| 姿势吧（zsnovel） | `src/zishiba/` | `zs:` | 按 `BookProvider` 注册，**作为独立平台暴露在搜索结果中** |
| 笔下文学（hen0） | `src/hen0/` | `hen0:` | 同上 |

需要引入**晋江（付费平台）**后，重构展示与搜索机制：资源站不再作为平台出现，而是作为晋江等付费平台背后的**平台池成员**。

### 1.2 目标

1. 搜索结果**只显示 4 个平台**：番茄、七猫、书旗、晋江（+ 聚合列表中的"其他"图标）。
2. **晋江是逻辑平台**：官方源（jjwxc）只负责搜索与元数据；内容获取由其**平台池**（当前 = 姿势吧 + 笔下文学）承担。
3. 资源站条目按"书名 / 作者 / 简介"与晋江官网条目做匹配度判定：
   - 高度相似 → 判定属于晋江 → 展示为晋江，下载走该资源站；
   - 与免费三平台重合 → **隐藏**；
   - 都不匹配 → 显示"其他"图标。
4. 平台池**可共享**：后续接入新的付费平台时，若这两个资源站也收录该平台内容，新平台可复用同一个池（不必重写资源站逻辑）。
5. 资源站不稳定 → 平台池需要健康度、熔断、动态调度。
6. 图标：裁切 `img/晋江.png` 主体（1200×630，Logo 仅占 40~50%，四周大量白边）。

### 1.3 非目标

- 不实现晋江官网 VIP 章节下载/绕过；
- 不改变三个免费平台的搜索与下载逻辑；
- 首版不做跨资源站的书籍身份映射（同一本书在两个资源站的映射关系）。

---

## 2. 现有架构盘点

```
UI(Web app.js / TUI)
   │  GET /api/search?q=&provider=&page=
   ▼
src/ui/web/routes/search.rs          ← 番茄 spawn_blocking + 遍历 providers() 并发
   ▼
src/provider.rs  BookProvider trait  ← 六类能力 + 编译期 REGISTRY
   ▼
shuqi / qimao / zishiba / hen0       ← 各源适配器（feature 门控）
（番茄不进注册表，是默认回退源）
```

关键事实（已核实）：

| # | 事实 | 位置 |
|---|---|---|
| 1 | `BookProvider` trait + 编译期注册表，新源 4 步接入 | `src/provider.rs:75-174` |
| 2 | 搜索结果为 `serde_json::Value` 弱类型，`book_id` 带前缀（`sq:`/`qm:`/`zs:`/`hen0:`，无前缀=番茄） | `search.rs:97-123` |
| 3 | 结果规范化（cover/description/score）在聚合后统一做 | `search.rs:132-190` |
| 4 | 前端平台注册表 + 前缀→徽章/图标硬编码 | `templates/app.js:264-271, 365-371` |
| 5 | 图标编译期内嵌（`include_bytes!`），逐个注册路由 | `templates.rs:9-13`、`router.rs:20-39` |
| 6 | 无平台池 / 全局限流 / 熔断；仅章节下载有 `segment_pool` 与重试 | — |
| 7 | 平台 id/名称/前缀/图标映射分散在 4 处 | 见 1/4/5 行 |

**结论**：`BookProvider` 继续承担"可下载源"角色；新增 `OfficialSource`（官方元数据源）与 Pool/Aggregation 两层，把"资源站直接展示"的生命周期彻底改掉。

---

## 3. 核心概念模型

```
逻辑平台（用户看到的 tab）
├── 免费平台：番茄 / 七猫 / 书旗     ← 自身即物理源，直接展示
├── 付费平台：晋江（可扩展）        ← 官方源 + 平台池
└── 其他：不是 tab，只是图标         ← 池内无法归类的残留条目
```

### 3.1 三个实体

| 实体 | 说明 | 现有对应 |
|---|---|---|
| **付费平台 (PaidPlatform)** | 用户在搜索页看到的平台，如晋江。持有：显示名、官方源、所属平台池 | 新增（晋江第一个） |
| **平台池 (PlatformPool)** | 一组可用于为该付费平台提供内容的资源站。**可被多个付费平台共享** | 新增（晋江的池 = 姿势吧 + 笔下文学） |
| **资源站 (ResourceSite)** | 实际提供搜索/下载能力的站点，实现既有 `BookProvider` | `src/zishiba`、`src/hen0` |

### 3.2 关系（多对多，支持共享）

```
PaidPlatform  1 ──── n  PoolMembership  n ──── 1  ResourceSite
     jinjiang ──┐                                 ┌── zishiba
                ├── shared_pool_a ───────────────┤
  新平台(未来) ──┘                                 └── hen0
```

```jsonc
// 配置即真相（见 §10）
platforms.paid.jinjiang = { display_name: "晋江", official_source: "jjwxc", pool: "shared_pool_a" }
pools.shared_pool_a = ["zishiba", "hen0"]
// 未来接入新付费平台时，若这两个站也收录其内容：
platforms.paid.new_platform = { official_source: "xxx", pool: "shared_pool_a" }   // 直接复用
```

**共享带来的两个必须处理的问题**（§5.4 详述）：
1. **查询去重**：同一个资源站被多个池引用时，同一关键词只查一次，结果扇出给所有引用它的池；
2. **归属冲突**：同一条资源站条目可能同时"像"晋江和"像"新平台，需按相似度得分与优先级裁决。

### 3.3 展示规则

| 资源站条目命中情况 | 处理 | 展示 |
|---|---|---|
| 与某付费平台官方条目高度相似（或官方直接命中） | 归入该平台 | 平台=晋江，`book_id` 保留资源站前缀（下载走资源站） |
| 与番茄/七猫/书旗任一结果重合 | **丢弃** | 不显示 |
| 都不匹配 | 保留 | 平台=其他（"其他"图标，无独立 tab） |
| 仅官方命中、但池内无对应资源 | 不展示（无可下载内容） | — |

> 设计取舍：官方源只做判定，**不产生可下载条目**。这样也避免了"看起来能下、点了失败"的体验。

---

## 4. 新架构总览

```
┌──────────────────────────────────────────────────────────────────┐
│ Presentation（app.js）                                            │
│   4 个平台 tab：番茄 / 七猫 / 书旗 / 晋江                            │
│   图标：fqnovel / qmnovel / sqnovel / jjwxc(裁切) / other          │
└──────────────────────────────▲───────────────────────────────────┘
                               │ display_platform 标注后的条目
┌──────────────────────────────┴───────────────────────────────────┐
│ Aggregation（新增 src/aggregate/）                                 │
│   SearchAggregator                                                │
│   ├─ 阶段一：并发搜 免费三平台 + 各付费平台官方源（OfficialSource）    │
│   ├─ 阶段二：按池健康度动态决定问哪些资源站（站点级去重）               │
│   ├─ ClaimResolver：付费平台归属判定（相似度+优先级）                 │
│   └─ Filter：与免费平台重合隐藏 / 残留归"其他"                        │
└───────────▲───────────────────────────────▲──────────────────────┘
            │                               │
┌───────────┴──────────────┐   ┌────────────┴───────────────────────┐
│ FreePlatforms            │   │ PlatformPool（新增 src/pool/）      │
│ provider.rs: 番茄/七猫/书旗 │   │  池定义（成员可共享）+ 站点健康度      │
│ （可下载）                │   │  熔断/冷却/动态调度（站点级）          │
└──────────────────────────┘   └────────────┬───────────────────────┘
                                            │
                             ┌──────────────┴──────────────┐
                             │ ResourceSites（可下载）      │
                             │  zishiba / hen0（BookProvider）│
                             └─────────────────────────────┘
```

### 4.1 分层职责

| 层 | 职责 | 变更 |
|---|---|---|
| FreePlatforms | 免费平台，直接展示与下载 | 保持不变 |
| OfficialSource | 付费平台官方搜索/元数据（jjwxc） | 新增 trait + 实现 |
| PlatformPool | 池定义（成员共享）+ 站点健康度/熔断/调度 | 新增 |
| Aggregation | 两阶段编排、归属判定、去重隐藏、其他归类 | 新增（取代 search.rs 的直接遍历） |
| Presentation | 只认 `display_platform` | 改动 |

---

## 5. 平台池设计（`src/pool/`）

### 5.1 模型

```rust
/// 池定义：静态配置，可被多个付费平台引用（共享）
pub struct PoolDef {
    pub id: PoolId,                 // "shared_pool_a"
    pub members: Vec<SiteId>,       // ["zishiba", "hen0"]
}

/// 站点运行时状态：站点级、全局（一个站点被多池共享时共享同一份健康度）
pub struct SiteState {
    pub breaker: Breaker,           // Closed / Open(until) / HalfOpen
    pub window: VecDeque<Attempt>,  // 最近 N 次搜索请求
    pub avg_latency: Ewma,
}

/// 池视图：面向某个付费平台的聚合可用性
pub struct PoolView<'a> {
    pub def: &'a PoolDef,
    pub available: Vec<SiteId>,     // 过滤掉 Open 的站点
    pub degraded: bool,             // 可用成员 < 成员数
}
```

**关键点**：健康度绑定在**站点**上而不是池上。站点被多个池共享时，健康度天然共享（一个站点挂了，所有引用它的付费平台同时降级），这正是"共用"语义的合理体现。

### 5.2 熔断 / 恢复

- 三态：`Closed` →（连续失败 ≥ 3）→ `Open`（冷却 5 分钟，期间调度直接跳过）→ `HalfOpen`（放行 1 次探测）→ 成功回 `Closed` / 失败回 `Open`（冷却指数退避，上限 30 分钟）。
- 只统计**搜索请求**；下载不计入（用户主动点击的下载失败应直接报错）。
- 官方源（jjwxc）同样需要熔断器（独立于资源站），因为官网也可能限流/改版。

### 5.3 动态调度（两阶段）

```
阶段一（用户响应关键路径）
  并发：番茄 + 七猫 + 书旗 + 每个启用付费平台的 OfficialSource
  超时：15s（沿用现值）
  → 主结果立即返回

阶段二（池内资源站，动态决策）
  对每个"官方有结果"的付费平台：
    a) 取其 PoolView.available（已过滤熔断站点）
    b) 站点级去重：同一站点被多个池引用时只查询一次，结果扇出
    c) 每站点独立 spawn_blocking + 独立超时（默认 10s，可按 avg_latency 收紧）
    d) 总预算 8s，到点返回已完成部分
  官方源无结果的付费平台 → 默认不查其池（可 force 开关）
```

> 官方源不可用时：该付费平台的池仍可查询，但结果**无法判定归属**，按规则落入"其他"（不冒充晋江）。

### 5.4 共享池的冲突裁决

同一条资源站条目可能同时匹配多个付费平台的官方条目（例：资源站同时收录了晋江与新平台的书，书名/作者都相似）：

```
对条目 r：
  claims = [(platform, score) for each paid platform whose official result matched]
  取 score 最高者；
  若 score 差 < 0.05 → 按配置 platform_priority 定序（默认按平台注册顺序，晋江优先）；
  任何 score < 对应阈值 → 不计入该平台
  claims 为空 → 其他
```

### 5.5 下载通道选择

- 条目所属的平台只是**展示归属**，`book_id` 仍是资源站前缀。
- 下载直接用该条目的资源站（`prepare_plan` 已由既有实现提供）。
- 同一本书若在多个池成员都有，v2 不做跨站等价映射，用户看到几条就下几条（与现状一致）。

---

## 6. 晋江源实现方案（基于抓包数据）

### 6.1 抓包资产

- `examples/晋江搜索.saz`（2.92MB，99 个会话）
- `examples/晋江搜索_解析结果/`（238 个文件，同构目录树）

### 6.2 接口清单

| 接口 | 方法 | 加密 | 返回 | 用途 |
|---|---|---|---|---|
| `/app.jjwxc/android/harmony/NovelSearch/keywordSearch` | POST | **请求体为应用层密文**（16 进制，64/80/144 字节 = AES 块整数倍） | 完整列表：`novelid`、`novelname`、`authorname`、`novelintroshort`、`tags`、`novelClass`、`cover`、`novelsize`… | App 搜索（不采用） |
| `/app.jjwxc/android/harmony/NovelSearch/associativeSearch` | POST | 同上（密文） | 仅骨架：`novelId`、`novelName`、`authorId`、`authorName` | 联想（不采用） |
| `https://app-cdn.jjwxc.net/androidapi/novelbasicinfo?novelId=` | GET | **无** | 完整元数据：`novelIntro`（HTML 转义长简介）、`novelTags`、`novelChapterCount`、`novelCover`、`novelScore`… | 详情补全/精确判定 |
| `https://www.jjwxc.net/search.php?kw=…&t=1` | GET | **无加密**；`kw` 需 **GBK 百分号编码**；响应为混合编码 HTML | `h3.title a[href*=onebook.php?novelid=]` 书名、`div.intro` 简介、`div.info` 作者链接；`&p=N` 翻页（已验证） | **官方搜索主通道** |

### 6.3 真实（非重放）验证结果

已用真实网络请求验证：

| 验证项 | 结果 |
|---|---|
| `GET app-cdn.jjwxc.net/androidapi/novelbasicinfo?novelId=4449074` | **HTTP 200**，4778 字节，0.20s，返回完整 JSON（含 `novelIntro`/`novelTags`/章节数） |
| `GET www.jjwxc.net/search.php?kw=<GBK编码>&t=1` | **HTTP 200**，结果与关键词高度相关（"救了病娇"命中目标书 `novelid=4449074`） |
| 分页 `&p=2` | **已验证**：第 2 页首条与第 1 页不同；单页约 24~25 条 |
| App `keywordSearch` | 请求体为密文，需逆向 App 加密；**v1 不采用** |

> 
整本实测（135 章，章内 5 页并行）：

| 下载线程 | 耗时 | 成功/失败 |
|---|---|---|
| 1 | ~75s（外推） | — |
| 4 | **31.0s** | **135 / 0** |
| 6 | 31.3s | 86 / 49（触发站点拒绝） |

> 注意：6 线程的高并发会让站点对本机 IP 进入临时封禁（HTTP 000，需等待冷却）。
> 生产配置固定为 `hen0_workers: 4`；若站点开始拒绝，请降到 2 或 1，并等待冷却。结论：官方源可以用**公开网页搜索 + 详情接口**实现，完全绕开 App 加密，规避与番茄 unidbg 类似的签名维护成本。

### 6.4 实网踩坑（实施中确认，务必保留）

1. **`kw` 必须按 GBK 编码后再百分号编码**。用 UTF-8 传参会把关键词解成乱码，返回完全不相关的结果（实测返回「绘声绘色」类书籍）。实现见 `urlencode_query()`。
2. **响应是混排编码**：同一页里书名可能是 UTF-8，而作者/简介/模板是 GBK；个别标题字符还会被污染成 `?`（如 `故国秋?…`）。因此按**非 ASCII 连续片段**逐段解码（`decode_mixed_html()`）：UTF-8 干净则用；大部分字节有效时用 UTF-8 lossy 保住可读前缀；否则 GBK。
3. **标题会插入高亮空格**（如 `救了病娇 后他总想杀我[穿书]`）。匹配前统一做 `normalize_title`（去空白/标点/小写），不受影响。
4. 结果页单页约 24~25 条，`has_more` 以 `>= 20` 判定；`p=N` 翻页可用。
5. **晋江封面 CDN 会拒绝番茄域名的 Referer**（`i2-static.jjwxc.net` 对 `Referer: https://fanqienovel.com/` 返回 403）。
   封面代理 `media_fetch::fetch_bytes` 对 `jjwxc.net` 域名改用 `Referer: https://www.jjwxc.net/`。

### 6.5 官方源实现（`src/official/jjwxc`）

```rust
pub struct OfficialBook {
    pub official_id: String,   // novelid
    pub title: String,
    pub author: String,
    pub intro_short: String,   // 搜索页 div.intro（可能截断）
    pub intro_full: Option<String>, // 详情接口补全
    pub url: String,
}

pub trait OfficialSource: Send + Sync {
    fn id(&self) -> &'static str;               // "jjwxc"
    fn display_name(&self) -> &'static str;     // "晋江"
    fn search(&self, keyword: &str, page: u32) -> Result<Vec<OfficialBook>>;
    fn fetch_detail(&self, official_id: &str) -> Result<OfficialBook>;
}
```

实现要点：

- 搜索页使用 GB18030 解码（`encoding_rs::GBK`），选择器按抓包 HTML 结构解析（`h3.title > a`、`div.intro`、`div.info > a[href*=oneauthor]`）；
- 搜索页简介常被截断，对**临界匹配**（0.65~0.80）的候选再调 `novelbasicinfo` 取全简介复核，控制请求量；
- 详情接口 HTML 转义（`&lt;br/&gt;`）需反转义后去除标签；
- 反爬风险：频率控制（全局 ≤1 req/s），失败计入官方源熔断器。

---

## 7. 晋江采纳逻辑（`src/aggregate/classifier.rs`）

### 7.1 匹配度算法

```
归一化：
  norm_title(t): 去书名号/空格/全半角统一/去括号副标题（《XXX》(修真) → XXX）/小写
  norm_author(a): 去空格

score(r, o) = 0.55 × title_sim   （字符 bigram Jaccard 或归一化编辑相似度）
            + 0.30 × author_eq   （归一化后完全相等=1.0，否则 0.0）
            + 0.15 × intro_sim   （简介前 80 字符相似度；一方为空按 0.5 折中）

判定（默认值，配置化）：
  score ≥ 0.80                    → 采纳
  0.65 ≤ score < 0.80 且 author_eq → 采纳（简介常被资源站改写，作者相同优先）
  其余                             → 不采纳
```

### 7.2 与免费平台的重合隐藏

```
title_sim ≥ 0.90 且 author_eq = 1 → 视为与免费平台重合 → 隐藏
```

判定优先级：**与免费平台重合隐藏 > 官方命中/付费平台采纳 > 其他**。
用户要求"默认不要与现有三个免费平台重复"，因此即使资源站条目同时能匹配晋江官方条目，
只要它与免费平台结果重合，也一律隐藏（免费平台已有该书，无需重复展示）。

### 7.3 归类输出

| 判定 | `display_platform` | `book_id` | 图标 |
|---|---|---|---|
| 采纳为晋江 | `"jinjiang"` | 资源站原前缀（`zs:`/`hen0:`） | jjwxc（裁切后） |
| 与免费平台重合 | 丢弃 | — | — |
| 其他 | `"other"` | 资源站原前缀 | other（新增通用图标） |

### 7.4 卡片展示元数据来源（v2.2）

搜索结果卡片展示**完整的目标平台（晋江官网）元数据**；只有下载路由与内容来自资源站：

| 字段 | 来源 | 说明 |
|---|---|---|
| 书名 / 作者 / 封面 / 简介 / 分类 / 标签 / 状态 / 评分 / 字数 / 章节数 | **目标平台（晋江官网）** | 命中后拉取 `novelbasicinfo` 详情覆盖；简介截断 300 字 |
| `book_id` / 下载路由 | **下载源（资源站）** | 章节与正文仍由资源站提供 |
| `download_sources` / `download_source` / `source_count` | 聚合层 | 多下载源信息，见 §7.5 |
| `official_id` / `official_url` / `meta_source` | 目标平台 | 辅助字段，前端不展示 |

实现要点：

- 官方详情按 `official_id` 在单次搜索内缓存，最多补全 8 本（官方站限速 1 req/s）；
- 条目打 `meta_source=official`，`search.rs` 的通用规范化会跳过这些条目，防止资源站的 raw 封面/简介回写覆盖官方数据；
- 「其他」类条目不做覆盖，仍展示下载源信息。

### 7.5 同一本书的多个下载源（v2.3）

同一本官网书可能被多个资源站同时命中。聚合层按 **`official_id` 分组**，每组只输出一个卡片条目：

| 字段 | 说明 |
|---|---|
| 卡片展示 | 完整官网元数据（书名/作者/封面/简介/分类/标签/状态/评分/字数/章节数） |
| `book_id` | **匹配度最高的下载源**的 book_id（并列时按站点顺序，姿势吧优先） |
| `download_source` | 默认下载源的 site id |
| `download_sources` | 所有命中源的数组，按匹配度降序：`{site, site_name, book_id, matched_score}` |
| `source_count` | 命中源数量（前端 >1 时显示「N 个下载源」标签） |

详情页（预览弹窗）根据 `download_sources` 生成切换导航：

```
[官网] [姿势吧] [笔下文学]
```

- **官网**：展示官网完整信息，仅供查看；确认按钮禁用并提示切换下载源；
- **下载源 N**：调用 `/api/preview/<book_id>` 展示该源自己的书名/作者/目录/章节数/封面等；
- **切换标签 = 切换下载源**：确认下载使用当前标签对应的 `book_id`。
### 7.6 多下载源排序权重（含章节数，v2.4）

采纳判定仍使用文本权重（书名 0.55 / 作者 0.30 / 简介 0.15）；**同一本书命中多个下载源**时，用下列权重重新排序并选出默认源：

| 分量 | 权重 | 计算 |
|---|---|---|
| 书名 | 0.45 | 归一化后字符 bigram Dice 相似度 |
| 作者 | 0.25 | 归一化后完全一致 = 1，否则 0 |
| 简介 | 0.10 | 归一化后前 80 字符相似度 |
| 章节数 | 0.20 | `min(源,官网) / max(源,官网)`；取不到时按 0.5 折中 |

章节数通过 `BookProvider::fetch_chapter_count`（默认复用 `fetch_update_state`）并行获取，只对多源分组执行。

实测（《不断作死后我成了白月光》，官网 133 章）：

| 下载源 | 章节数 | 书名 | 作者 | 简介 | 章节相似度 | 综合 |
|---|---|---|---|---|---|---|
| 笔下文学 | 135 | 1.0 | ✓ | 0.4304 | 0.9852 | **0.94** |
| 姿势吧 | 123 | 1.0 | ✓ | 0.4304 | 0.9248 | 0.93 |

默认下载源 = 综合分最高的 **笔下文学 (`bx:586264`)**。
### 7.7 详情页性能优化（v2.5）

实测瓶颈：姿势吧目录 **10 章/页、123 章共 13 页**，旧预览调用 `prepare_download_plan` 拉完整目录需 ~9s；而详情页只展示“章节总数 + 首章 + 末章”，不需要完整章列表。

优化项：

| 优化 | 说明 |
|---|---|
| 轻量预览摘要 | `BookProvider::preview_summary`：详情 + 目录第 1 页 + 最后一页（3 个请求），免费平台/不支持的源回退完整计划 |
| 服务端预览缓存 | `/api/preview` 结果按 `book_id|hint_title` 缓存 5 分钟（最多 200 条） |
| 补档轻量化 | `/api/preview/:id/archive` 同样使用摘要，不再后台拉完整目录 |
| 前端悬停预取 | 卡片 mouseover 150ms 后预取 `/api/preview` 并写入 localStorage；点击直接命中缓存 |
| 多源预取 | 打开详情页后后台预取其他下载源的详情，切换标签时秒开 |

实测（《不断作死后我成了白月光》）：

| 操作 | 优化前 | 优化后 |
|---|---|---|
| 姿势吧详情（冷） | 9~10s | **2.02s** |
| 姿势吧详情（二次） | 9~10s | **0.01s** |
| 笔下文学详情（冷/热） | 1.8s | **1.41s / 0.00s** |
| 姿势吧补档 | ~9s | **1.91s** |
### 7.8 下载速度优化（v2.6）

笔下文学的章节标题自带总分页数（如 `第120章(1/5)`），据此把**章内分页并行化**：
抓到第 1 页后，第 2..N 页最多 5 页/批并发抓取；标题无页数或并行失败时回退串行。

实测（《不断作死后我成了白月光》，`bx:586264`，5 章，`max_workers=1`）：

| 方案 | 5 章耗时 | 折合 |
|---|---|---|
| 优化前（分页串行，1 线程） | ~12.5s | ~2.5s/章 |
| 章内并行 3 页/批，1 线程 | 2.91s | ~0.58s/章 |
| 章内并行 5 页/批，1 线程 | **2.72s** | **~0.54s/章** |
| 章内并行 3 页/批，2 线程 | 3.43s | 更慢 |
| 章内并行 3 页/批，4 线程 | 4.13s | 更慢 |

结论：
- 全书 135 章的下载预计约 **75 秒**（不含导出）；
- 该站对**多章并发**反而更慢（请求互相拖慢），因此保持 `max_workers: 1`，靠章内并行提速。

---

## 8. 搜索时序

```
前端 app.js           search.rs(改造)          免费三平台   OfficialSource×n   平台池→资源站
   │ GET /api/search   │                         │             │                │
   │──────────────────►│ spawn_blocking          │             │                │
   │                   │────────────────────────►│             │                │
   │                   │──────────────────────────────────────►│                │
   │◄──────────────────│ 阶段一返回（4 平台结果）  │             │                │
   │  GET /api/search/stage2?token=…（轮询）                                     │
   │──────────────────►│ 池调度（站点级去重、熔断过滤、8s 预算）                     │
   │                   │───────────────────────────────────────────────────────►│
   │◄──────────────────│ 阶段二增量：晋江归属条目 / 其他条目                          │
```

响应扩展（向后兼容）：

```jsonc
{
  "all_items": [ { "book_id": "zs:123", "title": "…",
                   "display_platform": "jinjiang",   // 新增
                   "matched_score": 0.87 } ],        // 新增（仅池条目，便于调试）
  "provider_meta": { …, "jinjiang": { … },
                     "pool": { "zishiba": "closed", "hen0": "open" } }  // 新增
}
```

---

## 9. 文件级改动清单

| 动作 | 路径 | 内容 |
|---|---|---|
| 新增 | `src/official/`（`mod.rs`、`jjwxc/`） | `OfficialSource` trait + 晋江网页搜索/详情实现（GB18030 解析） |
| 新增 | `src/pool/`（`mod.rs`、`pool.rs`、`breaker.rs`） | 池定义（成员共享）、站点健康度、熔断 |
| 新增 | `src/aggregate/`（`mod.rs`、`classifier.rs`、`resolver.rs`） | 两阶段编排、匹配判定、冲突裁决、去重隐藏 |
| 改动 | `src/ui/web/routes/search.rs` | 主源并发改调 `SearchAggregator`；新增 stage2 轮询端点 |
| 改动 | `src/provider.rs` | `providers()` 仅保留免费平台；新增 `resource_providers()`（zishiba/hen0 移出默认展示） |
| 改动 | `src/ui/web/templates/app.js` | `PROVIDER_REGISTRY` 收敛为 4 平台 + other；徽章读 `display_platform` |
| 改动 | `router.rs`、`templates.rs` | 注册 `/assets/jjwxc.png`、`/assets/other.png` |
| 改动 | `Cargo.toml` | 新 feature `jjwxc`；新增 `strsim` |
| 改动 | `config.yml` + `base_system/config.rs` | §10 配置项 |
| 资源 | `img/晋江.png → img/jjwxc.png` | 裁切绿色圆角 Logo 主体（非白像素包围盒，留 ~4% 边距，输出 256×256） |
| 新增 | `img/other.png` | "其他"通用图标（灰底书本占位） |

建议同批收敛：把 4 处分散的平台映射统一为 Rust 侧 `PLATFORM_TABLE`，由 `/api/providers` 下发，前端不再硬编码。

---

## 10. 配置设计

```yaml
platforms:
  paid:
    jinjiang:
      display_name: 晋江
      official_source: jjwxc
      pool: shared_pool_a          # 引用池（可共享）
      matcher:
        accept: 0.80
        accept_with_author: 0.65
        hide_dup: 0.90
  # 未来新增付费平台：实现一个 OfficialSource，然后引用已有池即可
  # new_platform:
  #   display_name: XXX
  #   official_source: xxx
  #   pool: shared_pool_a

pools:
  shared_pool_a:
    members: [zishiba, hen0]       # 本池成员（站点可属于多个池）
    failure_threshold: 3
    cooldown_secs: 300
    cooldown_max_secs: 1800
    stage2_budget_secs: 8
    stage2_timeout_secs: 10

official_sources:
  jjwxc:
    rate_limit_qps: 1
    detail_enrich: borderline     # always | borderline | never
```

---

## 11. 测试策略（强调真实非重放）

| 层级 | 内容 | 说明 |
|---|---|---|
| 单元 | `classifier.rs` | 构造已知 pair 断言三分支：改名续作、同作者不同书、书名含副标题、资源站改写简介 |
| 单元 | 池熔断状态机 | 连续失败→Open→冷却→HalfOpen→恢复；共享站点影响多池 |
| 单元 | 冲突裁决 | 一条目同时匹配两个付费平台时的得分/优先级裁决 |
| **真实端到端** | jjwxc 官方搜索 | 直接请求 `www.jjwxc.net/search.php`，断言 HTTP 200、GB18030 解码正确、解析出 ≥1 条含 novelid/书名/作者的条目（非重放） |
| **真实端到端** | jjwxc 详情 | 直接请求 `novelbasicinfo`，断言 `novelIntro`/`novelTags` 非空 |
| 端到端 | 聚合搜索 | 真实请求 `/api/search`：①无 `zs:`/`hen0:` 平台 tab；②晋江条目 `display_platform=jinjiang`；③免费平台同书不重复出现；④池状态透出正确 |
| 故障注入 | 资源站不可用 | 屏蔽一个站点 → 该站点熔断、另一站点照常；晋江结果仍可展示；官方源不可用 → 池结果落"其他" |
| 手动 | 图标 | 裁切后的 jjwxc.png / other.png 在两个主题下的显示效果 |

抓包数据（`examples/晋江搜索_解析结果`）用于**解析器单测的离线样本**，真实端到端另跑 live 用例（`#[ignore]` + 手动/定时），两者不可互相替代。

### 11.1 本轮已执行结果

| 项 | 结果 |
|---|---|
| `cargo test --bin unified-novel-downloader` | **124 passed / 0 failed**（9 个实网用例默认 ignore） |
| `jjwxc_live_search` / `jjwxc_live_detail` | **2 passed**（真实请求，非重放） |
| `live_aggregate_jinjiang` | **passed**：官方 25 条；池条目 2 条；目标书判定晋江 **score=0.94**；另一条正确落"其他" |
| HTTP 端到端（本地 Web 服务） | `GET /api/search?providers=fanqie,shuqi,qimao,jinjiang` → 22 条；书旗 10 / 七猫 10 / 晋江 1 / 其他 1；**无 `zishiba`/`hen0` 平台条目** |
| 单平台路径 | `provider=jinjiang` → 仅晋江 2 条；`provider=qimao` → 仅七猫 10 条；`provider=fanqie`（未配置签名）→ 0 条 |
| 图标/首页 | `/assets/jjwxc.png`、`/assets/other.png` HTTP 200；首页只有 4 个平台复选框 |
| 前端语法 | `node --check app.js` 通过 |

---

## 12. 分阶段实施

1. **P0 官方源**：`OfficialSource` trait + jjwxc 网页搜索/详情实现 + 真实端到端用例（抓包样本做离线解析测试）。
2. **P1 判定器**：`classifier`/`resolver` + 单元测试（可独立验证）。
3. **P2 聚合接线**：`SearchAggregator` 替换 search.rs 遍历；资源站归类/隐藏；前端 4 平台 + 其他图标；图标裁切。阶段二先同请求阻塞（预算受控）。
4. **P3 平台池**：池定义（共享）、站点健康度/熔断、动态调度、池状态透出。
5. **P4（可选）**：stage2 增量推送；平台映射表前后端收敛；跨站书籍等价映射。

每阶段 feature 门控独立可交付、可回滚。

---

## 13. 已确认结论与遗留问题

### 13.1 已确认

1. 两个资源站 = 现有 zishiba（姿势吧）与 hen0（笔下文学）。
2. "其他"类结果无独立 tab，只在聚合列表中以"其他"图标区分。
3. **平台池是付费平台的池**：晋江的池 = 上述两个资源站；池成员可被多个付费平台共享（未来新平台可直接复用）。

### 13.2 遗留问题（实施中处理）

1. 匹配阈值 0.80/0.65/0.90 为经验初值，需按真实数据校准（本轮实测目标书 0.94，非目标书被正确排除）；
2. 资源站条目的"简介"字段质量参差，临界匹配依赖详情补全（最多 3 次/轮）；
3. 共享池下若两个付费平台同时命中同一资源，默认"晋江优先"，是否改为严格按得分裁决需产品确认；
4. 任务卡/下载库的平台图标目前依赖浏览器 localStorage（`tnd.book_platform`）兜底：换设备/清缓存后，`zs:`/`bx:` 历史下载会显示"其他"。彻底解决需在下载记录中持久化 `display_platform`（后续项）。