//! Terminal password prompts shared by every CLI command that touches the
//! vault, so they all follow the same rules:
//! - an existing password is asked once;
//! - a new password is always asked twice;
//! - passwords go straight into `Zeroizing`;
//! - a CLI command never silently creates a vault.

use anyhow::{Result, anyhow, bail};
use zeroize::Zeroizing;

use crate::{SecurityManager, VaultSession};

fn read_password(label: &str) -> Result<Zeroizing<String>> {
    Ok(Zeroizing::new(
        rpassword::prompt_password(label).map_err(|e| anyhow!("Failed to read password: {e}"))?,
    ))
}

/// Unlocks `session` from an interactive prompt if it isn't unlocked yet.
///
/// A vault that has never been set up is an error. Creating one here, from
/// a single unconfirmed prompt, while the user thinks they are just
/// unlocking, would be exactly the silent-setup trap this replaces. The
/// first-time setup happens in the TUI (which asks twice); a dedicated
/// `moku vault init` command is planned.
pub async fn ensure_unlocked(
    session: &VaultSession,
    security: &SecurityManager,
    prompt: &str,
) -> Result<()> {
    if session.is_unlocked() {
        return Ok(());
    }
    if !security.is_vault_initialized() {
        bail!(
            "the moku vault hasn't been set up yet: open `moku` (the TUI) once and create it there"
        );
    }
    let password = read_password(prompt)?;
    let key = security
        .unlock_vault(password)
        .await
        .map_err(|e| anyhow!("Vault unlock failed: {e}"))?;
    session.unlock(key);
    Ok(())
}

/// Asks for a NEW password twice and returns it only if both entries
/// match and it isn't empty. A fresh password has nothing else to check a
/// typo against.
pub fn prompt_new_password(label: &str, confirm_label: &str) -> Result<Zeroizing<String>> {
    let first = read_password(label)?;
    let second = read_password(confirm_label)?;
    check_new_password(first, &second)
}

fn check_new_password(first: Zeroizing<String>, second: &str) -> Result<Zeroizing<String>> {
    if first.is_empty() {
        bail!("password cannot be empty");
    }
    if *first != *second {
        bail!("passwords did not match");
    }
    Ok(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_new_password_rules() {
        assert!(check_new_password(Zeroizing::new(String::new()), "").is_err());
        assert!(check_new_password(Zeroizing::new("a".into()), "b").is_err());
        assert_eq!(
            check_new_password(Zeroizing::new("same".into()), "same")
                .unwrap()
                .as_str(),
            "same"
        );
    }

    #[tokio::test]
    async fn test_ensure_unlocked_never_creates_a_vault() {
        let dir = tempfile::tempdir().unwrap();
        let security = SecurityManager::new_with_root(dir.path().to_path_buf());
        let session = VaultSession::new();
        let err = ensure_unlocked(&session, &security, "pw: ")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("hasn't been set up"));
        assert!(!security.is_vault_initialized());
    }

    #[tokio::test]
    async fn test_ensure_unlocked_is_a_no_op_when_already_unlocked() {
        let dir = tempfile::tempdir().unwrap();
        let security = SecurityManager::new_with_root(dir.path().to_path_buf());
        let session = VaultSession::new();
        let key = SecurityManager::derive_key("x", &[1u8; 16]).await.unwrap();
        session.unlock(key);
        ensure_unlocked(&session, &security, "pw: ").await.unwrap();
    }
}
