# test-fixtures/v0.2.7 — frozen on-disk format samples

These files were written by **moku-bin 0.2.7's own code**. They exist so
that future on-disk format changes can be tested against real old-format
bytes: moving crypto into its own crate, keyslot-based `meta.json`, and
the sled → SQLite store. Every future migration hop must be able to read
these.

**Never regenerate or edit these files.** They are frozen:

- `SHA256SUMS` lists every file.
- `moku-core/tests/fixture_v0_2_7.rs::v0_2_7_fixtures_are_byte_for_byte_frozen`
  fails if any file is changed, added or removed.
- If that test fails, restore the files from git. Don't update the
  manifest.

The generator that produced them is kept for the record in
`moku-bin/tests/gen_fixtures_v0_2_7.rs`. It is `#[ignore]`d and refuses
to run if this directory already exists.

## All data is synthetic

Nothing here comes from a real user or from a real data directory. It was
built from scratch in a temporary directory.

- Values are only `foo` / `bar` / `baz` / `example.com|org|net`.
- Passwords are fixed test strings.
- Timestamps are fixed (`1700000000`).
- The TOTP seed `JBSWY3DPEHPK3PXP` is the well-known RFC/Google example
  seed.

| Test password | Used for |
|---|---|
| `foo-test-password` | the app vault in `datadir/vault/meta.json` |
| `bar-volume-password` | the Custom-mode volume `volumes/custom-bar/vault/meta.json` |
| `foo-export-password` | the secrets export `msxp/export-v1.msxp` |
| (key K1) `derive_key("correct horse", [0x01; 16])` | `blobs/legacy-gcm.bin` |

## Contents

### `datadir/`: a mixed pre-0.2.1 moku data directory (K10)

`datadir/vault/meta.json` holds the salt and the AES-GCM canary. There is
deliberately **no `.key_scheme_version` marker**; this models an install
that predates it, and it reads as version 0.

| Module / key | Status | Key scheme | Storage | Content |
|---|---|---|---|---|
| `todo` / `items` | Encrypted | `Legacy` (V0, raw master key) | Embedded | 3 `Task`s: `foo task`, `bar subtask` (child of the first), `baz task` |
| `secrets` / `entries` | Encrypted | `Legacy` (V0) | Embedded | 2 `SecretEntry`s: `foo-api` = `bar-secret-value`; `bar-login` = `baz-secret-value` + TOTP seed |
| `bookmark` / `bookmarks_encrypted` | Encrypted | `PerModuleV1` (HKDF subkey) | **External** `.blob` | 3 `Bookmark`s; the third has a 60 KiB `"A"` description to cross the 50 KiB External threshold |
| `rss` / `feeds` | Plaintext | `Legacy` (field default for plaintext) | Embedded | 1 feed `https://example.com/feed.xml` "Foo Feed" |
| `rss` / `items` | Plaintext | `Legacy` | Embedded | 1 item "Foo item 1" |

The V0 records were written straight into sled, the way pre-0.2.0 code
stored them. Today's `StorageManager::save` can only write the current
scheme.

### `volumes/default-foo/`: Default-mode volume (K10b, K8)

The volume has no vault of its own. Its key is
`derive_default_volume_master_key(<datadir app master>, "default-foo")`,
followed by `derive_volume_keys`. To open it, unlock `datadir/` with
`foo-test-password`.

Plaintext tree:

- `/bar.txt` = `bar\n`
- `/docs/foo.md` = `# Foo\n\nbar baz\n`
- `/docs/big.bin` = 10000 bytes of `i % 251` (spans multiple 4 KiB blocks)

The data is stored as MKV1 files with AES-SIV-encrypted names and per-dir
`.moku_dir_iv` files.

### `volumes/custom-bar/`: Custom-mode volume (K10b, K8)

This volume has its own `vault/meta.json` (password `bar-volume-password`)
and the same plaintext tree as `default-foo`.

### `msxp/export-v1.msxp`: encrypted secrets export, MSXP v1 (K9)

The same 2 entries as `datadir` secrets, exported with
`moku_secrets::engine::export_encrypted(.., "foo-export-password")`.

### `blobs/legacy-gcm.bin`: raw `SecurityManager::encrypt` output (K7b)

`encrypt(b"foo-legacy-plaintext", K1)`, framed as `nonce(12) ‖ ct ‖ tag`.

## Tests that read these

- `moku-core/tests/fixture_v0_2_7.rs`: format mix, unlock + load of every
  record, wrong password, K7b, frozen-manifest check.
- `moku-volume-daemon/tests/fixture_v0_2_7.rs`: both volumes open and their
  trees read back exactly.
- `modules/moku-secrets/tests/fixture_v0_2_7.rs`: MSXP v1 import.

Every test copies the fixtures into a temporary directory first. Opening a
vault, sled DB or volume can write to it, and these files must never
change.
