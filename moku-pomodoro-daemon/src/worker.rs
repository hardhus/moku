//! The pomodoro daemon's own process: `run_worker` is its entry point
//! (invoked via the hidden `moku pomodoro run-worker` subcommand, re-
//! exec'd the same way `moku-volume-daemon`'s mount worker is). Starts
//! **idle-but-listening** — the pid file and control channel are up
//! immediately, but no timer runs until the first `Start` request arrives,
//! so "spawn a fresh daemon and start it" and "send a new plan to an
//! already-running idle daemon" are the exact same code path.
//!
//! When a plan runs all the way to completion (or a `Skip` walks past the
//! last phase), the daemon notifies (unless it was a `Skip`, which is
//! always silent — see `model::Cursor::advance`'s doc comment) and then
//! exits the process entirely: there is no "idle, plan finished" state to
//! linger in, since a client just restarts it fresh either way.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use moku_notify::NotificationRequest;
use tokio::time::Instant;

use crate::control::{self, Connection, Listener};
use crate::model::{AdvanceOutcome, Cursor, PhaseKind, Plan};
use crate::protocol::{PhaseSnapshot, PomodoroRequest, PomodoroResponse, StatusSnapshot};

struct ActiveRun {
    plan: Plan,
    cursor: Cursor,
    notify: bool,
    paused: bool,
    /// Instant the current phase naturally expires. `None` while paused
    /// (see `remaining_at_pause`).
    deadline: Option<Instant>,
    /// Remaining duration snapshotted at the moment of pausing —
    /// `Resume` recomputes `deadline = Instant::now() + this`, avoiding
    /// drift from naive per-second decrementing.
    remaining_at_pause: Option<Duration>,
}

impl ActiveRun {
    fn start(plan: Plan, notify: bool) -> Result<ActiveRun, &'static str> {
        crate::model::validate(&plan).map_err(|_| "plan has no reachable phase or a 0-minute phase")?;
        let Some(cursor) = Cursor::start(&plan) else {
            return Err("plan has no reachable phase");
        };
        let minutes = cursor
            .current_phase(&plan)
            .map(|p| p.minutes)
            .unwrap_or(0);
        Ok(ActiveRun {
            plan,
            cursor,
            notify,
            paused: false,
            deadline: Some(Instant::now() + Duration::from_secs(minutes as u64 * 60)),
            remaining_at_pause: None,
        })
    }

    fn set_deadline_for_current_phase(&mut self) {
        let minutes = self
            .cursor
            .current_phase(&self.plan)
            .map(|p| p.minutes)
            .unwrap_or(0);
        let remaining = Duration::from_secs(minutes as u64 * 60);
        if self.paused {
            self.remaining_at_pause = Some(remaining);
            self.deadline = None;
        } else {
            self.deadline = Some(Instant::now() + remaining);
            self.remaining_at_pause = None;
        }
    }
}

pub async fn run_worker() -> Result<()> {
    crate::pid::write().context("failed to write pid file")?;
    let mut listener = match Listener::bind().await {
        Ok(l) => l,
        Err(e) => {
            crate::pid::remove();
            return Err(e);
        }
    };

    let result = main_loop(&mut listener).await;
    crate::pid::remove();
    result
}

async fn deadline_sleep(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending::<()>().await,
    }
}

async fn main_loop(listener: &mut Listener) -> Result<()> {
    let mut run: Option<ActiveRun> = None;

    loop {
        let next_deadline = run.as_ref().and_then(|r| r.deadline);

        tokio::select! {
            _ = deadline_sleep(next_deadline) => {
                let Some(active) = run.as_mut() else { continue };
                match active.cursor.advance(&active.plan) {
                    AdvanceOutcome::Next => {
                        let phase = active
                            .cursor
                            .current_phase(&active.plan)
                            .expect("AdvanceOutcome::Next implies a current phase");
                        if active.notify {
                            send_notification(phase_transition_body(phase.kind, phase.minutes));
                        }
                        active.set_deadline_for_current_phase();
                    }
                    AdvanceOutcome::Complete => {
                        if active.notify {
                            send_notification("Plan tamamlandı! 🎉".to_string());
                        }
                        return Ok(());
                    }
                }
            }
            accepted = listener.accept() => {
                let mut conn = accepted.context("failed to accept a control connection")?;
                if handle_connection(&mut conn, &mut run).await {
                    return Ok(());
                }
            }
            _ = tokio::signal::ctrl_c() => {
                return Ok(());
            }
        }
    }
}

fn phase_transition_body(kind: PhaseKind, minutes: u32) -> String {
    match kind {
        PhaseKind::Work => format!("Çalışma başlıyor ({minutes} dk)"),
        PhaseKind::Break => format!("Mola başlıyor ({minutes} dk)"),
        PhaseKind::LongBreak => format!("Uzun mola başlıyor ({minutes} dk)"),
    }
}

fn send_notification(body: String) {
    moku_notify::send(NotificationRequest {
        title: "Pomodoro".to_string(),
        body,
        action: None,
    });
}

/// Reads one request off `conn`, dispatches it, and replies. Returns
/// `true` if the daemon should now exit (an explicit `Stop`, or a `Skip`
/// that walked past the plan's last phase — see the module doc comment
/// for why that also exits rather than idling).
async fn handle_connection(conn: &mut Connection, run: &mut Option<ActiveRun>) -> bool {
    let Ok(req) = conn.recv_request().await else {
        return false; // connection dropped before a full request arrived
    };
    let (resp, should_stop) = dispatch(req, run);
    let _ = conn.send_response(&resp).await; // best-effort; client may already be gone
    should_stop
}

fn dispatch(req: PomodoroRequest, run: &mut Option<ActiveRun>) -> (PomodoroResponse, bool) {
    match req {
        PomodoroRequest::Start { plan, notify } => match ActiveRun::start(plan, notify) {
            Ok(active) => {
                *run = Some(active);
                (PomodoroResponse::Ok, false)
            }
            Err(message) => (
                PomodoroResponse::Error {
                    message: message.to_string(),
                },
                false,
            ),
        },
        PomodoroRequest::Pause => with_active(run, |active| {
            if !active.paused && let Some(deadline) = active.deadline {
                active.remaining_at_pause = Some(deadline.saturating_duration_since(Instant::now()));
                active.deadline = None;
                active.paused = true;
            }
            PomodoroResponse::Ok
        }),
        PomodoroRequest::Resume => with_active(run, |active| {
            if active.paused {
                let remaining = active.remaining_at_pause.take().unwrap_or_default();
                active.deadline = Some(Instant::now() + remaining);
                active.paused = false;
            }
            PomodoroResponse::Ok
        }),
        PomodoroRequest::Reset => with_active(run, |active| match active.cursor.reset(&active.plan) {
            Some(()) => {
                active.paused = false;
                active.set_deadline_for_current_phase();
                PomodoroResponse::Ok
            }
            None => PomodoroResponse::Error {
                message: "plan has no reachable phase".to_string(),
            },
        }),
        PomodoroRequest::Skip => {
            let Some(active) = run.as_mut() else {
                return (
                    PomodoroResponse::Error {
                        message: "no plan running".to_string(),
                    },
                    false,
                );
            };
            match active.cursor.advance(&active.plan) {
                AdvanceOutcome::Next => {
                    active.set_deadline_for_current_phase();
                    (PomodoroResponse::Ok, false)
                }
                // Silent, like every Skip outcome — and the plan is now
                // finished, so the daemon exits just like a natural
                // completion would (no lingering idle state either way).
                AdvanceOutcome::Complete => {
                    *run = None;
                    (PomodoroResponse::Ok, true)
                }
            }
        }
        PomodoroRequest::Stop => {
            *run = None;
            (PomodoroResponse::Ok, true)
        }
        PomodoroRequest::QueryStatus => (PomodoroResponse::Status(status_snapshot(run)), false),
    }
}

fn with_active(
    run: &mut Option<ActiveRun>,
    f: impl FnOnce(&mut ActiveRun) -> PomodoroResponse,
) -> (PomodoroResponse, bool) {
    match run.as_mut() {
        Some(active) => (f(active), false),
        None => (
            PomodoroResponse::Error {
                message: "no plan running".to_string(),
            },
            false,
        ),
    }
}

fn status_snapshot(run: &Option<ActiveRun>) -> StatusSnapshot {
    let Some(active) = run else {
        return StatusSnapshot {
            paused: false,
            current_phase: None,
            plan_complete: false,
        };
    };
    let current_phase = active.cursor.current_phase(&active.plan).map(|p| {
        let total_secs = p.minutes * 60;
        let remaining_secs = if active.paused {
            active
                .remaining_at_pause
                .map(|d| d.as_secs_f64())
                .unwrap_or(total_secs as f64)
        } else {
            active
                .deadline
                .map(|d| d.saturating_duration_since(Instant::now()).as_secs_f64())
                .unwrap_or(0.0)
        };
        PhaseSnapshot {
            kind: p.kind,
            label: p.label.map(str::to_string),
            total_secs,
            remaining_secs,
        }
    });
    StatusSnapshot {
        paused: active.paused,
        current_phase,
        // The daemon always fully exits on completion (see the module doc
        // comment) rather than lingering in an "idle, plan finished"
        // state, so a live QueryStatus never actually observes this as
        // true — kept in the wire format for a future daemon revision
        // that might want to report a just-finished run before exiting.
        plan_complete: false,
    }
}

// --- Client-side helpers shared by the CLI and the TUI's daemon_client ---

const SPAWN_RETRY_ATTEMPTS: usize = 20;
const SPAWN_RETRY_DELAY: Duration = Duration::from_millis(100);

fn expect_ok(result: Result<PomodoroResponse>) -> Result<()> {
    match result? {
        PomodoroResponse::Ok => Ok(()),
        PomodoroResponse::Error { message } => Err(anyhow!(message)),
        PomodoroResponse::Status(_) => Ok(()),
    }
}

/// If a daemon is already running, sends it `Start` directly. Otherwise
/// spawns a fresh worker process and retries the connection briefly until
/// it comes up. Either way, the caller ends up with a running plan.
pub async fn ensure_running_and_start(plan: Plan, notify: bool) -> Result<()> {
    crate::model::validate(&plan).map_err(|e| anyhow!("invalid pomodoro plan: {e}"))?;
    let req = PomodoroRequest::Start { plan, notify };

    if let Ok(resp) = control::send_request(&req).await {
        return expect_ok(Ok(resp));
    }

    spawn_worker()?;

    for _ in 0..SPAWN_RETRY_ATTEMPTS {
        tokio::time::sleep(SPAWN_RETRY_DELAY).await;
        if let Ok(resp) = control::send_request(&req).await {
            return expect_ok(Ok(resp));
        }
    }
    bail!("pomodoro daemon didn't respond after starting")
}

fn spawn_worker() -> Result<()> {
    let exe = std::env::current_exe().context("failed to resolve current executable")?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("pomodoro").arg("run-worker");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP — same rationale as
        // moku-volume-daemon's mount worker spawn: no console flash, and a
        // Ctrl+C in the parent's terminal doesn't also kill this worker.
        cmd.creation_flags(0x08000000 | 0x00000200);
    }
    cmd.spawn().context("failed to spawn pomodoro daemon")?;
    Ok(())
}

/// Stopping an already-stopped daemon is a no-op, not an error — matches
/// `daemon_client::query_status`'s "unreachable == not running" contract.
pub async fn stop() -> Result<()> {
    match control::send_request(&PomodoroRequest::Stop).await {
        Ok(resp) => expect_ok(Ok(resp)),
        Err(_) => Ok(()),
    }
}

pub async fn pause() -> Result<()> {
    expect_ok(control::send_request(&PomodoroRequest::Pause).await)
}

pub async fn resume() -> Result<()> {
    expect_ok(control::send_request(&PomodoroRequest::Resume).await)
}

pub async fn reset() -> Result<()> {
    expect_ok(control::send_request(&PomodoroRequest::Reset).await)
}

pub async fn skip() -> Result<()> {
    expect_ok(control::send_request(&PomodoroRequest::Skip).await)
}

pub async fn query_status() -> Result<Option<StatusSnapshot>> {
    match control::send_request(&PomodoroRequest::QueryStatus).await {
        Ok(PomodoroResponse::Status(s)) => Ok(Some(s)),
        Ok(PomodoroResponse::Ok) => Ok(None),
        Ok(PomodoroResponse::Error { message }) => Err(anyhow!(message)),
        Err(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Block, RepeatCount};

    fn tiny_plan() -> Plan {
        vec![
            Block::Phase {
                kind: PhaseKind::Work,
                minutes: 1,
                label: None,
            },
            Block::Phase {
                kind: PhaseKind::Break,
                minutes: 1,
                label: None,
            },
        ]
    }

    #[test]
    fn test_active_run_start_rejects_invalid_plan() {
        let empty: Plan = Vec::new();
        assert!(ActiveRun::start(empty, true).is_err());
    }

    #[test]
    fn test_active_run_start_sets_deadline_for_first_phase() {
        let active = ActiveRun::start(tiny_plan(), true).unwrap();
        assert!(active.deadline.is_some());
        assert!(!active.paused);
    }

    #[test]
    fn test_dispatch_pause_then_resume_preserves_remaining_without_advancing_cursor() {
        let active = ActiveRun::start(tiny_plan(), true).unwrap();
        let mut run = Some(active);

        let (resp, stop) = dispatch(PomodoroRequest::Pause, &mut run);
        assert!(matches!(resp, PomodoroResponse::Ok));
        assert!(!stop);
        assert!(run.as_ref().unwrap().paused);
        assert!(run.as_ref().unwrap().deadline.is_none());

        let (resp, _) = dispatch(PomodoroRequest::Resume, &mut run);
        assert!(matches!(resp, PomodoroResponse::Ok));
        assert!(!run.as_ref().unwrap().paused);
        assert!(run.as_ref().unwrap().deadline.is_some());

        // Still on the same (first) phase — pause/resume never touch the
        // cursor.
        let phase = run.as_ref().unwrap().cursor.current_phase(&run.as_ref().unwrap().plan);
        assert_eq!(phase.map(|p| p.kind), Some(PhaseKind::Work));
    }

    #[test]
    fn test_dispatch_skip_advances_cursor_without_notification_side_effects() {
        let active = ActiveRun::start(tiny_plan(), true).unwrap();
        let mut run = Some(active);
        let (resp, stop) = dispatch(PomodoroRequest::Skip, &mut run);
        assert!(matches!(resp, PomodoroResponse::Ok));
        assert!(!stop);
        let phase = run.as_ref().unwrap().cursor.current_phase(&run.as_ref().unwrap().plan);
        assert_eq!(phase.map(|p| p.kind), Some(PhaseKind::Break));
    }

    #[test]
    fn test_dispatch_skip_past_last_phase_completes_and_signals_stop() {
        let active = ActiveRun::start(tiny_plan(), true).unwrap();
        let mut run = Some(active);
        dispatch(PomodoroRequest::Skip, &mut run); // -> Break
        let (resp, stop) = dispatch(PomodoroRequest::Skip, &mut run); // -> Complete
        assert!(matches!(resp, PomodoroResponse::Ok));
        assert!(stop, "skipping past the last phase should exit the daemon");
        assert!(run.is_none());
    }

    #[test]
    fn test_dispatch_reset_returns_to_first_phase_and_unpauses() {
        let active = ActiveRun::start(tiny_plan(), true).unwrap();
        let mut run = Some(active);
        dispatch(PomodoroRequest::Skip, &mut run);
        dispatch(PomodoroRequest::Pause, &mut run);

        let (resp, _) = dispatch(PomodoroRequest::Reset, &mut run);
        assert!(matches!(resp, PomodoroResponse::Ok));
        assert!(!run.as_ref().unwrap().paused);
        let phase = run.as_ref().unwrap().cursor.current_phase(&run.as_ref().unwrap().plan);
        assert_eq!(phase.map(|p| p.kind), Some(PhaseKind::Work));
    }

    #[test]
    fn test_dispatch_stop_clears_run_and_signals_stop() {
        let active = ActiveRun::start(tiny_plan(), true).unwrap();
        let mut run = Some(active);
        let (resp, stop) = dispatch(PomodoroRequest::Stop, &mut run);
        assert!(matches!(resp, PomodoroResponse::Ok));
        assert!(stop);
        assert!(run.is_none());
    }

    #[test]
    fn test_dispatch_pause_without_active_run_is_an_error_not_a_panic() {
        let mut run: Option<ActiveRun> = None;
        let (resp, stop) = dispatch(PomodoroRequest::Pause, &mut run);
        assert!(matches!(resp, PomodoroResponse::Error { .. }));
        assert!(!stop);
    }

    #[test]
    fn test_status_snapshot_reports_idle_when_no_run() {
        let snapshot = status_snapshot(&None);
        assert!(!snapshot.paused);
        assert!(snapshot.current_phase.is_none());
        assert!(!snapshot.plan_complete);
    }

    #[test]
    fn test_status_snapshot_reports_paused_remaining_from_snapshot_not_wall_clock() {
        let active = ActiveRun::start(tiny_plan(), true).unwrap();
        let mut run = Some(active);
        dispatch(PomodoroRequest::Pause, &mut run);
        let snapshot = status_snapshot(&run);
        assert!(snapshot.paused);
        let phase = snapshot.current_phase.expect("phase present");
        // remaining_at_pause is snapshotted once at Pause time and never
        // recomputed from the wall clock while paused, so it should read
        // as (just under) the full phase length regardless of how long
        // the test itself takes to run this far.
        assert!(
            (59.0..=60.0).contains(&phase.remaining_secs),
            "{}",
            phase.remaining_secs
        );
    }

    #[test]
    fn test_status_snapshot_remaining_secs_has_genuine_sub_second_precision() {
        // Proves remaining_secs is real, continuous precision (not
        // truncated to whole seconds) — two snapshots a few ms apart must
        // differ, and differ by roughly that many ms.
        let active = ActiveRun::start(tiny_plan(), true).unwrap();
        let run = Some(active);
        let a = status_snapshot(&run).current_phase.unwrap().remaining_secs;
        std::thread::sleep(Duration::from_millis(20));
        let b = status_snapshot(&run).current_phase.unwrap().remaining_secs;
        assert!(
            a != b,
            "remaining_secs should carry genuine sub-second precision, not be truncated to whole seconds"
        );
        assert!(b < a, "remaining time should have counted down");
        assert!(
            (a - b) < 1.0,
            "two snapshots 20ms apart should differ by well under a second"
        );
    }

    #[test]
    fn test_phase_transition_body_mentions_minutes() {
        assert!(phase_transition_body(PhaseKind::Work, 45).contains("45"));
        assert!(phase_transition_body(PhaseKind::Break, 15).contains("15"));
        assert!(phase_transition_body(PhaseKind::LongBreak, 30).contains("30"));
    }

    #[test]
    fn test_repeat_infinite_never_reaches_stop_via_skip() {
        let plan: Plan = vec![Block::Repeat {
            count: RepeatCount::Infinite,
            blocks: tiny_plan(),
        }];
        let active = ActiveRun::start(plan, true).unwrap();
        let mut run = Some(active);
        for _ in 0..20 {
            let (_, stop) = dispatch(PomodoroRequest::Skip, &mut run);
            assert!(!stop, "an infinite plan should never signal stop via skip");
        }
    }
}
