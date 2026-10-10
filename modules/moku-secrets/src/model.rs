use std::time::{SystemTime, UNIX_EPOCH};

use rand::RngCore;
use rand::rngs::OsRng;
use moku_crypto::SecretString;
use serde::{Deserialize, Serialize};

/// Bridges `SecretString` through serde as a plain JSON string (the
/// on-disk shape is unchanged). moku-crypto has no serde dependency, so
/// the bridge lives with the type that needs it. The deserialized `String`
/// is moved into the `SecretString`, not copied.
mod secret_string {
    use moku_crypto::SecretString;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &SecretString, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(value.expose())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<SecretString, D::Error> {
        Ok(SecretString::from(String::deserialize(deserializer)?))
    }

    pub mod option {
        use super::*;

        pub fn serialize<S: Serializer>(
            value: &Option<SecretString>,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            match value {
                Some(v) => serializer.serialize_some(v.expose()),
                None => serializer.serialize_none(),
            }
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<SecretString>, D::Error> {
            Ok(Option::<String>::deserialize(deserializer)?.map(SecretString::from))
        }
    }
}

/// One stored secret. No `Clone`: the value and TOTP seed are
/// `SecretString`s, and copies of secrets are only ever made on purpose.
#[derive(Serialize, Deserialize, Debug)]
pub struct SecretEntry {
    pub id: String,
    pub name: String,
    #[serde(with = "secret_string")]
    pub value: SecretString,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    /// Base32 TOTP seed, if this entry has a linked authenticator code.
    #[serde(default, with = "secret_string::option")]
    pub totp_seed: Option<SecretString>,
    #[serde(default)]
    pub notes: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

impl SecretEntry {
    pub fn new(name: String, value: impl Into<SecretString>) -> Self {
        let now = now_secs();
        Self {
            id: random_id(),
            name,
            value: value.into(),
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
        let a = SecretEntry::new("a".into(), "x");
        let b = SecretEntry::new("b".into(), "y");
        assert_ne!(a.id, b.id);
        assert_eq!(a.id.len(), 16);
    }

    #[test]
    fn test_new_entry_timestamps_set() {
        let e = SecretEntry::new("a".into(), "x");
        assert!(e.created_at > 0);
        assert_eq!(e.created_at, e.updated_at);
    }
}
