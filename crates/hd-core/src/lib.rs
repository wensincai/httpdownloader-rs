//! hd-core: platform-agnostic download engine core.
//!
//! Rust rewrite of erickutcher/httpdownloader (GPLv3).
//! See `output/docs/Rust改写_架构设计.md` for the mapping to the original C code.

pub mod auth;
pub mod error;
pub mod engine;
pub mod file_writer;
pub mod ftp;
pub mod http;
pub mod model;
pub mod proxy;
pub mod queue;
pub mod range;
pub mod settings;
pub mod sftp;
pub mod throttle;
pub mod url;

pub use error::{Error, Result};
pub use model::{DownloadInfo, DownloadState};
pub use url::Url;
