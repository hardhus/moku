pub mod engine;

use anyhow::{Context as AnyhowContext, Result};
use arboard::Clipboard;
use async_trait::async_trait;
use colored::*;
use moku_core::{CliContext, CliModule, ModuleId, ModuleMeta};
use std::fs;
use std::path::Path;
use std::time::Instant;

pub use engine::{ContextEngine, ContextSettings};

pub struct ContextModule;

impl ContextModule {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ContextModule {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleMeta for ContextModule {
    fn id(&self) -> ModuleId {
        ModuleId::CONTEXT
    }

    fn title(&self) -> &'static str {
        "Context Compiler"
    }
}

#[async_trait]
impl CliModule for ContextModule {
    async fn run(&self, args: &[String], ctx: &CliContext) -> Result<()> {
        let start_time = Instant::now();
        
        let root_path = args.get(0).cloned().unwrap_or_else(|| ".".to_string());
        let out_file = args.get(1).cloned();

        let root = Path::new(&root_path);

        println!(
            "{} {}",
            "🔍 Scanning:".cyan().bold(),
            root.canonicalize()?.display()
        );

        let settings: ContextSettings = ctx.config.resolve_module_config("context");

        let engine = ContextEngine::new(settings);
        let files = engine.scan_files(root);

        let (final_output, file_count) = engine.build_output(root, &files)?;

        if final_output.is_empty() {
            println!("{}", "⚠️  No matching files found.".yellow());
            return Ok(());
        }

        if let Some(out_path) = out_file {
            fs::write(&out_path, &final_output).context("Failed to write output file")?;
            println!(
                "\n{}",
                format!("✅ SUCCESS! Written to: {}", out_path)
                    .green()
                    .bold()
            );
        } else {
            let mut clipboard = Clipboard::new().context("Clipboard access failed")?;
            clipboard
                .set_text(final_output.clone())
                .context("Clipboard write failed")?;
            println!("\n{}", "✅ SUCCESS! Copied to clipboard!".green().bold());
        }

        println!(
            "{} {} chars, {} files ({:.2?})",
            "📊 Stats:".yellow(),
            final_output.len(),
            file_count,
            start_time.elapsed()
        );

        Ok(())
    }
}
