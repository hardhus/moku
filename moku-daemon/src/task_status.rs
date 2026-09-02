use std::path::Path;

use serde::{Deserialize, Serialize};
use anyhow::Result;

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct TaskStatus {
    pub id: String,
    pub last_run_secs: Option<u64>,   // Unix timestamp
    pub last_item_count: usize,
    pub last_error: Option<String>,
}

const STATUS_FILE: &str = "daemon-tasks.json";

pub fn write_statuses(data_dir: &Path, statuses: &[TaskStatus]) -> Result<()> {
    let path = data_dir.join(STATUS_FILE);
    let json = serde_json::to_string_pretty(statuses)?;
    std::fs::write(path, json)?;
    Ok(())
}

pub fn read_statuses(data_dir: &Path) -> Vec<TaskStatus> {
    let path = data_dir.join(STATUS_FILE);
    let Ok(s) = std::fs::read_to_string(&path) else { return Vec::new(); };
    serde_json::from_str(&s).unwrap_or_default()
}
