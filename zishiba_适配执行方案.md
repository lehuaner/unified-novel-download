# 识字吧(zishiba.com)平台适配执行方案

> 状态:**已实施完成,实网 e2e 全绿** —— 最终结论与验证证据见《zishiba_适配报告.md》。补充发现:站点文档页 WAF 拒绝桌面 UA(与出口 IP 无关),适配器已全程使用移动端 UA,手机/电脑均可正常访问。
> 依据:examples/zishiba_解析结果(360 手机浏览器抓包,2026-11-05,完整覆盖 搜索→结果→详情→目录→正文 全链路)。
> 目标:新增识字吧平台适配器;重点解决 **搜索关键字过长不可搜** 与 **搜索 30 秒冷却** 两个问题;用抓包数据做非重放实网 e2e;输出适配报告。

---

## 1. 抓包分析结论(接口规格)

站点为**帝国CMS(EmpireCMS)**架构(PHP 8.2 / nginx),`e/` 目录、`vbemn` Cookie 前缀均为其特征。无需签名、无需会话引导(首页响应不种 Cookie,首个搜索 POST 无 Cookie 也能成功)。

### 1.1 全链路接口

| 环节 | 请求 | 关键要求 | 响应 |
|---|---|---|---|
| 搜索 | `POST {base}/e/search/index.php` | 表单:`tbname=bookname&show=title%2Cwriter&tempid=1&keyboard=<URL编码>`(首页搜索表单原文) | 成功:`302 → Location: result/?searchid=N` + `Set-Cookie: vbemnlastsearchtime=<unix秒>` |
| 搜索结果 | `GET {base}/e/search/result/?searchid=N` | — | HTML,条目 `li.search-wrap-first` |
| 书籍详情 | `GET {base}/book/{url_id}/` | — | HTML,`og:novel:*` meta 全套 + `section#bookDetails[data-bookid=内部ID]` |
| 目录 | `GET {base}/e/extend/bookpage/pages.php?id={内部ID}&pageNum={0起}&dz=asc` | XHR 头(`X-Requested-With: XMLHttpRequest` + Referer 书籍页) | JSON:`{total, pageSize:10, totalPage, list:[{id,title,pic}]}` |
| 章节正文 | `GET {base}/book/{url_id}/{chapter_id}.html` | — | HTML,`h2#title` + `div#text` 内 `<p>` 段落 |

**关键 ID 关系**(易踩坑):URL 书籍 ID 与内部 ID 不同(如 `/book/113075/` 的 `data-bookid=115955`)。目录接口用**内部 ID**;章节正文 URL 用 **URL ID**(`{base}/book/113075/1334231.html`)。

**搜索结果无翻页**:两次实测分别为 20 条与 1 条,页面无任何分页控件(EmpireCMS 模板每页封顶 20)→ 适配层 `has_more` 恒为 `false`,`page>1` 返回空。

**搜索表单**:首页为 `show=title,writer`(书名+作者);结果页内嵌表单为 `show=title`。取首页形态,作者名也可命中。

### 1.2 两大问题的根因(抓包实锤)

**① 搜索冷却 30 秒**

同一客户端抓包时间线(服务端时间 17:36:39 起):

| 时刻 | 间隔 | 结果 | Set-Cookie? |
|---|---|---|---|
| 17:36:39 | — | ✅ 302 成功(searchid=2987) | ✅ 1793900199 |
| 17:36:45 | +6s | ❌ 200「**系统限制的搜索时间间隔为 30 秒,请稍后再搜索**」 | ❌ |
| 17:36:53 | +14s | ❌ 同上 | ❌ |
| 17:37:12 | +33s | ✅ 执行但无结果「没有搜索到相关的内容」 | ✅ 1793900232 |
| 17:37:42 | +30s | ❌「系统限制的搜索关键字只能在 2~20 个字符之间」 | ❌ |
| 17:37:50 | +38s | ✅ 302 成功(searchid=16830) | ✅ 1793900269 |

结论:
- 冷却窗口 **30 秒**,按 Cookie `vbemnlastsearchtime` 中记录的**上次真正执行的搜索**起算;
- **被冷却拒绝的请求不刷新 Cookie**(17:36:45/17:36:53 两次拒绝均无 Set-Cookie);
- **真正执行的搜索无论成败都刷新 Cookie**(无结果那次也 Set-Cookie);
- 无 Cookie 的"新客户端"首搜不受限,但**不做丢 Cookie 绕过**(那是规避站点规则,且可能触发 IP 风控)。

**② 搜索关键字过长不可搜**

错误文案「系统限制的搜索关键字只能在 2~20 个字符之间」中的"字符"实为 **UTF-8 字节数**(PHP `strlen`,EmpireCMS 默认 `searchkeymaxlen=20`)。用 content-length 反推验证(表单底座 54 字节 + 每汉字编码 9 字节):

| 请求 | body 长度 | 关键字 | UTF-8 字节 | 结果 |
|---|---|---|---|---|
| sid516 | 72 | 病娇(2字) | 6 | ✅ |
| sid545/551/554 | 81 | 3字 | 9 | 通过长度校验(败于冷却/无结果) |
| sid569 | 99 | 救了病娇后他(5字) | 15 | ✅ |
| sid567 | 135 | 9字 | 27 | ❌ 关键字过长 |

边界:接受上限落在 16~27 字节之间,与 20 字节(EmpireCMS 默认)吻合 → **汉字最多 6 个(18 字节),第 7 个汉字(21 字节)即被拒**。书名超 6 字就"搜不了",这就是"不可搜索文字过长"。

### 1.3 错误页统一形态

三种失败均返回 `HTTP 200` + `<title>信息提示</title>`,具体原因在正文 `<p>` 内:

1. `系统限制的搜索时间间隔为 30 秒,请稍后再搜索`(冷却,未执行)
2. `系统限制的搜索关键字只能在 2~20 个字符之间`(长度,未执行)
3. `没有搜索到相关的内容`(已执行,空结果,**会刷新冷却**)

页面另含 3 秒后 `history.go(-1)` 的 JS,无有效内容,可按 `<p>` 文本区分。

---

## 2. 适配设计

### 2.1 模块结构

完全对齐 qimao/shuqi 的**单文件模块模式**,零新依赖(regex/serde_json/reqwest 均已有;**不启用 reqwest cookies feature**,Cookie 手工管理,与项目"不引入 Cookie jar"现状一致):

```
src/zishiba/
├── mod.rs          # 全部逻辑(约 700~800 行,含单元测试)
└── testdata/       # 抓包夹具(入库,共 ~67KB,见 §4.1)
```

`mod.rs` 分节(抄 qimao 排布):模块文档(接口清单+两大问题结论)→ 常量 → book_id 识别 → 客户端(冷却状态机)→ 搜索/详情/目录/正文 → `prepare_zishiba_plan` → `download_zishiba_into_manager` → `search_items`(Web UI)→ `#[cfg(test)]`。

### 2.2 识别与路由

- 前缀:**`zs:`**(与 `qm:`/`sq:` 无冲突);搜索结果 `book_id` 字段一律 `zs:{url_id}`。
- `normalize_book_input` 识别:`zs:113075`、`https://www.zishiba.com/book/113075/`、`…/book/113075/1334231.html`(从章节 URL 也可提书 ID)。插入位置在 `book_id.rs` 现有 shuqi/qimao 判断之后(纯数字默认番茄的语义不变)。
- 命名:provider 名 `zishiba`,UI 显示名 **姿势吧**(站点自称号,见页面 title「小说搜索-xxx-姿势吧」)。

### 2.3 关键字超长治理(核心一)

新增 `sanitize_keyword(raw) -> Result<String>`,全项目第一处搜索关键字预处理:

1. `trim()` 首尾空白;
2. 按 **UTF-8 字节**校验:不足 2 字节(如单写 "a")→ 报错提示;超过 20 字节 → **按字符边界截断到 ≤20 字节**(纯汉字即 6 字/18 字节,ASCII 20 字符);
3. 截断说明记入 tracing 日志;截断只可能让搜索从"必失败"变"可能命中",收益为正。

不做的:先发全长关键字再回退(>20 字节被拒是确定性结论,白发一次白白消耗一次冷却窗口);不做丢 Cookie 绕过。

纯数字关键字:与其他源一致,zishiba 结果页无 bookId 直查接口,走正常搜索即可(搜索 `show=writer` 还能按作者名搜)。

### 2.4 搜索冷却治理(核心二)

**决策点 A 已定:绕过(净身请求)+ 兜底降级。**

抓包证据:首个搜索 POST **无任何 Cookie 即成功**(sid516),全部冷却拒绝都发生在回发了 `vbemnlastsearchtime` 的请求上 → 30 秒限频按 **Cookie 会话**裁决,而非(可验证的)IP 维度。"新访客首搜"不受限。

策略:**搜索链路不回发该 Cookie(净身请求)**,每次搜索在服务端视角等同"新访客首搜"。这与用户开无痕窗口搜一次的请求特征完全一致:单用户、人类手速、单并发,不构造任何超出真人使用强度的负载;不使用代理轮换/IP 伪造/并发扇出等升级手段。

```
┌─ 主路径:净身请求
│   search POST 不携带 vbemnlastsearchtime;响应中的 Set-Cookie 记录但不回发。
│   连续搜索不再有 30s 阻塞,秒级返回。
│
├─ 礼貌下限:SEARCH_MIN_GAP(默认 3s)
│   两次搜索 POST 间强制最小间隔(Mutex<Instant> 节流),防突发连点;
│   测试可注入缩小。
│
└─ 兜底降级:防线三
    若站点实际按 IP 限频(净身请求仍收到「时间间隔」错误页):
    last_executed: Mutex<Option<Instant>> 记录上次「真正执行」时刻,
    以 30s+1s 裕量等待后重试一次;再失败才报错。
    → 假设失效时自动退化为"阻塞等待"模式,结果仍有保证。
```

- **判定"真正执行"的唯一依据是 302**(拿到 searchid);200+信息提示=未执行(长度/冷却)或已执行无结果(解析 `<p>` 区分),仅"已执行"才更新 `last_executed`。
- reqwest 客户端 `redirect(Policy::none)`:搜索 POST 的 302 必须自己接住(区分成败),从 `Location` 正则提 `searchid` 后再 GET 结果页;详情/目录/正文均为直连 URL,不受影响。
- 空结果(`没有搜索到相关的内容`)是**正常业务结果**而非错误:返回空列表。
- 其余端点(结果页/详情/目录/正文)同样不带 Cookie(服务端不要求,抓包证实结果页公有)。

**边界声明(写入报告)**:净身请求绕过的是 Cookie 维度限频;若站点未来升级 IP 限频或人机校验,兜底逻辑自动退回等待模式,不做进一步规避。

### 2.5 解析实现

项目无 scraper/select,沿用正则 + `OnceLock` 手法(network.rs 风格),夹具回归测试保障:

- **搜索结果**:按 `<li class="search-wrap-first">` 切块,逐块提 `href="…/book/(\d+)/"`、`<span class="title">`、`<h5 class="book-author">作者[:：]`、封面 `img src`、简介 `<p>`;实体解码复用 `book_parser::html_utils::unescape_basic_entities`。
- **书籍详情**:正则提 `og:novel:book_name/author/status/update_time/latest_chapter_name/latest_chapter_url`、`og:image`、`og:description`、`og:novel:category`,`data-bookid="(\d+)"` 提内部 ID;`完本→finished=true`,`连载→false`。
- **目录**:serde_json 解析,`totalPage` 上限保护(防异常死循环),逐页 `pageNum=0..totalPage-1` 拉满,`ChapterRef{id, title}`(id=JSON `id`,即章节页文件名)。
- **正文**:定位 `id="text"` 开 div 到 `id="blankDiv"` 之间的片段(比"到最近 `</div>`"抗嵌套),再抽 `<p>…</p>` 逐段实体解码、剔空,重组干净 `<p>` HTML(与 save_chapter 的 html 期望一致);无 `<p>` 时按失败章处理。标题取 `h2#title`。
- 详情/目录/正文请求**不受冷却限制**(抓包证实),但下载并发对 小站礼貌:`max_workers.clamp(1, 4)`(书旗为 1..8)。

### 2.6 下载流程与更新检测

- `prepare_zishiba_plan(config, book_id, meta_hint)`:详情(meta+内部 ID)→ 全量目录 → `merge_meta_prefer_hint_name` → best-effort 封面下载(复用 qimao 的 `download_cover` 模式)。
- `download_zishiba_into_manager(...)`:**逐章型**(同书旗),crossbeam 工作池 `clamp(1,4)` 并发拉章节页,`save_chapter / append_downloaded_chapter / save_error_chapter / save_download_status` + `reporter` 进度 + cancel 检查,全部对齐书旗既有调用序。
- 更新检测:`novel_updates.rs` 增 `BookSource::Zishiba`,`fetch_zishiba_state`:详情页(完结态)+ 目录第 0 页(`total` 即章节数)。

### 2.7 Web 搜索接入

`search_items(client, keyword, page) -> (Vec<Value>, bool)`,字段对齐 qimao/shuqi 约定(`book_id="zs:…"`、title/author/raw/description/cover_url/category/finished/chapter_count 等,缺失置 Null 不补全);`page>1 → ([], false)`。`provider_meta` 报 `{tabs:[], selector:null, has_more:false}`(同书旗形态)。

---

## 3. 接线点清单(全部改动文件)

| # | 文件 | 改动 |
|---|---|---|
| 1 | `Cargo.toml` | `zishiba = []` feature 并加入 `default`(MAINTAINING 教训:漏加 default 会导致本地测不到编译错) |
| 2 | `src/main.rs` | `#[cfg(feature = "zishiba")] mod zishiba;` |
| 3 | `src/base_system/book_id.rs` | `normalize_book_input` 增 zishiba 分支(shuqi/qimao 之后) |
| 4 | `src/download/plan.rs` | `prepare_download_plan` 增 `zs:` 路由(注:纯数字回退链不加 zishiba,其 ID 与番茄/书旗/七猫无区分度,误命中风险大于收益) |
| 5 | `src/download/downloader.rs` | `download_chapters_into_manager` 增 `zs:` 路由 |
| 6 | `src/base_system/novel_updates.rs` | `BookSource::Zishiba` + `source_of` + `fetch_zishiba_state` |
| 7 | `src/ui/web/routes/search.rs` | `want_zishiba` 分支 + spawn_blocking + provider_meta + provider_has_more |
| 8 | `src/ui/web/routes/preview.rs:223` | `is_fanqie` 否定链加 `!starts_with("zs:")` |
| 9 | `src/ui/tui/mod.rs` | `search_books` 增 zishiba 段 |
| 10 | `src/ui/web/templates/index.html` | provider chip(图标 `/assets/zsnovel.svg`) |
| 11 | `src/ui/web/templates/app.js` | `PROVIDER_REGISTRY` 加项;搜索/加载更多 URL 分支(现 else 落 shuqi,必须显式拆分);`parseBookId` 加 `zs:` 前缀与 `zishiba.com/book/{id}` URL 识别 |
| 12 | `src/ui/web/templates.rs` + `router.rs` + `routes/index.rs` | 图标常量/路由/Content-Type(`image/svg+xml`) |
| 13 | `README.md` | 支持源表加一行(`zs:` 前缀、姿势吧/识字吧) |

图标:抓包中 favicon.ico 的响应体实为 HTML 错误页(未拿到真图),故**生成一个简易 SVG 图标**("字"字圆角块),后续可随时替换 `img/zsnovel.svg`。

---

## 4. E2E 测试方案(**实网非重放**,主体)

**核心原则:e2e 直接访问 zishiba.com 真站,不做 mock、不做流量重放。抓包数据的角色是"真值锚点"**——搜索词、书 ID、作者、章节数、章节 ID、正文原句等断言值全部来自抓包,实网返回必须与抓包事实一致,才算适配正确。

### 4.1 形态

- 位置:`src/zishiba/mod.rs` `#[cfg(test)]`(crate 为 bin 目标,tests/ 无法引用内部模块);live 用例标 `#[ignore = "实网 e2e:需联网访问 zishiba.com"]`,命令 `cargo test zishiba -- --ignored --nocapture`;
- 单条有序链路用例 `e2e_live_full_chain`(静态 Mutex 串行化,防并行测试互相污染冷却观测;负载=人类手速级);
- 离线部分仅保留**轻量解析回归**(`include_str!` 夹具直接断言正则解析,防回归用,不是 e2e 主路径);~~本地 mock 服务器重放~~(已按评审意见砍掉)。

### 4.2 实网用例清单(`e2e_live_full_chain`,有序断言)

| # | 步骤 | 真值锚点(来自抓包) | 断言 |
|---|---|---|---|
| ① | 搜索「病娇」 | searchid=2987,20 条,首条 `/book/123808/`《社恐小姐被病娇带回家之后》作者 西样 | 非空,含 `zs:123808`,首条书名/作者一致 |
| ② | **冷却实测**:sleep 5s 后同词再搜(30s 窗口内) | 抓包:回发 Cookie 的请求 6s/14s 均被拒 | **净身请求下即时成功**(计时 <5s);若被拒→兜底等待重试仍须出结果,并在输出中记录"IP 限频假设成立" |
| ③ | **长关键字**:搜全名「救了病娇后他总想杀我[穿书]」(13 字/41B) | 抓包:截断词「救了病娇后他」(5字/15B)→ searchid=16830 恰命中 `/book/113075/` | 搜索成功(不触发"2~20"错误页),结果含 `zs:113075` |
| ④ | 详情 `zs:113075` | og:novel:book_name《救了病娇后他总想杀我[穿书]》/author 星棘/status 完本/data-bookid=115955 | 四项逐一相等 |
| ⑤ | 全量目录 | total=98,pageSize=10,totalPage=10,首章 id=1334231「第1章」,末章 id=1334419「第98章 番外6 襄陵奇遇记(二)」 | 98 章,首末章 id/title 相等 |
| ⑥ | 第 1 章正文 | `/book/113075/1334231.html`,`h2#title`=「第1章」,正文起于「白稚醒了。」 | 标题相等;正文含该句且无 script 残留 |
| ⑦ | **真实下载**:临时目录 Config → `prepare_zishiba_plan` → `download_zishiba_into_manager`(仅前 3 章) | — | 计划 meta/98 章;3 章全部 save 成功,正文文件非空 |

### 4.3 离线解析回归(轻量,非重点)

夹具入库 `src/zishiba/testdata/`(9 个文件 ~67KB,列表见 §4.2 各行来源):搜索结果 2987/16830 两页、错误页 3 张、书籍页、目录页 0/9、章节页。`include_str!` 直接断言:结果页条目数与首条字段、og meta 提取、目录 JSON 解析、正文 `<p>` 抽取、`sanitize_keyword` 边界(13 字→6 字、ASCII 25→20、单字符报错等)。

### 4.4 质量门

`scripts/maintain.ps1` 全绿:`cargo fmt --check` / `cargo test` / `clippy -D warnings`;并按 MAINTAINING 跑 `--no-default-features --features shuqi,qimao,tts-native,clipboard` 交叉组合确认未破坏无 zishiba 构建。

---

## 5. UI / 前端改动

- 搜索页第 4 个 chip(姿势吧,默认勾选);`PROVIDER_REGISTRY` 加 `{name:'zishiba', label:'姿势吧', prefix:'zs:'}`;
- 搜索/加载更多:zishiba 用 `page` 游标但 `has_more` 恒 false,实际不会发起第 2 页(与后端一致);
- 粘贴 `https://www.zishiba.com/book/113075/` 或章节页 URL → 自动识别为 `zs:113075`;
- 结果卡右上来源角标用新图标;
- TUI 搜索自动带上该源(复用聚合逻辑)。

---

## 6. 验收标准

1. `cargo test` 全绿,其中 §4.3 的 1~13 全部通过(离线、秒级、可重复);
2. 手跑 live 冒烟(§4.3-14)真实搜到《救了病娇后他总想杀我[穿书]》并可完整下载 98 章(txt/epub 导出正常);
3. Web UI:勾选姿势吧搜「病娇」出 20 条;搜 13 字长书名能自动截断命中;**30s 内连搜即时返回结果(净身请求),且请求不携带 `vbemnlastsearchtime`**;模拟 IP 限频场景下自动等待重试仍能出结果;粘贴书籍 URL 可直达预览;
4. 更新检测对 `zs:` 书籍给出章节数与完结态;
5. 适配报告输出(见 §8)。

---

## 7. 风险与边界

| 风险 | 评估 | 对策 |
|---|---|---|
| 「2~20 字符」实为字节是从抓包反推+EmpireCMS 默认值推断,未见官方源码 | 低(9字27B被拒/5字15B通过,硬证据夹逼) | 截断阈值做成常量;若 live 冒烟发现 18B 仍被拒,降到 6 字节下界再验 |
| 错误页文案未来变化 | 低 | 解析按「200+信息提示页→提取 `<p>` 文本→关键词归类」,未归类文本原样进错误消息 |
| 站点对高频抓取升级风控(存在 `e/extend/DoTimeRepage` 统计 beacon,未见拦截行为) | 中 | 目录/正文并发压到 ≤4;搜索保持 3s 礼貌间隔;净身请求负载特征等同"真人开无痕窗首搜",不做代理轮换/IP 伪造等升级规避 |
| **净身请求假设失效**(站点实际按 IP 限频,抓包无法证伪) | 中 | 兜底防线自动退化为"等待 30s+重试",结果仍有保证;live 冒烟专项验证(连搜两次看是否被拒) |
| 长书名截到 6 字后结果噪音变大 | 必然 | 截断是站点硬限制下的最优解;`show=title,writer` 保留作者搜;文档向用户说明 |
| 搜索结果封顶 20 条、无翻页 | 站点特性 | 如实 has_more=false,报告说明 |
| `data-bookid` 与 URL ID 混淆 | 已识别 | 代码内两 ID 分开命名(`url_id`/`internal_id`)并有注释 |

---

## 8. 产物

1. `src/zishiba/`(mod.rs + testdata)+ 13 处接线 + `img/zsnovel.svg`;
2. e2e 测试随 `cargo test` 常驻;
3. **适配报告** `zishiba_适配报告.md`(仓库根,与 AI_NOT_FIX_CASES.md 同级):接口规格、两大问题的根因与解法、测试结果(含用例通过清单)、限制与后续建议;
4. 本执行方案文档(评审后可归档/删除)。

## 9. 实施顺序

① 夹具入库 → ② `src/zishiba/mod.rs`(识别→客户端冷却状态机→解析→plan/download→search_items)→ ③ Cargo/main/路由五点接线 → ④ Web/TUI/前端/图标/README → ⑤ e2e 全绿 + maintain 脚本 → ⑥ live 冒烟(需联网,逐项验证冷却/截断真值)→ ⑦ 适配报告。

---

## 附:待确认决策点

| # | 决策 | 结论 |
|---|---|---|
| A | 30s 内连搜时,识字吧源的行为 | **绕过**:搜索链路净身请求(不回发 `vbemnlastsearchtime`),负载等同"新访客首搜";配 3s 礼貌间隔 + IP 限频兜底降级(见 §2.4)。已确认。 |
| B | 关键字截断是否在前端提示 | 静默 + tracing 日志,字段预留(不阻塞) |
| C | 命名与图标 | provider=`zishiba`、显示「姿势吧」、SVG 临时图标,后续可换 |
| D | 纯数字 ID 是否加入番茄失败回退链 | **不加入**(无区分度,误命中风险大) |
