use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub rows: usize,
    pub cols: usize,
    pub tile_dirs: Vec<String>,
}

pub fn default_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "cctiles")
        .map(|dirs| dirs.config_dir().join("config.toml"))
}

pub fn load(path: &Path) -> Option<Config> {
    let text = std::fs::read_to_string(path).ok()?;
    toml::from_str(&text).ok()
}

pub fn save(path: &Path, config: &Config) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(config).map_err(std::io::Error::other)?;
    std::fs::write(path, text)
}
