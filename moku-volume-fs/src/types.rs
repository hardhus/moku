/// A path inside the virtual (decrypted) file tree, always absolute
/// ("/", "/notes/today.md", ...). Kept as an owned '/'-separated string —
/// platform-independent, since the virtual tree looks identical on Windows
/// and Unix regardless of the backing OS.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct VirtualPath(String);

impl VirtualPath {
    pub fn root() -> Self {
        Self("/".to_string())
    }

    pub fn parse(path: &str) -> Self {
        let normalized = if path.is_empty() || path == "/" {
            "/".to_string()
        } else {
            let trimmed = path.trim_end_matches('/');
            if trimmed.starts_with('/') {
                trimmed.to_string()
            } else {
                format!("/{trimmed}")
            }
        };
        Self(normalized)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0 == "/"
    }

    pub fn components(&self) -> Vec<&str> {
        if self.is_root() {
            Vec::new()
        } else {
            self.0.trim_start_matches('/').split('/').collect()
        }
    }

    pub fn file_name(&self) -> Option<&str> {
        self.components().last().copied()
    }

    pub fn parent(&self) -> Option<VirtualPath> {
        let comps = self.components();
        if comps.is_empty() {
            return None;
        }
        if comps.len() == 1 {
            return Some(VirtualPath::root());
        }
        Some(VirtualPath::parse(&format!("/{}", comps[..comps.len() - 1].join("/"))))
    }

    pub fn join(&self, name: &str) -> VirtualPath {
        if self.is_root() {
            VirtualPath::parse(&format!("/{name}"))
        } else {
            VirtualPath::parse(&format!("{}/{name}", self.0))
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    File,
    Directory,
}

#[derive(Clone, Debug)]
pub struct Attr {
    pub kind: FileKind,
    pub size: u64,
    pub created_at: u64,
    pub modified_at: u64,
}

#[derive(Clone, Debug)]
pub struct DirEntry {
    pub name: String,
    pub kind: FileKind,
}

/// Engine-level error, kept as a small closed set so OS mount shims
/// (`moku-volume-mount`) can map each variant to the right errno /
/// NTSTATUS without string-sniffing (plan §2).
#[derive(Debug, thiserror::Error)]
pub enum VolumeFsError {
    #[error("no such file or directory")]
    NotFound,
    #[error("file or directory already exists")]
    AlreadyExists,
    #[error("not a directory")]
    NotADirectory,
    #[error("is a directory")]
    IsADirectory,
    #[error("directory not empty")]
    NotEmpty,
    #[error("name too long")]
    NameTooLong,
    #[error("volume is full (quota exceeded)")]
    QuotaExceeded,
    #[error("invalid file handle")]
    BadFileHandle,
    /// Another engine (another process, or another handle in this one)
    /// already holds this volume's `VolumeLock`.
    #[error("volume is already open (mounted or in use by another moku process)")]
    Locked,
    /// An offset/length past what a u64 file size can address.
    #[error("file too large")]
    FileTooLarge,
    /// On-disk bytes that aren't in the volume format (bad header magic, a
    /// truncated block, a malformed name or `.moku_dir_iv`, ...).
    #[error("corrupt volume data: {0}")]
    Corrupt(String),
    /// A directory with encrypted entries lost its `.moku_dir_iv`: its
    /// names can't be decrypted until the file is restored. Never
    /// "repaired" by minting a new IV (that would orphan every entry).
    #[error(
        "{} is missing its .moku_dir_iv: the names in this directory can't be decrypted without it (restore it from a backup)",
        dir.display()
    )]
    MissingDirIv { dir: std::path::PathBuf },
    /// AEAD authentication failed for a content block or a file name:
    /// wrong key, or the bytes were modified/rolled back on disk.
    #[error("integrity check failed for a {0} (wrong key, or it was modified on disk)")]
    Integrity(&'static str),
    #[error("failed to {what}")]
    IoAt {
        what: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Io(std::io::Error),
    #[error(transparent)]
    Write(#[from] moku_sys::util::AtomicWriteError),
    #[error(transparent)]
    Crypto(#[from] moku_crypto::CryptoError),
}

impl VolumeFsError {
    pub(crate) fn io_at(what: &'static str) -> impl FnOnce(std::io::Error) -> Self {
        move |source| VolumeFsError::IoAt { what, source }
    }
}

impl From<std::io::Error> for VolumeFsError {
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::NotFound => Self::NotFound,
            std::io::ErrorKind::AlreadyExists => Self::AlreadyExists,
            _ => Self::Io(e),
        }
    }
}

pub type VResult<T> = std::result::Result<T, VolumeFsError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_normalizes_trailing_slash() {
        assert_eq!(VirtualPath::parse("/notes/").as_str(), "/notes");
    }

    #[test]
    fn test_parse_adds_leading_slash() {
        assert_eq!(VirtualPath::parse("notes").as_str(), "/notes");
    }

    #[test]
    fn test_root_components_empty() {
        assert!(VirtualPath::root().components().is_empty());
    }

    #[test]
    fn test_components_split_nested_path() {
        let p = VirtualPath::parse("/a/b/c.md");
        assert_eq!(p.components(), vec!["a", "b", "c.md"]);
    }

    #[test]
    fn test_parent_of_top_level_is_root() {
        let p = VirtualPath::parse("/a.md");
        assert_eq!(p.parent().unwrap(), VirtualPath::root());
    }

    #[test]
    fn test_parent_of_nested_path() {
        let p = VirtualPath::parse("/a/b/c.md");
        assert_eq!(p.parent().unwrap(), VirtualPath::parse("/a/b"));
    }

    #[test]
    fn test_root_has_no_parent() {
        assert!(VirtualPath::root().parent().is_none());
    }

    #[test]
    fn test_join_from_root() {
        assert_eq!(VirtualPath::root().join("a.md"), VirtualPath::parse("/a.md"));
    }

    #[test]
    fn test_join_from_nested() {
        let p = VirtualPath::parse("/a");
        assert_eq!(p.join("b.md"), VirtualPath::parse("/a/b.md"));
    }
}
