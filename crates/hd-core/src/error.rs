//! Unified error type.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid url: {0}")]
    InvalidUrl(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("http status {status} on {url}")]
    HttpStatus { status: u16, url: String },

    #[error("too many redirects (last: {url})")]
    TooManyRedirects { url: String },

    #[error("malformed http response: {0}")]
    MalformedResponse(String),

    #[error("server does not support range requests; cannot resume/partition (http {0})")]
    RangeNotSupported(u16),

    #[error("auth not supported or missing credentials: {0}")]
    Auth(String),

    #[error("tls error: {0}")]
    Tls(#[from] rustls::Error),

    #[error("ssh error: {0}")]
    Ssh(#[from] russh::Error),

    #[error("sftp error: {0}")]
    Sftp(#[from] russh_sftp::client::error::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("download canceled")]
    Canceled,

    #[error("{0}")]
    Other(String),
}
