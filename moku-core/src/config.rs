// Temporary facade while P2 splits moku-core: config now lives in
// moku-config. Removed once every crate imports moku_config directly.
pub use moku_config::{
    ConfigManager, GeneralSettings, KeyBindings, MokuConfig, StorageSettings, ThemeColors,
};
