# httpdownloader-rs

**HTTP Downloader 的 Rust 改写项目** —— 基于对原版 [erickutcher/HTTP-Downloader](https://github.com/erickutcher/httpdownloader)（C/C++，GPLv3）的逆向分析与源码研读，用 Rust（tokio + rustls）从零重写的多协议多分段下载器。

> **项目来源说明**：本项目不是原版的 fork，而是一项独立的重写（rewrite）工作。原版为 Windows 原生 C/C++ 程序（IOCP 引擎 + Win32 自绘 UI，约 10 万行）；本项目在阅读原版源码、完成逆向分析报告与软件工程三件套（需求与功能分析 / 架构设计 / 研发计划，见 `docs/`）之后，按里程碑 M0–M4 逐步用 Rust 重新实现其核心能力。因原版采用 GPLv3，本项目同样以 **GPL-3.0-or-later** 发布。

## 核心特性

- **协议**：HTTP/HTTPS（rustls TLS）、FTP/FTPS（AUTH TLS + PBSZ 0 + PROT P）、SFTP（russh，OpenSSH known_hosts 校验 + TOFU）
- **多分段下载**：Range 分段 + 动态分段重分配（对齐原版 `UpdateRangeList` 语义：先完成者认领最大空闲区间）
- **断点续传**：取消/失败时自动落盘 `.hdmeta`（分段账本序列化），重跑自动续传，完成即清除
- **认证**：Basic、Digest（RFC 2617/7616，MD5/SHA-256，qop=auth/legacy；RFC 向量单元测试精确命中）、Cookie jar（RFC 6265 域/路径匹配）
- **代理**：HTTP CONNECT 隧道 + SOCKS5（RFC 1928，含 RFC 1929 用户名/密码认证）
- **限速**：全局令牌桶 + 任务级限速
- **两个前端**：`hd`（CLI，队列/历史/并发调度）与 `hd-gui`（egui 图形界面），共享同一套 `%APPDATA%\httpdownloader-rs\` 存储

## 仓库结构

```
crates/
  hd-core/    平台无关下载引擎与协议库（engine/http/ftp/sftp/proxy/range/auth/queue）
  hd-cli/     命令行前端 hd
  hd-gui/     egui 图形前端 hd-gui
docs/         需求与功能分析、架构设计、研发计划（软件工程三件套）
tests/        Python 测试服务器（Range/慢速/认证/代理）
```

## 构建

需要 Rust 1.85+（edition 2024）。

```powershell
cargo build --release          # 产出 target/release/hd.exe 与 hd-gui.exe
cargo test                     # 41 个单元测试
```

## 用法

### 直连模式

```powershell
hd <URL> [-o 输出路径] [-p 分段数] [--limit 1M]
        [-a user:pass] [-H "K: V"] [-C "sid=abc"]
        [--proxy http://u:p@host:port | socks5://u:p@host:port]
        [--key <SSH私钥>]
```

### 队列模式

```powershell
hd add <URL...> [同上选项]   # 入队（重名自动 file (1).bin 处理）
hd ls                        # 查看队列
hd run                       # 并发处理队列（Ctrl-C 落盘 .hdmeta，重跑自动续传）
hd pause|resume|retry|rm <id>
hd set global-limit 2M       # 全局限速
hd set max-concurrent 5      # 并发下载数
hd set download-dir D:\dl    # 默认下载目录
```

### 图形界面

```powershell
hd-gui    # 下载列表（进度/速度/状态）、添加对话框、设置、明暗主题、剪贴板 URL 监视
```

## 验证

- `cargo test`：41 个单元测试全绿（含 RFC 2617 Digest 官方测试向量、Cookie 匹配、RangeLedger 重分配等）
- 端到端（本地测试服务器）：5 MB 文件 8 分段 / 代理隧道 / 2 MB 断点续传，MD5 均与源文件一致；Basic/Digest 认证通过、无凭据正确报错；队列 add → pause → run → resume → completed → history 全链路走通

## 与原版的对应关系（部分）

| 原版（C/C++） | 本项目（Rust） |
|---|---|
| IOCP 引擎 + UpdateRangeList | tokio 任务 + `RangeLedger`（claim_into 动态重分配） |
| schannel TLS | rustls |
| psftp.dll（SFTP） | russh + russh-sftp（ring 后端） |
| pcre2 URL 嗅探 | clap 参数 / 剪贴板监视 |
| Win32 自绘列表 UI（约 45% 代码量） | egui 前端 |

## 已知限制（详见 docs/研发计划）

- FTP/FTPS/SFTP 暂无本地服务端端到端测试（协议解析有单元测试覆盖）
- SFTP 单流下载（SFTP 协议无 Range 语义）
- GUI 暂无系统托盘；M5（Web API、浏览器扩展、i18n、安装包）未实施

## 许可证

**GPL-3.0-or-later** —— 衍生自 erickutcher/HTTP-Downloader（GPLv3），完整许可证见 [LICENSE](LICENSE)。

本项目仅供学习研究，原版软件版权归 erickutcher 所有，详情见[原仓库](https://github.com/erickutcher/httpdownloader)。
