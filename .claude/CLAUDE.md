# Moku — project conventions

Rust workspace: a TUI app (`moku-bin`) built from independent per-feature
modules (`modules/moku-*`, plus top-level crates like `moku-vault-daemon`,
`moku-daemon`, `moku-lua`) around a shared `moku-core` (theme, config,
storage, security, module traits, keybinding resolution).

## Versioning convention

- **`moku-bin`'s `Cargo.toml` version is the one git tags track.** Bump
  its patch number (`0.2.0` → `0.2.1` → `0.2.2` → ...) whenever a
  distinct, complete-enough change or feature lands — not on every
  commit, and not silently: flag that a bump looks due and ask before
  doing it.
- **A patch bump gets a plain commit, no git tag.** Only two things get
  a git tag (`vX.Y.0`): moving to a new minor/major version (e.g.
  `0.2.x` → `0.3.0`), or a hotfix release. `v0.2.0` (this file's
  "Health-check passes" section) is the precedent for the tagged case;
  patch bumps in between accumulate untagged on `dev`/`main`.
- Storage/crypto schema versioning (`KeyScheme`, below) is a completely
  separate axis from this — the app version and the on-disk schema
  version are allowed to move independently, on their own schedules.

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
  (`moku-vault-fs/src/keys.rs`), per-volume via `moku-vault-daemon/src/
  registry.rs`'s `derive_default_volume_master_key`, or per-module via
  `moku-core/src/storage/keys.rs`'s `derive_module_storage_key` (info
  string `moku-core/storage/<module_id>/v1` — see below). One
  `Hkdf::new(None, master)` + one `hk.expand(info, &mut out)` call per
  subkey. Bump the `/v1` suffix (not the string itself) if a derivation
  ever needs to change incompatibly.
- **`moku-core`'s general storage layer (`StorageManager::save`/`load`)
  uses per-module HKDF subkeys, not the raw vault master key directly.**
  Every module's encrypted data (todo/bookmark/secrets/rss/...) is
  encrypted under `derive_module_storage_key(master, module_id)`, never
  the raw master key — closing the same "raw key as cipher key" gap the
  bullet above already covered for `moku-vault-fs`/`moku-vault-daemon`.
- **`StorageEnvelope.key_scheme` is a numbered migration chain, not a
  one-off flag.** `KeyScheme` (`moku-core/src/storage/envelope.rs`) is
  `V0` (raw master key, `#[default]`/`#[serde(rename = "Legacy")]`) or
  `V1` (per-module HKDF subkey, `#[serde(rename = "PerModuleV1")]`,
  current) — the `serde(rename)`s pin the on-disk JSON tags to their
  original strings forever, so every already-written record (including
  already-migrated production data) keeps deserializing correctly no
  matter what the Rust-side variant names become. `KeyScheme::version()`
  and `CURRENT_KEY_SCHEME` give a plain `u16` ordinal instead of matching
  every past variant everywhere that cares. Adding a future `V2` means:
  (1) a new variant + its `version()` arm, (2) a `resolve_key_for_scheme`
  match arm in `storage::manager`, (3) bumping `CURRENT_KEY_SCHEME`, and
  (4) — only if `V2` changes the decrypted JSON shape, not just the key
  derivation — a `match` arm in `envelope::data_transform_for_hop`.
  `StorageManager::migrate_key_scheme_to_latest` (per module) /
  `migrate_all_key_schemes` (every `ENCRYPTABLE_MODULES` module id, only
  advances the marker below on full success) walk a record straight from
  whatever version it's on to `CURRENT_KEY_SCHEME` — a caller never
  needs to know or replay intermediate hops. This is a completely
  separate concern from `migrate_module_encryption`'s encrypted/
  plaintext config toggle — that method no longer touches `key_scheme`
  as a side effect; re-keying only ever happens through the two methods
  above.
- **The data directory self-describes its own key-scheme version,
  independent of any in-app config.** `StorageManager` writes a small
  `.key_scheme_version` marker file at the vault root (sibling to every
  module's own subdirectory) the moment a brand-new `vault_root` is
  created — a fresh install starts life already on `CURRENT_KEY_SCHEME`,
  nothing to ever migrate. `data_dir_key_scheme_version()` reads it (a
  missing file reads as `0`, exactly `KeyScheme::default()`'s meaning)
  and needs no unlock — it's a plain version number, not decrypted data
  — so a bare copy/backup of the data directory alone is always enough
  to know its version, with zero dependency on `config.toml` or which
  machine it's opened on. `moku-bin/src/main.rs` reads it at startup;
  `app_loop.rs`, the first time the vault is actually unlocked in that
  run, either applies `migrate_all_key_schemes` silently and reports the
  result via `ctx.show_info` (small gap, below
  `SCHEMA_UPGRADE_PROMPT_THRESHOLD`) or shows a small full-pane confirm
  overlay first (Enter/`y` upgrade now, Esc/`n` skip — asked again next
  launch, nothing persisted) for a larger gap. The manual "migrate to
  encrypted" action (Settings → Storage tab's `m`/`Shift+M`, or `moku
  config migrate`) is unrelated and still only toggles encrypted vs.
  plaintext — it never touches the key-scheme marker or version.
  **Any new module added to `ENCRYPTABLE_MODULES`
  (`modules/moku-settings/src/tabs/storage.rs`) must also be added to the
  identical list in `moku-bin/src/config_cmd.rs`** (now `pub(crate)`
  there so `app_loop.rs` can reuse the same list directly instead of a
  third hand-kept copy) — the two are meant to stay in sync (TUI vs. CLI
  equivalents of the same migration action).
- **Every secret value in memory is `secrecy::SecretBox<T>` where `T:
  Zeroize`, or `zeroize::Zeroizing<T>` for a plain owned value like a
  password `String` that isn't going through `SecretBox`'s access-control
  API.** Never a plain `String`/`[u8; N]` for a password, key, or seed
  that outlives a single local computation — this includes TUI input
  buffers accumulating a password/secret keystroke-by-keystroke
  (`moku-lock-screen`'s `input`, `moku-vault-daemon`'s `CreateForm`/
  `PasswordPrompt` fields, `moku-secrets`'s `AddState.value`/
  `ExportState.password`, `SecretEntry.value`) and byte buffers crossing a
  process boundary (`moku-vault-daemon/src/worker.rs`'s mount-secret stdin
  protocol). `SecurityManager::initialize_vault`/`unlock_vault` take
  `Zeroizing<String>`, not `String` — wrap a freshly-read
  `rpassword::prompt_password()` result in `Zeroizing::new(...)`
  immediately, don't let it sit as a bare `String` even briefly.
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
- **Anything that becomes part of a URL — or a value interpolated into
  one — is a leak surface.** An API key or token must never sit in a URL
  query string — `reqwest::Error`'s `Display` includes the failed request
  URL, so a transport failure wrapped in `anyhow` can print it straight to
  the terminal. Send credentials as a header (`x-goog-api-key`-style)
  instead; see `modules/moku-commit/src/engine.rs`. Where a request
  definition lets a user interpolate a resolved secret into *any* field
  (`modules/moku-http`'s `{{secrets.NAME}}`), also redact every resolved
  secret value out of error/log text before it's shown — see
  `modules/moku-http/src/engine.rs`'s `redact_secrets`.
- **Give every `reqwest::Client` a bounded `.timeout(...)`** — never
  `reqwest::Client::new()`/bare `reqwest::get` with no timeout. See
  `modules/moku-rss/src/engine.rs::build_client` for the pattern (also
  applied in `modules/moku-http`, `modules/moku-commit`).
- **A byte-offset string truncation must never be a raw `&s[..n]` slice.**
  Text of external origin (a git diff, a feed/task error string) isn't
  guaranteed ASCII, so an arbitrary byte offset can land mid-codepoint and
  panic. Always go through `moku_core::util::truncate_at_char_boundary`.

## Theme / UI conventions

- `moku_core::MokuTheme` (`moku-core/src/theme.rs`) is the single palette
  every module draws from: `base_fg`/`base_bg`, `border`, `selection_fg`/
  `selection_bg`, `info`, `warning`, `error`, `success`. Never hardcode a
  `ratatui::style::Color` in a module — reference the matching theme field
  so a user's custom `ThemeColors` config actually takes effect everywhere.
- **Forms show every field at once; `Tab` moves focus; `Enter` submits from
  any field.** No sequential per-field wizard (ask name, then ask size,
  then ask password...). See `moku-vault-daemon`'s `CreateForm`/
  `PasswordPrompt`, `modules/moku-rss`'s `EditFeed` view, and
  `modules/moku-secrets`'s `AddState`/`ExportState` for the pattern — the
  latter two used to be sequential per-field wizards (`AddStage`/
  `ExportStage`) and were converted to this shape; if you ever find a
  `*Stage`/`*Step` enum driving a form, that's the signal to convert it.
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
    no behavioral payoff; `moku-launcher/src/lib.rs` at ~1200 lines is the
    same story — one cohesive filter+browse state machine, not several
    independent flows, so it stays one file despite its size).
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
  result the user needs to see. This includes a plugin runtime surfacing
  its own errors to the user: `moku-lua`'s `on_event`/`on_init` call
  failures go through `ctx.show_error`, not a swallowed `unwrap_or`.
- `let _ = fallible_call();` is only acceptable when the call is genuinely
  best-effort AND a comment says why (e.g. an unmount attempt before a
  delete, where failure just means "wasn't mounted, continue anyway").
  Otherwise propagate with `?` or match `Ok`/`Err` and call `ctx.show_error`
  — don't let a real failure look like unconditional success. This
  includes a background process spawn (`cmd.spawn()`) — match on the
  `Result` rather than assuming a spawned process implies success.
- Never `unwrap()`/`expect()` on fallible user- or filesystem-derived
  input. If an invariant genuinely can't fail, prefer an explicit
  `let Some(x) = ... else { ... }`/comment over a bare `unwrap()`, so the
  reasoning is visible and the failure mode (if the invariant ever breaks)
  is a graceful skip, not a panic.
- Prefer a single-in-flight-action guard (`busy: bool` or equivalent) for
  any module that spawns a background task from a keypress — otherwise key-
  mashing can spawn duplicate concurrent operations. Pattern used by
  `moku-rss` (`is_refreshing`), `moku-http` (`is_running`),
  `moku-vault-daemon` (`busy`), and `modules/moku-settings`'s Storage tab
  (`busy`, guarding `migrate_module_encryption` — a real race, not just a
  wasted-work perf issue, since two concurrent migrations of the same
  module race on the same sled keys).
- Give any network call a bounded timeout (see the crypto section above)
  and, where a module fetches from more than one source, fan them out
  concurrently (`futures::future::join_all`) rather than serially — a
  single slow/unresponsive source should never block the rest or the whole
  UI. See `modules/moku-rss/src/engine.rs`'s feed fetch and
  `moku-core/src/module/registry.rs::collect_dashboard_summaries` (every
  visible module's Dashboard summary fetched concurrently, since
  `dashboard_summary` takes `&self` and is therefore provably independent
  across modules).
- **Offload genuinely slow/blocking work (a full filesystem walk, `sled`'s
  first `open` on a path, a large synchronous write) to
  `tokio::task::spawn_blocking`** rather than running it inline on an
  async fn called from the TUI event loop — a single-threaded TUI freezes
  for the whole duration otherwise. See `modules/moku-satz`'s vault index
  build, `moku-core/src/storage/manager.rs::get_or_open_db`.
- **A tree/list flattened for display from a flat `Vec<T>` + parent-id
  links should be built with a pre-indexed `parent -> children` map (one
  pass, O(n)), not by having the recursive walk rescan the whole slice at
  every level (O(n²)).** See `modules/moku-todo/src/model.rs::build_view`.

## Health-check passes

### 2026-09-04/05 (first pass)

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
refactor with an unchanged test count before/after.

### 2026-09-05 (second pass)

A deeper, from-scratch re-audit requested after the first pass, this time
covering modules the first pass touched less (moku-satz, moku-lock-screen,
moku-settings, moku-http, moku-vault-fs, moku-vault-mount, moku-lua,
moku-commit) via three parallel security/performance/quality review
agents. Two larger architectural changes were made with explicit user
sign-off (both described in the crypto section above): (1) `moku-core`'s
storage layer moved from encrypting every module directly with the raw
vault master key to per-module HKDF subkeys, with a transparent
backward-compatible read path (`KeyScheme::Legacy`) so no existing user's
data needed any migration to keep working, and an existing UI action
(Settings → Storage `m`/`Shift+M`) transparently doubling as the explicit
upgrade trigger; (2) `modules/moku-secrets`'s `SecretEntry.value` moved
from a plain `String` to `Zeroizing<String>` (with the module's
sequential Add/Export forms converted to the standard all-fields-at-once
shape while those files were already being touched). Everything else was
a smaller, self-contained fix: an unchecked-overflow write path in
`moku-vault-fs`, a mount left stuck on `moku-vault-mount`'s WinFsp start
failure, missing `reqwest` timeouts in `moku-http`/`moku-commit`, a
secret-redaction gap in `moku-http`'s error text, un-zeroized stdin
buffers in the vault mount-worker protocol, an O(n²) tree-flatten in
`moku-todo`, a blocking vault walk in `moku-satz`, a serial Dashboard
summary fetch, a missing busy-guard on the Settings storage migration
action (a real correctness race, not just perf), two byte-slice
char-boundary panics (`moku-commit`'s diff truncation, `moku-daemon`'s
error truncation — now sharing `moku_core::util::truncate_at_char_boundary`),
a couple of hardcoded theme colors in `moku-satz`'s graph view, a
swallowed Lua runtime error in `on_event`, a silent quota-accounting
drift in `moku-vault-fs`, and a misleading "started" toast on a daemon
spawn failure. Test coverage was also added for three previously-untested
modules: `moku-lock-screen` (had zero tests), `moku-satz`'s pure
list/filter logic, and `moku-lua`'s bridge-draining/event-dispatch logic.

**Deliberately NOT changed this pass** (evaluated, judged out of scope or
already-accepted):
- `moku-vault-daemon/src/control.rs`'s control-channel named pipe uses the
  OS default DACL (same class as the vault-mount pipe's already-accepted
  limitation) — local-only, same-user DoS risk (a forced unmount), no
  confidentiality impact. Fixing it needs a real Windows security-
  descriptor (not just a doc comment), which is a meaningfully separate
  chunk of work from everything else in this pass.
- `sled`'s long-standing lack of active maintenance — an informational,
  forward-looking note, not something to react to by swapping storage
  backends in a pass whose explicit goal was "same behavior, safer/
  cleaner code."
- `VolumeSecret::Password`/`MountSecret::Password` (moku-vault-daemon,
  worker.rs) still carry a plain `String`, not `Zeroizing<String>` — the
  same class of gap the crypto section's TUI-input-buffer fix addresses,
  but scoped out here since it cascades through the whole per-volume-vault
  subsystem (registry.rs, worker.rs, several CLI commands) rather than
  being a contained, single-file change like the others. Worth doing in a
  dedicated pass.
- `modules/moku-secrets`'s CLI `Show`/export code paths and
  `delete_volume`'s lack of secure-overwrite remain as previously
  documented/accepted.

Don't redo this same full-repo sweep without a specific new reason —
check `git log` for what's changed since.

### 2026-09-06 — v0.2.0 tagged, key-scheme migration chain built

The second pass's state (per-module HKDF storage keys, the `moku-secrets`
Zeroizing refactor, everything above) was locked in as `moku-bin` `0.2.0`
and tagged `v0.2.0` on `main` — the first git tag under the versioning
convention at the top of this file, and the precedent for what "tag-
worthy" means (a real release point, not every patch bump).

Immediately after, `KeyScheme::Legacy`/`PerModuleV1` (the ad hoc,
one-off migration built for the second pass) was generalized into the
permanent `V0`/`V1`/`CURRENT_KEY_SCHEME` chain and `.key_scheme_version`
data-directory marker described in the crypto section above — the
explicit goal being that any future key-scheme or storage-format change
only ever needs one new variant plus one migration hop, never a bespoke
one-time mechanism again. `migrate_module_encryption`'s old
`stale_key_scheme` side effect (upgrading key scheme as a side effect of
the unrelated encrypted/plaintext toggle) was removed in favor of the
dedicated `migrate_key_scheme_to_latest`/`migrate_all_key_schemes`. This
work stayed on `dev`, untagged, per the versioning convention above —
`moku-bin` will get a `0.2.1` patch bump for it once flagged and
confirmed.
