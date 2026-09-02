use std::sync::Arc;

use arc_swap::ArcSwap;
use clap::Parser;
use color_eyre::Result;
use color_eyre::eyre::eyre;

mod app_loop;
mod cli;
mod config_cmd;
mod registry;
mod tui;
mod utils;
mod vault_cmd;

use moku_core::{
    CliContext, ConfigManager, ModuleId, MokuConfig, SecurityManager, StorageManager, VaultSession,
};

use crate::app_loop::run;
use crate::cli::{Cli, Commands, DaemonCommands};
use crate::registry::{build_cli_registry, build_tui_registry};
use crate::utils::{init_errors, init_logging};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.portable {
        moku_core::dirs::init_portable_mode().map_err(|e| eyre!(e))?;
    }

    if let Some(Commands::Daemon { sub: Some(sub_cmd) }) = &cli.command {
        match sub_cmd {
            DaemonCommands::Start => {
                if moku_daemon::status::is_running() {
                    println!("Daemon is already running.");
                    return Ok(());
                }
                let exe = std::env::current_exe().map_err(|e| eyre!(e))?;
                let mut cmd = std::process::Command::new(&exe);
                cmd.arg("daemon").arg("run");
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
                }
                match cmd.spawn() {
                    Ok(child) => {
                        println!("Daemon started in background (PID: {})", child.id());
                    }
                    Err(e) => {
                        return Err(eyre!("Failed to spawn daemon: {}", e));
                    }
                }
                return Ok(());
            }
            DaemonCommands::Stop => {
                return moku_daemon::status::stop_daemon()
                    .map_err(|e| eyre!(e));
            }
            DaemonCommands::Run => {
                return moku_daemon::worker::run_worker()
                    .await
                    .map_err(|e| eyre!(e));
            }
            DaemonCommands::Status => {
                return moku_daemon::status::print_status()
                    .map_err(|e| eyre!(e));
            }
            DaemonCommands::EnableAutostart => {
                let exe = std::env::current_exe().map_err(|e| eyre!(e))?;
                return moku_daemon::autostart::set_autostart(true, &exe, &["daemon", "run"])
                    .map_err(|e| eyre!(e));
            }
            DaemonCommands::DisableAutostart => {
                let exe = std::env::current_exe().map_err(|e| eyre!(e))?;
                return moku_daemon::autostart::set_autostart(false, &exe, &["daemon", "run"])
                    .map_err(|e| eyre!(e));
            }
        }
    }

    if let Some(Commands::Vault { sub }) = &cli.command {
        // Vault volumes are entirely independent of moku's own vault/
        // session/storage — each has its own SecurityManager rooted at
        // its own directory (see moku-vault-daemon::registry) — so this
        // is handled before any of that is constructed, same early-exit
        // shape as Commands::Config below.
        if let Err(e) = vault_cmd::handle(sub).await {
            eprintln!("{}", e);
            std::process::exit(1);
        }
        return Ok(());
    }

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

    if let Some(Commands::Config { sub }) = &cli.command {
        // Matches the error-reporting convention used by CLI module
        // dispatch below (eprintln + exit) rather than letting the Err
        // propagate through main()'s own Result — main.rs never otherwise
        // returns an Err this way, and doing so here triggered a runaway
        // panic/eyre-hook interaction (see tui::restore() in utils.rs)
        // when the terminal was never put into raw/alternate-screen mode.
        if let Err(e) = config_cmd::handle(sub, &config, &session, &security, &storage).await {
            eprintln!("{}", e);
            std::process::exit(1);
        }
        return Ok(());
    }

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
