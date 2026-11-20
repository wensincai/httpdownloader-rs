//! Settings model + persistence (mirrors the registry-backed settings; we use JSON).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// global speed limit in bytes/s; 0 = unlimited (mirrors original)
    pub global_speed_limit: u64,
    /// max simultaneous downloads in queue (mirrors original max_downloads)
    pub max_concurrent_downloads: u32,
    /// default number of parts per download
    pub default_parts: u16,
    pub download_directory: Option<PathBuf>,
    pub user_agent: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            global_speed_limit: 0,
            max_concurrent_downloads: 3,
            default_parts: 1,
            download_directory: None,
            user_agent: None,
        }
    }
}

impl Settings {
    pub fn config_dir() -> PathBuf {
        // mirror original's %APPDATA% location
        let base = std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir());
        base.join("httpdownloader-rs")
    }

    pub fn settings_path() -> PathBuf {
        Self::config_dir().join("settings.json")
    }

    pub fn load() -> Self {
        match std::fs::read_to_string(Self::settings_path()) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        let dir = Self::config_dir();
        std::fs::create_dir_all(&dir)?;
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(Self::settings_path(), json)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let s = Settings { default_parts: 8, global_speed_limit: 1024, ..Default::default() };
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.default_parts, 8);
        assert_eq!(back.max_concurrent_downloads, 3); // default preserved via #[serde(default)]
    }
}
