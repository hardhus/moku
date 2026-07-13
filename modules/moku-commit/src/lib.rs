use std::env;

use anyhow::{Context, Result};
use async_trait::async_trait;

pub mod engine;

use moku_core::{CliContext, CliModule, ModuleId, ModuleMeta};

pub use engine::{CommitEngine, CommitSettings};

pub struct CommitModule;

impl CommitModule {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CommitModule {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleMeta for CommitModule {
    fn id(&self) -> ModuleId {
        ModuleId::new("commit")
    }

    fn title(&self) -> &'static str {
        "AI Commit Generator"
    }
}

#[async_trait]
impl CliModule for CommitModule {
    async fn run(&self, _args: &[String], ctx: &CliContext) -> Result<()> {
        dotenvy::dotenv().ok();

        let api_key = env::var("GEMINI_API_KEY").context(
            "GEMINI_API_KEY not found. Check your .env file or system environment variables.",
        )?;

        if api_key.trim().is_empty() {
            return Err(anyhow::anyhow!("GEMINI_API_KEY cannot be empty."));
        }

        let settings: CommitSettings = ctx.config.resolve_module_config("commit");
        let engine = CommitEngine::new(settings.clone());

        println!("🔍 Scanning staged changes...");

        let diff = match engine.get_staged_diff()? {
            Some(d) => d,
            None => {
                println!("⚠️  No staged changes found. Please run 'git add' first.");
                return Ok(());
            }
        };

        println!(
            "🧠 Gemini is thinking... (Limit: {} chars)",
            settings.char_limit
        );

        let message = engine.generate_commit_message(&api_key, &diff).await?;

        println!("\n✅ Suggested Commit Message:\n");
        println!("{}", message);
        println!("\n--------------------------------");

        Ok(())
    }
}
