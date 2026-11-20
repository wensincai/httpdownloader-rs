//! Pre-allocated file writer — mirrors `SetFileValidData` + per-part writes.
//!
//! The original preallocates the file size then guards concurrent writes with
//! LockFileEx. In Rust, part intervals are mutually exclusive by the RangeLedger,
//! so plain `write_at` (seek + write) is race-free under our design.

use std::io::SeekFrom;
use std::path::Path;

use tokio::fs::File;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

pub struct FileWriter {
    file: File,
}

impl FileWriter {
    /// Create (or truncate to) a file of `size` bytes. `size == None` means
    /// unknown length: the file grows as we write.
    pub async fn create(path: &Path, size: Option<u64>) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }
        let file = File::create(path).await?;
        if let Some(sz) = size {
            file.set_len(sz).await?;
        }
        Ok(FileWriter { file })
    }

    /// Open for resume: never truncates; only grows the file to `size`.
    /// (A resume run must not destroy already-written bytes.)
    pub async fn open(path: &Path, size: Option<u64>) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .await?;
        if let Some(sz) = size {
            file.set_len(sz).await?;
        }
        Ok(FileWriter { file })
    }

    pub async fn write_at(&mut self, offset: u64, buf: &[u8]) -> std::io::Result<()> {
        self.file.seek(SeekFrom::Start(offset)).await?;
        self.file.write_all(buf).await?;
        Ok(())
    }

    /// Flush to disk (original calls FlushFileBuffers on completion).
    pub async fn sync(&mut self) -> std::io::Result<()> {
        self.file.flush().await?;
        self.file.sync_all().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_and_verify() {
        let dir = std::env::temp_dir().join("hd_core_test");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let p = dir.join("seg_test.bin");
        let mut w = FileWriter::create(&p, Some(10)).await.unwrap();
        w.write_at(4, b"hello").await.unwrap();
        w.sync().await.unwrap();
        let data = tokio::fs::read(&p).await.unwrap();
        assert_eq!(data.len(), 10);
        assert_eq!(&data[4..9], b"hello");
        let _ = tokio::fs::remove_file(&p).await;
    }
}
