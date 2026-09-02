use std::path::PathBuf;
use std::sync::OnceLock;

use crate::NotificationRequest;

const AUMID: &str = "com.hardhus.moku";
const ICON_ICO_BYTES: &[u8] = include_bytes!("../../assets/moku.ico");

pub(crate) fn send(req: NotificationRequest) {
    ensure_registered();

    if let Err(e) = notify_rust::Notification::new()
        .app_id(AUMID)
        .summary(&req.title)
        .body(&req.body)
        .timeout(notify_rust::Timeout::Milliseconds(7000))
        .show()
    {
        tracing::warn!("Bildirim gönderilemedi: {e:?}");
    }
    // Tıklama aksiyonu v1'de Windows'ta desteklenmiyor — WinRT toast'ın
    // Activated event'i bir COM activator kaydı gerektiriyor, bu ayrı ve
    // ağır bir iş (bkz. plan sonundaki "kapsam dışı").
}

/// Süreç başına BİR KEZ: registry'ye AUMID + ikon kaydı yapar ve mevcut
/// process'i o AUMID ile ilişkilendirir. Hatalar sessizce loglanır —
/// kayıt başarısız olsa bile bildirim varsayılan ikonla gösterilmeye
/// devam eder (çökme yok).
pub(crate) fn ensure_registered() {
    static DONE: OnceLock<()> = OnceLock::new();
    DONE.get_or_init(|| {
        if let Err(e) = register() {
            tracing::warn!(
                "Windows bildirim kaydı başarısız (bildirimler varsayılan ikonla devam eder): {e:?}"
            );
        }
    });
}

fn register() -> anyhow::Result<()> {
    use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
    use windows::core::PCWSTR;
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let icon_path = extract_icon()?;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(format!(r"Software\Classes\AppUserModelId\{AUMID}"))?;
    key.set_value("DisplayName", &"Moku")?;
    key.set_value("IconUri", &icon_path.to_string_lossy().to_string())?;

    let wide: Vec<u16> = AUMID.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        SetCurrentProcessExplicitAppUserModelID(PCWSTR(wide.as_ptr()))?;
    }
    Ok(())
}

fn extract_icon() -> anyhow::Result<PathBuf> {
    let data_dir = moku_core::dirs::get_data_dir()?;
    std::fs::create_dir_all(&data_dir)?;
    let path = data_dir.join("moku.ico");
    std::fs::write(&path, ICON_ICO_BYTES)?;
    Ok(path)
}
