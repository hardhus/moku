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
}

impl Cli {
    pub fn target_module(&self) -> ModuleId {
        match &self.command {
            Some(Commands::Todo) => ModuleId::TODO,
            Some(Commands::Context { .. }) => ModuleId::new("context"),
            Some(Commands::Commit) => ModuleId::new("commit"),
            Some(Commands::Rss { .. }) => ModuleId::RSS,
            None => ModuleId::LAUNCHER,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_target_module_resolution() {
        let cli_none = Cli { command: None };
        assert_eq!(cli_none.target_module(), ModuleId::LAUNCHER);

        let cli_todo = Cli {
            command: Some(Commands::Todo),
        };
        assert_eq!(cli_todo.target_module(), ModuleId::TODO);

        let cli_commit = Cli {
            command: Some(Commands::Commit),
        };
        assert_eq!(cli_commit.target_module(), ModuleId::new("commit"));

        let cli_context = Cli {
            command: Some(Commands::Context {
                path: ".".to_string(),
                out: None,
            }),
        };
        assert_eq!(cli_context.target_module(), ModuleId::new("context"));

        let cli_rss = Cli {
            command: Some(Commands::Rss { args: vec![] }),
        };
        assert_eq!(cli_rss.target_module(), ModuleId::RSS);
    }
}
