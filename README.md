# Unified Novel Downloader

一个用 Rust 编写的多源小说下载器：搜索、抓取、导出为 txt / EPUB / PDF，并可选生成有声书。提供 TUI、Web UI、无障碍命令行三种入口，支持断点续传与下载库管理。

项目早期基于 [Dlmily/Tomato-Novel-Downloader-Lite](https://github.com/Dlmily/Tomato-Novel-Downloader-Lite) 衍生，后经完全重构，现由 [lehuaner](https://github.com/lehuaner) 维护。

> 本程序完全免费。如果你在别处为此付费，你被欺骗了。

---

## 支持的小说源

| 源 | 书籍 ID 写法 | 搜索 | 目录来源 | 正文来源 | 开箱可用 |
| --- | --- | --- | --- | --- | --- |
| 番茄 / 扇听（fanqie） | `7020269838396296228`、分享链接、短链 | Web UI ✅ / TUI ✅（分类 tab + 筛选器 + 分页） | 网页解析 | 第三方 API 地址池 **或** 本地 unidbg 签名 sidecar | ⚠️ 需二选一配置，见[正文获取方式](#正文获取方式) |
| 书旗（shuqi） | `sq:8969239` | ✅ | 书旗开放接口 | 书旗章节接口 | ✅ |
| 七猫（qimao） | `qm:152109` | ✅ | 七猫接口 | 整本缓存包 | ✅ |

- 无前缀的纯数字 ID 默认按番茄处理；若番茄侧解析失败，会自动回退尝试书旗、七猫。
- 番茄的目录与书本信息由网页接口解析获得，不依赖任何官方客户端库。
- **本程序不提供需要登录、VIP 付费章节的下载能力。**

---

## 快速开始

### 1. 直接下载可执行文件

到 [Releases](https://github.com/lehuaner/unified-novel-download/releases) 展开最新版本的 **Assets**，按系统选择：

| 系统 | 文件名 |
| --- | --- |
| Windows x64 | `UnifiedNovelDownloader-Win64-v<版本号>.exe` |
| Windows ARM64 | `UnifiedNovelDownloader-WinArm64-v<版本号>.exe` |
| Linux x64 / ARM64 | `UnifiedNovelDownloader-Linux_amd64-v<版本号>` / `-Linux_arm64-…` |
| 软路由 / NAS（musl） | `UnifiedNovelDownloader-Linux_musl_amd64-v<版本号>` 等 |
| macOS（Apple 芯片 / Intel） | `UnifiedNovelDownloader-macOS_arm64-v<版本号>` / `-macOS_amd64-…` |
| Android（Termux） | `UnifiedNovelDownloader-Android_arm64-v<版本号>` |

首次下载新书建议使用 Web UI 或 TUI；命令行模式只保留"更新本地已有小说"的能力。

### 2. 一键安装脚本（Linux / macOS / Termux）

```sh
bash <(curl -sL https://raw.githubusercontent.com/lehuaner/unified-novel-download/main/installer.sh)
```

脚本会探测系统与架构、拉取对应资产、去掉版本号重命名为 `UnifiedNovelDownloader-<平台>`；在 Termux 下还会生成 `run.sh` 直接启动 Web UI。

### 3. Docker（Web UI）

镜像：[`lehuaner/unified-novel-download-webui`](https://hub.docker.com/r/lehuaner/unified-novel-download-webui)

- `latest`：glibc 版（常规服务器 / 桌面）
- `latest-musl`：musl 版（软路由 / NAS）
- 另外每次发版还会推送 `v<版本号>-glibc` 与 `v<版本号>-musl` 固定标签

```sh
docker run -d \
    --name unified-novel-webui \
    -p 18423:18423 \
    -v /host/data:/data \
    -e UNIFIED_WEB_ADDR=0.0.0.0:18423 \
    -e UNIFIED_WEB_PASSWORD=你的密码 \
    lehuaner/unified-novel-download-webui:latest --server --data-dir /data
```

Docker 镜像已内置"禁用程序自更新"标记，升级请重新拉取镜像。

---

## 使用方式

### Web UI（推荐，功能最全）

```sh
UnifiedNovelDownloader-Win64-v<版本号>.exe --server
```

浏览器打开 `http://127.0.0.1:18423/`。当前只有**搜索**与**下载库**两个页面，具体能力：

- 多源搜索（番茄分类 tab / 筛选器，书旗、七猫翻页），支持“加载更多”、搜索记录与一键清空
- 书籍预览：核对封面 / 作者 / 字数 / 章节数，可选填下载范围（如 `1-50`）后再开始
- 下载完成后弹窗确认书名与输出格式
- 下载库浏览（按目录，不递归平铺）：刷新扫描、列数切换、按卡片下载成品文件、删除文件、目录打包为 zip（保留目录结构，适配有声书）
- 进行中的任务以下载库卡片形式展示：进度百分比、排队中 / 下载中 / 失败 / 已取消状态徽章，并提供**取消任务**按钮
- 已下载书籍若有新章节，卡片上显示“可更新 +N 章”标记
- 密码锁登录与“受信任设备”免密；明暗主题切换

> 配置修改与程序自更新**未在 Web UI 提供入口**（`/api/status`、`/api/app_update`、`/api/history` 等接口仍存在，但前端无对应页面）。需要改配置、看下载历史或检查更新，请使用 TUI 或老 CLI。

局域网 / 公网访问：

```sh
# 监听所有网卡（同时监听 IPv4 与 IPv6，用逗号或分号分隔）
UNIFIED_WEB_ADDR=0.0.0.0:18423,[::]:18423

# 启用密码锁
UnifiedNovelDownloader-Win64-v<版本号>.exe --server --password 你的密码

# HTTPS / 反向代理部署时给登录 Cookie 加 Secure 标志
UnifiedNovelDownloader-Win64-v<版本号>.exe --server --cookie-secure
```

> Web UI 面向自建/局域网使用。若要暴露到公网，请放在反向代理与 HTTPS 之后，并务必启用密码锁。登录后勾选"受信任设备"可免重复输入密码。

### TUI（默认入口）

直接运行二进制即进入 TUI：搜索书籍、按 book_id 或粘贴分享链接下载、断点续传、失败重试、区间选择、格式选择、配置编辑、程序更新。

- 输入关键词即调用多源搜索；番茄结果依赖下方 [正文获取方式](#正文获取方式) 中的 unidbg sidecar 配置，未配置时只返回书旗与七猫结果。
- 选中条目后进入预览，确认范围再下载，链路与 Web UI 完全一致。
- 需要 `Ctrl+V` 粘贴：桌面端开箱可用；Android Termux 需安装 Termux:API 并执行 `pkg install termux-api`。

### 无障碍老 CLI

为视障用户保留的纯文本界面。启用方式：首次进入程序时按三下 `o` 回车，或按一下方向键再按三下 `o`（切换成功会发出提示音）。

老 CLI 仅保留：更新本地已有小说、查看下载历史、修改配置、检查更新。**已禁用新建下载与搜索下载。**

### 命令行模式（非交互，仅更新已有书籍）

如果你需要在自动化脚本中更新**本地已经下载过**的书籍（例如为 Kindle 自动追更）：

```sh
# 更新指定书籍
UnifiedNovelDownloader-Win64-v<版本号>.exe --update <book_id>

# 示例
UnifiedNovelDownloader-Win64-v<版本号>.exe --update 7318247498772674083

# 失败章节重试一次
UnifiedNovelDownloader-Win64-v<版本号>.exe --update <book_id> --retry-failed
```

说明：

- 非交互模式，执行后立即开始更新，无需输入
- 使用 `config.yml` 中的保存路径与下载设置
- **已禁用 `--download` 新建下载**，以降低脚本批量滥用风险
- `--update` 只允许更新默认保存目录下**已有本地下载记录**的书籍；没有记录时会拒绝执行并提示改用 Web UI / TUI 完成首次下载
- 只接受 book_id（可用 `sq:` / `qm:` 前缀），不支持搜索

---

## 正文获取方式

番茄正文支持两条路径，二者选其一即可（书旗、七猫无需任何配置）：

**A. 第三方 API 地址池** —— 在 `config.yml` 中填写 `api_endpoints`

```yaml
api_endpoints:
  - https://<可用端点>
```

**B. 本地 unidbg 签名 sidecar** —— 自算签名直连，需自行部署 `unidbg-boot-server`

```yaml
unidbg_signer_url: http://127.0.0.1:8099
```

优先级：`unidbg_signer_url` 非空时走 B；否则走 A 的地址池（会先预热探测可用端点）。两者都为空时，下载会直接报错并提示如何配置。

其余关键配置项（`config.yml`，程序首次运行自动生成）：

| 配置项 | 说明 |
| --- | --- |
| `novel_format` | 输出格式：`txt` / `epub` / `pdf`，也支持"散装文件"或"下载后询问" |
| `max_workers` | 并发线程数（默认 1；请勿盲目调高，会加大上游压力） |
| `request_timeout` / `max_retries` / `min_wait_time` / `max_wait_time` / `min_connect_timeout` | 超时与退避重试 |
| `save_path` | 保存目录，留空则使用程序所在目录 |
| `enable_audiobook` 及 `audiobook_*` | 有声小说开关与参数 |
| `first_line_indent_em` | EPUB 首行缩进 |
| `preferred_book_name_field` | 书名字段优先级，可设 `ask_after_download` 每次询问 |
| `download_comment_images` / `download_comment_avatars` / `media_download_workers` / `media_limit_per_chapter` / `media_max_dimension_px` | 章节内图片与头像的下载策略 |
| `force_convert_images_to_jpeg` / `convert_heic_to_jpeg` / `jpeg_quality` / `keep_heic_original` | 图片转码策略（HEIC 自动转 JPEG 以兼容阅读器） |
| `auto_clear_dump` / `allow_overwrite_files` / `auto_open_downloaded_files` | 缓存清理、覆盖与下载后打开 |

> 段评（段落评论）功能当前不可用：其抓取依赖官方 API 通道，本项目的构建方案不提供，相关开关已从 TUI / 老 CLI / Web UI 中移除。实现代码保留在仓库中，待接入第三方段评接口后恢复。

---

## 有声小说

内置 [msedge-tts](https://github.com/hs-cn/msedge-tts) 语音合成，可在文本下载后自动生成音频：

- 在配置中开启 `是否生成有声小说` 即可；默认发音人 `zh-CN-XiaoxiaoNeural`，可调语速 / 音量 / 音调与输出格式（`mp3` / `wav`）。音调支持 Hz 写法（如 `+2Hz`、`-10Hz`），留空或 0 表示不调整。
- `有声小说并发数` 默认 24，按机器与网络状况调整；生成过程显示进度。
- 音频存放在输出目录下的 `{书名}_audio` 文件夹，按章节顺序命名（如 `0001-第一章.mp3`）；若已下载到封面会在该目录生成 `cover.jpg`，便于播放器识别封面。
- 断点续跑：已存在且非空的章节音频会被跳过，只补生成缺失章节。
- 也支持第三方 TTS：把 `audiobook_tts_provider` 设为 `third_party`，并配置 `audiobook_tts_api_url` / `audiobook_tts_api_token` / `audiobook_tts_model`（可指向本地服务）。
- 使用 Edge TTS 需联网访问微软服务；生成失败时可在日志中查看详细错误。

---

## 构建

单一构建方案：目录与正文均由第三方解析获取，全部小说源已在 `default` feature 中。

```sh
# 默认构建（番茄 + 书旗 + 七猫 + TTS + 桌面剪贴板）
cargo build --release
```

输出二进制为 `target/release/unified-novel-downloader`（Windows 下带 `.exe`）。

轻量 / 交叉编译场景（musl、Android）改用不依赖原生库的 TTS 后端：

```sh
cargo build --release --no-default-features --features shuqi,qimao,tts-native,clipboard
```

可用 feature：

| feature | 作用 |
| --- | --- |
| `shuqi` / `qimao` | 启用书旗 / 七猫源（已在 `default` 中） |
| `tts` | Edge TTS（`msedge-tts`，默认启用） |
| `tts-native` | 以 `tungstenite` 实现 TTS，避免 openssl/curl 等原生依赖，适合 musl / android |
| `clipboard` / `clipboard-arboard` | TUI 剪贴板；Android 走 Termux API，桌面走 arboard |
| `docker` | Docker 专用构建，关闭程序自更新逻辑 |

> ⚠️ 不要使用 `cargo build --all-features`：`tts` 与 `tts-native` 是两套互斥后端，`docker` 会关闭自更新。
> ⚠️ 本地验证请勿随意加 `--no-default-features`，否则书旗 / 七猫的代码路径根本不会被编译，错误会漏到 CI 才暴露。日常检查与 feature 组合约定见 [MAINTAINING.md](./MAINTAINING.md)。

开发调试（Windows，含 sidecar 联动与热重载）：

```powershell
.\run-dev.bat            # cargo-watch 自动重建
.\run-dev.bat --no-watch # 直接 cargo run
```

---

## 更新机制

- **检查更新**：TUI / 老 CLI / Web UI 均可检查 GitHub Releases 新版本。
- **自更新**：`--self-update`（或界面内操作）下载对应平台资产并替换当前可执行文件；Windows 通过临时 `.bat` 完成替换。
- **热更新**：版本相同但二进制摘要不同时，启动会自动拉取同版本修正包替换。开发态（`cargo run` / target 目录内运行）自动跳过。
- 若下载加速节点不可用，可设置 `TND_DISABLE_ACCEL=1` 直连 GitHub。

---

## 环境变量

| 变量 | 说明 | 默认 |
| --- | --- | --- |
| `UNIFIED_WEB_ADDR` | Web UI 监听地址，支持 IPv6（`[::]:18423`）与逗号/分号分隔多地址 | `127.0.0.1:18423` |
| `UNIFIED_WEB_PASSWORD` | Web UI 密码锁 | 无 |
| `UNIFIED_WEB_COOKIE_SECURE` / `UNIFIED_COOKIE_SECURE` | 给登录 Cookie 加 `Secure`（等价 `--cookie-secure`） | `false` |
| `UNIFIED_DATA_DIR` | Docker entrypoint 使用的数据目录（等价 `--data-dir`） | 无 |
| `TND_DISABLE_ACCEL` | 置 `1` 时禁用下载加速，直连 GitHub | 未设置 |

> 项目改名前这些变量以 `TOMATO_` 为前缀（如 `TOMATO_WEB_ADDR`）。读取侧仍接受旧名以保证既有部署可用，新部署请一律使用 `UNIFIED_*`。

`--data-dir <路径>` 用于指定数据目录（`config.yml` 与 `logs` 存放位置），Docker 部署时挂载卷到该目录即可。

---

## 常见问题

1. **番茄下载报"第三方 API 地址池为空"**
   按[正文获取方式](#正文获取方式)配置 `api_endpoints` 或 `unidbg_signer_url`。书旗、七猫不受影响。

2. **下载章节失败**
   多半不是接口完全失效，常见原因是并发过多导致接口临时熔断，稍后再试；也请确认书籍本身已更新。

3. **能不能调大线程数提速？**
   不建议。调高 `max_workers` 会显著加大上游服务器压力，容易导致接口熔断，反而所有人都下载失败。

4. **章节很多怎么办？**
   保守建议单本不超过 1500 章，过大书目请分段下载。

5. **手机端怎么用？**
   Android Termux 可用，但 TUI/CLI 对小屏不友好，推荐在 Termux 启动 Web UI（`--server`），用手机浏览器操作，或让同局域网其它设备访问。

6. **代理 / VPN 导致失败**
   请使用直连网络，任何影响网络正常性的代理都可能导致接口不可用。

---

## 注意事项

接口随时可能失效，遇到问题请到 [Issues](https://github.com/lehuaner/unified-novel-download/issues) 反馈。

下载内容仅供个人阅读，请勿转载、传播或用于任何侵犯他人权益的行为；因使用本程序产生的任何法律责任由使用者自行承担，作者与贡献者不承担任何损失或后果。

## 免责声明

本程序仅供 Rust 网络爬虫技术、网页数据处理及相关研究的学习用途。请勿将其用于任何违反法律法规或侵犯他人权益的活动。使用前请确认遵守适用的法律法规以及目标网站的使用政策，如有疑问请咨询专业法律顾问。

## 感谢

- 感谢原作者 Dlmily（<https://github.com/Dlmily>）的基础项目，本程序由此衍生后完全重构
- 感谢一路以来的用户，欢迎点 Star 与提建议，你们的反馈是我持续更新的最大动力 ❤️
