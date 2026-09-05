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
    #[arg(
        long,
        help = "Initialize portable mode by creating `moku-data` next to the executable"
    )]
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
    /// View or change per-module storage encryption settings.
    Config {
        #[command(subcommand)]
        sub: ConfigCommands,
    },
    /// Manage encrypted, mountable volumes (create/list/status/resize).
    /// Mount/unmount are added in a later phase.
    Vault {
        #[command(subcommand)]
        sub: VaultCommands,
    },
    /// Manage a satz-powered Markdown notes vault (index/stats/list/
    /// resolve/daily/fmt/graph).
    #[command(disable_help_flag = true)]
    Notes {
        // Same disable_help_flag rationale as Rss: lets NotesCliModule's
        // own clap subcommand parser generate real per-subcommand help.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Manage a vault-backed password/secrets store (generate/add/list/
    /// show/totp/remove/export/import).
    #[command(disable_help_flag = true)]
    Secrets {
        // Same disable_help_flag rationale as Rss/Notes: lets
        // SecretsCliModule's own clap subcommand parser generate real
        // per-subcommand help.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run and inspect HTTP requests from a TOML collection file
    /// (run/show/new) — usable as CI test automation.
    #[command(disable_help_flag = true)]
    Http {
        // Same disable_help_flag rationale as Rss/Notes/Secrets: lets
        // HttpCliModule's own clap subcommand parser generate real
        // per-subcommand help.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand, Clone, PartialEq, Debug)]
pub enum VaultCommands {
    /// Create a new encrypted volume. Any of name/size/password-mode left
    /// unset is asked for interactively, so `moku vault create` alone
    /// works as a full wizard, while `moku vault create NAME --size ...
    /// --default-password` skips straight to just the password prompt.
    Create {
        name: Option<String>,
        /// Human-readable size, e.g. "10GB" or "512MiB".
        #[arg(long)]
        size: Option<String>,
        /// Use a password independent from moku's own vault password.
        /// Skips the interactive mode question.
        #[arg(long)]
        custom_password: bool,
        /// Use moku's own vault password for this volume. Skips the
        /// interactive mode question (the symmetric counterpart to
        /// --custom-password, for non-interactive/scripted use).
        #[arg(long)]
        default_password: bool,
        /// Where to create the volume's directory. Defaults to the
        /// current directory.
        #[arg(long)]
        path: Option<String>,
    },
    /// List all encrypted volumes with their usage and mount status.
    List,
    /// Show one volume's status in detail.
    Status { name: String },
    /// Change a volume's size limit (takes effect on its next mount).
    Resize {
        name: String,
        #[arg(long)]
        size: String,
    },
    /// Mount a volume as a real drive/folder. Prompts for its password.
    Mount {
        name: String,
        /// Drive letter ("X:") or an empty NTFS folder path.
        #[arg(long)]
        mountpoint: String,
    },
    /// Unmount a volume.
    Unmount { name: String },
    /// Permanently delete a volume and all its data. Unmounts it first
    /// automatically if it's currently mounted. Prompts for confirmation
    /// unless --yes is given.
    Delete {
        name: String,
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Register an existing volume directory (containing its own
    /// volume.json — e.g. moved by hand, or created before moku tracked
    /// volume locations) so it can be managed by name/id like any other.
    Import { path: String },
    /// Internal: runs the actual mount in a dedicated child process,
    /// spawned by `Mount`. Reads the password from stdin. Not for direct use.
    #[command(hide = true)]
    MountWorker {
        name: String,
        #[arg(long)]
        mountpoint: String,
    },
}

#[derive(Subcommand, Clone, PartialEq, Debug)]
pub enum ConfigCommands {
    /// Show the effective (config-resolved) encryption setting for every
    /// module that supports it.
    ShowEncrypt,
    /// Set (or clear) a per-module encryption override in config.toml,
    /// then migrate that module's existing stored data to match. Prompts
    /// for the vault password if migrating to encrypted (or if any
    /// existing record needs decrypting) and the vault isn't unlocked.
    SetEncrypt {
        /// Module id: todo, bookmark, or rss.
        module: String,
        /// "true" or "false".
        #[arg(action = clap::ArgAction::Set)]
        value: bool,
    },
    /// Re-run migration for one module (or all supported modules, if
    /// omitted) against its currently configured encryption setting,
    /// without changing config — useful to reconcile drift after editing
    /// config.toml by hand.
    Migrate {
        /// Module id: todo, bookmark, or rss. All supported modules if omitted.
        module: Option<String>,
    },
}

#[derive(Subcommand, Clone, PartialEq, Debug)]
pub enum DaemonCommands {
    /// Start the daemon in the background (no terminal window). Returns immediately.
    Start {
        /// Internal: set only by the registered autostart entry. A
        /// freshly-launched console-subsystem exe with no parent console
        /// (as happens when Windows runs the HKCU Run key at logon) gets a
        /// new console window from the OS before our code ever runs; this
        /// flag tells the handler to free/hide that console immediately,
        /// before printing anything, so the window never becomes visible.
        #[arg(long, hide = true)]
        from_autostart: bool,
    },
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
            // Always handled by an early return in main.rs before this
            // matters for registry dispatch; SETTINGS is the closest
            // semantic match since it's a config-editing command.
            Some(Commands::Config { .. }) => ModuleId::SETTINGS,
            // Same as Config: always intercepted early in main.rs.
            Some(Commands::Vault { .. }) => ModuleId::SETTINGS,
            Some(Commands::Notes { .. }) => ModuleId::NOTES,
            Some(Commands::Secrets { .. }) => ModuleId::SECRETS,
            Some(Commands::Http { .. }) => ModuleId::HTTP,
            None => ModuleId::LAUNCHER,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_target_module_resolution() {
        let cli_none = Cli {
            command: None,
            portable: false,
        };
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
            command: Some(Commands::Daemon {
                sub: Some(DaemonCommands::Start {
                    from_autostart: false,
                }),
            }),
            portable: false,
        };
        assert_eq!(cli_daemon_start.target_module(), ModuleId::DAEMON);

        let cli_daemon_stop = Cli {
            command: Some(Commands::Daemon {
                sub: Some(DaemonCommands::Stop),
            }),
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

    #[test]
    fn test_vault_create_parses_with_no_name_or_size() {
        // Both are optional now — missing ones fall back to an interactive
        // prompt at runtime rather than a clap parse error.
        let cli = Cli::try_parse_from(["moku", "vault", "create"]).unwrap();
        let Some(Commands::Vault {
            sub: VaultCommands::Create { name, size, .. },
        }) = cli.command
        else {
            panic!("expected Vault::Create");
        };
        assert_eq!(name, None);
        assert_eq!(size, None);
    }

    #[test]
    fn test_vault_create_parses_fully_flag_driven() {
        let cli = Cli::try_parse_from([
            "moku",
            "vault",
            "create",
            "myvol",
            "--size",
            "10GB",
            "--default-password",
        ])
        .unwrap();
        let Some(Commands::Vault {
            sub:
                VaultCommands::Create {
                    name,
                    size,
                    custom_password,
                    default_password,
                    ..
                },
        }) = cli.command
        else {
            panic!("expected Vault::Create");
        };
        assert_eq!(name.as_deref(), Some("myvol"));
        assert_eq!(size.as_deref(), Some("10GB"));
        assert!(!custom_password);
        assert!(default_password);
    }

    #[test]
    fn test_vault_create_custom_password_flag_parses() {
        let cli =
            Cli::try_parse_from(["moku", "vault", "create", "myvol", "--custom-password"]).unwrap();
        let Some(Commands::Vault {
            sub: VaultCommands::Create {
                custom_password, ..
            },
        }) = cli.command
        else {
            panic!("expected Vault::Create");
        };
        assert!(custom_password);
    }

    #[test]
    fn test_vault_delete_parses_with_and_without_yes() {
        let cli = Cli::try_parse_from(["moku", "vault", "delete", "myvol"]).unwrap();
        let Some(Commands::Vault {
            sub: VaultCommands::Delete { name, yes },
        }) = cli.command
        else {
            panic!("expected Vault::Delete");
        };
        assert_eq!(name, "myvol");
        assert!(!yes);

        let cli = Cli::try_parse_from(["moku", "vault", "delete", "myvol", "--yes"]).unwrap();
        let Some(Commands::Vault {
            sub: VaultCommands::Delete { yes, .. },
        }) = cli.command
        else {
            panic!("expected Vault::Delete");
        };
        assert!(yes);

        let cli = Cli::try_parse_from(["moku", "vault", "delete", "myvol", "-y"]).unwrap();
        let Some(Commands::Vault {
            sub: VaultCommands::Delete { yes, .. },
        }) = cli.command
        else {
            panic!("expected Vault::Delete");
        };
        assert!(yes);
    }
}
