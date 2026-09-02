use anyhow::{Result, bail};
use async_trait::async_trait;
use clap::{Parser, Subcommand};

use moku_core::{CliContext, CliModule, ModuleId, ModuleMeta};
use satz_core::VaultGraph;

use crate::engine::{NotesConfig, build_index, ensure_daily_note, resolve_vault_root};

pub struct NotesCliModule;

impl NotesCliModule {
    pub fn new() -> Self {
        Self
    }
}

impl Default for NotesCliModule {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleMeta for NotesCliModule {
    fn id(&self) -> ModuleId {
        ModuleId::NOTES
    }
    fn title(&self) -> &'static str {
        ModuleId::NOTES.title()
    }
    fn encrypt_by_default(&self) -> bool {
        // Reads/writes plain markdown files directly on disk — never goes
        // through moku's own StorageManager, so this has no bearing on it.
        false
    }
}

#[derive(Parser, Debug)]
#[command(name = "notes", no_binary_name = true)]
struct NotesArgs {
    #[command(subcommand)]
    cmd: Option<NotesCmd>,
}

#[derive(Subcommand, Debug)]
enum NotesCmd {
    /// Walk and index the vault; report how many documents were parsed.
    Index { path: Option<String> },
    /// Show summary statistics for the vault (default when no subcommand is given).
    Stats { path: Option<String> },
    /// List documents, optionally filtered.
    List {
        #[arg(long)]
        tag: Option<String>,
        #[arg(long)]
        orphans: bool,
        #[arg(long)]
        broken: bool,
        path: Option<String>,
    },
    /// Resolve a link target (path, stem, title, or alias) to its document id.
    Resolve { target: String, path: Option<String> },
    /// Create (if missing) and print the path of today's daily note.
    Daily { path: Option<String> },
    /// Format markdown files with satz's deterministic formatter.
    Fmt {
        /// Write changes to disk. Without this, only reports which files would change.
        #[arg(long)]
        write: bool,
        path: Option<String>,
    },
    /// Export the note-link graph.
    Graph {
        /// Output format: "dot" (Graphviz) or "json".
        #[arg(short, long, default_value = "dot")]
        format: String,
        /// Write to a file instead of stdout.
        #[arg(short, long)]
        out: Option<String>,
        path: Option<String>,
    },
}

#[async_trait]
impl CliModule for NotesCliModule {
    async fn run(&self, args: &[String], ctx: &CliContext) -> Result<()> {
        let parsed = match NotesArgs::try_parse_from(args) {
            Ok(p) => p,
            Err(e) if e.exit_code() == 0 => {
                print!("{e}");
                return Ok(());
            }
            Err(e) => bail!("{e}"),
        };

        let config: NotesConfig = ctx.config.resolve_module_config("notes");

        match parsed.cmd.unwrap_or(NotesCmd::Stats { path: None }) {
            NotesCmd::Index { path } => {
                let root = resolve_vault_root(&config, path.as_deref())?;
                let index = build_index(&root)?;
                println!("Indexed {} document(s) under {}", index.doc_count(), root.display());
            }
            NotesCmd::Stats { path } => {
                let root = resolve_vault_root(&config, path.as_deref())?;
                let stats = build_index(&root)?.stats();
                println!("Documents:    {}", stats.doc_count);
                println!("Links:        {}", stats.total_links);
                println!("Broken links: {}", stats.broken_links);
                println!("Unique tags:  {}", stats.unique_tags);
                println!("Orphan docs:  {}", stats.orphan_docs);
                println!("Headings:     {}", stats.total_headings);
                println!("Words:        {}", stats.total_words);
            }
            NotesCmd::List { tag, orphans, broken, path } => {
                let root = resolve_vault_root(&config, path.as_deref())?;
                let index = build_index(&root)?;
                if let Some(tag) = tag {
                    for doc in index.docs_with_tag(&tag) {
                        println!("{}", doc.path.display());
                    }
                } else if orphans {
                    for doc in index.orphan_docs() {
                        println!("{}", doc.path.display());
                    }
                } else if broken {
                    for (doc, links) in index.docs_with_broken_links() {
                        for (link, _resolution) in links {
                            println!("{}: broken link -> '{}'", doc.path.display(), link.target_doc);
                        }
                    }
                } else {
                    for doc in index.documents() {
                        println!("{}", doc.path.display());
                    }
                }
            }
            NotesCmd::Resolve { target, path } => {
                let root = resolve_vault_root(&config, path.as_deref())?;
                let index = build_index(&root)?;
                match index.resolve_link(&target) {
                    Some(id) => println!("{}", id.as_str()),
                    None => bail!("could not resolve '{target}'"),
                }
            }
            NotesCmd::Daily { path } => {
                let root = resolve_vault_root(&config, path.as_deref())?;
                let (note_path, created) = ensure_daily_note(&root)?;
                if created {
                    println!("✅ Created {}", note_path.display());
                } else {
                    println!("{}", note_path.display());
                }
            }
            NotesCmd::Fmt { write, path } => {
                let root = resolve_vault_root(&config, path.as_deref())?;
                let docs = satz_core::walk_vault(&root)?;
                let fmt_config = satz_core::config::FormatterConfig::default();
                let mut changed = 0usize;
                for doc in &docs {
                    let full_path = root.join(&doc.path);
                    let source = std::fs::read_to_string(&full_path)?;
                    let formatted = satz_core::formatter::format_document(&source, &fmt_config);
                    if formatted != source {
                        changed += 1;
                        if write {
                            std::fs::write(&full_path, &formatted)?;
                        } else {
                            println!("would reformat: {}", full_path.display());
                        }
                    }
                }
                if write {
                    println!("✅ Reformatted {changed} file(s).");
                } else {
                    println!("{changed} file(s) would be reformatted (pass --write to apply).");
                }
            }
            NotesCmd::Graph { format, out, path } => {
                let root = resolve_vault_root(&config, path.as_deref())?;
                let index = build_index(&root)?;
                let graph = VaultGraph::build(&index);
                let output = match format.as_str() {
                    "json" => graph.export_json()?,
                    _ => graph.export_dot(),
                };
                match out {
                    Some(file) => {
                        std::fs::write(&file, &output)?;
                        println!("✅ Wrote graph ({} nodes, {} edges) to {file}", graph.node_count(), graph.edge_count());
                    }
                    None => println!("{output}"),
                }
            }
        }
        Ok(())
    }
}
