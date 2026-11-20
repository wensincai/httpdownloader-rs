//! Download task model — mirrors `DOWNLOAD_INFO` (connection.h) and the status codes.

use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::range::RangeLedger;
use crate::url::Url;

/// Credentials for HTTP auth / FTP login / SFTP. (Originally UI-supplied;
/// serialized with the task so queue persistence round-trips them.)
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AuthCreds {
    pub user: String,
    pub pass: String,
}

impl AuthCreds {
    pub fn new(user: &str, pass: &str) -> Self {
        AuthCreds { user: user.into(), pass: pass.into() }
    }
}

/// Mirrors the original STATUS_* values (globals.h): the visible subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DownloadState {
    Queued,
    Connecting,
    Downloading,
    Pausing,
    Paused,
    Stopping,
    Stopped,
    Completed,
    Failed,
    Skipped,
}

/// One download task. Corresponds to DOWNLOAD_INFO minus UI-only fields (icon, treenode).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadInfo {
    pub id: u64,
    pub original_url: String,
    /// current URL (after redirects)
    pub url: Url,
    pub file_path: PathBuf,
    /// `None` when server sent no Content-Length (chunked)
    pub file_size: Option<u64>,
    pub downloaded: u64,
    /// requested number of parts (original: di->parts)
    pub parts: u16,
    /// currently active parts (original: di->active_parts)
    pub active_parts: u16,
    pub state: DownloadState,
    /// bytes/second, aggregated by the engine once per second
    pub speed: u64,
    pub retry_count: u8,
    /// per-task limit in bytes/s (original: download_speed_limit)
    pub speed_limit: Option<u64>,
    /// HTTP auth / FTP / SFTP credentials
    #[serde(default)]
    pub auth: Option<AuthCreds>,
    /// static extra request headers (HTTP only)
    #[serde(default)]
    pub extra_headers: Vec<(String, String)>,
    /// proxy configuration (HTTP(S) only)
    #[serde(default)]
    pub proxy: Option<crate::proxy::ProxyConfig>,
    /// private key file for sftp public-key auth
    #[serde(default)]
    pub sftp_key: Option<PathBuf>,
    pub created_at: SystemTime,
}

impl DownloadInfo {
    pub fn new(id: u64, url: Url, file_path: PathBuf, parts: u16) -> Self {
        DownloadInfo {
            id,
            original_url: url.to_string(),
            url,
            file_path,
            file_size: None,
            downloaded: 0,
            parts: parts.max(1),
            active_parts: 0,
            state: DownloadState::Queued,
            speed: 0,
            retry_count: 0,
            speed_limit: None,
            auth: None,
            extra_headers: Vec::new(),
            proxy: None,
            sftp_key: None,
            created_at: SystemTime::now(),
        }
    }
}

/// Persisted on pause/stop so a process restart can resume (original relies on
/// in-memory DOWNLOAD_INFO + probing the server again; we persist the ledger too).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeState {
    pub url: String,
    pub file_path: PathBuf,
    pub ledger: RangeLedger,
    pub downloaded: u64,
    pub file_size: Option<u64>,
}
