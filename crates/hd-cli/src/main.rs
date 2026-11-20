//! hd — CLI frontend for the hd-core download engine (M3).
//!
//! Modes:
//!   hd <URL> [opts]            direct download (single shot)
//!   hd add <URL...> [opts]     append tasks to the queue
//!   hd ls                      show queue
//!   hd run                     process the queue (max N concurrent)
//!   hd pause|resume|retry|rm <id>
//!   hd set <key> <value>       global-limit | max-concurrent | default-parts | download-dir
//!
//! Persistence: %APPDATA%\httpdownloader-rs\{settings.json,queue.json,history.csv,*.hdmeta}

use std::sync::{Arc, Mutex};
use std::time::Instant;

use clap::{Parser, Subcommand};
use tokio::sync::{broadcast, mpsc};

use hd_core::engine::{Engine, Event, RunParams};
use hd_core::model::{AuthCreds, DownloadInfo, DownloadState};
use hd_core::queue::{append_history, Queue};
use hd_core::settings::Settings;
use hd_core::throttle::Throttle;
use hd_core::engine::{hdmeta_path, load_resume};
use hd_core::{Result, Url};

#[derive(Parser)]
#[command(name = "hd", version, about = "HTTP Downloader (Rust rewrite)")]
struct Cli {
    /// Direct-download URL (when not using subcommands)
    url: Option<String>,

    #[command(subcommand)]
    cmd: Option<Cmd>,

    // ---- direct-mode options (shared shape with Add) ----
    /// Output path (file or directory)
    #[arg(short, long)]
    output: Option<String>,
    /// Number of parallel parts (needs server Range support)
    #[arg(short, long, default_value_t = 1)]
    parts: u16,
    /// Per-task speed limit, e.g. "1M", "500K", bytes if bare
    #[arg(short, long)]
    limit: Option<String>,
    /// Credentials as user:pass (Basic/Digest/FTP/SFTP)
    #[arg(short, long)]
    auth: Option<String>,
    /// Extra request header, repeatable, format "Name: value"
    #[arg(short = 'H', long = "header")]
    headers: Vec<String>,
    /// Cookie header string (e.g. "sid=abc; theme=dark")
    #[arg(short = 'C', long = "cookie")]
    cookie: Option<String>,
    /// Proxy url: http://[user:pass@]host:port or socks5://...
    #[arg(long)]
    proxy: Option<String>,
    /// Private key file (SFTP public-key auth)
    #[arg(long)]
    key: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Add one or more URLs to the queue
    Add {
        urls: Vec<String>,
        #[arg(short, long)]
        output: Option<String>,
        #[arg(short, long, default_value_t = 1)]
        parts: u16,
        #[arg(short, long)]
        limit: Option<String>,
        #[arg(short, long)]
        auth: Option<String>,
        #[arg(short = 'H', long = "header")]
        headers: Vec<String>,
        #[arg(short = 'C', long = "cookie")]
        cookie: Option<String>,
        #[arg(long)]
        proxy: Option<String>,
        #[arg(long)]
        key: Option<String>,
    },
    /// Show the download queue
    Ls,
    /// Process the queue until it is drained
    Run,
    /// Mark a queued task as paused (skipped by `run`)
    Pause { id: u64 },
    /// Re-queue a paused task (resumes from .hdmeta when present)
    Resume { id: u64 },
    /// Re-queue a failed task
    Retry { id: u64 },
    /// Remove a task from the queue
    Rm { id: u64 },
    /// Change settings: global-limit | max-concurrent | default-parts | download-dir
    Set { key: String, value: String },
}

fn parse_limit(s: &str) -> Option<u64> {
    let s = s.trim().to_uppercase();
    if s.is_empty() {
        return None;
    }
    let (num, mult) = match s.strip_suffix('M') {
        Some(n) => (n, 1024.0 * 1024.0),
        None => match s.strip_suffix('K') {
            Some(n) => (n, 1024.0),
            None => (s.as_str(), 1.0),
        },
    };
    num.trim().parse::<f64>().ok().map(|v| (v * mult) as u64)
}

fn default_filename(url: &Url) -> String {
    let res = url.resource.trim_end_matches('/');
    let name = res.rsplit('/').next().unwrap_or("download.bin");
    if name.is_empty() {
        "download.bin".into()
    } else {
        name.to_string()
    }
}

fn parse_auth(s: &str) -> Option<AuthCreds> {
    s.split_once(':').map(|(u, p)| AuthCreds::new(u, p))
}

fn parse_headers(raw: &[String], cookie: &Option<String>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for h in raw {
        match h.split_once(':') {
            Some((k, v)) => out.push((k.trim().to_string(), v.trim().to_string())),
            None => eprintln!("warning: ignoring malformed header '{h}' (want 'Name: value')"),
        }
    }
    if let Some(c) = cookie {
        out.push(("Cookie".into(), c.clone()));
    }
    out
}

fn resolve_out(url: &Url, output: &Option<String>, settings: &Settings) -> std::path::PathBuf {
    match output {
        Some(p) => {
            let pb = std::path::PathBuf::from(p);
            if pb.is_dir() || p.ends_with('/') || p.ends_with('\\') {
                pb.join(default_filename(url))
            } else {
                pb
            }
        }
        None => settings
            .download_directory
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(default_filename(url)),
    }
}

#[allow(clippy::too_many_arguments)]
fn make_info(
    id: u64,
    url_str: &str,
    output: &Option<String>,
    parts: u16,
    limit: &Option<String>,
    auth: &Option<String>,
    headers: &[String],
    cookie: &Option<String>,
    proxy: &Option<String>,
    key: &Option<String>,
    settings: &Settings,
) -> Result<DownloadInfo> {
    let url = Url::parse(url_str)?;
    let file_path = resolve_out(&url, output, settings);
    let mut info = DownloadInfo::new(id, url, file_path, parts.max(1));
    info.speed_limit = limit.as_deref().and_then(parse_limit);
    info.auth = auth.as_deref().and_then(parse_auth);
    info.extra_headers = parse_headers(headers, cookie);
    info.proxy = proxy.as_deref().map(hd_core::proxy::ProxyConfig::parse).transpose()?;
    info.sftp_key = key.as_deref().map(std::path::PathBuf::from);
    Ok(info)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let settings = Settings::load();

    match &cli.cmd {
        Some(Cmd::Ls) => return cmd_ls(),
        Some(Cmd::Run) => return cmd_run(&settings).await,
        Some(Cmd::Pause { id }) => return cmd_set_state(*id, DownloadState::Paused, &[DownloadState::Queued]),
        Some(Cmd::Resume { id }) => return cmd_set_state(*id, DownloadState::Queued, &[DownloadState::Paused]),
        Some(Cmd::Retry { id }) => return cmd_set_state(*id, DownloadState::Queued, &[DownloadState::Failed, DownloadState::Stopped]),
        Some(Cmd::Rm { id }) => return cmd_rm(*id),
        Some(Cmd::Set { key, value }) => return cmd_set(&settings, key, value),
        Some(Cmd::Add { urls, output, parts, limit, auth, headers, cookie, proxy, key }) => {
            return cmd_add(urls, output, *parts, limit, auth, headers, cookie, proxy, key, &settings)
        }
        None => {}
    }

    // direct mode
    match &cli.url {
        Some(url_str) => {
            let mut q = Queue::default();
            let id = 1;
            let info = make_info(
                id,
                url_str,
                &cli.output,
                cli.parts,
                &cli.limit,
                &cli.auth,
                &cli.headers,
                &cli.cookie,
                &cli.proxy,
                &cli.key,
                &settings,
            )?;
            q.items.push(info);
            direct_download(&settings, q.items.pop().unwrap()).await
        }
        None => {
            eprintln!("no URL or subcommand given (try `hd <URL>`, `hd add <URL>`, `hd run`, `hd ls`)");
            std::process::exit(2);
        }
    }
}

// ---------------------------------------------------------------------------
// queue commands
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn cmd_add(
    urls: &[String],
    output: &Option<String>,
    parts: u16,
    limit: &Option<String>,
    auth: &Option<String>,
    headers: &[String],
    cookie: &Option<String>,
    proxy: &Option<String>,
    key: &Option<String>,
    settings: &Settings,
) -> Result<()> {
    if urls.is_empty() {
        eprintln!("hd add: no URLs given");
        std::process::exit(2);
    }
    let mut q = Queue::load();
    let base = q.next_id();
    for (i, u) in urls.iter().enumerate() {
        let id = base + i as u64;
        let mut info =
            make_info(id, u, output, parts, limit, auth, headers, cookie, proxy, key, settings)?;
        // avoid silently overwriting an existing file (mirrors the original's
        // "name (1)" uniquifier when the target path is taken)
        let mut n = 1;
        let stem = info.file_path.file_stem().map(|s| s.to_os_string());
        let ext = info.file_path.extension().map(|s| s.to_os_string());
        let taken = |p: &std::path::Path| p.exists() || q.items.iter().any(|d| d.file_path == p);
        while taken(&info.file_path) {
            let mut name = stem.clone().unwrap_or_default();
            name.push(format!(" ({n})"));
            if let Some(e) = &ext {
                name.push(".");
                name.push(e.clone());
            }
            info.file_path = info.file_path.with_file_name(name);
            n += 1;
        }
        println!("added #{id}  {u}  -> {}", info.file_path.display());
        q.items.push(info);
    }
    q.save()?;
    Ok(())
}

fn cmd_ls() -> Result<()> {
    let q = Queue::load();
    if q.items.is_empty() {
        println!("(queue empty)");
        return Ok(());
    }
    println!("{:<4} {:<10} {:>9} {:>10}  {:<6}  file", "id", "state", "size", "done", "parts");
    for d in &q.items {
        let size = d.file_size.map(fmt_size).unwrap_or_else(|| "?".into());
        let done = fmt_size(d.downloaded);
        println!(
            "{:<4} {:<10} {:>9} {:>10}  {:<6}  {}",
            d.id,
            format!("{:?}", d.state),
            size,
            done,
            d.parts,
            d.file_path.display()
        );
    }
    Ok(())
}

fn cmd_set_state(id: u64, to: DownloadState, allowed: &[DownloadState]) -> Result<()> {
    let mut q = Queue::load();
    match q.by_id_mut(id) {
        Some(d) => {
            if !allowed.contains(&d.state) {
                eprintln!("#{id} is in state {d:?}; allowed: {allowed:?}");
                std::process::exit(1);
            }
            d.state = to;
            q.save()?;
            println!("#{id} -> {to:?}");
            Ok(())
        }
        None => {
            eprintln!("no task #{id}");
            std::process::exit(1);
        }
    }
}

fn cmd_rm(id: u64) -> Result<()> {
    let mut q = Queue::load();
    let path = q.by_id(id).map(|d| d.file_path.clone());
    if q.remove(id) {
        if let Some(p) = path {
            let _ = std::fs::remove_file(hdmeta_path(&p));
        }
        q.save()?;
        println!("removed #{id}");
        Ok(())
    } else {
        eprintln!("no task #{id}");
        std::process::exit(1);
    }
}

fn cmd_set(settings: &Settings, key: &str, value: &str) -> Result<()> {
    let mut s = settings.clone();
    match key {
        "global-limit" => s.global_speed_limit = parse_limit(value).unwrap_or(0),
        "max-concurrent" => {
            s.max_concurrent_downloads = value.parse().map_err(|_| {
                hd_core::Error::Other(format!("bad value '{value}' (want integer)"))
            })?
        }
        "default-parts" => {
            s.default_parts = value.parse().map_err(|_| {
                hd_core::Error::Other(format!("bad value '{value}' (want integer)"))
            })?
        }
        "download-dir" => s.download_directory = Some(std::path::PathBuf::from(value)),
        other => {
            eprintln!("unknown key '{other}' (want global-limit | max-concurrent | default-parts | download-dir)");
            std::process::exit(2);
        }
    }
    s.save()?;
    println!("saved: {key} = {value}");
    Ok(())
}

// ---------------------------------------------------------------------------
// download execution
// ---------------------------------------------------------------------------

fn build_params(info: &DownloadInfo) -> RunParams {
    RunParams {
        url: info.url.clone(),
        out_path: info.file_path.clone(),
        parts: info.parts,
        task_limit: info.speed_limit,
        auth: info.auth.clone(),
        extra_headers: info.extra_headers.clone(),
        proxy: info.proxy.clone(),
        resume: load_resume(&info.file_path),
        sftp_key: info.sftp_key.clone(),
    }
}

async fn direct_download(settings: &Settings, info: DownloadInfo) -> Result<()> {
    println!("URL   : {}", info.url);
    println!("Output: {}", info.file_path.display());
    println!("Parts : {}", info.parts);
    if let Some(l) = info.speed_limit {
        println!("Limit : {l} B/s");
    }
    if let Some(p) = &info.proxy {
        println!("Proxy : {p}");
    }

    let global = Arc::new(Mutex::new(Throttle::new(settings.global_speed_limit)));
    let engine = Arc::new(Engine::new());

    // Ctrl-C: the engine's cancel path saves .hdmeta; here we hard-exit after
    // letting the engine observe the signal via process exit in direct mode.
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        println!("\ninterrupted (resume with `hd <URL> -o <path>`)");
        std::process::exit(130);
    });

    let (evt_tx, evt_rx) = broadcast::channel(1024);
    let (_cancel_tx, cancel_rx) = mpsc::channel::<()>(1);
    let printer = tokio::spawn(progress_printer(evt_rx, false));
    let params = build_params(&info);
    let res = engine.run(params, global, cancel_rx, evt_tx).await;
    let _ = printer.await;

    match res {
        Ok(()) => {
            append_history(
                info.id,
                &info.url.to_string(),
                &info.file_path.display().to_string(),
                info.downloaded,
                "completed",
                "",
            )?;
            println!("done");
            Ok(())
        }
        Err(e) => {
            append_history(
                info.id,
                &info.url.to_string(),
                &info.file_path.display().to_string(),
                info.downloaded,
                "failed",
                &e.to_string(),
            )?;
            Err(e)
        }
    }
}

async fn cmd_run(settings: &Settings) -> Result<()> {
    let global = Arc::new(Mutex::new(Throttle::new(settings.global_speed_limit)));
    let engine = Arc::new(Engine::new());
    let max = settings.max_concurrent_downloads.max(1) as usize;

    // Ctrl-C cancels the whole process; engine.run already persists .hdmeta
    // through its own cancel path — but here we exit directly, so persist via
    // running tasks being aborted: simplest is to exit(130) like direct mode.
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        println!("\ninterrupted (resume later with `hd run`)");
        std::process::exit(130);
    });

    loop {
        let mut q = Queue::load();
        let runnable: Vec<DownloadInfo> = q
            .items
            .iter()
            .filter(|d| d.state == DownloadState::Queued)
            .cloned()
            .collect();
        if runnable.is_empty() {
            let active = q
                .items
                .iter()
                .filter(|d| d.state == DownloadState::Downloading || d.state == DownloadState::Connecting)
                .count();
            if active == 0 {
                println!("queue drained");
                return Ok(());
            }
        }

        let mut batch = Vec::new();
        for info in runnable.into_iter().take(max) {
            if let Some(d) = q.by_id_mut(info.id) {
                d.state = DownloadState::Connecting;
            }
            batch.push(info);
        }
        q.save()?;

        let mut joins = Vec::new();
        for info in batch {
            println!("start #{} {}", info.id, info.url);
            let global2 = global.clone();
            let engine2 = engine.clone();
            joins.push((
                info.id,
                tokio::spawn(async move {
                    let evt_tx = broadcast::channel(1024).0;
                    let (_cancel_tx, cancel_rx) = mpsc::channel::<()>(1);
                    let params = build_params(&info);
                    let printer = tokio::spawn(progress_printer(evt_tx.subscribe(), true));
                    // track the downloaded total for queue/history bookkeeping
                    let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
                    let tracker = {
                        let rx = evt_tx.subscribe();
                        let c = counter.clone();
                        tokio::spawn(async move {
                            let mut rx = rx;
                            loop {
                                match rx.recv().await {
                                    Ok(Event::Progress(_, t)) => {
                                        c.store(t, std::sync::atomic::Ordering::Relaxed)
                                    }
                                    Ok(Event::Completed) | Ok(Event::Failed(_)) | Err(_) => break,
                                    Ok(_) => {}
                                }
                            }
                        })
                    };
                    let res = engine2.run(params, global2, cancel_rx, evt_tx).await;
                    let _ = printer.await;
                    let _ = tracker.await;
                    let mut info = info;
                    info.downloaded = counter.load(std::sync::atomic::Ordering::Relaxed);
                    if res.is_ok() && info.file_size.is_none() {
                        info.file_size = Some(info.downloaded);
                    }
                    (info, res)
                }),
            ));
        }

        for (id, j) in joins {
            let (info, res) = j.await.map_err(|e| hd_core::Error::Other(format!("join: {e}")))?;
            let mut q = Queue::load();
            match res {
                Ok(()) => {
                    if let Some(d) = q.by_id_mut(id) {
                        d.state = DownloadState::Completed;
                        d.downloaded = info.downloaded;
                        d.file_size = d.file_size.or(info.file_size);
                    }
                    let _ = append_history(id, &info.url.to_string(), &info.file_path.display().to_string(), info.downloaded, "completed", "");
                    println!("#{} completed", id);
                }
                Err(e) => {
                    if let Some(d) = q.by_id_mut(id) {
                        d.state = DownloadState::Failed;
                    }
                    let _ = append_history(id, &info.url.to_string(), &info.file_path.display().to_string(), info.downloaded, "failed", &e.to_string());
                    println!("#{} failed: {e}", id);
                }
            }
            q.save()?;
        }
    }
}

async fn progress_printer(mut rx: broadcast::Receiver<Event>, multi: bool) {
    let mut last_total = 0u64;
    let mut last_tick = Instant::now();
    let mut speed = 0u64;
    loop {
        match rx.recv().await {
            Ok(Event::Progress(delta, total)) => {
                last_total = total;
                let el = last_tick.elapsed().as_secs_f64();
                if el >= 1.0 {
                    speed = (delta as f64 / el) as u64;
                    last_tick = Instant::now();
                } else {
                    speed = ((speed as f64) + delta as f64 / el.max(0.05)) as u64 / 2;
                }
            }
            Ok(Event::StateChanged(s)) => {
                if s != DownloadState::Downloading {
                    println!("[state] {s:?}");
                }
            }
            Ok(Event::Failed(m)) => eprintln!("failed: {m}"),
            Ok(Event::Completed) | Err(_) => break,
        }
        if multi {
            use std::io::Write;
            print!("\r\x1b[2K{} @ {}/s", fmt_size(last_total), fmt_size(speed));
            let _ = std::io::stdout().flush();
        } else {
            eprint!("\r{} @ {}/s   ", fmt_size(last_total), fmt_size(speed));
        }
    }
}

fn fmt_size(n: u64) -> String {
    const U: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n}B")
    } else {
        format!("{v:.1}{}", U[i])
    }
}
