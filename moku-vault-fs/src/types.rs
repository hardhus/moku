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
/// (`moku-vault-mount`) can map each variant to the right errno /
/// NTSTATUS without string-sniffing (plan §2).
#[derive(Debug)]
pub enum VaultFsError {
    NotFound,
    AlreadyExists,
    NotADirectory,
    IsADirectory,
    NotEmpty,
    NameTooLong,
    QuotaExceeded,
    BadFileHandle,
    Other(anyhow::Error),
}

impl std::fmt::Display for VaultFsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "no such file or directory"),
            Self::AlreadyExists => write!(f, "file or directory already exists"),
            Self::NotADirectory => write!(f, "not a directory"),
            Self::IsADirectory => write!(f, "is a directory"),
            Self::NotEmpty => write!(f, "directory not empty"),
            Self::NameTooLong => write!(f, "name too long"),
            Self::QuotaExceeded => write!(f, "volume is full (quota exceeded)"),
            Self::BadFileHandle => write!(f, "invalid file handle"),
            Self::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for VaultFsError {}

impl From<anyhow::Error> for VaultFsError {
    fn from(e: anyhow::Error) -> Self {
        Self::Other(e)
    }
}

impl From<std::io::Error> for VaultFsError {
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::NotFound => Self::NotFound,
            std::io::ErrorKind::AlreadyExists => Self::AlreadyExists,
            _ => Self::Other(e.into()),
        }
    }
}

pub type VResult<T> = std::result::Result<T, VaultFsError>;

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
