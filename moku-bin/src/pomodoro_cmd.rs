use anyhow::Result;

use moku_core::MokuConfig;
use moku_pomodoro::engine::PomodoroConfig;

use crate::cli::PomodoroCommands;

/// Dispatches `moku pomodoro <sub>`. `Start`/`List` need `config` (to
/// resolve/list profiles); the others don't touch it at all — see
/// `main.rs`'s interception point for why this still receives it
/// unconditionally (mirrors `config_cmd::handle`'s position/shape).
pub async fn handle(sub: &PomodoroCommands, config: &MokuConfig) -> Result<()> {
    match sub {
        PomodoroCommands::Start { name } => {
            let pomodoro_config: PomodoroConfig = config.resolve_module_config("pomodoro");
            println!(
                "{}",
                moku_pomodoro::cli_module::start(&pomodoro_config, name.as_deref()).await?
            );
            Ok(())
        }
        PomodoroCommands::List => {
            let pomodoro_config: PomodoroConfig = config.resolve_module_config("pomodoro");
            println!("{}", moku_pomodoro::cli_module::list(&pomodoro_config));
            Ok(())
        }
        PomodoroCommands::Stop => {
            println!("{}", moku_pomodoro::cli_module::stop().await?);
            Ok(())
        }
        PomodoroCommands::Status => {
            println!("{}", moku_pomodoro::cli_module::status().await?);
            Ok(())
        }
        PomodoroCommands::Reset => {
            println!("{}", moku_pomodoro::cli_module::reset().await?);
            Ok(())
        }
        PomodoroCommands::RunWorker => moku_pomodoro::run_worker().await,
    }
}
