/// Bir bildirime tıklandığında yapılacak eylem.
///
/// v1'de sadece Linux/BSD'de gerçekten tetikleniyor (bkz. README notu) —
/// ama veri modeli baştan platform-bağımsız tasarlandı. Windows/macOS
/// desteği eklendiğinde bu enum'a dokunmaya gerek kalmayacak, sadece
/// platform-özel `send()` implementasyonları genişleyecek.
#[derive(Clone, Debug)]
pub enum NotificationAction {
    OpenUrl(String),
}

// Sadece xdg.rs (Linux/BSD backend) çağırıyor; Windows/macOS derlemesinde
// gerçekten kullanılmıyor (leftover değil — platform-özel çağıran o
// derlemelere hiç girmiyor). Uyarı sadece bu iki platformda susturuluyor,
// Linux'ta gerçek bir dead-code oluşursa yine yakalanır.
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
pub(crate) fn execute(action: &NotificationAction) {
    match action {
        NotificationAction::OpenUrl(url) => {
            if let Err(e) = moku_core::util::open_url(url) {
                tracing::warn!("Bildirim tıklaması: URL açılamadı: {e}");
            }
        }
    }
}
