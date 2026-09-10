//! CLI-side handlers for `moku pomodoro <start|list|stop|status|reset|run-worker>`.
//! `moku-bin/src/pomodoro_cmd.rs` is a thin dispatcher over these — same
//! split as `modules/moku-rss`'s `cli_module.rs` holding the real logic
//! while the top-level CLI only captures/routes args.

use anyhow::Result;
use moku_pomodoro_daemon::StatusSnapshot;

use crate::daemon_client;
use crate::engine::{CLASSIC_PROFILE_NAME, DetailLevel, PomodoroConfig, resolve_start_plan};

/// Starts (or re-sends the plan to an already-running) daemon using the
/// named profile, `default_profile`, or the built-in classic fallback —
/// see `engine::resolve_start_plan`, the same resolution the TUI's
/// Profile List "Enter" uses.
pub async fn start(config: &PomodoroConfig, name: Option<&str>) -> Result<String> {
    let (resolved_name, plan) =
        resolve_start_plan(config, name).map_err(|e| anyhow::anyhow!(e))?;
    daemon_client::start(plan, config.notify).await?;
    Ok(format!("Pomodoro daemon started ('{resolved_name}')."))
}

/// Lists every configured profile, marking `default_profile` and always
/// including the reserved `classic` fallback.
pub fn list(config: &PomodoroConfig) -> String {
    if config.profiles.is_empty() {
        return format!(
            "No profiles configured yet. '{CLASSIC_PROFILE_NAME}' is always available as a fallback.\nSet up real profiles from the Pomodoro screen in the TUI."
        );
    }
    let mut lines: Vec<String> = config
        .profiles
        .iter()
        .map(|p| {
            let is_default = config.default_profile.as_deref() == Some(p.name.as_str());
            format!("- {}{}", p.name, if is_default { " (default)" } else { "" })
        })
        .collect();
    lines.push(format!("- {CLASSIC_PROFILE_NAME} (built-in fallback)"));
    lines.join("\n")
}

pub async fn stop() -> Result<String> {
    daemon_client::stop().await?;
    Ok("Pomodoro daemon stopped.".to_string())
}

pub async fn reset() -> Result<String> {
    daemon_client::reset().await?;
    Ok("Pomodoro plan reset to its first phase.".to_string())
}

pub async fn status() -> Result<String> {
    match daemon_client::query_status().await? {
        None => Ok("Pomodoro daemon is not running.".to_string()),
        Some(snapshot) => Ok(format_status(&snapshot)),
    }
}

fn format_status(snapshot: &StatusSnapshot) -> String {
    match &snapshot.current_phase {
        None => "Pomodoro daemon is running (idle, no plan started).".to_string(),
        Some(phase) => {
            let state = if snapshot.paused { "paused" } else { "running" };
            format!(
                "{} ({state}) — {} remaining",
                phase.kind.label(),
                DetailLevel::Seconds.format(phase.remaining_secs)
            )
        }
    }
}

/// The hidden worker entry point, invoked as `moku pomodoro run-worker`.
pub async fn run_worker() -> Result<()> {
    moku_pomodoro_daemon::worker::run_worker().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Profile, classic_plan};
    use moku_pomodoro_daemon::PhaseKind;

    #[test]
    fn test_format_status_idle_daemon() {
        let snapshot = StatusSnapshot {
            paused: false,
            current_phase: None,
            plan_complete: false,
        };
        assert!(format_status(&snapshot).contains("idle"));
    }

    #[test]
    fn test_format_status_running_phase() {
        let snapshot = StatusSnapshot {
            paused: false,
            current_phase: Some(moku_pomodoro_daemon::PhaseSnapshot {
                kind: PhaseKind::Work,
                label: None,
                total_secs: 2700,
                remaining_secs: 754.0,
            }),
            plan_complete: false,
        };
        let text = format_status(&snapshot);
        assert!(text.contains("Work"));
        assert!(text.contains("12:34"));
        assert!(text.contains("running"));
    }

    #[test]
    fn test_format_status_paused_phase() {
        let snapshot = StatusSnapshot {
            paused: true,
            current_phase: Some(moku_pomodoro_daemon::PhaseSnapshot {
                kind: PhaseKind::Break,
                label: None,
                total_secs: 900,
                remaining_secs: 60.0,
            }),
            plan_complete: false,
        };
        assert!(format_status(&snapshot).contains("paused"));
    }

    #[tokio::test]
    async fn test_start_with_no_profiles_falls_back_to_classic_without_erroring() {
        let config = PomodoroConfig::default();
        // No daemon reachable in a unit test, so this will error trying to
        // reach it — but critically NOT because of plan resolution, which
        // this asserts by checking resolve_start_plan directly succeeds.
        let resolved = crate::engine::resolve_start_plan(&config, None);
        assert!(resolved.is_ok());
        assert_eq!(resolved.unwrap().1, classic_plan());
        let _ = start(&config, None).await; // exercised for coverage; daemon reachability not asserted here
    }

    #[test]
    fn test_list_empty_mentions_classic_fallback() {
        let config = PomodoroConfig::default();
        assert!(list(&config).contains(CLASSIC_PROFILE_NAME));
    }

    #[test]
    fn test_list_marks_default_profile() {
        let config = PomodoroConfig {
            profiles: vec![
                Profile {
                    name: "basic".to_string(),
                    plan: classic_plan(),
                },
                Profile {
                    name: "kitap".to_string(),
                    plan: classic_plan(),
                },
            ],
            default_profile: Some("kitap".to_string()),
            ..PomodoroConfig::default()
        };
        let text = list(&config);
        assert!(text.contains("kitap (default)"));
        assert!(text.contains("- basic"));
        assert!(!text.contains("basic (default)"));
    }
}
