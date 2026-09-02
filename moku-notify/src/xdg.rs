use crate::{NotificationRequest, actions};

const ICON_PNG_BYTES: &[u8] = include_bytes!("../../icon.png");

pub(crate) fn send(req: NotificationRequest) {
    let icon_path = extract_icon().ok();

    let mut notif = notify_rust::Notification::new();
    notif
        .appname("Moku")
        .summary(&req.title)
        .body(&req.body)
        .timeout(notify_rust::Timeout::Milliseconds(7000));

    if let Some(ref path) = icon_path {
        notif.icon(&path.to_string_lossy());
    }

    let has_action = req.action.is_some();
    if has_action {
        notif.action("default", "default");
    }

    match notif.show() {
        Ok(handle) => {
            if let Some(action) = req.action {
                // wait_for_action D-Bus üzerinde BLOKLAYAN bir çağrı —
                // tokio worker'ını tıkamamak için ayrı bir OS thread'inde
                // çalıştırıyoruz. Bu iş parçacığı bildirim kapanınca/
                // tıklanınca kendiliğinden biter.
                std::thread::spawn(move || {
                    handle.wait_for_action(|id| {
                        if id == "default" {
                            actions::execute(&action);
                        }
                    });
                });
            }
        }
        Err(e) => tracing::warn!("Bildirim gönderilemedi: {e:?}"),
    }
}

fn extract_icon() -> anyhow::Result<std::path::PathBuf> {
    let data_dir = moku_core::dirs::get_data_dir()?;
    std::fs::create_dir_all(&data_dir)?;
    let path = data_dir.join("moku-icon.png");
    std::fs::write(&path, ICON_PNG_BYTES)?;
    Ok(path)
}
