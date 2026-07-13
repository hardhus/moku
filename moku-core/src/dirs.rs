use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;

/// Returns the ProjectDirs struct which holds standard paths for the OS.
/// Organization: com, Qualifier: hardhus, Application: moku
pub fn get_project_dirs() -> Result<ProjectDirs> {
    let app_name = if cfg!(debug_assertions) {
        "moku_dev"
    } else {
        "moku"
    };
    ProjectDirs::from("com", "hardhus", app_name)
        .context("Failed to determine user home directory or access to system paths was denied.")
}

/// Returns the standard configuration directory path.
pub fn get_config_dir() -> Result<PathBuf> {
    let dirs = get_project_dirs()?;
    Ok(dirs.config_dir().to_path_buf())
}

/// Returns the local data directory path (for databases, logs, etc.).
pub fn get_data_dir() -> Result<PathBuf> {
    let dirs = get_project_dirs()?;
    Ok(dirs.data_local_dir().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_project_dirs_resolve() {
        let dirs = get_project_dirs();
        assert!(dirs.is_ok(), "Project directories could not be resolved!");
    }

    #[test]
    fn test_paths_contain_app_name() {
        let config_path = get_config_dir().unwrap();
        let data_path = get_data_dir().unwrap();

        let path_str = config_path.to_string_lossy().to_lowercase();
        let data_str = data_path.to_string_lossy().to_lowercase();

        assert!(
            path_str.contains("moku"),
            "Config path does not contain application name: {}",
            path_str
        );
        assert!(
            data_str.contains("moku"),
            "Data path does not contain application name: {}",
            data_str
        );
    }

    #[test]
    fn test_paths_are_absolute() {
        if let Ok(path) = get_config_dir() {
            assert!(path.is_absolute(), "Config path must be absolute!");
        }
        if let Ok(path) = get_data_dir() {
            assert!(path.is_absolute(), "Data path must be absolute!");
        }
    }
}
