use std::sync::Arc;

use arc_swap::ArcSwap;
use clap::Parser;
use color_eyre::Result;
use color_eyre::eyre::eyre;

mod app_loop;
mod cli;
mod registry;
mod tui;
mod utils;

use moku_core::{
    CliContext, ConfigManager, ModuleId, MokuConfig, SecurityManager, StorageManager, VaultSession,
};

use crate::app_loop::run;
use crate::cli::{Cli, Commands};
use crate::registry::{build_cli_registry, build_tui_registry};
use crate::utils::{init_errors, init_logging};

#[tokio::main]
async fn main() -> Result<()> {
    init_errors()?;
    let _guard = init_logging()?;

    let loaded_config = match ConfigManager::load().await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to load config: {}. Using default configuration.", e);
            MokuConfig::default()
        }
    };
    let config = Arc::new(ArcSwap::from_pointee(loaded_config.clone()));

    let cli = Cli::parse();

    // session, security, and storage are now initialized once and shared
    // across both CLI and TUI execution paths. This allows CLI modules
    // (such as RSS) to access the actual storage.
    let session = Arc::new(VaultSession::new());
    let security = Arc::new(SecurityManager::new().map_err(|e| eyre!(e))?);
    let storage = Arc::new(
        StorageManager::new(Arc::clone(&session))
            .await
            .map_err(|e| eyre!(e))?,
    );

    let cli_registry = build_cli_registry();
    let target_module = cli.target_module();

    if let Some(module) = cli_registry.get(target_module) {
        let args = match &cli.command {
            Some(Commands::Context { path, out }) => {
                let mut a = vec![path.clone()];
                if let Some(o) = out {
                    a.push(o.clone());
                }
                a
            }
            Some(Commands::Rss { args }) => args.clone(),
            _ => vec![],
        };

        let cli_ctx = CliContext {
            config: loaded_config,
            storage: Some(Arc::clone(&storage)),
        };

        if let Err(e) = module.run(&args, &cli_ctx).await {
            eprintln!("{}", e);
            std::process::exit(1);
        }
        return Ok(());
    }
    let target: ModuleId = cli.target_module();

    let registry = build_tui_registry(&*config.load());

    run(config, session, security, storage, registry, target).await
}
