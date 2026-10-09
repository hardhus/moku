use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// A failed [`atomic_write`]: which step failed, for which target file.
/// The target itself is untouched whenever this is returned.
#[derive(Debug, thiserror::Error)]
#[error("failed to {step} for {}", path.display())]
pub struct AtomicWriteError {
    pub path: PathBuf,
    pub step: &'static str,
    #[source]
    pub source: io::Error,
}

/// A URL [`open_url`] refused or failed to open.
#[derive(Debug, thiserror::Error)]
pub enum OpenUrlError {
    #[error("invalid URL")]
    Invalid(#[from] url::ParseError),
    #[error("refusing to open a non-http(s) URL: {0}")]
    UnsupportedScheme(String),
    #[error("failed to launch the URL handler")]
    Launch(#[source] io::Error),
    #[error("ShellExecuteW failed to open URL (code {0})")]
    ShellExecute(isize),
}

/// Writes `bytes` to `path` atomically: the data goes to a temp file in the
/// same directory, is fsynced, and is then renamed over `path` (replacing
/// it on Windows too). A crash or a full disk mid-write therefore leaves
/// either the complete old file or the complete new one, never a torn
/// mix. Every piece of moku state written to disk as a whole file
/// (`meta.json`, `volume.json`, `usage.json`, the volume index, config
/// files, ...) should go through this instead of a plain `fs::write`.
///
/// The parent directory must already exist.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), AtomicWriteError> {
    let fail = |step| {
        move |source| AtomicWriteError {
            path: path.to_path_buf(),
            step,
            source,
        }
    };
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(fail("create a temp file"))?;
    tmp.write_all(bytes).map_err(fail("write the temp file"))?;
    tmp.as_file()
        .sync_all()
        .map_err(fail("fsync the temp file"))?;
    tmp.persist(path)
        .map_err(|e| e.error)
        .map_err(fail("replace the file"))?;
    Ok(())
}

/// Async wrapper around [`atomic_write`] for callers on the tokio runtime,
/// so the fsync never blocks an async worker thread.
pub async fn atomic_write_async(path: PathBuf, bytes: Vec<u8>) -> Result<(), AtomicWriteError> {
    let target = path.clone();
    tokio::task::spawn_blocking(move || atomic_write(&path, &bytes))
        .await
        .map_err(|e| AtomicWriteError {
            path: target,
            step: "run the write task",
            source: io::Error::other(e),
        })?
}

/// Truncates `s` to at most `max_bytes` bytes without splitting a UTF-8
/// code point — plain `&s[..max_bytes]` panics ("byte index is not a char
/// boundary") whenever `max_bytes` lands mid-codepoint, which is a real
/// risk for any text of external origin (a git diff, an RSS feed's error
/// text, ...) that isn't guaranteed ASCII. Any byte-offset truncation of
/// such text should go through this helper instead of a raw slice index.
pub fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Varsayılan tarayıcıda (ya da ilişkili uygulamada) bir URL/dosya açar.
/// RSS modülünün "tarayıcıda aç" tuşu VE bildirim tıklama aksiyonları
/// bu tek yeri paylaşır — mantık iki kez yazılmıyor. `url` bir RSS
/// feed'inden (uzak, güvenilmeyen bir kaynaktan) geldiği için önce şema
/// doğrulanıyor — sadece `http`/`https` kabul ediliyor.
pub fn open_url(url: &str) -> Result<(), OpenUrlError> {
    require_http_scheme(url)?;

    #[cfg(target_os = "windows")]
    {
        open_url_windows(url)?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map_err(OpenUrlError::Launch)?;
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map_err(OpenUrlError::Launch)?;
    }
    Ok(())
}

/// Rejects anything that isn't a well-formed `http`/`https` URL — split
/// out from `open_url` so it can be unit-tested without actually invoking
/// the OS's URL-open mechanism (which would launch a real browser).
fn require_http_scheme(url: &str) -> Result<(), OpenUrlError> {
    let parsed = url::Url::parse(url)?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(OpenUrlError::UnsupportedScheme(url.to_string()));
    }
    Ok(())
}

/// Opens `url` via `ShellExecuteW` — the actual Win32 "run the default
/// handler for this" mechanism — instead of shelling out through
/// `cmd /C start`, which this used to do. `cmd.exe` re-parses its entire
/// command line for shell metacharacters (`&`, `|`, `^`, ...); since `url`
/// ultimately comes from a remote, attacker-influenceable RSS feed, a
/// malicious feed link previously had a real command-injection path
/// (Rust's own argv quoting doesn't help here — the vulnerable re-parsing
/// happens *inside* `cmd.exe` itself, after it receives whatever was
/// spawned). Calling the Shell API directly never invokes a command
/// interpreter, so there's nothing left to re-parse.
#[cfg(target_os = "windows")]
fn open_url_windows(url: &str) -> Result<(), OpenUrlError> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{HSTRING, PCWSTR};

    let operation = HSTRING::from("open");
    let file = HSTRING::from(url);
    // SAFETY: `operation` and `file` are valid, live `HSTRING`s for the
    // duration of this call; the remaining parameters are null (no
    // arguments/working directory needed to open a URL).
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(operation.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW returns a pseudo-HINSTANCE that's > 32 on success (a
    // legacy Win16 convention it still follows), not a real handle.
    if (result.0 as isize) <= 32 {
        return Err(OpenUrlError::ShellExecute(result.0 as isize));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_require_http_scheme_accepts_http_and_https() {
        assert!(require_http_scheme("http://example.com/feed").is_ok());
        assert!(require_http_scheme("https://example.com/feed").is_ok());
    }

    #[test]
    fn test_require_http_scheme_rejects_other_schemes() {
        // The exact injection this guards against: a malicious RSS feed
        // item whose `link` isn't a URL moku should ever hand to the OS's
        // default-handler mechanism at all.
        for url in ["file:///etc/passwd", "javascript:alert(1)", "ftp://example.com"] {
            assert!(
                matches!(require_http_scheme(url), Err(OpenUrlError::UnsupportedScheme(u)) if u == url),
                "{url} must be refused as an unsupported scheme"
            );
        }
    }

    #[test]
    fn test_require_http_scheme_rejects_malformed_urls() {
        assert!(matches!(require_http_scheme("not a url at all"), Err(OpenUrlError::Invalid(_))));
        assert!(matches!(require_http_scheme(""), Err(OpenUrlError::Invalid(_))));
    }

    #[test]
    fn test_atomic_write_creates_then_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        atomic_write(&path, b"first").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        atomic_write(&path, b"second, longer").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second, longer");
        // No temp files are left behind next to the target.
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("state.json")]);
    }

    #[test]
    fn test_atomic_write_fails_when_parent_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("state.json");
        let err = atomic_write(&path, b"x").unwrap_err();
        assert_eq!(err.path, path);
        assert_eq!(err.step, "create a temp file");
        assert!(!path.exists());
    }

    #[test]
    fn test_atomic_write_leftover_temp_file_does_not_touch_target() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        atomic_write(&path, b"good").unwrap();
        // Simulate a crash mid-write: a temp file was created and partially
        // written but never persisted.
        let mut stray = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
        stray.write_all(b"torn").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"good");
    }

    #[tokio::test]
    async fn test_atomic_write_async_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        atomic_write_async(path.clone(), b"async".to_vec())
            .await
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"async");
    }

    #[test]
    fn test_truncate_at_char_boundary_ascii() {
        assert_eq!(truncate_at_char_boundary("hello world", 5), "hello");
        assert_eq!(truncate_at_char_boundary("hi", 10), "hi");
        assert_eq!(truncate_at_char_boundary("hello", 0), "");
    }

    #[test]
    fn test_truncate_at_char_boundary_never_splits_a_codepoint() {
        // "é" is 2 bytes (U+00E9 in UTF-8: 0xC3 0xA9) — a byte offset of 1
        // would land mid-codepoint and panic on a raw `&s[..1]`.
        let s = "é";
        assert_eq!(s.len(), 2);
        assert_eq!(truncate_at_char_boundary(s, 1), "");
        assert_eq!(truncate_at_char_boundary(s, 2), "é");

        // A multi-byte emoji mid-string, offset landing inside it.
        let s = "ab🎉cd";
        let emoji_start = "ab".len();
        assert_eq!(truncate_at_char_boundary(s, emoji_start + 1), "ab");
    }
}
