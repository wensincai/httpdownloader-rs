# HTTP Downloader Rust 改写 — 架构设计

> 对应 SRS：`Rust改写_需求与功能分析.md`。版本 v1.0（2026-11-20）。

## 1. 总体结构：Cargo Workspace

```
httpdownloader-rs/
├── Cargo.toml                # workspace 根
├── crates/
│   ├── hd-core/              # 平台无关核心库（引擎+协议+模型）—— P0/P1 全部落在这里
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── error.rs          # 统一错误类型（thiserror）
│   │       ├── url.rs            # URL 解析（对标 ParseURL_A/W）
│   │       ├── http/
│   │       │   ├── mod.rs
│   │       │   ├── header.rs     # 头解析：GetHeaderValue/GetHTTPStatus/Content-Range/...（纯函数）
│   │       │   ├── auth.rs       # Basic / Digest（RFC 7616）挑战-应答
│   │       │   ├── cookie.rs     # Cookie jar（对标 dllrbt cookie tree）
│   │       │   └── client.rs     # HTTP/1.1 状态机客户端（对标 IOCPConnection 的 HTTP 分支）
│   │       ├── ftp/mod.rs        # FTP/FTPS 客户端（对标 ftp_parsing.cpp）
│   │       ├── sftp/mod.rs       # russh 封装（对标 sftp.cpp + lite_psftp）
│   │       ├── range.rs          # Range 区间账本（对标 RANGE_INFO + UpdateRangeList）
│   │       ├── throttle.rs       # 令牌桶限速（全局+单任务两级）
│   │       ├── engine.rs         # 下载引擎：任务生命周期/分段调度/重试（对标 IOCPDownloader+StartDownload）
│   │       ├── file_writer.rs    # 预分配文件+分段写锁（对标 SetFileValidData+LockFileEx）
│   │       ├── model.rs          # DownloadInfo/DownloadState 状态机（对标 DOWNLOAD_INFO）
│   │       ├── settings.rs       # 设置模型 + JSON 持久化（对标注册表）
│   │       └── history.rs        # 下载历史 CSV（对标 F3.3）
│   ├── hd-cli/               # 命令行前端（M3）
│   │   └── src/main.rs       # add/list/pause/resume/remove 子命令 + 进度渲染
│   └── hd-gui/               # Windows GUI（M4，暂占位）
│       └── src/main.rs
```

**分层规则**：hd-core 不依赖任何窗口/托盘代码，全部异步 API 以 `tokio` 为运行时；hd-cli/hd-gui 只消费 hd-core 的公开 trait。

## 2. crate 选型（替代原版自制/绑定部分）

| 原版实现 | Rust 选型 | 说明 |
|---|---|---|
| IOCP + 线程池（connection.cpp） | `tokio`（rt-multi-thread, net, io-util, time, sync） | IOCP 是 tokio Windows 后端的底层实现，语义等价 |
| 自写 HTTP 状态机 | 手写（`tokio::net::TcpStream` + `tokio_rustls`） | 保持与原版对拍能力；不引 reqwest 以便精确控制 Range/重试/限速语义 |
| Schannel/OpenSSL 双后端 | `tokio-rustls`（rustls + ring） | 单后端，证书校验默认开启 |
| 内嵌 PuTTY psftp | `russh` + `russh-sftp` | 指纹验证接口对齐 F2.7 |
| zlib 解压 | `flate2`（chunked/gzip 自动处理） | |
| dllrbt 红黑树 | `std::collections::BTreeMap` / `HashMap` | |
| pcre2 媒体嗅探（扩展用） | `regex` | |
| 注册表设置 | `serde` + `serde_json` | 文件位置 `%APPDATA%\httpdownloader-rs\settings.json` |
| 自绘 UI | M4 再定：`native-windows-gui`（贴近原版）或 `egui`（快） | P2 决策点 |

## 3. 核心数据模型（对标 DOWNLOAD_INFO / SOCKET_CONTEXT）

```rust
// model.rs —— 状态机与原版 STATUS_* 对齐
pub enum DownloadState {
    Queued, Connecting, Downloading, Pausing, Paused,
    Stopping, Stopped, Completed, Failed, Skipped, Moving,
}

pub struct DownloadInfo {
    pub id: u64,
    pub url: Url,                    // 当前（可能重定向后）URL
    pub original_url: String,
    pub file_path: PathBuf,
    pub file_size: Option<u64>,      // Content-Length 未知的分块传输为 None
    pub downloaded: u64,
    pub parts: u16,                  // 目标分段数（对标 di->parts）
    pub active_parts: u16,           // （对标 di->active_parts）
    pub state: DownloadState,
    pub speed: u64,                  // B/s，由引擎每秒聚合
    pub retry_count: u8,
    pub speed_limit: Option<u64>,
    pub auth: Option<AuthCredentials>,
    pub proxy: Option<ProxyInfo>,
    pub cookies: CookieJar,
    pub custom_headers: Vec<(String, String)>,
    pub post_body: Option<Vec<u8>>,
    pub created_at: SystemTime,
}

// range.rs —— 对标 RANGE_INFO 双链表：下载中的区间 / 待分配队列
pub struct RangeLedger {
    /// 每个分段当前负责的 [start, end]；completed 段移除
    pub parts: Vec<PartRange>,
    /// 引擎重分配的空闲区间（对标 range_queue）
    pub unassigned: Vec<(u64, u64)>,
}
impl RangeLedger {
    pub fn split_evenly(total: u64, parts: u16) -> Self;
    pub fn reassign(&mut self, failed_part: usize) -> Option<(u64, u64)>; // 对标 UpdateRangeList
    pub fn next_offset(&mut self, part: usize, n: u64) -> Option<(u64, u64)>;
}
```

## 4. 引擎并发设计（对标 IOCPDownloader）

```
Engine (std::sync 域)
 ├── DownloadHandle { info: Arc<RwLock<DownloadInfo>>, cmd_tx: mpsc::Sender<Cmd>, events: broadcast::Sender<Event> }
 ├── 每个下载 = 一个 supervisor task：
 │     ├── 循环：按 RangeLedger 为每个未完成 part spawn part-task
 │     ├── part-task: connect(+TLS/代理) → 发请求 → 头解析（纯函数） → 循环 read → throttle.acquire(n) → writer.write_at(offset)
 │     ├── 限速：全局令牌桶 + 任务令牌桶串联 acquire
 │     ├── 错误分类：可重试（连接断/超时/5xx）→ 退避重试（retries 上限）→ RangeLedger.reassign
 │     └── 事件：Event::Progress{delta} / StateChanged / Completed / Failed(reason) → CLI/GUI 订阅
 └── 文件：单文件，tokio::fs::File.set_len 预分配 + 每 part 独立偏移写（无需 LockFileEx——part 间区间互斥由 RangeLedger 保证）
```

与原版的关键差异（设计决策）：
1. 原版一个 SOCKET_CONTEXT 处理 IO 事件 + 协议状态混在一个巨型 switch；Rust 把**协议解析（纯函数）与 I/O（async）分离**，解析层可单测。
2. 原版 keep-alive 复用连接池简化为 per-part 独立连接（小文件多连接略浪费，行为差异记录在案，M2 复评）。
3. 全局状态（session totals、状态计数）改为 engine 内部的 `watch`/`broadcast` 通道，不再用 CRITICAL_SECTION。

## 5. HTTP 状态机（对标 ParseHTTPHeader/GetHTTPHeader）

```
[连接] → CONNECT/代理握手(+TLS) → 发送请求行+头
  → 读头（≤ MAX_HEADER, 8KB） → ParseHTTPHeader:
      status 3xx → HandleRedirect（深度上限，对标原版 MAX_REDIRECTS 逻辑）
      WWW-Authenticate → auth.rs 应答后重发
      Content-Encoding: gzip → flate2 流式解包
      Transfer-Encoding: chunked → chunk 状态子机
      Content-Length / Content-Range → 更新 file_size 与 RangeLedger
  → body 循环 → 完成 → keep-alive 判定
```

## 6. 测试策略

| 层 | 方法 |
|---|---|
| 协议解析 | golden test：从原版测试流量（自建 nginx + httpbin）抓包转固定字节流，逐函数对拍 |
| Range/限速/重试 | 纯逻辑单测（确定性，无网络） |
| 引擎集成 | `tokio::test` + 内置微型 HTTP 服务器（`axum` dev-dependency）模拟：慢速服务器、断连、302 链、Digest 挑战 |
| 差分验收 | 原版 exe 与 Rust CLI 对同一 URL 集下载，比对文件哈希与行为日志（A1 验收） |

## 7. 风险与对策

| 风险 | 等级 | 对策 |
|---|---|---|
| Digest 认证细节（qop/algorithm 变体）与原版不一致 | 中 | 以 RFC 7616 用例集+抓包对拍 |
| rustls 与 Schannel 证书链/受信根差异 | 低 | rustls 支持 native-certs 加载系统根 |
| FTP 主动模式（PORT 监听）防火墙行为 | 中 | 与原版相同方案（本地监听），P1 验收时真机测试 |
| SFTP 原版是 PuTTY 代码语义，russh 算法覆盖面 | 中 | M2 先跑通常用 KEX/cipher 组合，缺口再补 |
| GUI 工作量（45%）失控 | 高 | P2 独立里程碑，先 native-windows-gui 做骨架评审 |
| GPLv3 合规 | — | 仓库保留 LICENSE 与源码出处声明 |
