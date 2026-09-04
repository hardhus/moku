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
            DaemonCommands::Start { from_autostart } => {
                let from_autostart = *from_autostart;
                // Launched directly by the Windows Run key at logon, this
                // process has no parent console, so the OS allocates a new,
                // visible one just for it before main() runs. Free it
                // immediately — before any print — so it never flashes on
                // screen. Manual `moku daemon start` runs never set this
                // flag, so their console (the user's own terminal) is left
                // alone and the status messages below still print normally.
                #[cfg(windows)]
                if from_autostart {
                    unsafe {
                        let _ = windows::Win32::System::Console::FreeConsole();
                    }
                }
                if moku_daemon::status::is_running() {
                    if !from_autostart {
                        println!("Daemon is already running.");
                    }
                    return Ok(());
                }
                let exe = std::env::current_exe().map_err(|e| eyre!(e))?;
                let mut cmd = std::process::Command::new(&exe);
                cmd.arg("daemon").arg("run");
                // Without this, the worker inherits our stdin/stdout/stderr
                // handles by default and keeps them open for as long as it
                // runs — e.g. a caller capturing our output via a pipe (or
                // command substitution) would block forever waiting for
                // EOF, since the long-lived detached worker never closes
                // its inherited copy of that pipe.
                cmd.stdin(std::process::Stdio::null());
                cmd.stdout(std::process::Stdio::null());
                cmd.stderr(std::process::Stdio::null());
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
                }
                match cmd.spawn() {
                    Ok(child) => {
                        if !from_autostart {
                            println!("Daemon started in background (PID: {})", child.id());
                        }
                    }
                    Err(e) => {
                        if from_autostart {
                            return Ok(());
                        }
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
                moku_daemon::autostart::set_autostart(true, &exe, &["daemon", "start", "--from-autostart"])
                    .map_err(|e| eyre!(e))?;
                println!("✅ Moku added to system autostart.");
                return Ok(());
            }
            DaemonCommands::DisableAutostart => {
                let exe = std::env::current_exe().map_err(|e| eyre!(e))?;
                moku_daemon::autostart::set_autostart(false, &exe, &["daemon", "start", "--from-autostart"])
                    .map_err(|e| eyre!(e))?;
                println!("🧹 Moku removed from system autostart.");
                return Ok(());
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
            Some(Commands::Notes { args }) => args.clone(),
            Some(Commands::Secrets { args }) => args.clone(),
            Some(Commands::Http { args }) => args.clone(),
            _ => vec![],
        };

        let cli_ctx = CliContext {
            config: loaded_config,
            storage: Some(Arc::clone(&storage)),
            session: Some(Arc::clone(&session)),
            security: Some(Arc::clone(&security)),
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
