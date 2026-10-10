use anyhow::{Context, Result, anyhow};
use moku_config::MokuConfig;
use moku_core::ModuleId;
use moku_store::StorageManager;
use moku_crypto::SecretString;
use totp_rs::{Algorithm, Secret, TOTP};

use crate::model::SecretEntry;

const STORAGE_KEY: &str = "entries";

/// Loads every stored secret. Nothing stored yet is an empty list; any
/// real failure (locked vault, wrong key, corrupt record, shape change) is
/// an `Err`, never an empty list, so no caller can mistake unreadable data
/// for "no secrets" and then save over it.
pub async fn load_entries(storage: &StorageManager) -> Result<Vec<SecretEntry>> {
    Ok(storage.load_opt(ModuleId::SECRETS.as_str(), STORAGE_KEY).await?.unwrap_or_default())
}

pub async fn save_entries(storage: &StorageManager, config: &MokuConfig, entries: &[SecretEntry]) -> Result<()> {
    let encrypt = moku_config::resolve_encryption(config, ModuleId::SECRETS.as_str(), true)?;
    storage.save(ModuleId::SECRETS.as_str(), STORAGE_KEY, entries, encrypt).await?;
    Ok(())
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
        csv_field(e.value.expose()),
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
/// even if the main vault is lost. Writes the current format (MSXP v2, see
/// `crate::msxp`).
pub async fn export_encrypted(entries: &[SecretEntry], password: &SecretString) -> Result<Vec<u8>> {
    crate::msxp::export(entries, password).await
}

/// Decrypts an export file of any supported MSXP version.
pub async fn import_encrypted(data: &[u8], password: &SecretString) -> Result<Vec<SecretEntry>> {
    Ok(crate::msxp::import(data, password).await?.entries)
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
        let bytes = export_encrypted(&entries, &"correct horse battery staple".into()).await.unwrap();
        let imported = import_encrypted(&bytes, &"correct horse battery staple".into()).await.unwrap();
        assert_eq!(imported.len(), 2);
        assert_eq!(imported[1].value.expose(), "sekrit");
    }

    #[tokio::test]
    async fn test_encrypted_import_wrong_password_fails() {
        let entries = sample_entries();
        let bytes = export_encrypted(&entries, &"correct-password".into()).await.unwrap();
        let result = import_encrypted(&bytes, &"wrong-password".into()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_encrypted_import_rejects_bad_magic() {
        let result = import_encrypted(b"not-a-real-export", &"any".into()).await;
        assert!(result.is_err());
    }
}
