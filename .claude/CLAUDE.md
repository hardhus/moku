# Moku — project conventions

Rust workspace: a TUI app (`moku-bin`) built from independent per-feature
modules (`modules/moku-*`, plus top-level crates like `moku-volume-daemon`,
`moku-daemon`, `moku-lua`) on top of a small stack of layered library
crates (see "Crate layers" below), with `moku-core` holding only the
UI/module layer (module traits, contexts, registry, router, keybinding
resolution, theme, toasts, widgets).

## Crate layers (since P2)

Lower crates never depend on higher ones, and nothing below `moku-core`
pulls in ratatui, crossterm or sled (check with `cargo tree -p <crate>
-e normal`):

| Crate | Owns | Depends on |
|---|---|---|
| `moku-sys` | `dirs`, `util::{atomic_write(_async), truncate_at_char_boundary, open_url, error_chain}` | — |
| `moku-crypto` | `Key<P>` + roles, `SecretString`, `KeyCell`, `kdf` (`password_key`, `KdfParams`), `purpose` (HKDF), `seal`/`open` + `Aad`/`Alg`, `format_v1`, `BlockAead`/`NameSiv`, `keyslot`, `random`, `CryptoError` (since P3) | — (**no file IO, no tokio, no serde**, by design) |
| `moku-vault` | `SecurityManager` (meta.json, init/unlock, async `derive_key`), `VaultSession` (`KeyCell<RootKey>`), `cli::{ensure_unlocked, read_password, prompt_new_password}`, `VaultError` | crypto, sys |
| `moku-config` | `MokuConfig` + sections, `ThemeColors`, `ConfigHandle`, `ConfigIssue`/`ConfigError`, `resolve_encryption` | sys |
| `moku-store` | `StorageManager`, envelope/`KeyScheme` tag, `StoreError`, `MigrationReport` (reads only the current data format) | crypto, vault, sys |
| `moku-migrate` | `FORMAT` marker, `detect_format`, hop registry `HOPS`, `upgrade` + backups, `volume` chain; every reader of an old on-disk format (since P4) | crypto, vault, sys (**not** store: hops carry frozen copies of old types) |
| `moku-core` | module traits, `AppContext`/`CliContext`/`DaemonContext`, registry, router, keys, `MokuTheme`, toasts, widgets | config, vault, store, sys |

`moku-vault` is the app vault crate (new in P2). It is unrelated to the
pre-rename `moku-vault-*` crates that the historical notes below
mention, which are today's `moku-volume-*`.

Import items from the crate that owns them (`moku_vault::SecurityManager`,
`moku_store::StorageManager`, `moku_config::MokuConfig`, ...); `moku-core`
re-exports none of them. Crypto primitives go in `moku-crypto`, and only
there (see "Security / crypto conventions"); anything that touches key
material on disk or needs a runtime goes in `moku-vault`.

## Errors (since P2)

- **Library crates return typed `thiserror` enums**: `CryptoError`,
  `VaultError`, `StoreError`, `ConfigError`/`ConfigIssue`,
  `VolumeFsError`, `MountError`, `VolumeError`, plus `moku-sys`'s
  `DirsError`/`AtomicWriteError`/`OpenUrlError`. One variant per case a
  caller handles differently (`VaultError::WrongPassword` vs.
  `MetadataCorrupt`, `StoreError::Tampered` vs. `SchemaMismatch`, ...);
  never an `Other(anyhow)` catch-all in a library enum (moku-volume-
  daemon's `VolumeError::Other` is the one exception: registry plumbing
  nothing branches on). `moku-bin`, `modules/*` and the module traits
  stay on `anyhow`; `match` the typed error before `?` when the caller
  needs to tell cases apart.
- **`{e:#}` only shows the cause chain for an `anyhow::Error`.** On a
  typed error it prints the top line and drops the causes. To show a
  typed error to the user, either convert it first
  (`anyhow::Error::from(e)` / `?` into an anyhow context) or format it
  with `moku_sys::util::error_chain(&e)`.
- **Errors crossing a process boundary are a code plus a message**, never
  a parsed stderr line: the mount worker prints `MOKU_MOUNT_ERR
  <code>\t<message>` (`moku-volume-daemon`'s `MountErrorCode`), the
  parent turns it into `MountOutcome::Failed { code, message }`. Codes
  never change meaning, and an unknown code parses as `Unknown`. P8/P9
  move the same set into the agent IPC.

## Config (since P2)

- **`ConfigHandle` (moku-config) is the only owner of config.toml.**
  Never read/write the file directly or keep a private copy.
  Modules change config only through `ctx.update_config(|cfg| ..)` or
  `ctx.update_module_config::<T>(id, |t| ..)`; both save immediately
  (no Ctrl+S, no debounce), atomically, and only the keys that changed,
  so comments, ordering and unknown keys survive. They return whether it
  was saved and show the error toast themselves.
- **Read a module's settings with `ctx.module_config::<T>(id)`**
  (or `ConfigHandle::module_config`/`module_keys`): on a wrong-typed
  table it returns `T`'s defaults *and* reports a `ConfigIssue`, which the
  TUI shows as a warning (once per session; `app_loop` drains
  `take_issues`), the CLI prints to stderr, the daemon logs.
  `MokuConfig::resolve_module_config` itself returns `Result`; a pure
  render path that can't report may `unwrap_or_default()` with a comment
  pointing at the event path that does.
- **Every module config struct has `#[serde(default)]`.** A missing field
  means its default; only a wrong-typed field is an error.
- **A broken config is never overwritten.** An unparseable/unreadable
  file (or one from a newer `config_version`) runs read-only on defaults
  with a warning and a `config.toml.broken-<ts>` copy. A section or module
  table that doesn't parse falls back to defaults and is write-locked
  (`ConfigError::SectionInvalid`), so a default never replaces what the
  user wrote. `update_module` refuses to write a table that doesn't
  currently parse as `T`.
- **`resolve_encryption` fails closed**: it returns `Result<bool,
  ConfigIssue>`. A save path propagates the error (the write is refused);
  a gate that only decides "does this need the vault" treats it as
  encrypted (`unwrap_or(true)`). Never fall back to plaintext.
- Use `ModuleId::X.as_str()` for module ids, never a string literal.
- **config.toml has its own format chain** (`moku-config/src/migrate.rs`,
  since P4): `ConfigHop`s on the raw `toml_edit` document, counter
  `config_version` (base 1 = a file without it). `ConfigHandle::open`
  runs pending hops with a `config.toml.pre-v<N>-<ts>` copy first; a
  failed hop writes nothing and opens read-only. The chain is empty
  today; a config shape change adds a hop and bumps
  `CURRENT_CONFIG_VERSION`.

## Data format and migration (since P4)

- **The data directory has one format number, and the runtime only knows
  the latest one.** `<data_dir>/FORMAT` (TOML, next to `vault/`):
  `data_format` decides, `written_by` / `components` are informational.
  It's read without unlocking (`moku_migrate::detect_format`) and only
  ever written with `atomic_write`. Without `FORMAT`, a `vault/` is a
  pre-P4 directory whose format is inferred from the old
  `vault/.key_scheme_version` (0 or 1; missing/garbage = 0); only
  moku-migrate reads that file.
- **All code that reads an old on-disk format lives in moku-migrate.**
  Runtime crates read the current format and nothing else: `moku-store`
  returns `StoreError::NeedsUpgrade` for an encrypted `KeyScheme::V0`
  record instead of decrypting it. No dual read paths, no background
  conversion; the only cost to an up-to-date user is reading `FORMAT`.
- **A format change = one hop.** A new file in `moku-migrate/src/hops/`,
  appended to `HOPS`, `CURRENT_DATA_FORMAT` bumped, its components in
  `format::components_of`, and a test against a copy of
  `test-fixtures/` (plus synthetic states the fixtures lack). Never a
  one-off migration mechanism. A hop:
  - is **frozen** once shipped (a later bug gets a new hop, never an
    edit);
  - declares **its own copies** of the old types it reads (see
    `h0000_key_scheme_v1.rs`'s `Envelope`), never a runtime type;
  - is **crash safe and idempotent**: builds beside the old data and
    swaps atomically (e.g. External blob first via `atomic_write`, then
    the envelope; a retry recognises a half-done record).
- **`moku_migrate::upgrade`** re-detects the format, refuses a newer or
  unreadable one, refuses `Locked` if a hop `needs_unlock`, refuses
  `InUse` if any module DB's sled lock is held (probed with
  `File::try_lock`, not `sled::open`, which would rewrite files before the
  backup), backs up `vault/` (+ `FORMAT`) to
  `backups/data-v<from>-<unix>/` (via a `.partial-` name; never deleted
  automatically), then runs each hop and writes `FORMAT` right after it,
  so a failure leaves the marker at that hop's `from`.
- **What the format gates:**
  - `main.rs` checks it before `StorageManager::new` (which creates
    `vault/`): newer or unreadable `FORMAT` exits with the error (fail
    closed, KN-4.5); fresh or current gets `FORMAT` stamped; behind is
    handed to the TUI.
  - TUI (`moku-bin/src/data_upgrade.rs`): after the first unlock a
    full-pane prompt (steps, backup location, daemon note) always asks;
    there is no silent path. Confirm stops the daemon
    (`stop_daemon_and_wait`), `close_all_dbs()`, runs `upgrade` on
    `spawn_blocking` with a progress screen (keys and auto-lock held off).
    "Not now" persists nothing: every vault-gated module opens the prompt
    instead of itself (`navigate` in `app_loop.rs`), the launcher and lock
    screen never are, and the next launch asks again.
  - CLI commands run on a behind directory with a stderr notice; old
    records surface as `NeedsUpgrade`.
  - The daemon runs only on a current (or fresh) format
    (`moku_daemon::worker::check_data_format`).
- **`moku data status | upgrade | backups`** is the data-format upgrade.
  It is unrelated to `moku config migrate` / Settings → Storage
  `m`/`Shift+M`, which only switch a module between encrypted and
  plaintext (a user preference, not a format). Keep the two apart in
  code and `--help`.
- **`KeyScheme::V0` is a tag, not a migration step.** Every plaintext
  record carries `"Legacy"`, so the variant stays; only its decryption is
  gone from the runtime. The `serde(rename)`s pin `"Legacy"` /
  `"PerModuleV1"` forever.
- **Volumes have their own counter**, `volume.json`'s `format` (missing =
  0, not written when 0), upgraded lazily per volume by the mount worker
  right after `VolumeLock::acquire` and before any key is derived
  (`moku_migrate::volume`). Never in the global upgrade: volumes can be
  on unplugged drives, Custom ones have their own password. A newer
  volume format is `VolumeError::NewerFormat` / wire code `newer_format`.
- Tests build contexts with `ConfigHandle::detached(..)`, which never
  touches disk. Before P2, pomodoro tests wrote the real moku_dev
  config.toml.

## Versioning convention

- **Branches:** work happens on `dev`, which may be temporarily broken
  mid-phase. `main` must never be broken: build and `cargo test
  --workspace` are green at every merge. `main` is updated by
  fast-forwarding it to `dev`, and only with the user's go-ahead. Nothing
  is pushed unless asked.
- **Every `dev` → `main` merge bumps `moku-bin`'s patch version by one**
  (`0.2.8` → `0.2.9` → ...), as its own `chore: bump moku-bin to X`
  commit right before the merge. That is the only version rule: no git
  tags are created (the user decided this on 2026-09-28; the older
  `v0.x.y` tags are history). A move to a new minor version happens
  only when the user asks for it.
- Storage/crypto schema versioning (`KeyScheme`, below) is a completely
  separate axis from this — the app version and the on-disk schema
  version are allowed to move independently, on their own schedules.

## Security / crypto conventions

- **Only `moku-crypto` touches cryptographic primitives (since P3).** No
  other crate may depend on `aes`, `aes-gcm`, `aes-siv`, `chacha20`,
  `chacha20poly1305`, `poly1305`, `ghash`, `polyval`, `hkdf` or `argon2`,
  or name `aes_gcm::`/`aes_siv::`/`chacha20poly1305::`/`hkdf::`/`argon2::`;
  `moku-crypto/tests/crypto_crate_ban.rs` enforces it. Need something
  new? Add it to `moku-crypto` with tests. This keeps the rules below
  enforced by types and a dependency upgrade (P10) to one crate.
- **Keys are `moku_crypto::Key<P, N = 32>`**, typed by role (`RootKey`,
  `PasswordKey`, `Kek`, `ModuleStorageKey`, `VolumeMasterKey`,
  `VolumeContentKey`, `VolumeNameKey` (N = 64), `ExportKey`,
  `RecoverySecret`). Heap-boxed, wiped on drop, no `Clone`
  (`duplicate()` is the visible copy), redacted `Debug`. `from_slice` only
  for key bytes from a trusted channel (worker stdin, test vectors);
  `expose()` only where the bytes must be handed on. Every `Key`
  allocation goes through `alloc_key_box` in `key.rs` (P9 moves it to
  locked memory).
- **Argon2id is the only password KDF.** `moku_crypto::password_key(&SecretString,
  salt, KdfParams)`, Argon2id v0x13, `kdf::DEFAULT_KDF` = 64 MiB, t = 3,
  p = 4, 32-byte output (`moku-crypto/src/kdf.rs`). Reuse these params for
  any new password-derived key; don't invent new ones per feature. New
  formats record their `KdfParams` (MSXP v2, P5 keyslots) so they can be
  raised later; formats that don't (meta.json v1, MSXP v1) imply
  `DEFAULT_KDF`. It is synchronous: async callers use `spawn_blocking`.
- **Never use a raw master/password-derived key directly as a cipher key.**
  Every HKDF derivation lives in `moku-crypto/src/purpose.rs`: one
  `Purpose` variant per info string, and a **named function** per
  derivation with role-typed input and output (`module_storage_key`,
  `default_volume_master`, `volume_keys`, `secrets_export_key`; keyslot
  KEKs in `keyslot.rs`). The generic `derive` is crate-private on purpose
  (audit KN-3.2): a new derivation = a new variant + a named function + a
  KAT, never an ad-hoc HKDF call. New info strings follow
  `moku/<area>/<purpose>/v<N>` + length-prefixed (u16 BE) context fields.
  Bump `v<N>` (never edit a string) if a derivation must change.
  **The four pre-P3 strings are permanent, byte-for-byte**, odd shapes
  included: `moku-core/storage/<module_id>/v1`,
  `moku-vault-daemon/default-volume/v1/<id>`,
  `moku-vault-fs/content-key/v1`, `moku-vault-fs/name-key/v1` (three say
  "vault" because they predate the vault -> volume rename). Pinned by
  known-answer tests: `moku-crypto/tests/{crypto_kat, purpose_kat}.rs`
  (K1, K2, K7a, every info string, the new derivations),
  `moku-volume-fs/tests/crypto_kat.rs` (K4, K6),
  `moku-volume-daemon/tests/crypto_kat.rs` (K3, K5), plus the v0.2.7
  fixtures. Never "update" a KAT's expected value to match new code: a
  failing KAT means existing data would no longer decrypt.
- **New ciphertext formats use `moku_crypto::seal`/`open`** with
  XChaCha20-Poly1305 (audit KN-3.1). AAD is mandatory: an `Aad` is built
  from a domain string plus length-prefixed context fields, and only
  `AeadKey` roles are accepted (a root/password/volume-master key won't
  compile). Output is self-describing, `alg || nonce || ct || tag`; `Alg`
  ids are permanent (future cascade suites get new ids). Existing formats
  stay as they are: MKV1 volume blocks (`BlockAead`, AES-GCM, keyed once
  per opened volume) and names (`NameSiv`).
- **`moku_crypto::format_v1` is the one deliberate exception**: the
  pre-P3 `nonce || ct || tag` AES-GCM format without AAD, accepting a key
  of any role, still written by storage envelopes and the vault canary
  and read for V0 records and MSXP v1. Don't use it for anything new. P6
  deletes the write path (store moves to `seal`) and moves the read path
  into the migration crate; MSXP v1 import keeps a permanent reader. Its
  explicitly named role adoptions (`adopt_password_key_as_root`,
  `custom_volume_master`) exist because today's vault master *is* the
  Argon2 output; don't add more.
- **Zeroization claims are proven by tests, not assumed.**
  `moku-crypto/tests/zeroize_on_free.rs` installs a checking allocator
  and asserts that freed key boxes, cipher states, Argon2 memory and
  plaintext buffers are all zero. Its `library_fact_*` tests record what
  each RustCrypto crate wipes by itself at the locked version: argon2
  0.5.3 doesn't wipe `hash_password_into`'s memory (so `password_key`
  passes its own `Zeroizing` buffer) and polyval 0.6.2's autodetect
  backend never drops its GHASH state (so every cipher moku-crypto builds
  lives in `WipeOnDrop`). The `zeroize` features of aes, aes-gcm, ghash,
  polyval, chacha20, poly1305 and argon2 must be enabled one by one (they
  don't propagate). A dependency upgrade must keep this test green. Only
  heap memory can be checked this way; that's one more reason keys live
  in boxes.
- **`moku-store`'s storage layer (`StorageManager::save`/`load`) uses
  per-module HKDF subkeys, not the raw vault master key directly.**
  Every module's encrypted data (todo/bookmark/secrets/rss/...) is
  encrypted and read under `module_storage_key(root, module_id)`
  (`KeyScheme::V1`). Records still under the raw root key (data format
  0) are moku-migrate's hop 0 -> 1, never read by the runtime (see "Data
  format and migration").
- **`ENCRYPTABLE_MODULES` exists twice** (`modules/moku-settings/src/
  tabs/storage.rs` and `moku-bin/src/config_cmd.rs`, TUI vs. CLI of the
  encrypted/plaintext toggle): a module added to one goes in the other.
  Nothing else uses it; the data-format hops find module directories on
  disk. P6 replaces both with a registry declaration.
- **Every secret in memory has a wiping type (since P3, `secrecy` is
  gone):** keys are `Key<P>`, passwords/seeds/secret values are
  `moku_crypto::SecretString`, other plaintext byte buffers are
  `zeroize::Zeroizing<Vec<u8>>`. Never a plain `String`/`[u8; N]` for a
  password, key, or seed that outlives a single local computation — this
  includes TUI input buffers accumulating a secret keystroke-by-keystroke
  (`moku-lock-screen`'s `input`/`confirm`, `moku-volume-daemon`'s
  `CreateForm`/`PasswordPrompt`, `moku-secrets`'s `AddState.value`/
  `ExportState`, `SecretEntry.value`/`totp_seed`), the volume
  `VolumeSecret`/`MountSecret` passwords, and byte buffers crossing a
  process boundary (`moku-volume-daemon/src/worker.rs`'s stdin protocol).
  `SecretString` has no `Clone`/`Display`; its `push` grows the buffer
  itself and wipes the old one, `pop` overwrites, `take()` moves a form's
  password on without copying, `duplicate()` is the visible copy. Start
  input buffers with `SecretString::with_capacity(128)`. Read CLI
  passwords with `moku_vault::cli::read_password` (straight into a
  `SecretString`), never a bare `rpassword::prompt_password()` `String`.
- **The vault's root key is only ever borrowed: `ctx.session.with(|root|
  ..)`** (`VaultSession` keeps it in a `KeyCell`). Derive what you need
  inside the closure (a module or volume key) and return it; no IO or
  Argon2 inside. Nothing can hold the root key outside the session, so
  `lock()` really zeroizes it (waiting for running `with` calls).
  `VaultSession::current()` no longer exists.
- **Secrets are never CLI flags/arguments.** Not `--password`, not
  `--totp-seed`, nothing that would show up in `ps`/Task Manager/shell
  history. Always prompt interactively via `rpassword::prompt_password`
  (matches `modules/moku-secrets/src/cli_module.rs`'s `Add`/`Export`/
  `Import`), or — for a spawned worker process needing a secret handed to it
  by its parent (e.g. `moku-volume-daemon`'s mount worker) — pass it over
  stdin with a small tagged protocol, never argv or an environment variable.
- **Password-prompt UX rule (sharpened 2026-10-10):** ask **twice**
  (password + confirmation) whenever a typo would create something
  protected by the wrong password — a new vault, volume or export file,
  a password change — since there is nothing else to check it against.
  Ask **once** when a typo can only fail with "wrong password" (unlock,
  mount, import), or when an already-proven password is reused (`moku
  secrets export-upgrade`). Don't invent a third pattern.
  Implementations: the lock screen's setup mode, the volume create form,
  the secrets export form (password + Confirm fields),
  `moku_vault::cli::prompt_new_password` for CLI.
- **Shared delete-confirmation convention**: a destructive delete goes
  through `moku_core::keys::{is_delete_bypass, resolve_confirm_delete_key,
  ConfirmDeleteKey}` — plain `d`/`Delete` opens a confirm prompt
  (`ConfirmDeleteKey::Confirm`/`Cancel` from Enter/y vs. Esc/n), `Shift+D`
  (`is_delete_bypass`) deletes immediately, bypassing the prompt. Every
  module with a delete action (`moku-todo`, `moku-bookmark`,
  `moku-secrets`, `moku-rss`, `moku-volume-daemon`) uses this same pair of
  helpers rather than reimplementing the key matching.
- **Untrusted external input needs its own validation layer before it
  reaches an OS call.** `moku-sys/src/util.rs`'s `open_url` validates the
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
  panic. Always go through `moku_sys::util::truncate_at_char_boundary`.

## Data-safety conventions (from the 0.3-foundations P1 phase)

- **Whole-file state is written with `moku_sys::util::atomic_write`**
  (or `atomic_write_async`): temp file in the same directory, fsync,
  rename over the target. Never a plain `fs::write` for `meta.json`,
  `volume.json`, `usage.json`, the volume index, config files or any
  future state file. A torn write of `meta.json` loses the salt and with
  it every encrypted record.
- **A failed load is never "empty".** Use `StorageManager::load_opt`.
  `Ok(None)` means nothing was stored yet (start empty or seed); `Err`
  means something IS stored but couldn't be read.
  - On `Err`, the module opens read-only: it shows the persistent
    `moku_core::draw_load_error_banner`, refuses every mutating key
    with `show_warning`, and its save path refuses to write.
  - CLI commands propagate the error instead of saving.
  - The old pattern, `load(..).unwrap_or_default()` followed by a save,
    silently overwrote real data; see todo/secrets/bookmark/rss.
  - A `TuiModule::init` must not return `Err` for a data problem, since
    that tears down the whole app. Stay open read-only instead.
- **An unreachable volume is never dropped from the index** (since P4,
  KN-4.6). An unplugged drive and a folder deleted by hand look the same
  (`!dir.exists()`), so such an entry stays registered and is listed by
  `registry::offline_volumes()` (CLI `volume list`, TUI block under the
  list); its id stays taken. Only `volume delete <id>` forgets it, and on
  an offline volume that removes just the index entry.
- **A volume is opened only under its `VolumeLock`.**
  `VolumeEngine::open_volume(.., lock)` requires one. Take it with
  `VolumeLock::acquire(volume_dir)` BEFORE deriving keys, so a second
  opener fails fast instead of after the Argon2 wait.
  `VolumeLock::is_held` / `status::is_in_use` are pre-prompt hints
  only. Two engines on one volume corrupt data.
- **A lost `.moku_dir_iv` is an error, never silently regenerated.** It
  is created only for a directory with no encrypted entries yet, and
  never over an existing one.
- **The mount worker only ever gets one volume's key.** Stdin tag `0x02`
  carries the derived `Key<VolumeMasterKey>`; the TUI derives it inside
  `ctx.session.with(..)`, so the app key never leaves the session. Tag
  `0x01` (`MountSecret::AppKey`, app root key) is still accepted for
  upgrade compatibility but never sent; it goes away in P9. Creating a
  Default-mode volume needs no key at all (`VolumeSecret::AppVault`).
- **CLI vault access goes through `moku_vault::cli`:**
  `ensure_unlocked` asks once and never creates a vault; first-time
  setup is the TUI's (or a future `moku vault init`).
  `prompt_new_password` asks twice. Never create a vault from a single
  prompt.
- **Password buffers are wiped with `zeroize()`, never `clear()`.**
  `clear()` only resets the length and leaves the bytes behind.
- **`let _ = fallible()` needs a one-line comment saying why ignoring it
  is safe.** Otherwise log it (`tracing::warn!`) or surface it.
- **Show error chains:** user-facing error text uses `{e:#}`
  (`show_error(format!("...: {e:#}"))`, fatal CLI prints), so the cause
  isn't cut off. That only works on `anyhow::Error`; for a typed error
  use `moku_sys::util::error_chain` (see "Errors" above).

## Theme / UI conventions

- `moku_core::MokuTheme` (`moku-core/src/theme.rs`) is the single palette
  every module draws from: `base_fg`/`base_bg`, `border`, `selection_fg`/
  `selection_bg`, `info`, `warning`, `error`, `success`. Never hardcode a
  `ratatui::style::Color` in a module — reference the matching theme field
  so a user's custom `ThemeColors` config actually takes effect everywhere.
- **Forms show every field at once; `Tab` moves focus; `Enter` submits from
  any field.** No sequential per-field wizard (ask name, then ask size,
  then ask password...). See `moku-volume-daemon`'s `CreateForm`/
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
  params) and `moku-volume-daemon/src/tui_module/{mount_prompt,
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
    with no independent per-variant state (e.g. `moku-volume-daemon/src/
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
  `moku-volume-daemon` (`busy`), and `modules/moku-settings`'s Storage tab
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
  build, `moku-store/src/manager.rs::get_or_open_db`.
- **A tree/list flattened for display from a flat `Vec<T>` + parent-id
  links should be built with a pre-indexed `parent -> children` map (one
  pass, O(n)), not by having the recursive walk rescan the whole slice at
  every level (O(n²)).** See `modules/moku-todo/src/model.rs::build_view`.

## Test fixtures and refactor plan

- **`test-fixtures/v0.2.7/` is frozen.** Synthetic samples of every
  on-disk format moku-bin 0.2.7 wrote: a mixed V0/V1/External/plaintext
  data dir, a Default- and a Custom-mode volume, an MSXP v1 export, a
  raw legacy GCM blob. Future format migrations are tested against these.
  Never regenerate or edit them. `SHA256SUMS` +
  `moku-store/tests/fixture_v0_2_7.rs` fail on any change; restore from
  git instead. Tests always copy them to a temp dir before opening
  anything. Fixtures must only ever contain synthetic data
  (foo/bar/example.com, fixed test passwords). Never copy anything from
  a real data dir or `moku_dev`.
- **`modules/moku-secrets/tests/fixtures/msxp-v2.msxp` is frozen the
  same way** (P3): the first MSXP v2 export, produced once by an
  `#[ignore]`d generator that refuses to overwrite it. Module test
  fixtures are `-text` in `.gitattributes`.
- **Export files (MSXP) are not data-dir formats.** They live wherever
  the user put them, so the migration chain can't upgrade them: import
  reads every MSXP version forever, export writes only the latest, and
  `moku secrets export-upgrade` rewrites an old file on request
  (`modules/moku-secrets/src/msxp.rs`).
- The 0.3-foundations refactor plan and its per-phase status live in
  `audit/99-checklist.md` (+ `audit/checklist/P0..P10`). That folder is
  local and excluded via `.git/info/exclude`, never committed. This file
  holds the conventions; `audit/` holds the plan.

## Health-check passes

(Historical record. Crate names below are as they were at the time: the
volume crates were still called `moku-vault-*` before the vault -> volume
rename in `115d981`; today they are `moku-volume-*`.)

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
- `moku-volume-daemon/src/control.rs`'s control-channel named pipe uses the
  OS default DACL (same class as the volume-mount pipe's already-accepted
  limitation) — local-only, same-user DoS risk (a forced unmount), no
  confidentiality impact. Fixing it needs a real Windows security-
  descriptor (not just a doc comment), which is a meaningfully separate
  chunk of work from everything else in this pass.
- `sled`'s long-standing lack of active maintenance — an informational,
  forward-looking note, not something to react to by swapping storage
  backends in a pass whose explicit goal was "same behavior, safer/
  cleaner code."
- (Closed in P3: both are `SecretString` now.)
  `VolumeSecret::Password`/`MountSecret::Password` (moku-volume-daemon,
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
data-directory marker then described in the crypto section — the
explicit goal being that any future key-scheme or storage-format change
only ever needs one new variant plus one migration hop, never a bespoke
one-time mechanism again. `migrate_module_encryption`'s old
`stale_key_scheme` side effect (upgrading key scheme as a side effect of
the unrelated encrypted/plaintext toggle) was removed in favor of the
dedicated `migrate_key_scheme_to_latest`/`migrate_all_key_schemes`. This
work stayed on `dev`, untagged, per the versioning convention above —
`moku-bin` will get a `0.2.1` patch bump for it once flagged and
confirmed.

(Superseded in P4, 0.2.12: that chain became hop 0 -> 1 of moku-migrate's
data-format chain, and `.key_scheme_version` is now only read there, to
infer the format of a directory without `FORMAT`. See "Data format and
migration".)
