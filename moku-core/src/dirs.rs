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

fn detect_portable_root() -> Option<PathBuf> {
    #[cfg(test)]
    {
        if let Ok(path) = std::env::var("MOKU_TEST_PORTABLE_ROOT") {
            let p = PathBuf::from(path);
            if p.is_dir() {
                return Some(p);
            }
        }
    }

    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;
    let portable = exe_dir.join("moku-data");
    if portable.is_dir() {
        Some(portable)
    } else {
        None
    }
}

pub fn init_portable_mode() -> Result<()> {
    let exe = std::env::current_exe().context("Failed to determine current executable path")?;
    let exe_dir = exe
        .parent()
        .context("Failed to get directory of current executable")?;
    let portable = exe_dir.join("moku-data");
    if !portable.exists() {
        std::fs::create_dir_all(&portable).context("Failed to create moku-data directory")?;
        println!("✅ Portable mode initialized at: {:?}", portable);
    } else {
        println!("ℹ️ Portable mode already active at: {:?}", portable);
    }
    Ok(())
}

pub fn get_config_dir() -> Result<PathBuf> {
    if let Some(root) = detect_portable_root() {
        return Ok(root);
    }
    let dirs = get_project_dirs()?;
    Ok(dirs.config_dir().to_path_buf())
}

pub fn get_data_dir() -> Result<PathBuf> {
    if let Some(root) = detect_portable_root() {
        return Ok(root);
    }
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

        // Standard or portable path should contain "moku"
        assert!(
            path_str.contains("moku"),
            "Config path missing app name: {}",
            path_str
        );
        assert!(
            data_str.contains("moku"),
            "Data path missing app name: {}",
            data_str
        );
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

    #[test]
    fn test_detect_portable_root() {
        let temp = tempfile::tempdir().unwrap();
        let portable_dir = temp.path().join("moku-data");

        // Set env override for testing
        unsafe {
            std::env::set_var("MOKU_TEST_PORTABLE_ROOT", &portable_dir);
        }

        // When there is no moku-data, it should return None
        assert!(detect_portable_root().is_none());

        // Create the directory
        std::fs::create_dir(&portable_dir).unwrap();

        // Now it should detect it
        assert!(detect_portable_root().is_some());
        assert_eq!(detect_portable_root().unwrap(), portable_dir);

        // Clean up env override
        unsafe {
            std::env::remove_var("MOKU_TEST_PORTABLE_ROOT");
        }
    }
}
