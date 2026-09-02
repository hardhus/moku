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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_write_read_round_trip() {
        let dir = tempdir().unwrap();
        let statuses = vec![
            TaskStatus {
                id: "rss".to_string(),
                last_run_secs: Some(1_700_000_000),
                last_item_count: 3,
                last_error: None,
            },
            TaskStatus {
                id: "other".to_string(),
                last_run_secs: None,
                last_item_count: 0,
                last_error: Some("boom".to_string()),
            },
        ];

        write_statuses(dir.path(), &statuses).unwrap();
        let loaded = read_statuses(dir.path());

        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id, "rss");
        assert_eq!(loaded[0].last_run_secs, Some(1_700_000_000));
        assert_eq!(loaded[0].last_item_count, 3);
        assert_eq!(loaded[0].last_error, None);
        assert_eq!(loaded[1].id, "other");
        assert_eq!(loaded[1].last_error, Some("boom".to_string()));
    }

    #[test]
    fn test_read_missing_file_returns_empty() {
        let dir = tempdir().unwrap();
        assert!(read_statuses(dir.path()).is_empty());
    }

    #[test]
    fn test_read_corrupted_file_returns_empty() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(STATUS_FILE), "not valid json").unwrap();
        assert!(read_statuses(dir.path()).is_empty());
    }

    #[test]
    fn test_write_overwrites_previous_contents() {
        let dir = tempdir().unwrap();
        write_statuses(
            dir.path(),
            &[TaskStatus { id: "a".to_string(), ..Default::default() }],
        )
        .unwrap();
        write_statuses(
            dir.path(),
            &[TaskStatus { id: "b".to_string(), ..Default::default() }],
        )
        .unwrap();

        let loaded = read_statuses(dir.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "b");
    }
}
