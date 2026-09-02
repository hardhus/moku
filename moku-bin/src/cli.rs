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
    /// Open the Todo List module.
    Todo,
    /// Scan a codebase and compile its contents into a single AI-ready context blob.
    #[command(alias = "ctx")]
    Context {
        #[arg(default_value = ".")]
        path: String,

        #[arg(short, long)]
        out: Option<String>,
    },
    /// Generate a commit message from the staged diff with AI.
    #[command(alias = "co")]
    Commit,
    /// Manage RSS subscriptions and list cached articles.
    #[command(disable_help_flag = true)]
    Rss {
        // disable_help_flag lets -h/--help pass through into `args` instead
        // of being intercepted here, so RssCliModule's own clap subcommand
        // parser (add/remove/list/test-notify) can generate real help for
        // them instead of this catch-all showing just `[ARGS]...`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
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
            Some(Commands::Context { .. }) => ModuleId::CONTEXT,
            Some(Commands::Commit) => ModuleId::COMMIT,
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
        assert_eq!(cli_commit.target_module(), ModuleId::COMMIT);

        let cli_context = Cli {
            command: Some(Commands::Context {
                path: ".".to_string(),
                out: None,
            }),
            portable: false,
        };
        assert_eq!(cli_context.target_module(), ModuleId::CONTEXT);

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

    #[test]
    fn test_context_alias_ctx() {
        let cli = Cli::try_parse_from(["moku", "ctx", "."]).unwrap();
        assert!(matches!(cli.command, Some(Commands::Context { .. })));
    }

    #[test]
    fn test_commit_alias_co() {
        let cli = Cli::try_parse_from(["moku", "co"]).unwrap();
        assert_eq!(cli.command, Some(Commands::Commit));
    }
}
