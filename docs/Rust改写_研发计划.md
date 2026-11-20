# HTTP Downloader Rust 改写 — 研发计划

> 配套文档：`Rust改写_需求与功能分析.md`（SRS）、`Rust改写_架构设计.md`（HLD）。
> 周期为相对工期（单人全职估算）；每里程碑有明确 DoD（完成定义）。

## 里程碑总览

| 里程碑 | 内容 | 交付物 | DoD（验收） | 工期 |
|---|---|---|---|---|
| **M0 脚手架** | workspace、CI、错误/模型/URL 模块 | 可编译测试的 hd-core 骨架 | `cargo test` 全绿，clippy 无警告 | 2 天 |
| **M1 引擎最小闭环** | HTTP(S) 单段下载 + 重定向 + 限速 + 重试 + 预分配写盘 | CLI 可下载单个 URL | A2 子集：100MB 文件下载哈希正确、限速误差 <10% | 1.5 周 |
| **M2 协议完整（P0+P1）** | 多分段 Range 账本、gzip/chunked、Cookie、Basic/Digest、代理、FTP、SFTP(russh)、rustls | hd-core API 完整 | A1/A3：对拍用例集通过、解析覆盖 ≥85% | 2–3 周 |
| **M3 CLI 成品** | add/list/pause/resume/remove/retry 子命令、历史 CSV、设置文件、队列与并发限制 | 可日常使用的 CLI 下载器 | P0 全用例走通；与原版差分验收 | 1 周 |
| **M4 GUI（P2）** | 主窗口列表、添加对话框、设置页、托盘、剪贴板监视、深色模式 | Windows 安装包 | 功能矩阵 ≥90%（F4.1–F4.6） | 4–6 周 |
| **M5 收尾** | Web 服务器 API + 浏览器扩展适配、i18n、打包发布 | v0.1 release + 扩展 | 扩展可推 URL 到 GUI | 1–2 周 |

## M1 内部任务分解（本周执行）

1. `error.rs` / `model.rs` / `range.rs` / `throttle.rs`（纯逻辑，先行单测）
2. `url.rs`：解析 + 默认端口 + 路径转义（对拍原版 ParseURL_A 用例）
3. `http/header.rs`：状态行/头解析纯函数（对照 http_parsing.cpp 函数清单逐个映射）
4. `http/client.rs`：单连接 GET → body 落盘（含 3xx 跟随上限）
5. `file_writer.rs`：set_len 预分配 + write_at
6. `engine.rs` v0：单段 supervisor + 进度事件 + Ctrl-C 暂停持久化（.hdmeta 文件存 RangeLedger）
7. 集成测试：内置 axum mock（慢速/断连/302/404/Range）

## M2 任务分解

1. RangeLedger 多分段 + part 重分配（对照 connection.cpp UpdateRangeList 语义）
2. gzip/deflate/chunked 流式处理
3. Cookie jar（域/路径/过期；对照 http_parsing.cpp ParseCookies）
4. Basic + Digest（qop=auth, MD5/SHA-256）
5. 代理（CONNECT 隧道 / SOCKS4a/5 + 认证）
6. FTP/FTPS（PASV/EPSV/REST；主动模式 P1 末）
7. SFTP（russh）：密码/密钥、known_hosts 指纹
8. TLS 收尾：SNI、系统根证书、版本下限
9. golden test 基建：录制原版流量 fixtures

## M3 任务分解

CLI 设计（clap）：`hd add <URL> [-p 8] [-o dir] [--limit 1M]`、`hd ls`、`hd pause|resume|rm <id>`、`hd set global-limit <n>`；持久化 `%APPDATA%\httpdownloader-rs\`（settings.json + queue.json + history.csv + *.hdmeta）。

## M4 决策点（届时评审）

- **UI 框架二选一**：native-windows-gui（1:1 复刻自绘列表观感）vs egui（开发速度）。默认推荐 **egui + windows 托盘**：treelistview 原版 7.6k 行在 egui 里约 800 行。
- 原版 dark_mode 单独 5.7k 行 → egui 自带主题，成本≈0。
- 多语言：沿用原版 locale JSON 思路，`fluent` 或简单键值表。

## 风险登记（滚动更新）

| # | 风险 | 缓解 | 状态 |
|---|---|---|---|
| R1 | Digest 变体兼容 | RFC 用例集 | M2 验证 |
| R2 | 原版行为细节（重试上限、重分配策略）与源码理解偏差 | 遇疑直接读对应源码行，注释标注出处 | 持续 |
| R3 | GUI 范围蔓延 | M4 先出静态骨架评审再实现 | 未开始 |
| R4 | russh 与 PuTTY 主机密钥格式差异（known_hosts 解析） | M2 spike 一天验证 | 未开始 |

## 质量门禁（每个 PR）

`cargo fmt --check` + `cargo clippy -- -D warnings` + `cargo test`（含集成测试）通过；协议行为改动必须附对拍测试。

## 当前进度

- [x] M0 需求分析、架构设计、研发计划（本文档）
- [x] M0 workspace 脚手架（19→29 单测全绿）
- [x] M1 引擎最小闭环（单/多分段、重定向、限速、重试、TLS、预分配；1MB 端到端 MD5 一致）
- [x] M2 核心（部分）：动态分段重分配（UpdateRangeList 语义）、Cookie jar、
      Basic + Digest（RFC 2617 向量验证）、401 挑战自动重试、HEAD 降级、CLI -a/-H/-C 参数
- [x] **M2 完成**（2026-11-20）：
      - 代理：HTTP CONNECT + SOCKS5（RFC 1928/1929 用户名密码认证），`--proxy http://|socks5://`
      - 断点续传：.hdmeta 持久化（取消/失败落盘、完成清除），engine 支持从 ResumeState 恢复
      - FTP/FTPS：PASV/EPSV + REST，多分段 = 每段独立控制+数据连接；FTPS 走 AUTH TLS+PBSZ 0+PROT P
      - SFTP：russh 0.63(ring) + russh-sftp 3，密码/密钥认证，OpenSSH known_hosts 校验 + TOFU 记录
      - E2E：多分段 MD5 一致、代理隧道 MD5 一致、.hdmeta 断点续传（预置 2MB 断点）MD5 一致
- [x] **M3 CLI 成品**（2026-11-20）：`hd <URL>` 直连模式保留；`add/ls/run/pause/resume/retry/rm/set`
      子命令；queue.json + history.csv 持久化；run 按 max_concurrent_downloads 并发调度；
      add 自动重名 uniquifier "name (1)"；history/queue 记录下载字节数
- [x] **M4 GUI**（2026-11-20）：hd-gui（eframe/egui 0.33）：下载列表（进度条/速度/状态/暂停/
      恢复/删除）、Add 对话框（URL/输出/parts/限速/凭据/代理/SFTP key）、设置窗口（全局限速/
      并发数/默认分段/下载目录）、明暗主题切换、剪贴板 URL 监视（arboard）；与 CLI 共享
      queue.json/settings.json/.hdmeta；冒烟通过
- [ ] M5 收尾：Web 服务器 API + 浏览器扩展适配、i18n、安装包、系统托盘（GUI 暂缺）

## 已知限制（滚动）

- FTP/FTPS/SFTP 无本地 E2E（无本地 FTP/SSH 服务端）；协议解析与握手序列有单测覆盖，
  建议后续对真实服务端（pyftpdlib / 本机 OpenSSH）补端到端用例
- SFTP 为单流下载（SFTP v3 无 Range 多段语义；并行需多 session，未做）
- GUI 无系统托盘、无开机自启（原版 P2 的托盘/最小化到托盘留到 M5）
- `hd run` 跨进程暂停：`pause` 修改 queue.json，正在运行的 `hd run` 进程不会即时中断
  （Ctrl-C 中断 run，引擎各自落盘 .hdmeta，重跑 `hd run` 自动续传）
- russh 使用 ring 后端（默认 aws-lc-rs 需 NASM；放弃了 RSA host-key 协商，现代 ed25519
  服务器不受影响）
