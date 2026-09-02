use anyhow::Result;

/// Varsayılan tarayıcıda (ya da ilişkili uygulamada) bir URL/dosya açar.
/// RSS modülünün "tarayıcıda aç" tuşu VE bildirim tıklama aksiyonları
/// bu tek yeri paylaşır — mantık iki kez yazılmıyor.
pub fn open_url(url: &str) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()?;
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
