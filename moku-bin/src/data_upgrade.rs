//! The TUI side of the data-format upgrade (audit P4): the full-pane
//! confirm prompt shown after the first unlock (or when entering an
//! encrypted module while an upgrade is pending), and the progress screen
//! while `moku_migrate::upgrade` runs on a blocking thread.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Text;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use moku_core::{ModuleId, MokuTheme};
use moku_migrate::{BACKUPS_DIR, DataFormat, DataState, MigrateError, Progress, UpgradeReport};
use moku_vault::VaultSession;

/// The moku-bin version, recorded in `FORMAT` by every upgrade/stamp.
pub(crate) const MOKU_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The data directory and, if it is behind, the format it is in.
pub(crate) struct DataDirStatus {
    pub dir: PathBuf,
    pub pending_upgrade: Option<DataFormat>,
}

pub(crate) type UpgradeTask = JoinHandle<Result<UpgradeReport, MigrateError>>;

/// What the upgrade UI is showing, if anything.
pub(crate) enum UpgradeScreen {
    /// Asking. `after` is where to go once answered.
    Prompt {
        after: ModuleId,
        daemon_running: bool,
    },
    /// `moku_migrate::upgrade` is running.
    Running {
        after: ModuleId,
        progress: watch::Receiver<Progress>,
        task: UpgradeTask,
    },
}

/// What happened to a running upgrade, for the app loop's `select!`.
pub(crate) enum UpgradeEvent {
    Progress,
    Done(Result<Result<UpgradeReport, MigrateError>, tokio::task::JoinError>),
}

/// Checks the data directory's format at startup, without unlocking.
///
/// - `Ok(None)`: current (or fresh). A missing `FORMAT` is written now;
///   failing to write it is only logged (it is retried next launch).
/// - `Ok(Some(from))`: behind; the TUI offers the upgrade after unlock.
/// - `Err`: newer than this build, or an unreadable `FORMAT` (fail
///   closed): the caller refuses to open the data.
pub(crate) fn startup_check(data_dir: &Path) -> anyhow::Result<Option<DataFormat>> {
    let state = moku_migrate::detect_format(data_dir)?;
    match state {
        DataState::Newer { found } => Err(MigrateError::NewerFormat {
            found,
            supported: moku_migrate::CURRENT_DATA_FORMAT,
        }
        .into()),
        DataState::Behind { from, .. } => Ok(Some(from)),
        DataState::Fresh | DataState::Current { .. } => {
            if let Err(e) = moku_migrate::stamp_if_needed(data_dir, state, MOKU_VERSION) {
                tracing::warn!(
                    "couldn't write the data FORMAT marker: {:#}",
                    anyhow::Error::from(e)
                );
            }
            Ok(None)
        }
    }
}

/// `Some(true)` = upgrade now, `Some(false)` = not now, `None` = keep
/// waiting (any other key).
pub(crate) fn prompt_response(ev: &Event) -> Option<bool> {
    let Event::Key(key) = ev else { return None };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    match key.code {
        KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => Some(true),
        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => Some(false),
        _ => None,
    }
}

/// Starts the upgrade on a blocking thread. The caller has already
/// stopped the daemon and closed its own sled handles.
pub(crate) fn start(
    data_dir: PathBuf,
    session: Arc<VaultSession>,
) -> (watch::Receiver<Progress>, UpgradeTask) {
    let (tx, rx) = watch::channel(Progress::default());
    let task = tokio::task::spawn_blocking(move || {
        moku_migrate::upgrade(&data_dir, &session, MOKU_VERSION, &|p| {
            // No receiver only means the screen went away; the upgrade
            // itself carries on and its result is still returned.
            let _ = tx.send(p);
        })
    });
    (rx, task)
}

/// Waits for the next progress update or the end of a running upgrade.
/// Pending forever when nothing is running, so it can sit in `select!`.
pub(crate) async fn next_event(screen: &mut Option<UpgradeScreen>) -> UpgradeEvent {
    let Some(UpgradeScreen::Running { progress, task, .. }) = screen else {
        return std::future::pending().await;
    };
    tokio::select! {
        changed = progress.changed() => match changed {
            Ok(()) => UpgradeEvent::Progress,
            // The sender is gone: the task has finished.
            Err(_) => UpgradeEvent::Done(task.await),
        },
        done = &mut *task => UpgradeEvent::Done(done),
    }
}

pub(crate) fn draw_prompt(
    f: &mut ratatui::Frame,
    area: Rect,
    theme: &MokuTheme,
    from: DataFormat,
    data_dir: &Path,
    daemon_running: bool,
) {
    let mut lines = vec![
        format!(
            "Your data is in format {from}; this moku uses {}.",
            moku_migrate::CURRENT_DATA_FORMAT
        ),
        String::new(),
        "Steps:".to_string(),
    ];
    for hop in moku_migrate::pending_hops(from) {
        lines.push(format!("  - {}", hop.description));
    }
    lines.push(String::new());
    lines.push("A backup of the data is saved first, to:".to_string());
    lines.push(format!("  {}", data_dir.join(BACKUPS_DIR).display()));
    if daemon_running {
        lines.push(String::new());
        lines.push("The background daemon will be stopped first.".to_string());
    }
    lines.push(String::new());
    lines.push(
        "Not now: encrypted modules stay closed until the update (asked again next launch; \
         `moku data upgrade` works too)."
            .to_string(),
    );
    lines.push(String::new());
    lines.push("[Enter/y] Update now   [Esc/n] Not now".to_string());

    let block = Block::default()
        .title(" Data format update ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.warning));
    let para = Paragraph::new(Text::from(lines.join("\n")))
        .block(block)
        .style(Style::default().fg(theme.base_fg).bg(theme.base_bg))
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

pub(crate) fn draw_running(
    f: &mut ratatui::Frame,
    area: Rect,
    theme: &MokuTheme,
    progress: &Progress,
) {
    let detail = if progress.hop.is_empty() {
        "Backing up the data...".to_string()
    } else {
        format!(
            "{}\n\n{}: {} record(s) converted",
            progress.hop, progress.module, progress.records
        )
    };
    let block = Block::default()
        .title(" Updating data format ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.info));
    let para = Paragraph::new(Text::from(format!(
        "{detail}\n\nPlease wait; don't close moku."
    )))
    .block(block)
    .style(Style::default().fg(theme.base_fg).bg(theme.base_bg))
    .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyEvent, KeyEventState, KeyModifiers};

    use super::*;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        })
    }

    fn render(draw: impl FnOnce(&mut ratatui::Frame)) -> String {
        let backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(draw).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn test_prompt_response_keys() {
        for code in [KeyCode::Enter, KeyCode::Char('y'), KeyCode::Char('Y')] {
            assert_eq!(prompt_response(&key(code)), Some(true));
        }
        for code in [KeyCode::Esc, KeyCode::Char('n'), KeyCode::Char('N')] {
            assert_eq!(prompt_response(&key(code)), Some(false));
        }
        assert_eq!(prompt_response(&key(KeyCode::Char('x'))), None);
    }

    #[test]
    fn test_prompt_shows_versions_steps_and_backup_location() {
        let theme = MokuTheme::default();
        let dir = PathBuf::from("C:/moku-test-data");
        let content = render(|f| draw_prompt(f, f.area(), &theme, DataFormat(0), &dir, true));
        assert!(content.contains("Data format update"));
        assert!(content.contains("format v0; this moku uses v1"));
        assert!(content.contains(moku_migrate::HOPS[0].description));
        assert!(content.contains("backups"));
        assert!(content.contains("daemon will be stopped"));
        assert!(content.contains("[Enter/y] Update now"));
    }

    #[test]
    fn test_running_screen_shows_progress() {
        let theme = MokuTheme::default();
        let progress = Progress {
            hop: "Some step",
            module: "todo".to_string(),
            records: 3,
        };
        let content = render(|f| draw_running(f, f.area(), &theme, &progress));
        assert!(content.contains("Some step"));
        assert!(content.contains("todo: 3 record(s) converted"));
    }
}
