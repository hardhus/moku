use crate::NotificationRequest;

/// macOS'ta paketlenmemiş (bundle'sız) bir binary'nin özel isim/ikonla
/// bildirim göstermesi mümkün değil — bu Moku'nun değil, işletim
/// sisteminin kısıtlaması (UNUserNotificationCenter geçerli bir
/// code-signed .app bundle istiyor). Bildirim yine de gösterilir, sadece
/// varsayılan host-process markasıyla. Tıklama aksiyonu v1'de yok.
pub(crate) fn send(req: NotificationRequest) {
    if let Err(e) = notify_rust::Notification::new()
        .summary(&req.title)
        .body(&req.body)
        .show()
    {
        tracing::warn!("Bildirim gönderilemedi: {e:?}");
    }
}
