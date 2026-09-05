use anyhow::{Context, Result, anyhow, bail};
use moku_core::{ModuleId, MokuConfig, SecurityManager, StorageManager};
use totp_rs::{Algorithm, Secret, TOTP};

use crate::model::SecretEntry;

const STORAGE_KEY: &str = "entries";

pub async fn load_entries(storage: &StorageManager) -> Vec<SecretEntry> {
    storage.load(ModuleId::SECRETS.as_str(), STORAGE_KEY).await.unwrap_or_default()
}

pub async fn save_entries(storage: &StorageManager, config: &MokuConfig, entries: &[SecretEntry]) -> Result<()> {
    let encrypt = moku_core::resolve_encryption(config, ModuleId::SECRETS.as_str(), true);
    storage.save(ModuleId::SECRETS.as_str(), STORAGE_KEY, &entries.to_vec(), encrypt).await
}

pub fn find_by_name<'a>(entries: &'a [SecretEntry], name: &str) -> Option<&'a SecretEntry> {
    entries.iter().find(|e| e.name.eq_ignore_ascii_case(name))
}

pub fn totp_code_now(seed_base32: &str) -> Result<String> {
    let secret_bytes = Secret::Encoded(seed_base32.to_string()).to_bytes().map_err(|e| anyhow!("invalid TOTP seed: {e:?}"))?;
    let totp = TOTP::new(Algorithm::SHA1, 6, 1, 30, secret_bytes, None, "moku".to_string())
        .map_err(|e| anyhow!("invalid TOTP parameters: {e:?}"))?;
    totp.generate_current().map_err(|e| anyhow!("failed to generate TOTP code: {e:?}"))
}

// --- Export / Import ---

const ENCRYPTED_MAGIC: &[u8; 4] = b"MSXP";
const ENCRYPTED_VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlainFormat {
    Json,
    Csv,
}

pub fn export_plain(entries: &[SecretEntry], format: PlainFormat) -> Result<Vec<u8>> {
    match format {
        PlainFormat::Json => Ok(serde_json::to_vec_pretty(entries)?),
        PlainFormat::Csv => {
            let mut out = String::from("name,value,category,username,url,notes\n");
            for e in entries {
                out.push_str(&csv_row(e));
                out.push('\n');
            }
            Ok(out.into_bytes())
        }
    }
}

fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn csv_row(e: &SecretEntry) -> String {
    [
        csv_field(&e.name),
        csv_field(&e.value),
        csv_field(e.category.as_deref().unwrap_or("")),
        csv_field(e.username.as_deref().unwrap_or("")),
        csv_field(e.url.as_deref().unwrap_or("")),
        csv_field(e.notes.as_deref().unwrap_or("")),
    ]
    .join(",")
}

pub fn import_plain_json(data: &[u8]) -> Result<Vec<SecretEntry>> {
    serde_json::from_slice(data).context("invalid JSON secrets export")
}

/// Encrypts `entries` under a password chosen at export time — deliberately
/// independent of moku's own vault password, so this backup remains usable
/// even if the main vault is lost. Self-describing envelope (magic +
/// version + salt + ciphertext) so it can be re-imported without any other
/// context.
pub async fn export_encrypted(entries: &[SecretEntry], password: &str) -> Result<Vec<u8>> {
    let salt = SecurityManager::generate_salt(16);
    let key = SecurityManager::derive_key(password, &salt).await?;
    let plaintext = serde_json::to_vec(entries)?;
    let ciphertext = SecurityManager::encrypt(&plaintext, &key)?;

    let mut out = Vec::with_capacity(4 + 1 + 1 + salt.len() + ciphertext.len());
    out.extend_from_slice(ENCRYPTED_MAGIC);
    out.push(ENCRYPTED_VERSION);
    out.push(salt.len() as u8);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

pub async fn import_encrypted(data: &[u8], password: &str) -> Result<Vec<SecretEntry>> {
    if data.len() < 6 || &data[0..4] != ENCRYPTED_MAGIC {
        bail!("not a moku-secrets encrypted export (bad magic)");
    }
    if data[4] != ENCRYPTED_VERSION {
        bail!("unsupported export version: {}", data[4]);
    }
    let salt_len = data[5] as usize;
    let salt_start = 6;
    let salt_end = salt_start + salt_len;
    if data.len() < salt_end {
        bail!("truncated export file");
    }
    let salt = &data[salt_start..salt_end];
    let ciphertext = &data[salt_end..];

    let key = SecurityManager::derive_key(password, salt).await?;
    let plaintext = SecurityManager::decrypt(ciphertext, &key).map_err(|_| anyhow!("wrong password or corrupted export"))?;
    serde_json::from_slice(&plaintext).context("decrypted export is not valid JSON")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SecretEntry;

    fn sample_entries() -> Vec<SecretEntry> {
        vec![SecretEntry::new("github".to_string(), "hunter2".to_string()), SecretEntry::new("aws".to_string(), "sekrit".to_string())]
    }

    #[test]
    fn test_find_by_name_case_insensitive() {
        let entries = sample_entries();
        assert!(find_by_name(&entries, "GitHub").is_some());
        assert!(find_by_name(&entries, "missing").is_none());
    }

    #[test]
    fn test_export_plain_json_roundtrip() {
        let entries = sample_entries();
        let bytes = export_plain(&entries, PlainFormat::Json).unwrap();
        let imported = import_plain_json(&bytes).unwrap();
        assert_eq!(imported.len(), 2);
        assert_eq!(imported[0].name, "github");
    }

    #[test]
    fn test_export_plain_csv_has_header_and_rows() {
        let entries = sample_entries();
        let bytes = export_plain(&entries, PlainFormat::Csv).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "name,value,category,username,url,notes");
        assert_eq!(lines.len(), 3); // header + 2 entries
    }

    #[test]
    fn test_csv_field_quotes_commas_and_quotes() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[tokio::test]
    async fn test_encrypted_export_import_roundtrip() {
        let entries = sample_entries();
        let bytes = export_encrypted(&entries, "correct horse battery staple").await.unwrap();
        let imported = import_encrypted(&bytes, "correct horse battery staple").await.unwrap();
        assert_eq!(imported.len(), 2);
        assert_eq!(imported[1].value.as_str(), "sekrit");
    }

    #[tokio::test]
    async fn test_encrypted_import_wrong_password_fails() {
        let entries = sample_entries();
        let bytes = export_encrypted(&entries, "correct-password").await.unwrap();
        let result = import_encrypted(&bytes, "wrong-password").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_encrypted_import_rejects_bad_magic() {
        let result = import_encrypted(b"not-a-real-export", "any").await;
        assert!(result.is_err());
    }
}
