//! Download queue + history persistence — M3 CLI/GUI shared storage
//! (mirrors the original's DOWNLOAD_INFO list backed by the registry/UI;
//! we keep queue.json + history.csv under the config dir).

use std::path::PathBuf;

use crate::error::Result;
use crate::model::DownloadInfo;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Queue {
    pub items: Vec<DownloadInfo>,
}

impl Queue {
    pub fn path() -> PathBuf {
        crate::settings::Settings::config_dir().join("queue.json")
    }

    pub fn load() -> Self {
        match std::fs::read_to_string(Self::path()) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Queue::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        let dir = crate::settings::Settings::config_dir();
        std::fs::create_dir_all(&dir)?;
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(Self::path(), json)?;
        Ok(())
    }

    pub fn next_id(&self) -> u64 {
        self.items.iter().map(|d| d.id).max().unwrap_or(0) + 1
    }

    pub fn by_id(&self, id: u64) -> Option<&DownloadInfo> {
        self.items.iter().find(|d| d.id == id)
    }

    pub fn by_id_mut(&mut self, id: u64) -> Option<&mut DownloadInfo> {
        self.items.iter_mut().find(|d| d.id == id)
    }

    pub fn remove(&mut self, id: u64) -> bool {
        let before = self.items.len();
        self.items.retain(|d| d.id != id);
        self.items.len() != before
    }

    pub fn history_path() -> PathBuf {
        crate::settings::Settings::config_dir().join("history.csv")
    }
}

/// Append a history record (timestamp,id,url,file,bytes,state[,error]).
pub fn append_history(id: u64, url: &str, file: &str, bytes: u64, state: &str, err: &str) -> Result<()> {
    use std::io::Write;
    let dir = crate::settings::Settings::config_dir();
    std::fs::create_dir_all(&dir)?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let fresh = !Queue::history_path().exists();
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(Queue::history_path())?;
    if fresh {
        writeln!(f, "timestamp,id,url,file,bytes,state,error")?;
    }
    writeln!(f, "{ts},{id},{url},{file},{bytes},{state},{err}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DownloadState;
    use crate::url::Url;

    #[test]
    fn queue_roundtrip() {
        let mut q = Queue::default();
        let u = Url::parse("https://example.com/a.zip").unwrap();
        q.items.push(DownloadInfo::new(1, u, std::path::PathBuf::from("a.zip"), 8));
        q.items[0].state = DownloadState::Paused;
        let json = serde_json::to_string(&q).unwrap();
        let back: Queue = serde_json::from_str(&json).unwrap();
        assert_eq!(back.items.len(), 1);
        assert_eq!(back.items[0].state, DownloadState::Paused);
        assert_eq!(back.items[0].parts, 8);
        assert_eq!(back.next_id(), 2);
    }
}
