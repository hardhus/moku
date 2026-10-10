//! MSXP: the encrypted secrets export file.
//!
//! An export is protected by its own password, deliberately independent
//! of the vault, so it still opens if the vault is lost. Export files live
//! outside the data directory, so they can't be migrated in place: every
//! version stays readable here, and only the latest is written.
//!
//! - **v1** (moku-bin <= 0.2.10, read only):
//!   `MSXP || 1 || salt_len (u8) || salt || nonce (12) || ct || tag`.
//!   AES-256-GCM keyed directly with Argon2id(password) at the default
//!   parameters (not recorded), no AAD.
//! - **v2** (current):
//!   `MSXP || 2 || kdf_id (u8, 1 = Argon2id) || m_kib (u32 LE) || t (u32 LE)
//!   || p (u32 LE) || salt_len (u8) || salt || seal output`.
//!   The key is HKDF(`moku/secrets/export/v2`) over Argon2id(password)
//!   with the recorded parameters; the seal output is self-describing
//!   (`alg || nonce || ct || tag`, XChaCha20-Poly1305) and its AAD is the
//!   whole header, so changing any header byte (e.g. lowering the KDF
//!   cost) makes the file fail to open.

use anyhow::{Context, Result, anyhow, bail};
use moku_crypto::kdf::{DEFAULT_KDF, KdfParams};
use moku_crypto::{Aad, Alg, ExportKey, Key, PasswordKey, SecretString, format_v1};
use zeroize::Zeroizing;

use crate::model::SecretEntry;

const MAGIC: &[u8; 4] = b"MSXP";
const KDF_ARGON2ID: u8 = 1;
const V2_AAD_DOMAIN: &str = "moku/secrets/export/v2";

/// Upper bounds on the KDF parameters a v2 file may ask for. They are read
/// before the file can be authenticated, so a crafted file must not be
/// able to make import allocate or spin without limit.
const MAX_M_KIB: u32 = 1024 * 1024; // 1 GiB
const MAX_T: u32 = 16;
const MAX_P: u32 = 16;
/// Argon2 needs at least 8 bytes of salt; moku writes 16.
const SALT_LEN_RANGE: std::ops::RangeInclusive<usize> = 8..=64;

/// Which format version an export file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    V1,
    V2,
}

/// What `import` found in a file.
pub struct Imported {
    pub entries: Vec<SecretEntry>,
    pub version: Version,
}

/// The format version of `data`, without decrypting anything.
pub fn version_of(data: &[u8]) -> Result<Version> {
    if data.len() < 6 || &data[0..4] != MAGIC {
        bail!("not a moku-secrets encrypted export (bad magic)");
    }
    match data[4] {
        1 => Ok(Version::V1),
        2 => Ok(Version::V2),
        v => bail!("unsupported export version: {v} (made by a newer moku?)"),
    }
}

/// Encrypts `entries` into a v2 export file.
pub async fn export(entries: &[SecretEntry], password: &SecretString) -> Result<Vec<u8>> {
    let plaintext = Zeroizing::new(serde_json::to_vec(entries)?);
    seal_v2(&plaintext, password).await
}

/// Decrypts an export file of any supported version.
pub async fn import(data: &[u8], password: &SecretString) -> Result<Imported> {
    let version = version_of(data)?;
    let plaintext = match version {
        Version::V1 => open_v1(data, password).await?,
        Version::V2 => open_v2(data, password).await?,
    };
    let entries =
        serde_json::from_slice(&plaintext).context("decrypted export is not valid JSON")?;
    Ok(Imported { entries, version })
}

/// Re-encrypts a v1 export as v2 under the same password. The password is
/// proven by opening the v1 file, so no new password is set (and none has
/// to be confirmed).
pub async fn upgrade_v1(data: &[u8], password: &SecretString) -> Result<Vec<u8>> {
    match version_of(data)? {
        Version::V1 => {}
        Version::V2 => bail!("this export is already in the current format (v2)"),
    }
    let plaintext = open_v1(data, password).await?;
    // Make sure it is a real export before writing it out again.
    serde_json::from_slice::<Vec<SecretEntry>>(&plaintext)
        .context("decrypted export is not valid JSON")?;
    seal_v2(&plaintext, password).await
}

async fn derive_password_key(
    password: &SecretString,
    salt: &[u8],
    params: KdfParams,
) -> Result<Key<PasswordKey>> {
    let password = password.duplicate();
    let salt = salt.to_vec();
    Ok(tokio::task::spawn_blocking(move || moku_crypto::password_key(&password, &salt, params))
        .await??)
}

async fn export_key(password: &SecretString, salt: &[u8], params: KdfParams) -> Result<Key<ExportKey>> {
    let pk = derive_password_key(password, salt, params).await?;
    Ok(moku_crypto::secrets_export_key(&pk))
}

fn v2_header(params: KdfParams, salt: &[u8]) -> Vec<u8> {
    let mut header = Vec::with_capacity(4 + 1 + 1 + 12 + 1 + salt.len());
    header.extend_from_slice(MAGIC);
    header.push(2);
    header.push(KDF_ARGON2ID);
    header.extend_from_slice(&params.m_kib.to_le_bytes());
    header.extend_from_slice(&params.t.to_le_bytes());
    header.extend_from_slice(&params.p.to_le_bytes());
    // SALT_LEN is 16, well within a u8.
    header.push(salt.len() as u8);
    header.extend_from_slice(salt);
    header
}

async fn seal_v2(plaintext: &[u8], password: &SecretString) -> Result<Vec<u8>> {
    let salt = moku_crypto::kdf::generate_salt();
    let header = v2_header(DEFAULT_KDF, &salt);
    let key = export_key(password, &salt, DEFAULT_KDF).await?;
    let aad = Aad::new(V2_AAD_DOMAIN).field(&header);
    let sealed = moku_crypto::seal(&key, Alg::XChaCha20Poly1305, &aad, plaintext)?;
    let mut out = header;
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// Parses a v2 header: KDF parameters, salt, and where the header ends.
fn parse_v2_header(data: &[u8]) -> Result<(KdfParams, &[u8], usize)> {
    const FIXED: usize = 4 + 1 + 1 + 12 + 1;
    if data.len() < FIXED {
        bail!("truncated export file");
    }
    if data[5] != KDF_ARGON2ID {
        bail!("unsupported key derivation in export file (id {})", data[5]);
    }
    let u32_at = |i: usize| u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
    let params = KdfParams {
        m_kib: u32_at(6),
        t: u32_at(10),
        p: u32_at(14),
    };
    if params.m_kib > MAX_M_KIB || params.t > MAX_T || params.p > MAX_P {
        bail!("export file asks for excessive key derivation cost; refusing to open it");
    }
    let salt_len = data[18] as usize;
    if !SALT_LEN_RANGE.contains(&salt_len) {
        bail!("export file has an invalid salt length");
    }
    let header_end = FIXED + salt_len;
    if data.len() < header_end {
        bail!("truncated export file");
    }
    Ok((params, &data[FIXED..header_end], header_end))
}

async fn open_v2(data: &[u8], password: &SecretString) -> Result<Zeroizing<Vec<u8>>> {
    let (params, salt, header_end) = parse_v2_header(data)?;
    let (header, sealed) = data.split_at(header_end);
    let key = export_key(password, salt, params).await?;
    let aad = Aad::new(V2_AAD_DOMAIN).field(header);
    moku_crypto::open(&key, &aad, sealed).map_err(|_| anyhow!("wrong password or corrupted export"))
}

async fn open_v1(data: &[u8], password: &SecretString) -> Result<Zeroizing<Vec<u8>>> {
    let salt_len = data[5] as usize;
    let salt_end = 6 + salt_len;
    if data.len() < salt_end {
        bail!("truncated export file");
    }
    let (salt, ciphertext) = (&data[6..salt_end], &data[salt_end..]);
    // v1 keys AES-GCM directly with the Argon2id output (format_v1).
    let key = derive_password_key(password, salt, DEFAULT_KDF).await?;
    format_v1::open_gcm_no_aad(&key, ciphertext).map_err(|_| anyhow!("wrong password or corrupted export"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> Vec<SecretEntry> {
        vec![
            SecretEntry::new("github".to_string(), "hunter2"),
            SecretEntry::new("aws".to_string(), "sekrit"),
        ]
    }

    /// A v1 file built the way 0.2.10 wrote it.
    async fn v1_file(entries: &[SecretEntry], password: &SecretString) -> Vec<u8> {
        let salt = [3u8; 16];
        let key = derive_password_key(password, &salt, DEFAULT_KDF).await.unwrap();
        let ct = format_v1::seal_gcm_no_aad(&key, &serde_json::to_vec(entries).unwrap()).unwrap();
        [&MAGIC[..], &[1, 16], &salt, &ct].concat()
    }

    #[tokio::test]
    async fn test_export_writes_v2_with_the_documented_header() {
        let bytes = export(&entries(), &"pw".into()).await.unwrap();
        assert_eq!(&bytes[0..4], b"MSXP");
        assert_eq!(bytes[4], 2, "version");
        assert_eq!(bytes[5], 1, "Argon2id");
        assert_eq!(&bytes[6..10], &65_536u32.to_le_bytes());
        assert_eq!(&bytes[10..14], &3u32.to_le_bytes());
        assert_eq!(&bytes[14..18], &4u32.to_le_bytes());
        assert_eq!(bytes[18], 16, "salt length");
        assert_eq!(bytes[19 + 16], Alg::XChaCha20Poly1305.id());
    }

    #[tokio::test]
    async fn test_v2_roundtrip_and_wrong_password() {
        let bytes = export(&entries(), &"correct".into()).await.unwrap();
        let imported = import(&bytes, &"correct".into()).await.unwrap();
        assert_eq!(imported.version, Version::V2);
        assert_eq!(imported.entries[1].value.expose(), "sekrit");
        assert!(import(&bytes, &"wrong".into()).await.is_err());
    }

    #[tokio::test]
    async fn test_any_changed_header_byte_fails_to_open() {
        let bytes = export(&entries(), &"pw".into()).await.unwrap();
        // Lower t from 3 to 2: still a valid header, but the AAD differs.
        let mut tampered = bytes.clone();
        tampered[10] = 2;
        let err = import(&tampered, &"pw".into()).await.err().unwrap();
        assert!(err.to_string().contains("wrong password or corrupted"), "{err:#}");
        // A salt byte.
        let mut tampered = bytes;
        tampered[20] ^= 1;
        assert!(import(&tampered, &"pw".into()).await.is_err());
    }

    #[tokio::test]
    async fn test_excessive_kdf_parameters_are_refused_before_deriving() {
        let mut header = v2_header(KdfParams { m_kib: MAX_M_KIB + 1, t: 1, p: 1 }, &[0; 16]);
        header.extend_from_slice(&[0u8; 64]);
        let err = import(&header, &"pw".into()).await.err().unwrap();
        assert!(err.to_string().contains("excessive"), "{err:#}");
    }

    #[tokio::test]
    async fn test_v1_still_imports_and_upgrades_to_v2() {
        let pw: SecretString = "pw".into();
        let v1 = v1_file(&entries(), &pw).await;
        let imported = import(&v1, &pw).await.unwrap();
        assert_eq!(imported.version, Version::V1);
        assert_eq!(imported.entries[0].name, "github");

        let v2 = upgrade_v1(&v1, &pw).await.unwrap();
        assert_eq!(version_of(&v2).unwrap(), Version::V2);
        let back = import(&v2, &pw).await.unwrap();
        assert_eq!(back.entries[0].value.expose(), "hunter2");

        assert!(upgrade_v1(&v1, &"wrong".into()).await.is_err());
        assert!(upgrade_v1(&v2, &pw).await.is_err(), "v2 is not upgraded again");
    }

    #[test]
    fn test_unknown_versions_and_bad_magic() {
        assert!(version_of(b"MSXP\x09rest").is_err());
        assert!(version_of(b"nope").is_err());
        assert!(version_of(b"XXXX\x02rest").is_err());
    }
}
