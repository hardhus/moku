use clap::{Parser, Subcommand};

use moku_core::ModuleId;

#[derive(Parser)]
#[command(
    name = "moku",
    author,
    version,
    about = "A modular TUI productivity tool."
)]
pub struct Cli {
    /// Initialize portable mode by creating `moku-data` next to the executable
    #[arg(long, help = "Initialize portable mode by creating `moku-data` next to the executable")]
    pub portable: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Clone, PartialEq, Debug)]
pub enum Commands {
    Todo,
    Context {
        #[arg(default_value = ".")]
        path: String,

        #[arg(short, long)]
        out: Option<String>,
    },
    Commit,
    /// Manage RSS subscriptions and list cached articles.
    Rss {
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Background daemon management.
    Daemon {
        #[command(subcommand)]
        sub: Option<DaemonCommands>,
    },
}

#[derive(Subcommand, Clone, PartialEq, Debug)]
pub enum DaemonCommands {
    /// Start the daemon in the background (no terminal window). Returns immediately.
    Start,
    /// Stop the running background daemon.
    Stop,
    /// Run the daemon worker in the foreground (used by autostart).
    Run,
    /// Show daemon status (checks PID file).
    Status,
    /// Register moku as a system autostart entry.
    EnableAutostart,
    /// Remove moku from system autostart.
    DisableAutostart,
}

impl Cli {
    pub fn target_module(&self) -> ModuleId {
        match &self.command {
            Some(Commands::Todo) => ModuleId::TODO,
            Some(Commands::Context { .. }) => ModuleId::new("context"),
            Some(Commands::Commit) => ModuleId::new("commit"),
            Some(Commands::Rss { .. }) => ModuleId::RSS,
            Some(Commands::Daemon { .. }) => ModuleId::DAEMON,
            None => ModuleId::LAUNCHER,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_target_module_resolution() {
        let cli_none = Cli { command: None, portable: false };
        assert_eq!(cli_none.target_module(), ModuleId::LAUNCHER);

        let cli_todo = Cli {
            command: Some(Commands::Todo),
            portable: false,
        };
        assert_eq!(cli_todo.target_module(), ModuleId::TODO);

        let cli_commit = Cli {
            command: Some(Commands::Commit),
            portable: false,
        };
        assert_eq!(cli_commit.target_module(), ModuleId::new("commit"));

        let cli_context = Cli {
            command: Some(Commands::Context {
                path: ".".to_string(),
                out: None,
            }),
            portable: false,
        };
        assert_eq!(cli_context.target_module(), ModuleId::new("context"));

        let cli_rss = Cli {
            command: Some(Commands::Rss { args: vec![] }),
            portable: false,
        };
        assert_eq!(cli_rss.target_module(), ModuleId::RSS);

        let cli_daemon = Cli {
            command: Some(Commands::Daemon { sub: None }),
            portable: false,
        };
        assert_eq!(cli_daemon.target_module(), ModuleId::DAEMON);

        let cli_daemon_start = Cli {
            command: Some(Commands::Daemon { sub: Some(DaemonCommands::Start) }),
            portable: false,
        };
        assert_eq!(cli_daemon_start.target_module(), ModuleId::DAEMON);

        let cli_daemon_stop = Cli {
            command: Some(Commands::Daemon { sub: Some(DaemonCommands::Stop) }),
            portable: false,
        };
        assert_eq!(cli_daemon_stop.target_module(), ModuleId::DAEMON);
    }
}
