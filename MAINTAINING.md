# 维护指南

这份文档记录本项目日常维护时推荐执行的检查流程，重点避免 Cargo feature 组合误用。

## Feature 组合约定

项目只有**一种构建方案**：目录与正文均由第三方解析获取，不存在官方 API 分支（`official-api` / `no-official-api` 已从 `Cargo.toml` 移除）。
`shuqi`、`qimao` 已列入 `default`，所以默认组合同时覆盖番茄 / 书旗 / 七猫三个源。

禁止使用的组合：

- `--all-features`：`tts` 与 `tts-native` 是两套互斥的 TTS 后端，`docker` 会关闭程序自更新，不能同时启用。

推荐维护时覆盖以下组合：

1. 默认组合（桌面 / 服务器）：`tts + clipboard + clipboard-arboard + shuqi + qimao`
2. 轻量级跨平台组合（musl / android）：`--no-default-features --features shuqi,qimao,tts-native,clipboard`
3. Docker 镜像组合：`--no-default-features --features shuqi,qimao,tts,docker`

> ⚠️ **历史教训**：`shuqi` / `qimao` 曾经不在 `default` 里，导致本地 `cargo test` / `cargo clippy` 根本
> 不会编译这两个源的代码路径，而 CI 与 Release 产物都带着它们 —— 编译错误只能等 CI 跑挂才发现。
> 因此现在 `default` 必须保留 `shuqi` / `qimao`；改动 `src/shuqi/`、`src/qimao/` 或下载主链路时，
> 一定用默认组合验证，不要随手加 `--no-default-features`。

## 本地检查

Windows PowerShell：

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\maintain.ps1
```

Linux/macOS/Git Bash：

```bash
bash ./scripts/maintain.sh
```

脚本会执行：

- `cargo fmt --all -- --check`
- `cargo test`（默认组合，含三个小说源）
- `cargo clippy --all-targets -- -D warnings`
- `cargo test --no-default-features --features shuqi,qimao,tts-native,clipboard`
- `cargo clippy --no-default-features --features shuqi,qimao,tts-native,clipboard --all-targets -- -D warnings`
- `cargo tree -d`

如果只想快速验证默认路径：

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\maintain.ps1 -SkipCross -SkipTree
```

## 提交前建议

- 业务行为变更：补单元测试或集成测试。
- 依赖变更：查看 `cargo tree -d`，避免引入明显重复/过重依赖。
- feature 变更：同步更新本文件、`Cargo.toml` feature 注释和 CI。
- 发布相关变更：确认 `.github/workflows/build-rust.yml` 的目标平台仍能覆盖。

## 已知维护重点

- 段评（segment comments）依赖官方 API 通道，配置入口已关闭：TUI / 老 CLI / Web UI 都不再提供开关，
  实现代码用 `#[cfg(any())]` 保留不编译（`segment_comments.rs` / `segment_pool.rs` / `segment_shared.rs`），
  接第三方段评接口时再把门控换回真实条件。
- PDF 相关依赖链会引入旧版 `image/time`，如果包体积成为问题，可考虑把 PDF 输出拆成可选 feature。
- 默认 `tts` 依赖较重；如需更轻发布包，可考虑提供默认关闭 TTS 的 lite 构建。
- Web UI 与 TUI 都是用户入口，涉及配置/下载流程时建议同时验证两边行为。
- 多源搜索目前只在 Web UI 提供；TUI 的搜索入口会直接提示改用 Web UI。
