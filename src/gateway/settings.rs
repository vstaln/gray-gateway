use std::path::Path;

use crate::cron::store::Origin as Route;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    pub owners: Vec<Route>,

    pub max_turns: usize,

    pub turn_timeout_secs: u64,

    pub workdir: Option<std::path::PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            owners: Vec::new(),
            max_turns: 2,
            turn_timeout_secs: 1800,
            workdir: None,
        }
    }
}

impl Settings {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("config.json");
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                log::warn!(
                    "gateway: {} unreadable ({e}); using defaults",
                    path.display()
                );
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }
}
