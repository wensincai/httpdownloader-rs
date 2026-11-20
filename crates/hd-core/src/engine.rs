//! Download engine — mirrors `StartDownload`/`IOCPDownloader` supervision loop
//! (connection.cpp) for HTTP(S), FTP(S) and SFTP.
//!
//! - HTTP(S): shared RangeLedger with dynamic reassignment (mirrors
//!   UpdateRangeList), Cookie jar, Basic/Digest challenge handling, proxy
//!   (CONNECT/SOCKS5), .hdmeta resume persistence.
//! - FTP(S): PASV/EPSV + REST; each part opens its own control+data pair.
//! - SFTP: single stream over russh (TOFU host keys).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc};

use crate::auth::{basic_authorization, digest_authorization, make_cnonce, parse_challenge, Challenge};
use crate::error::{Error, Result};
use crate::file_writer::FileWriter;
use crate::ftp::FtpClient;
use crate::http::client::request_stream;
use crate::http::cookie::CookieJar;
use crate::model::{DownloadState, ResumeState};
use crate::proxy::ProxyConfig;
use crate::range::{PartRange, RangeLedger};
use crate::throttle::Throttle;
use crate::url::{Scheme, Url};

#[derive(Debug, Clone)]
pub enum Event {
    StateChanged(DownloadState),
    /// (delta bytes, downloaded_total)
    Progress(u64, u64),
    Completed,
    Failed(String),
}

pub use crate::model::AuthCreds;

/// One download invocation. Groups what used to be 9 positional args.
pub struct RunParams {
    pub url: Url,
    pub out_path: PathBuf,
    pub parts: u16,
    /// per-task speed limit in bytes/s
    pub task_limit: Option<u64>,
    pub auth: Option<AuthCreds>,
    pub extra_headers: Vec<(String, String)>,
    pub proxy: Option<ProxyConfig>,
    /// resume from a previously saved ledger
    pub resume: Option<ResumeState>,
    /// private key file for sftp public-key auth
    pub sftp_key: Option<PathBuf>,
}

const MAX_PART_RETRIES: u8 = 4; // mirrors the original retries cap per context

pub struct Engine {
    next_id: AtomicU64,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// .hdmeta persistence (mirrors the original saving DOWNLOAD_INFO on pause/stop)
// ---------------------------------------------------------------------------

pub fn hdmeta_path(out: &Path) -> PathBuf {
    let mut name = out.file_name().map(|s| s.to_os_string()).unwrap_or_default();
    name.push(".hdmeta");
    out.with_file_name(name)
}

pub fn save_resume(out: &Path, st: &ResumeState) -> Result<()> {
    let json = serde_json::to_string_pretty(st)?;
    std::fs::write(hdmeta_path(out), json)?;
    Ok(())
}

pub fn load_resume(out: &Path) -> Option<ResumeState> {
    let p = hdmeta_path(out);
    let s = std::fs::read_to_string(p).ok()?;
    serde_json::from_str(&s).ok()
}

pub fn clear_resume(out: &Path) {
    let _ = std::fs::remove_file(hdmeta_path(out));
}

impl Engine {
    pub fn new() -> Self {
        Engine { next_id: AtomicU64::new(1) }
    }

    pub fn alloc_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Run one download to completion.
    pub async fn run(
        &self,
        p: RunParams,
        global_limit: Arc<Mutex<Throttle>>,
        cancel_rx: mpsc::Receiver<()>,
        events: broadcast::Sender<Event>,
    ) -> Result<()> {
        let out_path = p.out_path.clone();
        let res = match p.url.scheme {
            Scheme::Http | Scheme::Https => {
                self.run_http(p, global_limit, cancel_rx, events).await
            }
            Scheme::Ftp | Scheme::Ftps => {
                self.run_ftp(p, global_limit, cancel_rx, events).await
            }
            Scheme::Sftp => {
                self.run_sftp(p, global_limit, cancel_rx, events).await
            }
        };
        match &res {
            Ok(()) => clear_resume(&out_path),
            // canceled / failed → persist the ledger for resume
            Err(_) => {
                // per-scheme paths already saved where a ledger exists
            }
        }
        res
    }
}

/// Shared write sink factory — throttles, writes at the absolute offset,
/// updates counters and emits progress. Used by every scheme's part workers.
#[allow(clippy::too_many_arguments)]
fn make_sink(
    start: u64,
    fh: Arc<Mutex<std::fs::File>>,
    written: Arc<AtomicU64>,
    seek_err: Arc<Mutex<Option<std::io::Error>>>,
    global_limit: Arc<Mutex<Throttle>>,
    task_bucket: Option<Arc<Mutex<Throttle>>>,
    total: Arc<AtomicU64>,
    events: broadcast::Sender<Event>,
) -> impl FnMut(u64, &[u8]) + Send {
    move |_off: u64, data: &[u8]| {
        let n = data.len() as u64;
        let wait = global_limit.lock().unwrap().acquire(n);
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
        if let Some(bucket) = &task_bucket {
            let wait = bucket.lock().unwrap().acquire(n);
            if !wait.is_zero() {
                std::thread::sleep(wait);
            }
        }
        let cur = start + written.load(Ordering::Relaxed);
        let res = {
            let mut f = fh.lock().unwrap();
            use std::io::{Seek, Write};
            f.seek(std::io::SeekFrom::Start(cur)).and_then(|_| f.write_all(data))
        };
        if let Err(e) = res {
            *seek_err.lock().unwrap() = Some(e);
            return;
        }
        written.fetch_add(n, Ordering::Relaxed);
        let t = total.fetch_add(n, Ordering::Relaxed) + n;
        events.send(Event::Progress(n, t)).ok();
    }
}

/// Build an Authorization header for a 401 challenge.
fn build_auth_header(
    challenge_value: &str,
    auth: &Option<AuthCreds>,
    url: &Url,
    method: &str,
    nc: u32,
) -> Result<Option<String>> {
    let creds = match auth {
        Some(c) => c,
        None => return Ok(None),
    };
    match parse_challenge(challenge_value) {
        Some(Challenge::Basic) => Ok(Some(basic_authorization(&creds.user, &creds.pass))),
        Some(Challenge::Digest(p)) => {
            let resource = url.resource.split('?').next().unwrap_or("/").to_string();
            Ok(Some(digest_authorization(
                &p,
                &creds.user,
                &creds.pass,
                method,
                &resource,
                nc,
                &make_cnonce(nc),
            )))
        }
        None => Ok(None),
    }
}

impl Engine {
    /// HTTP(S) path: HEAD probe (auth retry, HEAD-failure degradation), shared
    /// ledger, per-part workers with cookie/auth/proxy support.
    async fn run_http(
        &self,
        p: RunParams,
        global_limit: Arc<Mutex<Throttle>>,
        mut cancel_rx: mpsc::Receiver<()>,
        events: broadcast::Sender<Event>,
    ) -> Result<()> {
        let RunParams { url, out_path, parts, task_limit, auth, extra_headers, proxy, resume, sftp_key: _ } = p;
        let mut seed_cookies: Vec<(String, String)> = Vec::new();
        let mut static_headers: Vec<(String, String)> = Vec::new();
        for (k, v) in extra_headers {
            if k.eq_ignore_ascii_case("Cookie") {
                seed_cookies.push((k, v));
            } else {
                static_headers.push((k, v));
            }
        }
        let jar = Arc::new(Mutex::new(CookieJar::new()));
        {
            let mut j = jar.lock().unwrap();
            for (_, v) in &seed_cookies {
                j.seed_header(&url, v);
            }
        }

        // 1. probe: size + range support (skipped on resume — the saved ledger
        // already encodes size/ranges).
        let (size, ledger) = if let Some(res) = &resume {
            events.send(Event::StateChanged(DownloadState::Downloading)).ok();
            (res.file_size, res.ledger.clone())
        } else {
            let mut probe_extra = static_headers.clone();
            if let Some(c) = jar.lock().unwrap().header_for(&url) {
                probe_extra.push(("Cookie".into(), c));
            }
            let mut extra_base = static_headers.clone();
            let probe_meta = match crate::http::client::probe(&url, &probe_extra, proxy.as_ref()).await {
                Ok(m) => Some(m),
                Err(e) => {
                    tracing::warn!(error = %e, "HEAD probe failed, degrading to single-part GET");
                    None
                }
            };
            let (headers, _degraded) = match &probe_meta {
                Some(m) if m.status == 401 => {
                    let value = crate::http::header::get_header(&m.headers, "WWW-Authenticate")
                        .unwrap_or("")
                        .to_string();
                    match build_auth_header(&value, &auth, &url, "GET", 1)? {
                        Some(h) => extra_base.push(("Authorization".into(), h)),
                        None => return Err(Error::Auth(value)),
                    }
                    (m.headers.clone(), false)
                }
                Some(m) if m.status >= 400 => {
                    tracing::warn!(status = m.status, "HEAD probe rejected, degrading to single-part GET");
                    (Vec::new(), true)
                }
                Some(m) => (m.headers.clone(), false),
                None => (Vec::new(), true),
            };
            let size = crate::http::header::content_length(&headers);
            let accepts = crate::http::header::accept_ranges(&headers);
            let use_parts = if accepts && size.map(|s| s > 0).unwrap_or(false) {
                parts.max(1)
            } else {
                1
            };
            events.send(Event::StateChanged(DownloadState::Connecting)).ok();
            let ledger = RangeLedger::split_evenly(size.unwrap_or(0), use_parts);
            (size, ledger)
        };

        // 2. pre-allocate (resume opens without truncate)
        let mut writer = FileWriter::open(&out_path, size).await?;

        // 3. shared partition ledger + part workers
        let ledger = Arc::new(Mutex::new(ledger));
        let total = Arc::new(AtomicU64::new(resume.as_ref().map(|r| r.downloaded).unwrap_or(0)));
        let task_bucket = task_limit.map(|r| Arc::new(Mutex::new(Throttle::new(r))));
        let auth = Arc::new(auth);
        let nc = Arc::new(AtomicU64::new(2)); // probe used 1
        let proxy = proxy.clone();
        let mut handles = Vec::new();
        let n_parts = ledger.lock().unwrap().parts.len();
        for idx in 0..n_parts {
            let handle = tokio::spawn(run_http_part(
                idx,
                url.clone(),
                out_path.clone(),
                ledger.clone(),
                global_limit.clone(),
                task_bucket.clone(),
                jar.clone(),
                auth.clone(),
                static_headers.clone(),
                proxy.clone(),
                nc.clone(),
                total.clone(),
                events.clone(),
            ));
            handles.push(handle);
        }

        events.send(Event::StateChanged(DownloadState::Downloading)).ok();

        // 4. join parts; cancel or fatal error stops the rest + persists ledger
        let mut first_err: Option<Error> = None;
        while let Some(h) = handles.pop() {
            
            tokio::select! {
                r = h => {
                    if let Ok(Err(e)) = r {
                        if first_err.is_none() {
                            first_err = Some(e);
                        }
                    }
                }
                _ = cancel_rx.recv() => {
                    for h in handles { h.abort(); }
                    let _ = writer.sync().await;
                    save_resume(&out_path, &ResumeState {
                        url: url.to_string(),
                        file_path: out_path.clone(),
                        ledger: ledger.lock().unwrap().clone(),
                        downloaded: total.load(Ordering::Relaxed),
                        file_size: size,
                    })?;
                    events.send(Event::StateChanged(DownloadState::Stopped)).ok();
                    return Err(Error::Canceled);
                }
            }
        }

        if let Some(e) = first_err {
            let _ = writer.sync().await;
            save_resume(&out_path, &ResumeState {
                url: url.to_string(),
                file_path: out_path.clone(),
                ledger: ledger.lock().unwrap().clone(),
                downloaded: total.load(Ordering::Relaxed),
                file_size: size,
            })?;
            events.send(Event::StateChanged(DownloadState::Failed)).ok();
            events.send(Event::Failed(e.to_string())).ok();
            return Err(e);
        }

        writer.sync().await?;
        events.send(Event::StateChanged(DownloadState::Completed)).ok();
        events.send(Event::Completed).ok();
        Ok(())
    }

    /// FTP(S) path: SIZE/REST probe, multi-part via per-part control+data
    /// connections positioned with REST.
    async fn run_ftp(
        &self,
        p: RunParams,
        global_limit: Arc<Mutex<Throttle>>,
        mut cancel_rx: mpsc::Receiver<()>,
        events: broadcast::Sender<Event>,
    ) -> Result<()> {
        let RunParams { url, out_path, parts, task_limit, auth, extra_headers: _, proxy: _, resume, sftp_key: _ } = p;
        let (user, pass) = ftp_credentials(&url, auth.as_ref());
        let use_explicit_tls = url.scheme == Scheme::Ftps;

        // probe: size + REST support
        let (size, rest_ok) = if let Some(res) = &resume {
            events.send(Event::StateChanged(DownloadState::Downloading)).ok();
            (res.file_size, true)
        } else {
            events.send(Event::StateChanged(DownloadState::Connecting)).ok();
            let mut c = FtpClient::connect(&url).await?;
            if use_explicit_tls {
                c.upgrade_tls(&url.host).await?;
            }
            c.login(&user, &pass).await?;
            c.binary_mode().await?;
            let size = c.size(&url.resource).await?;
            let rest_ok = c.rest_supported().await?;
            c.quit().await;
            (size, rest_ok)
        };

        let use_parts = if rest_ok && size.map(|s| s > 0).unwrap_or(false) {
            parts.max(1)
        } else {
            1
        };
        let ledger = if let Some(res) = &resume {
            res.ledger.clone()
        } else {
            RangeLedger::split_evenly(size.unwrap_or(0), use_parts)
        };

        // pre-allocate
        let mut writer = FileWriter::open(&out_path, size).await?;

        let ledger = Arc::new(Mutex::new(ledger));
        let total = Arc::new(AtomicU64::new(resume.as_ref().map(|r| r.downloaded).unwrap_or(0)));
        let task_bucket = task_limit.map(|r| Arc::new(Mutex::new(Throttle::new(r))));
        let auth = Arc::new(auth.clone());
        let mut handles = Vec::new();
        let n_parts = ledger.lock().unwrap().parts.len();
        for idx in 0..n_parts {
            let handle = tokio::spawn(run_ftp_part(
                idx,
                url.clone(),
                out_path.clone(),
                ledger.clone(),
                global_limit.clone(),
                task_bucket.clone(),
                auth.clone(),
                use_explicit_tls,
                total.clone(),
                events.clone(),
            ));
            handles.push(handle);
        }

        events.send(Event::StateChanged(DownloadState::Downloading)).ok();

        let mut first_err: Option<Error> = None;
        while let Some(h) = handles.pop() {
            
            tokio::select! {
                r = h => {
                    if let Ok(Err(e)) = r {
                        if first_err.is_none() {
                            first_err = Some(e);
                        }
                    }
                }
                _ = cancel_rx.recv() => {
                    for h in handles { h.abort(); }
                    let _ = writer.sync().await;
                    save_resume(&out_path, &ResumeState {
                        url: url.to_string(),
                        file_path: out_path.clone(),
                        ledger: ledger.lock().unwrap().clone(),
                        downloaded: total.load(Ordering::Relaxed),
                        file_size: size,
                    })?;
                    events.send(Event::StateChanged(DownloadState::Stopped)).ok();
                    return Err(Error::Canceled);
                }
            }
        }

        if let Some(e) = first_err {
            let _ = writer.sync().await;
            save_resume(&out_path, &ResumeState {
                url: url.to_string(),
                file_path: out_path.clone(),
                ledger: ledger.lock().unwrap().clone(),
                downloaded: total.load(Ordering::Relaxed),
                file_size: size,
            })?;
            events.send(Event::StateChanged(DownloadState::Failed)).ok();
            events.send(Event::Failed(e.to_string())).ok();
            return Err(e);
        }

        writer.sync().await?;
        events.send(Event::StateChanged(DownloadState::Completed)).ok();
        events.send(Event::Completed).ok();
        Ok(())
    }

    /// SFTP path: single stream over one SSH session (v3 protocol has no
    /// native multi-range; parallelism via multiple sessions is deferred).
    async fn run_sftp(
        &self,
        p: RunParams,
        global_limit: Arc<Mutex<Throttle>>,
        mut cancel_rx: mpsc::Receiver<()>,
        events: broadcast::Sender<Event>,
    ) -> Result<()> {
        let RunParams { url, out_path, parts: _, task_limit, auth, extra_headers: _, proxy: _, resume, sftp_key } = p;
        events.send(Event::StateChanged(DownloadState::Connecting)).ok();

        let start_off = resume.as_ref().map(|r| r.downloaded).unwrap_or(0);
        let ledger = Arc::new(Mutex::new(if let Some(res) = &resume {
            res.ledger.clone()
        } else {
            RangeLedger::split_evenly(0, 1) // open-ended until stat returns
        }));
        let size = resume.as_ref().and_then(|r| r.file_size);

        let mut writer = FileWriter::open(&out_path, size).await?;

        let user = url
            .username
            .clone()
            .or_else(|| auth.as_ref().map(|a| a.user.clone()))
            .ok_or_else(|| Error::Auth("sftp requires a username".into()))?;
        let pass = url
            .password
            .clone()
            .or_else(|| auth.as_ref().map(|a| a.pass.clone()));

        let total = Arc::new(AtomicU64::new(start_off));
        let task_bucket = task_limit.map(|r| Arc::new(Mutex::new(Throttle::new(r))));

        let worker = {
            let url = url.clone();
            let path = out_path.clone();
            let ledger = ledger.clone();
            let global_limit = global_limit.clone();
            let total = total.clone();
            let events = events.clone();
            let key = sftp_key.clone();
            async move {
                let mut session =
                    crate::sftp::connect_session(&url, &user, pass.as_deref(), key.as_deref(), None)
                        .await?;
                let (sftp, remote_size) = crate::sftp::open_remote(&mut session, &url.resource).await?;

                let (start, end) = {
                    let mut l = ledger.lock().unwrap();
                    // once the remote size is known, the open-ended part gets bounded
                    if let Some(sz) = remote_size {
                        if let Some(Some(pr)) = l.parts.first_mut() {
                            if pr.end == u64::MAX {
                                pr.end = sz;
                            }
                        }
                    }
                    let pr = l.parts
                        .first()
                        .and_then(|p| p.as_ref())
                        .ok_or_else(|| Error::Other("no sftp part".into()))?;
                    (pr.next_offset, pr.end)
                };

                let fh = Arc::new(Mutex::new(std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(&path)?));
                let written = Arc::new(AtomicU64::new(0));
                let seek_err: Arc<Mutex<Option<std::io::Error>>> = Arc::new(Mutex::new(None));
                let mut sink = make_sink(
                    start,
                    fh,
                    written.clone(),
                    seek_err.clone(),
                    global_limit,
                    task_bucket,
                    total.clone(),
                    events,
                );
                crate::sftp::download_range(&sftp, &url.resource, start, end, &mut sink).await?;
                if let Some(e) = seek_err.lock().unwrap().take() {
                    return Err(Error::Io(e));
                }
                let n = written.load(Ordering::Relaxed);
                {
                    let mut l = ledger.lock().unwrap();
                    if let Some(pr) = l.parts[0].as_mut() {
                        pr.next_offset = start + n;
                        if pr.end != u64::MAX && pr.next_offset >= pr.end {
                            l.parts[0] = None;
                        }
                    }
                }
                Ok(())
            }
        };

        events.send(Event::StateChanged(DownloadState::Downloading)).ok();

        let res = tokio::select! {
            r = worker => r,
            _ = cancel_rx.recv() => {
                let _ = writer.sync().await;
                save_resume(&out_path, &ResumeState {
                    url: url.to_string(),
                    file_path: out_path.clone(),
                    ledger: ledger.lock().unwrap().clone(),
                    downloaded: total.load(Ordering::Relaxed),
                    file_size: size,
                })?;
                events.send(Event::StateChanged(DownloadState::Stopped)).ok();
                return Err(Error::Canceled);
            }
        };

        if let Err(e) = res {
            let _ = writer.sync().await;
            save_resume(&out_path, &ResumeState {
                url: url.to_string(),
                file_path: out_path.clone(),
                ledger: ledger.lock().unwrap().clone(),
                downloaded: total.load(Ordering::Relaxed),
                file_size: size,
            })?;
            events.send(Event::StateChanged(DownloadState::Failed)).ok();
            events.send(Event::Failed(e.to_string())).ok();
            return Err(e);
        }

        writer.sync().await?;
        events.send(Event::StateChanged(DownloadState::Completed)).ok();
        events.send(Event::Completed).ok();
        Ok(())
    }
}

/// FTP credentials: URL userinfo > -a creds > anonymous.
fn ftp_credentials(url: &Url, auth: Option<&AuthCreds>) -> (String, String) {
    if let Some(u) = &url.username {
        return (u.clone(), url.password.clone().unwrap_or_default());
    }
    if let Some(a) = auth {
        return (a.user.clone(), a.pass.clone());
    }
    ("anonymous".into(), "anonymous@".into()) // mirrors the original default
}

/// Run one HTTP part: own slot in the shared ledger; claim unassigned work when
/// idle; resume from `next_offset` after mid-stream failure (mirrors range re-push).
#[allow(clippy::too_many_arguments)]
async fn run_http_part(
    idx: usize,
    url: Url,
    path: PathBuf,
    ledger: Arc<Mutex<RangeLedger>>,
    global_limit: Arc<Mutex<Throttle>>,
    task_bucket: Option<Arc<Mutex<Throttle>>>,
    jar: Arc<Mutex<CookieJar>>,
    auth: Arc<Option<AuthCreds>>,
    extra_base: Vec<(String, String)>,
    proxy: Option<ProxyConfig>,
    nc: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
    events: broadcast::Sender<Event>,
) -> Result<()> {
    let mut retries: u8 = 0;
    // Authorization header learned from a 401 challenge (retried automatically)
    let mut auth_hdr: Option<String> = None;

    loop {
        // ensure this slot has an interval (dynamic reassignment)
        {
            let mut l = ledger.lock().unwrap();
            if !l.claim_into(idx) {
                return Ok(()); // nothing left to claim — part finished
            }
        }

        let (start, end) = {
            let l = ledger.lock().unwrap();
            let p = l.parts[idx].as_ref().unwrap();
            (p.next_offset, p.end)
        };

        let fh = Arc::new(Mutex::new(
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .map_err(Error::Io)?,
        ));
        let written = Arc::new(AtomicU64::new(0));
        let seek_err: Arc<Mutex<Option<std::io::Error>>> = Arc::new(Mutex::new(None));
        let mut sink = make_sink(
            start,
            fh,
            written.clone(),
            seek_err.clone(),
            global_limit.clone(),
            task_bucket.clone(),
            total.clone(),
            events.clone(),
        );

        // request headers: learned auth + cookies + static extras
        let mut extra = extra_base.clone();
        if let Some(h) = &auth_hdr {
            extra.push(("Authorization".into(), h.clone()));
        }
        let cookie_hdr = {
            let j = jar.lock().unwrap();
            j.header_for(&url)
        };
        if let Some(c) = cookie_hdr {
            extra.push(("Cookie".into(), c));
        }

        // HTTP Range: end exclusive internally, inclusive on the wire
        let range = if end == u64::MAX { Some((start, u64::MAX)) } else { Some((start, end)) };

        let res =
            request_stream(&url, "GET", None, &extra, range, proxy.as_ref(), &mut sink).await;

        if let Some(e) = seek_err.lock().unwrap().take() {
            return Err(Error::Io(e));
        }

        let n = written.load(Ordering::Relaxed);

        // absorb Set-Cookie from the response
        if let Ok(meta) = &res {
            let mut j = jar.lock().unwrap();
            j.store_from_response(&url, &meta.headers);
        }

        // 401 challenge: build Authorization (Basic/Digest), rewind ledger, retry
        if let Ok(meta) = &res {
            if meta.status == 401 {
                if auth_hdr.is_some() {
                    return Err(Error::Auth("credentials rejected (401 after auth)".into()));
                }
                let value = crate::http::header::get_header(&meta.headers, "WWW-Authenticate")
                    .unwrap_or("")
                    .to_string();
                let call_nc = nc.fetch_add(1, Ordering::Relaxed) as u32;
                match build_auth_header(&value, &auth, &url, "GET", call_nc)? {
                    Some(h) => {
                        auth_hdr = Some(h);
                        // restore the full interval for the retry
                        let mut l = ledger.lock().unwrap();
                        l.parts[idx] = Some(PartRange { next_offset: start, end });
                        continue;
                    }
                    None => return Err(Error::Auth(value)),
                }
            }
        }

        // advance ledger; open-ended parts (unknown size) finish when the
        // response stream reached EOF cleanly (request_stream reads to EOF)
        {
            let mut l = ledger.lock().unwrap();
            if let Some(p) = l.parts[idx].as_mut() {
                p.next_offset = start + n;
                let complete = if p.end == u64::MAX {
                    res.is_ok()
                } else {
                    p.next_offset >= p.end
                };
                if complete {
                    l.parts[idx] = None;
                }
            }
        }

        match res {
            Ok(meta) if (200..300).contains(&meta.status) || meta.status == 206 => {
                if n == 0 {
                    retries += 1;
                    if retries > MAX_PART_RETRIES {
                        return Err(Error::Other(format!(
                            "part exhausted retries at offset {start}"
                        )));
                    }
                    crate::http::client::backoff(retries).await;
                } else {
                    retries = 0; // progress was made
                }
            }
            Ok(meta) => {
                return Err(Error::HttpStatus { status: meta.status, url: url.to_string() });
            }
            Err(e) => {
                retries += 1;
                if retries > MAX_PART_RETRIES {
                    return Err(e);
                }
                tracing::warn!(url = %url, offset = start + n, "part interrupted, retrying");
                crate::http::client::backoff(retries).await;
            }
        }
    }
}

/// Run one FTP part: fresh control+data pair per attempt, positioned with REST.
#[allow(clippy::too_many_arguments)]
async fn run_ftp_part(
    idx: usize,
    url: Url,
    path: PathBuf,
    ledger: Arc<Mutex<RangeLedger>>,
    global_limit: Arc<Mutex<Throttle>>,
    task_bucket: Option<Arc<Mutex<Throttle>>>,
    auth: Arc<Option<AuthCreds>>,
    use_explicit_tls: bool,
    total: Arc<AtomicU64>,
    events: broadcast::Sender<Event>,
) -> Result<()> {
    let (user, pass) = ftp_credentials(&url, (*auth).as_ref());
    let mut retries: u8 = 0;

    loop {
        let (start, end) = {
            let mut l = ledger.lock().unwrap();
            if !l.claim_into(idx) {
                return Ok(());
            }
            let p = l.parts[idx].as_ref().unwrap();
            (p.next_offset, p.end)
        };

        let fh = Arc::new(Mutex::new(
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .map_err(Error::Io)?,
        ));
        let written = Arc::new(AtomicU64::new(0));
        let seek_err: Arc<Mutex<Option<std::io::Error>>> = Arc::new(Mutex::new(None));
        let mut sink = make_sink(
            start,
            fh,
            written.clone(),
            seek_err.clone(),
            global_limit.clone(),
            task_bucket.clone(),
            total.clone(),
            events.clone(),
        );

        let res = ftp_retr_range(&url, &user, &pass, use_explicit_tls, start, end, &mut sink).await;

        if let Some(e) = seek_err.lock().unwrap().take() {
            return Err(Error::Io(e));
        }

        let n = written.load(Ordering::Relaxed);

        {
            let mut l = ledger.lock().unwrap();
            if let Some(p) = l.parts[idx].as_mut() {
                p.next_offset = start + n;
                let complete = p.next_offset >= p.end;
                if complete {
                    l.parts[idx] = None;
                }
            }
        }

        match res {
            Ok(()) => {
                if n == 0 && end != u64::MAX {
                    retries += 1;
                    if retries > MAX_PART_RETRIES {
                        return Err(Error::Other(format!(
                            "ftp part exhausted retries at offset {start}"
                        )));
                    }
                    crate::http::client::backoff(retries).await;
                } else {
                    retries = 0;
                }
            }
            Err(e) => {
                retries += 1;
                if retries > MAX_PART_RETRIES {
                    return Err(e);
                }
                tracing::warn!(url = %url, offset = start + n, "ftp part interrupted, retrying");
                crate::http::client::backoff(retries).await;
            }
        }
    }
}

/// One FTP transfer attempt: connect → login → TYPE I → REST/RETR → stream.
async fn ftp_retr_range<S>(
    url: &Url,
    user: &str,
    pass: &str,
    use_explicit_tls: bool,
    start: u64,
    end: u64,
    sink: &mut S,
) -> Result<()>
where
    S: FnMut(u64, &[u8]),
{
    let mut c = FtpClient::connect(url).await?;
    let outcome: Result<()> = async {
        if use_explicit_tls {
            c.upgrade_tls(&url.host).await?;
        }
        c.login(user, pass).await?;
        c.binary_mode().await?;

        let mut data = c.retr_at(&url.resource, start, &url.host).await?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut offset = start;
        loop {
            let n = data.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            sink(offset, &buf[..n]);
            offset += n as u64;
            if end != u64::MAX && offset >= end {
                // stop early — close data channel, expect 426 then 226 or ignore
                let _ = data.shutdown().await;
                break;
            }
        }
        // after closing early the server reports 426; a full transfer reports 226.
        if let Ok(r) = c.wait_reply().await {
            if r.kind() != 2 && !(r.code == 426) {
                tracing::debug!(code = r.code, text = %r.text, "ftp post-transfer reply");
            }
        }
        Ok(())
    }
    .await;
    c.quit().await;
    outcome
}
