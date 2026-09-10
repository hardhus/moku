//! The daemon's control-channel wire format: what a client (CLI/TUI) can
//! ask for, and what the daemon answers. Framed as a 4-byte little-endian
//! length prefix followed by that many bytes of `serde_json` — chosen over
//! newline-delimited JSON because a `Phase`'s `label` is free-typed text
//! that could contain an embedded newline, which a line-based framing
//! would mishandle.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::model::{PhaseKind, Plan};

/// A cap on any single message's declared length, purely to reject an
/// obviously-corrupt length prefix (e.g. a stray byte from a wrong
/// protocol version) instead of trying to allocate gigabytes for it.
const MAX_FRAME_LEN: u32 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PomodoroRequest {
    /// `notify` comes from the client's own resolved config
    /// (`PomodoroConfig.notify` in `modules/moku-pomodoro`) — the daemon
    /// crate itself has no dependency on that config schema, so the
    /// caller resolves and passes the flag in with every `Start`.
    Start { plan: Plan, notify: bool },
    Pause,
    Resume,
    Reset,
    Skip,
    Stop,
    QueryStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum PomodoroResponse {
    Ok,
    Status(StatusSnapshot),
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StatusSnapshot {
    pub paused: bool,
    pub current_phase: Option<PhaseSnapshot>,
    pub plan_complete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PhaseSnapshot {
    pub kind: PhaseKind,
    pub label: Option<String>,
    pub total_secs: u32,
    /// Full-precision remaining time (`Duration::as_secs_f64()`), not
    /// truncated to whole seconds — the client's highest detail tiers
    /// (`DetailLevel::Centiseconds`/`Full` in `modules/moku-pomodoro`)
    /// need genuinely live sub-second digits, not a display trick applied
    /// to an already-truncated value.
    pub remaining_secs: f64,
}

pub async fn write_framed<T: Serialize>(
    stream: &mut (impl AsyncWrite + Unpin),
    msg: &T,
) -> Result<()> {
    let bytes = serde_json::to_vec(msg).context("failed to serialize protocol message")?;
    let len: u32 = bytes
        .len()
        .try_into()
        .context("protocol message too large to frame")?;
    stream
        .write_all(&len.to_le_bytes())
        .await
        .context("failed to write frame length")?;
    stream
        .write_all(&bytes)
        .await
        .context("failed to write frame body")?;
    Ok(())
}

pub async fn read_framed<T: for<'de> Deserialize<'de>>(
    stream: &mut (impl AsyncRead + Unpin),
) -> Result<T> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .context("failed to read frame length")?;
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_LEN {
        bail!("frame length {len} exceeds the {MAX_FRAME_LEN}-byte cap");
    }
    let mut body = vec![0u8; len as usize];
    stream
        .read_exact(&mut body)
        .await
        .context("failed to read frame body")?;
    serde_json::from_slice(&body).context("failed to deserialize protocol message")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Block, RepeatCount};

    #[tokio::test]
    async fn test_request_round_trips_through_framing() {
        let plan: Plan = vec![Block::Repeat {
            count: RepeatCount::Infinite,
            blocks: vec![
                Block::Repeat {
                    count: RepeatCount::Finite(3),
                    blocks: vec![
                        Block::Phase {
                            kind: PhaseKind::Work,
                            minutes: 45,
                            label: None,
                        },
                        Block::Phase {
                            kind: PhaseKind::Break,
                            minutes: 15,
                            label: Some("short break".to_string()),
                        },
                    ],
                },
                Block::Phase {
                    kind: PhaseKind::LongBreak,
                    minutes: 30,
                    label: None,
                },
            ],
        }];
        let req = PomodoroRequest::Start {
            plan,
            notify: true,
        };

        let mut buf: Vec<u8> = Vec::new();
        write_framed(&mut buf, &req).await.unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let decoded: PomodoroRequest = read_framed(&mut cursor).await.unwrap();

        match (req, decoded) {
            (
                PomodoroRequest::Start {
                    plan: a,
                    notify: na,
                },
                PomodoroRequest::Start {
                    plan: b,
                    notify: nb,
                },
            ) => {
                assert_eq!(a, b);
                assert_eq!(na, nb);
            }
            _ => panic!("expected Start variant"),
        }
    }

    #[tokio::test]
    async fn test_every_request_variant_round_trips() {
        for req in [
            PomodoroRequest::Pause,
            PomodoroRequest::Resume,
            PomodoroRequest::Reset,
            PomodoroRequest::Skip,
            PomodoroRequest::Stop,
            PomodoroRequest::QueryStatus,
        ] {
            let mut buf: Vec<u8> = Vec::new();
            write_framed(&mut buf, &req).await.unwrap();
            let mut cursor = std::io::Cursor::new(buf);
            let _decoded: PomodoroRequest = read_framed(&mut cursor).await.unwrap();
        }
    }

    #[tokio::test]
    async fn test_status_response_round_trips() {
        let resp = PomodoroResponse::Status(StatusSnapshot {
            paused: true,
            current_phase: Some(PhaseSnapshot {
                kind: PhaseKind::Work,
                label: None,
                total_secs: 2700,
                remaining_secs: 42.5,
            }),
            plan_complete: false,
        });
        let mut buf: Vec<u8> = Vec::new();
        write_framed(&mut buf, &resp).await.unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let decoded: PomodoroResponse = read_framed(&mut cursor).await.unwrap();
        match (resp, decoded) {
            (PomodoroResponse::Status(a), PomodoroResponse::Status(b)) => assert_eq!(a, b),
            _ => panic!("expected Status variant"),
        }
    }

    #[tokio::test]
    async fn test_oversized_frame_length_is_rejected() {
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&(MAX_FRAME_LEN + 1).to_le_bytes());
        let mut cursor = std::io::Cursor::new(buf);
        let result: Result<PomodoroRequest> = read_framed(&mut cursor).await;
        assert!(result.is_err());
    }
}
