use anyhow::{Context, Result};
use tokio::fs;

use crate::dirs;

use super::schema::MokuConfig;

pub struct ConfigManager;

impl ConfigManager {
    /// Loads the configuration from `config.toml`.
    /// Creates a default file with comments if it doesn't exist.
    pub async fn load() -> Result<MokuConfig> {
        let config_dir = dirs::get_config_dir()?;
        let path = config_dir.join("config.toml");

        if !config_dir.exists() {
            fs::create_dir_all(&config_dir)
                .await
                .context("Failed to create config directory")?;
        }

        if !path.exists() {
            let default_config = MokuConfig::default();
            let toml_string = toml::to_string_pretty(&default_config)
                .context("Failed to serialize default config to TOML")?;

            let content = format!(
                "# -----------------------------------------------------------\n\
                 #  MOKU - CONFIGURATION FILE\n\
                 # -----------------------------------------------------------\n\n\
                 {}",
                toml_string
            );

            fs::write(&path, content)
                .await
                .context("Failed to write default config file")?;

            tracing::info!("New config file created at: {:?}", path);
        }

        let content = fs::read_to_string(&path)
            .await
            .context("Failed to read config file")?;

        let config: MokuConfig = toml::from_str(&content)
            .context("Invalid config file format! Please check the TOML structure.")?;

        Ok(config)
    }

    /// Saves the current configuration to disk asynchroneously.
    pub async fn save(config: &MokuConfig) -> Result<()> {
        let config_dir = dirs::get_config_dir()?;
        let path = config_dir.join("config.toml");

        let toml_string =
            toml::to_string_pretty(config).context("Failed to serialize config data to TOML")?;

        fs::write(path, toml_string)
            .await
            .context("Failed to write config file to disk")?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_config_serialization_cycle() {
        let mut config = MokuConfig::default();
        config.general.theme = "hacker".to_string();

        let toml_str = toml::to_string_pretty(&config).unwrap();
        let de_config: MokuConfig = toml::from_str(&toml_str).unwrap();

        assert_eq!(de_config.general.theme, "hacker");
        assert_eq!(de_config.keys.quit, "q");
    }

    #[tokio::test]
    async fn test_default_config_validity() {
        let default_config = MokuConfig::default();
        let toml_str = toml::to_string_pretty(&default_config).unwrap();

        // Verify that the generated TOML can be parsed back
        let parsed: MokuConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.general.theme, "system");
    }
}
