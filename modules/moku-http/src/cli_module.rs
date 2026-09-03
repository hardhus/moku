use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use clap::{Parser, Subcommand};

use moku_core::{CliContext, CliModule, ModuleId, ModuleMeta};

use crate::engine::{self, RunResult};

const TEMPLATE: &str = r#"[variables]
base_url = "https://example.com"

[[requests]]
name = "ping"
method = "GET"
url = "{{base_url}}/"

[[requests.assertions]]
type = "status"
equals = 200
"#;

pub struct HttpCliModule;

impl HttpCliModule {
    pub fn new() -> Self {
        Self
    }
}

impl Default for HttpCliModule {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleMeta for HttpCliModule {
    fn id(&self) -> ModuleId {
        ModuleId::HTTP
    }
    fn title(&self) -> &'static str {
        ModuleId::HTTP.title()
    }
    fn encrypt_by_default(&self) -> bool {
        // Collections are plain files on disk (meant to be git-tracked/
        // shared/run in CI), never moku's own StorageManager.
        false
    }
}

#[derive(Parser, Debug)]
#[command(name = "http", no_binary_name = true)]
struct HttpArgs {
    #[command(subcommand)]
    cmd: Option<HttpCmd>,
}

#[derive(Subcommand, Debug)]
enum HttpCmd {
    /// Run all (or one named) request in a collection file. Exits non-zero
    /// if any assertion fails — usable directly in CI/test automation.
    Run {
        file: String,
        /// Only run the request with this name.
        #[arg(long)]
        only: Option<String>,
        /// Override or add a variable, e.g. --var base_url=https://staging.example.com
        #[arg(long = "var", value_parser = parse_key_val)]
        vars: Vec<(String, String)>,
    },
    /// List the requests defined in a collection file, without running them.
    Show { file: String },
    /// Scaffold a new starter collection file.
    New { file: String },
}

fn parse_key_val(s: &str) -> std::result::Result<(String, String), String> {
    let (k, v) = s.split_once('=').ok_or_else(|| format!("expected key=value, got '{s}'"))?;
    Ok((k.to_string(), v.to_string()))
}

#[async_trait]
impl CliModule for HttpCliModule {
    async fn run(&self, args: &[String], ctx: &CliContext) -> Result<()> {
        let parsed = match HttpArgs::try_parse_from(args) {
            Ok(p) => p,
            Err(e) if e.exit_code() == 0 => {
                print!("{e}");
                return Ok(());
            }
            Err(e) => bail!("{e}"),
        };

        match parsed.cmd {
            None => bail!("no subcommand given — try `moku http --help`"),
            Some(HttpCmd::Run { file, only, vars }) => {
                let path = PathBuf::from(&file);
                let raw = std::fs::read_to_string(&path).map_err(|e| anyhow!("failed to read '{file}': {e}"))?;
                // secrets.* references need the vault unlocked first — same
                // pattern as moku-secrets's own CLI (config_cmd.rs's
                // ensure_unlocked_if_needed, generalized via CliContext).
                if !crate::interpolate::find_secret_refs(&raw).is_empty() {
                    ensure_unlocked(ctx).await?;
                }

                let storage = ctx.storage.as_deref();
                let results = engine::run_collection(&path, only.as_deref(), &vars, storage).await?;
                if results.is_empty() {
                    bail!("no matching requests found in '{file}'");
                }

                let mut any_failed = false;
                for r in &results {
                    print_result(r);
                    if !r.all_passed() {
                        any_failed = true;
                    }
                }
                if any_failed {
                    bail!("one or more requests failed");
                }
            }
            Some(HttpCmd::Show { file }) => {
                let (collection, _raw) = engine::load_collection(&PathBuf::from(&file))?;
                if collection.requests.is_empty() {
                    println!("No requests in '{file}'.");
                } else {
                    for req in &collection.requests {
                        println!("- {} [{}] {}", req.name, req.method, req.url);
                    }
                }
            }
            Some(HttpCmd::New { file }) => {
                let path = PathBuf::from(&file);
                if path.exists() {
                    bail!("'{file}' already exists");
                }
                std::fs::write(&path, TEMPLATE)?;
                println!("✅ Created {file}");
            }
        }
        Ok(())
    }
}

fn print_result(r: &RunResult) {
    match &r.error {
        Some(e) => {
            println!("✗ {} — error: {e} ({:.0}ms)", r.name, r.duration.as_secs_f64() * 1000.0);
        }
        None => {
            let status = r.status.map(|s| s.to_string()).unwrap_or_else(|| "-".to_string());
            let mark = if r.all_passed() { "✓" } else { "✗" };
            println!("{mark} {} — {status} ({:.0}ms)", r.name, r.duration.as_secs_f64() * 1000.0);
            for a in &r.assertion_results {
                println!("    {} {}", if a.passed { "✓" } else { "✗" }, a.description);
            }
        }
    }
}

/// Prompts for the vault password and unlocks the shared session if it
/// isn't already — mirrors `modules/moku-secrets/src/cli_module.rs`'s
/// `ensure_unlocked` (itself mirroring `moku-bin/src/config_cmd.rs`'s
/// `ensure_unlocked_if_needed`). Kept as its own small copy rather than a
/// shared moku-core helper — same reasoning as the vault daemon's
/// duplicated `pid_is_alive`: these are independent modules, not worth
/// coupling for a ~15-line function.
async fn ensure_unlocked(ctx: &CliContext) -> Result<()> {
    let (Some(session), Some(security)) = (&ctx.session, &ctx.security) else {
        bail!("vault access unavailable (internal error: CliContext.session/security is empty).");
    };
    if session.is_unlocked() {
        return Ok(());
    }

    let password = rpassword::prompt_password("Moku vault password: ").map_err(|e| anyhow!("Failed to read password: {e}"))?;
    let result = if security.is_vault_initialized() { security.unlock_vault(password).await } else { security.initialize_vault(password).await };
    match result {
        Ok(key) => {
            session.unlock(key);
            Ok(())
        }
        Err(e) => Err(anyhow!("Vault unlock failed: {e}")),
    }
}
