use serde::{Deserialize, Serialize};

pub const CURRENT_SCHEMA_VERSION: u16 = 1;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum StorageType {
    Embedded,
    External,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum EncryptionStatus {
    Plaintext,
    Encrypted,
}

#[derive(Serialize, Deserialize)]
pub struct StorageEnvelope {
    #[serde(default = "default_schema_version")]
    pub schema_version: u16,
    pub status: EncryptionStatus,
    pub storage_type: StorageType,
    pub payload: Vec<u8>,
    pub hash: Option<String>,
}

fn default_schema_version() -> u16 {
    1
}
