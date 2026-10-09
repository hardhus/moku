use std::io;
use std::path::PathBuf;

use directories::ProjectDirs;

/// Why one of moku's directories couldn't be resolved or created.
#[derive(Debug, thiserror::Error)]
pub enum DirsError {
    #[error(
        "failed to determine the user home directory, or access to system paths was denied"
    )]
    NoProjectDirs,
    #[error("failed to determine the current executable's directory")]
    CurrentExe(#[source] Option<io::Error>),
    #[error("failed to create {}", path.display())]
    CreateDir {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

type Result<T> = std::result::Result<T, DirsError>;

pub fn get_project_dirs() -> Result<ProjectDirs> {
    let app_name = if cfg!(debug_assertions) {
        "moku_dev"
    } else {
        "moku"
    };
    ProjectDirs::from("com", "hardhus", app_name).ok_or(DirsError::NoProjectDirs)
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
    let exe = std::env::current_exe().map_err(|e| DirsError::CurrentExe(Some(e)))?;
    let exe_dir = exe.parent().ok_or(DirsError::CurrentExe(None))?;
    let portable = exe_dir.join("moku-data");
    if !portable.exists() {
        std::fs::create_dir_all(&portable).map_err(|source| DirsError::CreateDir {
            path: portable.clone(),
            source,
        })?;
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

    /// `test_detect_portable_root` sets `MOKU_TEST_PORTABLE_ROOT`, which
    /// every directory lookup reads. Tests run in parallel, so each test
    /// that resolves a directory holds this lock; otherwise one test can
    /// see the portable root appear or vanish halfway through.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        // A test that panicked while holding the lock leaves it poisoned;
        // the guarded state (an env var) is still fine to use.
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn test_project_dirs_resolve() {
        let dirs = get_project_dirs();
        assert!(dirs.is_ok(), "Project directories could not be resolved!");
    }

    #[test]
    fn test_paths_contain_app_name() {
        let _env = env_lock();
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
        let _env = env_lock();
        if let Ok(path) = get_config_dir() {
            assert!(path.is_absolute());
        }
        if let Ok(path) = get_data_dir() {
            assert!(path.is_absolute());
        }
    }

    #[test]
    fn test_plugins_dir_is_under_config_dir() {
        let _env = env_lock();
        let config = get_config_dir().unwrap();
        let plugins = get_plugins_dir().unwrap();
        assert!(plugins.starts_with(config));
        assert!(plugins.ends_with("plugins"));
    }

    #[test]
    fn test_detect_portable_root() {
        let _env = env_lock();
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
