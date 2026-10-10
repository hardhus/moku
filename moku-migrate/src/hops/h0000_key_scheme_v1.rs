//! Hop 0 → 1: per-module encryption keys (`KeyScheme` V0 → V1).
//!
//! Format 0 encrypted records directly under the vault's root key (the
//! Argon2 output itself). Format 1 encrypts each module's records under
//! `module_storage_key(root, module_id)`. Plaintext records and records
//! already tagged `PerModuleV1` are left alone; so are their tags (plaintext
//! records keep `"Legacy"`, which the runtime reads as "no key").
//!
//! FROZEN (shipped in moku-bin 0.2.12): don't change this file. It reads
//! the sled store exactly as moku-bin 0.2.1–0.2.11 wrote it, through its
//! own copy of the envelope types below.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use moku_crypto::{CryptoError, Key, ModuleStorageKey, RootKey, format_v1, module_storage_key};
use moku_sys::util::error_chain;

use crate::{DataFormat, Hop, HopReport, MigrateError, MigrationCtx, RecordFailure};

pub(crate) const HOP: Hop = Hop {
    from: DataFormat(0),
    to: DataFormat(1),
    needs_unlock: true,
    description: "Per-module encryption keys (key scheme V0 → V1)",
    run,
};

/// The sled-era `StorageEnvelope` (moku-store), byte-for-byte as JSON.
#[derive(Serialize, Deserialize)]
struct Envelope {
    #[serde(default = "default_schema_version")]
    schema_version: u16,
    status: Status,
    storage_type: StorageType,
    payload: Vec<u8>,
    hash: Option<String>,
    #[serde(default)]
    key_scheme: Scheme,
}

fn default_schema_version() -> u16 {
    1
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
enum Status {
    Plaintext,
    Encrypted,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
enum StorageType {
    Embedded,
    External,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Default)]
enum Scheme {
    #[default]
    Legacy,
    PerModuleV1,
}

const VAULT_DIR: &str = "vault";
const LEGACY_KEY_SCHEME_MARKER: &str = ".key_scheme_version";

fn run(ctx: &MigrationCtx<'_>) -> Result<HopReport, MigrateError> {
    let vault = ctx.data_dir.join(VAULT_DIR);
    let mut report = HopReport::default();
    let mut failures = Vec::new();

    for module in module_dirs(&vault)? {
        let (migrated, skipped) =
            migrate_module(ctx, &vault.join(&module), &module, &mut failures)?;
        report.migrated += migrated;
        report.skipped += skipped;
        report.per_module.push((module, migrated));
    }
    if !failures.is_empty() {
        return Err(MigrateError::Records { failures });
    }

    // Keep the pre-FORMAT marker in step, so moku-bin 0.2.1–0.2.11 opened
    // on this directory doesn't offer its own (now no-op) migration.
    moku_sys::util::atomic_write(&vault.join(LEGACY_KEY_SCHEME_MARKER), b"1")?;
    Ok(report)
}

/// Every `vault/<module>/` that has a `db/` directory: every module that
/// ever stored anything, known to this build or not.
fn module_dirs(vault: &Path) -> Result<Vec<String>, MigrateError> {
    let entries = match fs::read_dir(vault) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(MigrateError::io("list the vault directory", vault)(e)),
    };
    let mut modules = Vec::new();
    for entry in entries {
        let entry = entry.map_err(MigrateError::io("list the vault directory", vault))?;
        if entry.path().join("db").is_dir() {
            modules.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    modules.sort();
    Ok(modules)
}

fn migrate_module(
    ctx: &MigrationCtx<'_>,
    dir: &Path,
    module: &str,
    failures: &mut Vec<RecordFailure>,
) -> Result<(usize, usize), MigrateError> {
    let db = sled::open(dir.join("db")).map_err(|e| MigrateError::from_open(module, e))?;
    // Format 0's cipher key *is* the root key, so this hop (and only this
    // hop) holds a copy of it for the length of one module.
    let (root, module_key) = ctx
        .session
        .with(|root| (root.duplicate(), module_storage_key(root, module)))
        .map_err(|_| MigrateError::Locked)?;

    let records: Vec<(sled::IVec, sled::IVec)> = db.iter().collect::<Result<_, _>>()?;
    let (mut migrated, mut skipped) = (0, 0);
    for (raw_key, value) in records {
        let key = String::from_utf8_lossy(&raw_key).into_owned();
        match convert(dir, module, &key, &value, &root, &module_key) {
            Ok(Some(envelope)) => {
                db.insert(&raw_key, envelope)?;
                migrated += 1;
                ctx.report(module, migrated);
            }
            Ok(None) => skipped += 1,
            Err(reason) => failures.push(RecordFailure {
                module: module.to_string(),
                key,
                reason,
            }),
        }
    }
    db.flush()?;
    Ok((migrated, skipped))
}

/// The new envelope bytes for one record, `None` if it needs no change.
///
/// An External record's ciphertext lives in its blob file. The blob is
/// written first (atomically), then the envelope. A crash in between
/// leaves a `Legacy` envelope over an already re-encrypted blob, which
/// fails under the root key; it is then tried under the module key and,
/// if that opens it, only the envelope is rewritten. An Embedded record
/// is one sled insert, so it can't be half converted.
fn convert(
    dir: &Path,
    module: &str,
    key: &str,
    value: &[u8],
    root: &Key<RootKey>,
    module_key: &Key<ModuleStorageKey>,
) -> Result<Option<Vec<u8>>, String> {
    let mut envelope: Envelope =
        serde_json::from_slice(value).map_err(|e| format!("unreadable envelope: {e}"))?;
    if envelope.status != Status::Encrypted || envelope.key_scheme != Scheme::Legacy {
        return Ok(None);
    }

    let blob_path = dir.join("blobs").join(format!("{module}_{key}.blob"));
    let ciphertext = match envelope.storage_type {
        StorageType::Embedded => std::mem::take(&mut envelope.payload),
        StorageType::External => fs::read(&blob_path)
            .map_err(|e| format!("can't read its external blob {}: {e}", blob_path.display()))?,
    };

    let new_ciphertext = match format_v1::open_gcm_no_aad(root, &ciphertext) {
        Ok(plaintext) => {
            Some(format_v1::seal_gcm_no_aad(module_key, &plaintext).map_err(|e| error_chain(&e))?)
        }
        Err(CryptoError::AuthFailed) if envelope.storage_type == StorageType::External => {
            format_v1::open_gcm_no_aad(module_key, &ciphertext).map_err(|_| {
                "fails authentication under both the old and the new key".to_string()
            })?;
            None
        }
        Err(e) => return Err(error_chain(&e)),
    };

    match envelope.storage_type {
        StorageType::Embedded => {
            envelope.payload = new_ciphertext.unwrap_or(ciphertext);
        }
        StorageType::External => {
            if let Some(ct) = new_ciphertext {
                moku_sys::util::atomic_write(&blob_path, &ct).map_err(|e| error_chain(&e))?;
            }
            envelope.payload = Vec::new();
        }
    }
    envelope.key_scheme = Scheme::PerModuleV1;
    serde_json::to_vec(&envelope)
        .map(Some)
        .map_err(|e| format!("can't encode the new envelope: {e}"))
}
