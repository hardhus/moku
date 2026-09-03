use std::time::{SystemTime, UNIX_EPOCH};

use rand::RngCore;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SecretEntry {
    pub id: String,
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    /// Base32 TOTP seed, if this entry has a linked authenticator code.
    #[serde(default)]
    pub totp_seed: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

impl SecretEntry {
    pub fn new(name: String, value: String) -> Self {
        let now = now_secs();
        Self {
            id: random_id(),
            name,
            value,
            category: None,
            username: None,
            url: None,
            totp_seed: None,
            notes: None,
            created_at: now,
            updated_at: now,
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// A short random id, not derived from the name (so renaming an entry
/// doesn't change its identity).
fn random_id() -> String {
    let mut bytes = [0u8; 8];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_entry_has_unique_id() {
        let a = SecretEntry::new("a".into(), "x".into());
        let b = SecretEntry::new("b".into(), "y".into());
        assert_ne!(a.id, b.id);
        assert_eq!(a.id.len(), 16);
    }

    #[test]
    fn test_new_entry_timestamps_set() {
        let e = SecretEntry::new("a".into(), "x".into());
        assert!(e.created_at > 0);
        assert_eq!(e.created_at, e.updated_at);
    }
}
