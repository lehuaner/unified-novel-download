# 姿势吧(zishiba.com)平台适配报告

> 适配日期:2026-11-06。依据:`examples/zishiba_解析结果` 抓包(2026-11-05,360 手机浏览器,完整覆盖搜索→结果→详情→目录→正文)。
> 配套文档:执行方案见 `zishiba_适配执行方案.md`(含决策记录)。

---

## 1. 交付概览

新增第四个小说源 **姿势吧(zishiba.com,帝国CMS 站)**,前缀 `zs:`:

- `src/zishiba/mod.rs`:适配器主体(约 1250 行,含离线回归测试与实网 e2e)
- `src/zishiba/testdata/`:9 份抓包夹具(~67KB,解析回归真值)
- 接线 **14 处**(原清单 13 处 + 实测发现的 `parse_book_folder_name`),列表见 §4
- `img/zsnovel.svg`:来源角标(抓包未取到站点图标,SVG 占位可替换)
- 无新依赖:复用现有 reqwest/regex/serde_json;**未启用** reqwest cookies feature(Cookie 手工管理)

**质量门**:`cargo test` 92 通过(含 zishiba 离线回归 7 项);`cargo clippy --all-targets` 0 警告;`cargo fmt --check` 通过;`--no-default-features --features shuqi,qimao,tts-native,clipboard` 交叉构建通过;MAINTAINING.md/README feature 约定已同步。

**实网 e2e:两个用例全部通过**(2026-11-06 真站,输出原文见 §3/§5)。

---

## 2. 接口规格(抓包逆向)

| 环节 | 端点 | 说明 |
|---|---|---|
| 搜索 | `POST /e/search/index.php` | 表单 `tbname=bookname&show=title%2Cwriter&tempid=1&keyboard=<URL编码>`;成功 `302 → result/?searchid=N` + `Set-Cookie: vbemnlastsearchtime`;失败 200「信息提示」错误页 |
| 搜索结果 | `GET /e/search/result/?searchid=N` | `li.search-wrap-first`,**单页封顶 20 条、无翻页控件** |
| 详情 | `GET /book/{url_id}/` | `og:novel:*` meta 全套 + `data-bookid`(内部 ID);**URL ID ≠ 内部 ID**(113075 ≠ 115955),目录用内部 ID、章节 URL 用 URL ID |
| 目录 | `GET /e/extend/bookpage/pages.php?id={内部ID}&pageNum={0起}&dz=asc` | JSON `{total,pageSize:10,totalPage,list[{id,title,pic}]}` |
| 正文 | `GET /book/{url_id}/{章节id}.html` | `h2#title` + `div#text` 内 `<p>` 段落 |

错误页统一为 200 + `<title>信息提示</title>`,`<p>` 内区分三种:`时间间隔 30 秒` / `关键字只能在 2~20 个字符之间` / `没有搜索到相关的内容`。

**文档页移动端 UA 要求(实网排障发现)**:站点 WAF 对**桌面 UA** 的 HTML 文档页请求(`/`、`/book/*`、`/e/search/result/*`)直接断开连接——桌面 Chrome、curl、Python、reqwest 全部被拒,与 TLS 指纹、请求头、Cookie、HTTP 协议版本、出口 IP 均无关(逐一排除过);**移动端 UA 一律放行**(手机浏览器可正常访问的原因)。搜索端点与目录接口不受此规则影响。适配器因此**全程使用移动端 UA**(通用安卓 Chrome 移动 UA 实测三类页面全通),这也与站点移动端页面形态一致。

---

## 3. 两大核心问题:根因与解法(实网验证)

### 3.1 搜索冷却 30 秒

**根因**(抓包时间线实证):冷却按 Cookie `vbemnlastsearchtime`(会话维度)裁决,窗口 30 秒;被拒请求不刷新 Cookie,真正执行的搜索(含「无结果」)都刷新;**无 Cookie 的请求视为新访客首搜,不受限**。

**解法**:搜索链路**净身请求**——搜索 POST 不回发该 Cookie,每次搜索等同新访客首搜;两次搜索间保留 3s 礼貌间隔;若站点实际按 IP 限频(净身仍被拒),兜底等待 30s+1s 裕量后重试一次。不做代理轮换等进一步规避。

**实网验证**:

```text
[②] 30s 窗口内连搜 20 条,耗时 0.42s(净身请求绕过生效)
```

抓包中浏览器 6s/14s 重搜均被拒,而适配层 5s 后重搜直接执行——绕过实测有效。搜索 POST 层专项用例(`e2e_live_search_post_layer`)另有逐项验证:首搜 `searchid=2987` 与抓包完全一致、窗口内连搜 0.59s 执行成功。

### 3.2 搜索关键字过长不可搜

**根因**:错误文案写「2~20 个字符」,实测按 **UTF-8 字节**计(PHP `strlen`,EmpireCMS 默认 `searchkeymaxlen=20`)。抓包夹逼:9 汉字 27B 被拒、5 汉字 15B 通过 → **汉字最多 6 个(18B),第 7 个汉字(21B)即被拒**。

**解法**:适配层搜索前置 `sanitize_keyword`:trim → <2 字节报错 → >20 字节按字符边界截断到 ≤20 字节(纯汉字即 6 字)。

**实网验证**(双向,POST 层用例同一测试内):

```text
[③a] 38 字节关键字 → 站点拒绝:「系统限制的搜索关键字只能在 2~20 个字符之间」(与抓包 sid567 一致,站点限制仍为 2~20 字节)
[③b] 截断词「救了病娇后他」(18 字节)→ 302 执行成功,searchid=16832(抓包 16830),耗时 0.58s
```

38 字节全名《救了病娇后他总想杀我[穿书]》被站点拒绝(镜像抓包),截断后执行成功;searchid 相比抓包偏移 2(期间多了两次搜索,缓存序号顺延),符合预期。

### 3.3 完整链路实网验证(搜索→结果→详情→目录→正文→真实下载)

`e2e_live_full_chain` 七步全绿(真站,2026-11-06):

```text
[⓪] 预检通过:HTML 文档页可达
[①] 「病娇」命中 20 条,耗时 0.44s                                    ← 抓包同为 20 条
[②] 30s 窗口内连搜 20 条,耗时 0.42s(净身请求绕过生效)
[③] 全名搜索(自动截断为 18 字节)命中 1 条,耗时 3.13s                 ← 抓包截词搜索恰 1 条
[④] 详情:救了病娇后他总想杀我[穿书] / 星棘 / 完本 / 内部ID 115955      ← 与抓包逐字一致
[⑤] 目录 98 章,首章「第1章」末章「第98章 番外6 襄陵奇遇记(二)」       ← 抓包 total=98
[⑥] 第1章正文 3868 字(含真值锚点「白稚醒了。」,无 script 残留)
[⑦] 下载 3 章:成功 3,失败 0(临时目录落盘)                          ← 真实下载链路
```

---

## 4. 接线清单(14 处)

| # | 文件 | 内容 |
|---|---|---|
| 1 | `Cargo.toml` | `zishiba = []` feature + 入 `default` |
| 2 | `src/main.rs` | `#[cfg(feature = "zishiba")] mod zishiba;` |
| 3 | `src/base_system/book_id.rs` | `zs:` 前缀 / `zishiba.com/book/{id}` URL 识别 |
| 4 | `src/download/plan.rs` | 下载计划路由(纯数字不回退 zishiba,ID 无区分度) |
| 5 | `src/download/downloader.rs` | 下载执行路由 |
| 6 | `src/base_system/novel_updates.rs` | `BookSource::Zishiba` + 更新检测(详情完结态 + 目录 total) |
| 7 | `src/base_system/novel_updates.rs` | `parse_book_folder_name` 认识 `zs:`(实测补漏:下载库目录扫描) |
| 8 | `src/ui/web/routes/search.rs` | provider 分支 + provider_meta + provider_has_more |
| 9 | `src/ui/web/routes/preview.rs` | `is_fanqie` 否定链加 `zs:` |
| 10 | `src/ui/tui/mod.rs` | TUI 聚合搜索分支 |
| 11 | `src/ui/web/templates/index.html` | 搜索源 chip |
| 12 | `src/ui/web/templates/app.js` | `PROVIDER_REGISTRY` + 搜索/加载更多分支 + `parseBookId` |
| 13 | `src/ui/web/templates.rs` + `router.rs` + `routes/index.rs` | 图标资源路由(`image/svg+xml`) |
| 14 | `README.md` + `MAINTAINING.md` | 支持源表 / feature 表 / 组合约定 |

---

## 5. 测试体系(非重放实网 e2e 为主体)

**实网 e2e**(`cargo test zishiba -- --ignored --nocapture`,静态锁串行、人类手速级负载),2026-11-06 全绿:

| 用例 | 覆盖 | 结果 |
|---|---|---|
| `e2e_live_search_post_layer` | 搜索 POST / 冷却绕过 / 截断双向 / 目录全链 | ✅ 五步全绿(§3.1/3.2 输出原文) |
| `e2e_live_full_chain` | 结果页解析 / 详情 / 目录 / 正文 / 真实下载 3 章 | ✅ 七步全绿(§3.3 输出原文) |

**离线回归**(`cargo test` 常跑,7 项):关键字截断边界(6 字 18B/ASCII 20/单字符报错/字符边界)、book_id 识别(前缀/URL/章节 URL)、搜索结果解析(20 条+首条全字段、1 条版)、详情 og 解析、目录 JSON(字符串/数字 total 兼容)、正文解析(段落/无 script)、三种错误页归类——断言值全部来自抓包夹具。

**排障记录(为何一度以为要换网络)**:初测时桌面 UA 下所有 HTML 页请求被断、接口路径却放行,曾误判为"站点按出口 IP 封锁文档页"(当时连桌面 Chrome 都被断,加重了误判);后由"手机能访问"这一现象反推出唯一未测变量——**User-Agent**,实测移动端 UA 即刻全通。此教训写入用例文档:网络类故障先对齐抓包请求的 UA 再下结论。

---

## 6. 已知限制

1. **搜索结果封顶 20 条、无翻页**:站点模板特性,`has_more` 恒 false。
2. **长书名截断到 6 汉字后噪音变大**:站点 20 字节硬限制下的最优解;搜索同时匹配书名+作者(`show=title,writer`),可改用作者名搜索。
3. **净身请求的边界**:绕过的是 Cookie 会话维度的冷却;负载等同"真人开新窗口首搜"(单用户、手速级、单并发),未使用代理轮换/IP 伪造;若站点未来升级 IP 限频,兜底自动退回等待模式。
4. **桌面 UA 会被文档页 WAF 断连**:适配器已内置移动端 UA 规避;若未来站点连移动端 UA 也封(或加签名/验证码),`get_text` 的重试与错误页归类会显式报错,可在 `src/zishiba/mod.rs` 增补对策。

## 7. 后续建议

- `img/zsnovel.svg` 为占位图标,可替换为站点真实 favicon。
- Web UI 实际体验验证(勾选姿势吧搜索/长书名搜索/粘贴 URL):适配层与路由已全部就位,实网链路已由 e2e 覆盖,建议日常使用中观察。
- 若站点给搜索加签名/验证码,参照 `testdata/` 错误页新增归类分支即可。
