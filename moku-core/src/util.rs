use anyhow::{Result, bail};

/// Varsayılan tarayıcıda (ya da ilişkili uygulamada) bir URL/dosya açar.
/// RSS modülünün "tarayıcıda aç" tuşu VE bildirim tıklama aksiyonları
/// bu tek yeri paylaşır — mantık iki kez yazılmıyor. `url` bir RSS
/// feed'inden (uzak, güvenilmeyen bir kaynaktan) geldiği için önce şema
/// doğrulanıyor — sadece `http`/`https` kabul ediliyor.
pub fn open_url(url: &str) -> Result<()> {
    require_http_scheme(url)?;

    #[cfg(target_os = "windows")]
    {
        open_url_windows(url)?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
    }
    Ok(())
}

/// Rejects anything that isn't a well-formed `http`/`https` URL — split
/// out from `open_url` so it can be unit-tested without actually invoking
/// the OS's URL-open mechanism (which would launch a real browser).
fn require_http_scheme(url: &str) -> Result<()> {
    let parsed = url::Url::parse(url).map_err(|e| anyhow::anyhow!("invalid URL: {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("refusing to open a non-http(s) URL: {url}");
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
fn open_url_windows(url: &str) -> Result<()> {
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
        bail!(
            "ShellExecuteW failed to open URL (code {})",
            result.0 as isize
        );
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
        assert!(require_http_scheme("file:///etc/passwd").is_err());
        assert!(require_http_scheme("javascript:alert(1)").is_err());
        assert!(require_http_scheme("ftp://example.com").is_err());
    }

    #[test]
    fn test_require_http_scheme_rejects_malformed_urls() {
        assert!(require_http_scheme("not a url at all").is_err());
        assert!(require_http_scheme("").is_err());
    }
}
