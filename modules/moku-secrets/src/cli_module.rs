use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use clap::{Parser, Subcommand};

use moku_core::{CliContext, CliModule, ModuleId, ModuleMeta};

use crate::engine::{self, PlainFormat};
use crate::generator::{self, CharsetOptions, DicewareOptions, Wordlist};
use crate::model::SecretEntry;

pub struct SecretsCliModule;

impl SecretsCliModule {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SecretsCliModule {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleMeta for SecretsCliModule {
    fn id(&self) -> ModuleId {
        ModuleId::SECRETS
    }
    fn title(&self) -> &'static str {
        ModuleId::SECRETS.title()
    }
}

#[derive(Parser, Debug)]
#[command(name = "secrets", no_binary_name = true)]
struct SecretsArgs {
    #[command(subcommand)]
    cmd: Option<SecretsCmd>,
}

#[derive(Subcommand, Debug)]
enum SecretsCmd {
    /// Generate a password or diceware passphrase (prints it, does not save it).
    Generate {
        #[arg(long, default_value_t = 20)]
        length: usize,
        #[arg(long)]
        diceware: bool,
        #[arg(long, default_value_t = 6)]
        words: usize,
        #[arg(long)]
        no_lowercase: bool,
        #[arg(long)]
        no_uppercase: bool,
        #[arg(long)]
        no_digits: bool,
        #[arg(long)]
        no_symbols: bool,
        #[arg(long)]
        no_number: bool,
    },
    /// Add a new secret entry (default when no subcommand is given).
    Add {
        name: String,
        /// Generate the value instead of prompting for it.
        #[arg(long)]
        generate: bool,
        #[arg(long)]
        category: Option<String>,
        #[arg(long)]
        username: Option<String>,
        #[arg(long)]
        url: Option<String>,
        /// Prompt for a TOTP shared secret to attach to this entry. Never
        /// a CLI flag value — a TOTP seed is a real second-factor secret,
        /// as sensitive as the entry's own value, and a flag would be
        /// visible in `ps`/Task Manager and shell history for as long as
        /// this process runs.
        #[arg(long)]
        totp: bool,
        #[arg(long)]
        notes: Option<String>,
    },
    /// List entries, optionally filtered by category.
    #[command(alias = "ls")]
    List {
        #[arg(long)]
        category: Option<String>,
    },
    /// Show one entry's details.
    Show {
        name: String,
        /// Print the actual value instead of masking it.
        #[arg(long)]
        reveal: bool,
    },
    /// Print the current TOTP code for an entry.
    Totp { name: String },
    /// Remove an entry.
    #[command(alias = "rm")]
    Remove { name: String },
    /// Export all entries to a file.
    Export {
        #[arg(long, value_enum, default_value = "encrypted")]
        format: ExportFormatArg,
        #[arg(long)]
        out: String,
    },
    /// Import entries from a previously exported file (merges by name).
    Import {
        file: String,
        #[arg(long, value_enum, default_value = "encrypted")]
        format: ImportFormatArg,
    },
}

#[derive(clap::ValueEnum, Clone, Debug)]
enum ExportFormatArg {
    Encrypted,
    Json,
    Csv,
}

#[derive(clap::ValueEnum, Clone, Debug)]
enum ImportFormatArg {
    Encrypted,
    Json,
}

#[async_trait]
impl CliModule for SecretsCliModule {
    async fn run(&self, args: &[String], ctx: &CliContext) -> Result<()> {
        let parsed = match SecretsArgs::try_parse_from(args) {
            Ok(p) => p,
            Err(e) if e.exit_code() == 0 => {
                print!("{e}");
                return Ok(());
            }
            Err(e) => bail!("{e}"),
        };

        let Some(storage) = &ctx.storage else {
            bail!(
                "secrets commands require storage access (internal error: CliContext.storage is empty)."
            );
        };

        // `generate` never touches storage — everything else does, and
        // entries are encrypted by default, so the vault needs to be
        // unlocked before any of them can read or write.
        if !matches!(parsed.cmd, Some(SecretsCmd::Generate { .. })) {
            ensure_unlocked(ctx).await?;
        }

        match parsed.cmd {
            None => bail!("no subcommand given — try `moku secrets --help`"),
            Some(SecretsCmd::Generate {
                length,
                diceware,
                words,
                no_lowercase,
                no_uppercase,
                no_digits,
                no_symbols,
                no_number,
            }) => {
                if diceware {
                    let opts = DicewareOptions {
                        word_count: words,
                        separator: "-".to_string(),
                        capitalize: false,
                        add_number: !no_number,
                        wordlist: Wordlist::EffLarge,
                    };
                    let phrase = generator::generate_diceware(&opts)?;
                    let bits =
                        generator::diceware_entropy_bits(words, Wordlist::EffLarge, !no_number);
                    println!("{phrase}");
                    println!(
                        "entropy: {:.1} bits ({})",
                        bits,
                        generator::strength_label(bits)
                    );
                } else {
                    let opts = CharsetOptions {
                        length,
                        lowercase: !no_lowercase,
                        uppercase: !no_uppercase,
                        digits: !no_digits,
                        symbols: !no_symbols,
                    };
                    let password = generator::generate_charset_password(&opts)?;
                    let bits = generator::charset_entropy_bits(&opts);
                    println!("{password}");
                    println!(
                        "entropy: {:.1} bits ({})",
                        bits,
                        generator::strength_label(bits)
                    );
                }
            }
            Some(SecretsCmd::Add {
                name,
                generate,
                category,
                username,
                url,
                totp,
                notes,
            }) => {
                let mut entries = engine::load_entries(storage).await;
                if engine::find_by_name(&entries, &name).is_some() {
                    bail!("an entry named '{name}' already exists");
                }
                let value = if generate {
                    let opts = CharsetOptions::default();
                    let pw = generator::generate_charset_password(&opts)?;
                    println!("Generated value: {pw}");
                    pw
                } else {
                    rpassword::prompt_password("Value: ")
                        .map_err(|e| anyhow!("Failed to read value: {e}"))?
                };
                let totp_seed = if totp {
                    Some(
                        rpassword::prompt_password("TOTP seed (base32): ")
                            .map_err(|e| anyhow!("Failed to read TOTP seed: {e}"))?,
                    )
                } else {
                    None
                };
                let mut entry = SecretEntry::new(name.clone(), value);
                entry.category = category;
                entry.username = username;
                entry.url = url;
                entry.totp_seed = totp_seed;
                entry.notes = notes;
                entries.push(entry);
                engine::save_entries(storage, &ctx.config, &entries).await?;
                println!("✅ Added '{name}'");
            }
            Some(SecretsCmd::List { category }) => {
                let entries = engine::load_entries(storage).await;
                let filtered: Vec<&SecretEntry> = entries
                    .iter()
                    .filter(|e| {
                        category
                            .as_deref()
                            .is_none_or(|c| e.category.as_deref() == Some(c))
                    })
                    .collect();
                if filtered.is_empty() {
                    println!("No secrets yet. To add one: moku secrets add <name>");
                } else {
                    for e in filtered {
                        println!(
                            "- {} [{}]",
                            e.name,
                            e.category.as_deref().unwrap_or("uncategorized")
                        );
                    }
                }
            }
            Some(SecretsCmd::Show { name, reveal }) => {
                let entries = engine::load_entries(storage).await;
                let entry = engine::find_by_name(&entries, &name)
                    .ok_or_else(|| anyhow!("no entry named '{name}'"))?;
                println!("Name:     {}", entry.name);
                println!("Category: {}", entry.category.as_deref().unwrap_or("-"));
                println!("Username: {}", entry.username.as_deref().unwrap_or("-"));
                println!("URL:      {}", entry.url.as_deref().unwrap_or("-"));
                println!(
                    "Value:    {}",
                    if reveal {
                        entry.value.to_string()
                    } else {
                        "•".repeat(entry.value.chars().count())
                    }
                );
                if let Some(notes) = &entry.notes {
                    println!("Notes:    {notes}");
                }
            }
            Some(SecretsCmd::Totp { name }) => {
                let entries = engine::load_entries(storage).await;
                let entry = engine::find_by_name(&entries, &name)
                    .ok_or_else(|| anyhow!("no entry named '{name}'"))?;
                let seed = entry
                    .totp_seed
                    .as_ref()
                    .ok_or_else(|| anyhow!("'{name}' has no TOTP seed configured"))?;
                println!("{}", engine::totp_code_now(seed)?);
            }
            Some(SecretsCmd::Remove { name }) => {
                let mut entries = engine::load_entries(storage).await;
                let before = entries.len();
                entries.retain(|e| !e.name.eq_ignore_ascii_case(&name));
                if entries.len() == before {
                    bail!("no entry named '{name}'");
                }
                engine::save_entries(storage, &ctx.config, &entries).await?;
                println!("🧹 Removed '{name}'");
            }
            Some(SecretsCmd::Export { format, out }) => {
                let entries = engine::load_entries(storage).await;
                let bytes = match format {
                    ExportFormatArg::Json => engine::export_plain(&entries, PlainFormat::Json)?,
                    ExportFormatArg::Csv => engine::export_plain(&entries, PlainFormat::Csv)?,
                    ExportFormatArg::Encrypted => {
                        let password = prompt_new_export_password()?;
                        engine::export_encrypted(&entries, &password).await?
                    }
                };
                std::fs::write(&out, bytes)?;
                println!("✅ Exported {} entries to {out}", entries.len());
            }
            Some(SecretsCmd::Import { file, format }) => {
                let data = std::fs::read(&file)?;
                let imported = match format {
                    ImportFormatArg::Json => engine::import_plain_json(&data)?,
                    ImportFormatArg::Encrypted => {
                        let password = rpassword::prompt_password("Export password: ")
                            .map_err(|e| anyhow!("Failed to read password: {e}"))?;
                        engine::import_encrypted(&data, &password).await?
                    }
                };
                let mut entries = engine::load_entries(storage).await;
                let mut added = 0usize;
                for entry in imported {
                    if engine::find_by_name(&entries, &entry.name).is_none() {
                        entries.push(entry);
                        added += 1;
                    }
                }
                engine::save_entries(storage, &ctx.config, &entries).await?;
                println!("✅ Imported {added} new entries (existing names were skipped).");
            }
        }
        Ok(())
    }
}

/// Prompts for the vault password and unlocks the shared session if it
/// isn't already — mirrors `moku-bin/src/config_cmd.rs`'s
/// `ensure_unlocked_if_needed`, generalized via `CliContext::session`/
/// `security` (added alongside this module) so any CLI module can do the
/// same, not just `moku config`.
async fn ensure_unlocked(ctx: &CliContext) -> Result<()> {
    let (Some(session), Some(security)) = (&ctx.session, &ctx.security) else {
        bail!("vault access unavailable (internal error: CliContext.session/security is empty).");
    };
    if session.is_unlocked() {
        return Ok(());
    }

    let password = zeroize::Zeroizing::new(
        rpassword::prompt_password("Moku vault password: ")
            .map_err(|e| anyhow!("Failed to read password: {e}"))?,
    );
    let result = if security.is_vault_initialized() {
        security.unlock_vault(password).await
    } else {
        security.initialize_vault(password).await
    };
    match result {
        Ok(key) => {
            session.unlock(key);
            Ok(())
        }
        Err(e) => Err(anyhow!("Vault unlock failed: {e}")),
    }
}

fn prompt_new_export_password() -> Result<String> {
    let p1 = rpassword::prompt_password("New export password: ")
        .map_err(|e| anyhow!("Failed to read password: {e}"))?;
    let p2 = rpassword::prompt_password("Confirm export password: ")
        .map_err(|e| anyhow!("Failed to read password: {e}"))?;
    if p1 != p2 {
        bail!("passwords did not match");
    }
    Ok(p1)
}
