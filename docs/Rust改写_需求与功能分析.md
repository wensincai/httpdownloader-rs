# HTTP Downloader Rust 改写 — 需求与功能分析

> 基准：`E:\reverseEngineering\20261112\httpdownloader`（erickutcher/httpdownloader，GPLv3，2015–2025）
> 分析日期：2026-11-20。本文是 Rust 改写项目的需求基线（SRS）。

## 1. 原项目规模盘点（源码实测）

| 指标 | 数值 |
|---|---|
| C/C++ 源码总行数（.cpp+.h） | 100,524 行 |
| .cpp 实现文件 | 45 个 |
| 最大文件 | connection.cpp 10,291 行（IOCP 下载引擎） |
| UI 层合计 | treelistview 7.6k + dark_mode 5.7k + wnd_proc_* 约 28k + menus/drag_drop/tray ≈ **45k 行（占 45%）** |
| 协议层 | http_parsing 6.7k + ftp_parsing 2.3k + sftp 3.4k + ssl_schannel/openssl 2.6k ≈ **15k 行** |
| 平台绑定层 | lite_*.cpp/h（18 个 DLL 的动态加载封装）≈ 8k 行 |
| 数据/工具层 | utilities 3.4k + file_operations 4.2k + categories 2.6k + list_operations 4.3k ≈ 14k 行 |
| 语言/字符串 | string_tables 874 行 + locale/ 目录（含简体中文） |

**关键结论：近半代码是 UI。协议与引擎核心约 20–25k 行，是 Rust 改写的价值密度最高的部分。**

## 2. 功能清单（源自码导出，编号便于追溯）

### F1 下载引擎（核心）
- F1.1 多分段（parts）并行下载，动态分割 Range，下载中重分配区间（UpdateRangeList）
- F1.2 IOCP 线程池异步 I/O；连接超时监视（Timeout 线程）；keep-alive 复用
- F1.3 断点续传：Range 续传、重试计数、暂停/恢复/重置/重启（ResetDownload/RestartDownload）
- F1.4 全局与单任务限速（download_speed_limit）
- F1.5 gzip/deflate 解压（zlib）；Transfer-Encoding: chunked 处理
- F1.6 同主机多文件共享限制（shared_info/host_list）；每主机 parts 上限
- F1.7 队列：启动队列、同时下载数限制、StartQueuedItem

### F2 协议支持
- F2.1 HTTP/1.1：重定向（HandleRedirect，含 301/302/303/307/308）、Content-Disposition 文件名、Last-Modified、ETag、Content-Range
- F2.2 认证：Basic、Digest（GetDigestValue/GetAuthenticate 挑战-应答）、NTLM 不支持（原版也无）
- F2.3 Cookie：jar 管理（dllrbt 红黑树存储、CopyCookieTree/ParseCookies/ConstructCookie）
- F2.4 自定义请求头、POST payload、自定义方法（GET/POST）
- F2.5 HTTPS：Schannel/OpenSSL 双后端（Rust 改用 rustls 单后端）；SSL 版本选择、证书校验、指纹提示（wnd_proc_fingerprint_prompt）
- F2.6 FTP/FTPS：USER/PASS/PASV/EPSV/EPRT/PORT（主动模式监听 CreateListenSocket）、REST 断点
- F2.7 SFTP：基于内嵌 PuTTY psftp 代码（lite_psftp）；主机指纹验证、密钥管理（wnd_proc_options_sftp_*）
- F2.8 代理：HTTP/HTTPS/SOCKS4/SOCKS5，代理认证

### F3 任务与数据管理
- F3.1 下载列表模型（DOWNLOAD_INFO：状态机 20+ 状态、速度、剩余时间、耗时）
- F3.2 分类（categories.cpp：按类型自动分类）
- F3.3 下载历史、CSV 导入导出
- F3.4 站点管理器（site_manager：站点级 Cookie/凭据/限速）
- F3.5 文件名冲突处理（重命名树 CreateFilenameTree/RenameFile）
- F3.6 设置持久化（注册表 HKCU\Software\HttpDownloader）→ Rust 改为 JSON/TOML 文件

### F4 界面（GUI 版）
- F4.1 主窗口全自绘树形列表（treelistview，列排序/筛选/搜索 wnd_proc_search）
- F4.2 添加下载对话框（URL 列表解析 ParseURLSettings）、速度限制对话框、更新下载对话框
- F4.3 设置页 9 个（常规/连接/代理/FTP/SFTP/SFTP密钥/指纹/外观/高级/Web服务器）
- F4.4 深色模式（dark_mode.cpp 5.7k）、拖放（drag_and_drop）、拖放悬浮窗（drop_window）
- F4.5 系统托盘（system_tray）、任务栏进度（taskbar）、剪贴板监视
- F4.6 多语言（string_tables + locale，含简体中文）
- F4.7 菜单系统（menus.cpp）

### F5 周边能力
- F5.1 本地 Web 服务器（StartServer，端口可配）→ 浏览器扩展对接
- F5.2 浏览器扩展：Chrome / Firefox（仓库内两个扩展目录，发送 URL/下载指令到 F5.1 的 Web API）
- F5.3 更新检查（GitHub releases，version.txt 指向 v1.0.6.9 安装包）
- F5.4 下载完成后：提示音、关机、退出程序
- F5.5 命令行参数（CL_ARGS in globals.h：启动时带 URL 直接下载）

## 3. 需求裁剪决策（P0/P1/P2）

| 优先级 | 范围 | 理由 |
|---|---|---|
| **P0（M1–M3，CLI 必达）** | F1 全部、F2.1–F2.4、F2.6、F2.8、F3.1/3.5/3.6、F5.4/5.5 | 下载器本体价值；无 UI 依赖，可充分单元测试 |
| **P1（M2 内）** | F2.5（rustls）、F2.7（russh）、F3.2/3.3、F5.3 | 用现成 crate 大幅降低成本 |
| **P2（M4，GUI）** | F4 全部、F5.1/5.2 | UI 是 45% 工作量；Web 服务器/扩展依赖 GUI 生命周期 |

**明确不做**：NTLM/Kerberos 认证（原版也没有）；BitTorrent/磁力（超范围）。

## 4. 非功能需求

| NFR | 原版基线 | Rust 改写目标 |
|---|---|---|
| 并发模型 | IOCP + 线程池 + CRITICAL_SECTION | tokio 异步运行时（Segment = task）；文件写入用 spawn_blocking |
| 内存安全 | 手工指针/临界区，逆向可见大量 cleanup 竞态处理 | 所有权模型天然消除；Range/Context 用 Arc<Mutex> 或 actor 化 |
| 依赖形态 | 动态解析 18 个 DLL（零 CRT 依赖诉求） | 静态链接；Windows 目标 <10MB 单文件 |
| 单元可测性 | 原版解析函数与 UI/全局状态耦合严重 | 协议解析 100% 纯函数化，golden test 对拍原版行为 |
| 可移植性 | 仅 Windows | hd-core 平台无关（Linux/macOS 可编译）；GUI 仅 Windows |
| 许可证 | GPLv3 | **改写作品必须继承 GPLv3**（衍生作品） |
| 兼容行为 | — | 响应头解析、重定向、Range 语义、限速粒度对拍原版（用真实服务器做差分测试） |

## 5. 验收标准（摘要，详见研发计划）

- A1：对 `httpbin`/本地 nginx 用例集，单段/多段/断点续传/重定向/限速行为与原版一致
- A2：10GB 文件 8 分段下载，内存占用 <100MB，无句柄泄漏（任务管理器对照）
- A3：协议解析模块行覆盖率 ≥85%（原版解析函数逐个有对应用例）
- A4：CLI 版可完成 P0 全部用例；GUI 版（P2）达到原版功能矩阵的 ≥90%

## 6. 与逆向产物的关系

前期 `output/HTTP_Downloader/` 的 Ghidra 分析（427 函数地图）在本项目中降级为**辅助校验材料**——源码公开后，函数级语义以源码为准；反编译产物仅在验证"运行时动态加载行为"（lite_dlls 机制）时仍有价值。
