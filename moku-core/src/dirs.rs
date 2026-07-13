use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;

pub fn get_project_dirs() -> Result<ProjectDirs> {
    let app_name = if cfg!(debug_assertions) {
        "moku_dev"
    } else {
        "moku"
    };
    ProjectDirs::from("com", "hardhus", app_name)
        .context("Failed to determine user home directory or access to system paths was denied.")
}

pub fn get_config_dir() -> Result<PathBuf> {
    let dirs = get_project_dirs()?;
    Ok(dirs.config_dir().to_path_buf())
}

pub fn get_data_dir() -> Result<PathBuf> {
    let dirs = get_project_dirs()?;
    Ok(dirs.data_local_dir().to_path_buf())
}

/// Directory where the user places Lua plugin scripts.
/// Lives under the config directory (in the same place as `config.toml`),
/// so that the user manages both config and plugins in a single directory.
pub fn get_plugins_dir() -> Result<PathBuf> {
    Ok(get_config_dir()?.join("plugins"))
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

        assert!(path_str.contains("moku"), "Config path missing app name: {}", path_str);
        assert!(data_str.contains("moku"), "Data path missing app name: {}", data_str);
    }

    #[test]
    fn test_paths_are_absolute() {
        if let Ok(path) = get_config_dir() {
            assert!(path.is_absolute());
        }
        if let Ok(path) = get_data_dir() {
            assert!(path.is_absolute());
        }
    }

    #[test]
    fn test_plugins_dir_is_under_config_dir() {
        let config = get_config_dir().unwrap();
        let plugins = get_plugins_dir().unwrap();
        assert!(plugins.starts_with(config));
        assert!(plugins.ends_with("plugins"));
    }
}
