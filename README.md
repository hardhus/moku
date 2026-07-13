# Moku

Moku is a secure, modular, Terminal User Interface (TUI) and CLI productivity suite written in Rust. It combines local encrypted storage, Vim-like keybindings, and AI-assisted developer utilities into a single, cohesive terminal workspace.

## Features

* **Modular Architecture:** Built as a Cargo workspace, separating the core engine, main binary, and independent feature modules.
* **Encrypted Vault:** Uses Argon2 for key derivation and AES-256-GCM for encryption. Your sensitive data (like bookmarks) is stored securely on disk using Sled as an embedded database.
* **Vim-Inspired Keybindings:** Navigate effortlessly using standard `j/k` motions, fuzzy search (`/`), and customizable shortcuts.
* **Dynamic Theming:** Built-in themes (System, Hacker, Light, Pastel) with real-time switching.
* **AI Developer Tools:** Includes CLI modules specifically designed to help developers interact with Large Language Models (LLMs).

---

## Project Structure & Modules

Moku operates both as a full-screen TUI application and a set of command-line utilities.

| Module | Interface | Description |
| --- | --- | --- |
| **Moku Launcher** | TUI | The central hub and lock screen to authenticate and access all other modules. |
| **Dashboard** | TUI | Provides a quick overview of system metrics (RAM/Swap usage) and task summaries. |
| **Todo** | TUI | A persistent, interactive task manager. |
| **Bookmark** | TUI | A secure, encrypted bookmark manager featuring fuzzy search, domain filtering, and clipboard integration. |
| **Settings** | TUI | Real-time configuration manager for themes, cursors, and module-specific options. |
| **Context** | CLI | Scans your codebase (respecting `.gitignore`) and compiles files into a single string to feed to LLMs. |
| **Commit** | CLI | Uses the Gemini API to analyze your staged Git diff and automatically generate a Conventional Commit message. |

---

## Installation & Setup

1. Ensure you have Rust and Cargo installed on your system.
2. Clone the repository to your local machine.
3. If you plan to use the `moku-commit` module, create a `.env` file in your working directory and add your Gemini API key: `GEMINI_API_KEY=your_api_key_here`
4. Build the project in release mode for optimal performance:
`cargo build --release`
5. The executable will be located in `target/release/moku`.

---

## Usage Guide

### TUI Mode

To launch the main Terminal User Interface, simply run the binary without any arguments:

`moku`

Upon first launch, you will be prompted to create a password for your encrypted vault. Subsequent launches will require this password to decrypt your secure modules (like Bookmarks).

**Global TUI Keybindings:**

* **Arrows / `j` / `k`:** Navigate lists
* **`Enter`:** Select / Confirm
* **`Esc`:** Go back / Return to Launcher
* **`q`:** Quit application
* **`/`:** Search (in supported modules)

### CLI Mode

You can bypass the TUI to use Moku's developer utilities directly from your terminal.

**Generate an AI Context from your codebase:**
Scans the current directory, ignoring unnecessary files, and copies the formatted codebase context to your clipboard.
`moku context`
`moku context --path /path/to/project --output context.txt`

**Generate a Git Commit Message:**
Analyzes your currently staged changes (`git add`) and proposes a professional commit message.
`moku commit`

---

## Configuration

Moku dynamically generates a `config.toml` file upon its first run. This file is located in your operating system's standard configuration directory (e.g., `~/.config/moku/config.toml` on Linux).

You can edit this file directly or use the built-in **Settings** module in the TUI to customize themes, keybindings, and module-specific limits (like the maximum character limit for the commit generator).
