mod actions;

#[cfg(not(any(windows, target_os = "macos")))]
mod xdg;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows_impl;

pub use actions::NotificationAction;

pub struct NotificationRequest {
    pub title: String,
    pub body: String,
    pub action: Option<NotificationAction>,
}

/// Bildirimi platforma uygun şekilde gösterir. Hata durumunda sessizce
/// `tracing::warn!` loglar — bildirim gönderimi hiçbir zaman çağıran
/// tarafın akışını (örn. RSS fetch döngüsünü) kesmemeli.
pub fn send(req: NotificationRequest) {
    #[cfg(not(any(windows, target_os = "macos")))]
    xdg::send(req);
    #[cfg(target_os = "macos")]
    macos::send(req);
    #[cfg(windows)]
    windows_impl::send(req);
}

/// Windows'ta AUMID/ikon kaydını erkenden dener (daemon başlangıcında
/// çağırmak için). ZORUNLU değil — `send()` ilk çağrısında aynı işi
/// tembel olarak zaten yapıyor; bu sadece hataların log'a erken
/// düşmesi için.
pub fn ensure_registered() {
    #[cfg(windows)]
    windows_impl::ensure_registered();
}
