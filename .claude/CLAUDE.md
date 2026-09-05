# Moku — project conventions

Rust workspace: a TUI app (`moku-bin`) built from independent per-feature
modules (`modules/moku-*`, plus top-level crates like `moku-vault-daemon`,
`moku-daemon`, `moku-lua`) around a shared `moku-core` (theme, config,
storage, security, module traits, keybinding resolution).

## Security / crypto conventions

- **Argon2id is the only password KDF.** Params are `Params::new(65536, 3, 4,
  Some(32))` (64 MiB, 3 iterations, 4-way parallelism, 32-byte output),
  `Algorithm::Argon2id`, `Version::V0x13` — see
  `moku-core/src/security/manager.rs`. Reuse these exact params for any new
  password-derived key; don't invent new ones per-feature.
- **Never use a raw master/password-derived key directly as a cipher key.**
  Derive independent subkeys via HKDF-SHA256 with a fixed, versioned
  domain-separation `info` string per purpose, e.g.
  `moku-vault-fs/content-key/v1` / `moku-vault-fs/name-key/v1`
  (`moku-vault-fs/src/keys.rs`), or per-volume via `moku-vault-daemon/src/
  registry.rs`'s `derive_default_volume_master_key`. One `Hkdf::new(None,
  master)` + one `hk.expand(info, &mut out)` call per subkey. Bump the `/v1`
  suffix (not the string itself) if a derivation ever needs to change
  incompatibly.
- **Every secret value in memory is `secrecy::SecretBox<T>` where `T:
  Zeroize`.** Never a plain `String`/`[u8; N]` for a password, key, or seed
  that outlives a single local computation.
- **Secrets are never CLI flags/arguments.** Not `--password`, not
  `--totp-seed`, nothing that would show up in `ps`/Task Manager/shell
  history. Always prompt interactively via `rpassword::prompt_password`
  (matches `modules/moku-secrets/src/cli_module.rs`'s `Add`/`Export`/
  `Import`), or — for a spawned worker process needing a secret handed to it
  by its parent (e.g. `moku-vault-daemon`'s mount worker) — pass it over
  stdin with a small tagged protocol, never argv or an environment variable.
- **Password-prompt UX rule**: verifying an existing password (login/mount)
  asks once; setting a *new* password (create/change) asks twice
  (password + confirmation), since a fresh password has nothing else to
  check a typo against. Don't invent a third pattern.
- **Shared delete-confirmation convention**: a destructive delete goes
  through `moku_core::keys::{is_delete_bypass, resolve_confirm_delete_key,
  ConfirmDeleteKey}` — plain `d`/`Delete` opens a confirm prompt
  (`ConfirmDeleteKey::Confirm`/`Cancel` from Enter/y vs. Esc/n), `Shift+D`
  (`is_delete_bypass`) deletes immediately, bypassing the prompt. Every
  module with a delete action (`moku-todo`, `moku-bookmark`,
  `moku-secrets`, `moku-rss`, `moku-vault-daemon`) uses this same pair of
  helpers rather than reimplementing the key matching.
- **Untrusted external input needs its own validation layer before it
  reaches an OS call.** `moku-core/src/util.rs`'s `open_url` validates the
  scheme is `http`/`https` (`url::Url::parse`) before ever invoking the
  Windows Shell API (`ShellExecuteW`, not a `cmd /C start` shell-out — the
  latter lets `cmd.exe`'s own re-parsing turn a feed-supplied URL containing
  shell metacharacters into a second command). Apply the same instinct to
  any other input that ends up passed to an OS API — parse/allowlist first.
- **Anything that becomes part of a URL is a leak surface.** An API key or
  token must never sit in a URL query string — `reqwest::Error`'s `Display`
  includes the failed request URL, so a transport failure wrapped in
  `anyhow` can print it straight to the terminal. Send credentials as a
  header (`x-goog-api-key`-style) instead; see `modules/moku-commit/src/
  engine.rs`.

## Theme / UI conventions

- `moku_core::MokuTheme` (`moku-core/src/theme.rs`) is the single palette
  every module draws from: `base_fg`/`base_bg`, `border`, `selection_fg`/
  `selection_bg`, `info`, `warning`, `error`, `success`. Never hardcode a
  `ratatui::style::Color` in a module — reference the matching theme field
  so a user's custom `ThemeColors` config actually takes effect everywhere.
- **Forms show every field at once; `Tab` moves focus; `Enter` submits from
  any field.** No sequential per-field wizard (ask name, then ask size,
  then ask password...). See `moku-vault-daemon`'s `CreateForm`/
  `PasswordPrompt` and `modules/moku-rss`'s `EditFeed` view for the pattern.
- Render tests use `ratatui::backend::TestBackend` + `Terminal::new` +
  `terminal.draw(...)`, then scan `terminal.backend().buffer().content` for
  expected substrings/positions. This is the standard way to assert on TUI
  output in this repo — don't invent a different rendering-test harness.
- Prefer plain ASCII glyphs for indicators drawn inline with text (collapse
  markers, checkboxes) over mixed-width Unicode/emoji — inconsistent
  terminal cell widths across fonts cause per-row misalignment (learned the
  hard way in the launcher and reapplied in `moku-todo`).

## File organization conventions

- **Split by variant/flow when a file mixes multiple independent
  flows/sub-views, each with its own state + draw + event handling.**
  Precedent: `modules/moku-settings/src/tabs/*.rs` (one file per tab).
  Applied later to `modules/moku-rss/src/tui_module/{view_split,
  view_edit_feed, view_detail}.rs` (a single `enum` with one match arm per
  variant — split as free functions taking the destructured state as
  params) and `moku-vault-daemon/src/tui_module/{mount_prompt,
  create_form}.rs` (independent `Option<T>` fields, not one enum — split as
  additional `impl VaultManagerModule { .. }` blocks in separate files, so
  moved methods keep `&mut self` unchanged; Rust allows a type's impl to
  span any number of files in the same crate).
  - A test that only touches a type's fully public API can stay wherever
    it was. A test that constructs/mutates a struct via field-literal
    syntax or direct field access must move into that type's own defining
    file (or the fields need `pub(super)`/`pub(crate)`) — a parent module
    can't reach a child module's private fields, even in `#[cfg(test)]`.
  - Keep as one file when it's a cohesive module of related free functions
    with no independent per-variant state (e.g. `moku-vault-daemon/src/
    registry.rs` — a god-struct split would just add file-jumping cost with
    no behavioral payoff).
- **`model.rs` pattern**: pure data types + their id-generation/tree
  helpers live in `model.rs`, separate from the TUI/CLI code that uses
  them. Established by `moku-bookmark`/`moku-http`/`moku-secrets`, applied
  to `moku-todo` (`Task`, `ViewRow`, `build_view`, `has_children`,
  `collect_subtree_ids`) and, for pure UI-math helpers with no data model,
  the same idea as a `viewport.rs`/`fuzzy.rs`-style sibling file
  (`modules/moku-launcher`).

## General code-quality rules

- Use `ctx.show_info` / `ctx.show_warning` / `ctx.show_error` consistently
  for user-visible toasts — don't print, log-only, or silently no-op a
  result the user needs to see.
- `let _ = fallible_call();` is only acceptable when the call is genuinely
  best-effort AND a comment says why (e.g. an unmount attempt before a
  delete, where failure just means "wasn't mounted, continue anyway").
  Otherwise propagate with `?` or match `Ok`/`Err` and call `ctx.show_error`
  — don't let a real failure look like unconditional success.
- Never `unwrap()`/`expect()` on fallible user- or filesystem-derived
  input. If an invariant genuinely can't fail, prefer an explicit
  `let Some(x) = ... else { ... }`/comment over a bare `unwrap()`, so the
  reasoning is visible and the failure mode (if the invariant ever breaks)
  is a graceful skip, not a panic.
- Prefer a single-in-flight-action guard (`busy: bool` or equivalent) for
  any module that spawns a background task from a keypress — otherwise key-
  mashing can spawn duplicate concurrent operations. Pattern used by
  `moku-rss` (`is_refreshing`), `moku-http` (`is_running`), and
  `moku-vault-daemon` (`busy`).
- Give any network call a bounded timeout and, where a module fetches from
  more than one source, fan them out concurrently (`futures::future::
  join_all`) rather than serially — a single slow/unresponsive source
  should never block the rest or the whole UI. See `modules/moku-rss/src/
  engine.rs`.

## Health-check pass — 2026-09-04/05

Full-repo review (security/crypto, correctness, performance, code
organization) requested and completed across these commits on `dev`:
`1d70c46` (open_url command-injection fix + defensive scheme allowlist,
Gemini key moved off the URL, secrets-as-CLI-flags removed, swallowed
`save_all`/`open_url` errors surfaced, RSS fetch timeout + concurrency,
task-status read added to the daemon dashboard), `5b13526` (RSS
`tui_module.rs` split into view_split/view_edit_feed/view_detail),
`b22a071` (vault-daemon `tui_module.rs` split into mount_prompt/
create_form), `e90b87e` (moku-todo model.rs, moku-launcher viewport.rs,
registry.rs unwrap guard). No feature was added or removed — every change
here is either a security fix, a real (small) behavior-bug fix, or a pure
refactor with an unchanged test count before/after. See this file's other
sections for the conventions that came out of that pass; don't redo this
same full-repo sweep without a specific new reason — check `git log`
for what's changed since.
