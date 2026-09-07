//! เชื่อม AACP เข้ากับ audio engine และ reconnect โดยไม่ปิด daemon

use std::time::Duration;

use airpods_audio::{AudioConfig, AudioEngine};
use airpods_core::aacp::AacpSession;
use airpods_core::framing::{demux_audio_sdu, is_audio_sdu};
use anyhow::Context;
use tokio::sync::{mpsc, watch};

use crate::service::{DesiredAudio, Event, SharedState};

enum AttemptExit {
    Reconfigure,
    Shutdown,
    Failed(String),
}

pub async fn lifecycle_loop(
    state: SharedState,
    events: mpsc::UnboundedSender<Event>,
    mut desired: watch::Receiver<DesiredAudio>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut reconnect_attempt = 0_u32;

    loop {
        if *shutdown.borrow() {
            break;
        }
        let target = desired.borrow().clone();
        if !target.enabled {
            set_audio_status(&state, &events, "idle", false, 0, None).await;
            reconnect_attempt = 0;
            if !wait_for_change(&mut desired, &mut shutdown).await {
                break;
            }
            continue;
        }
        if target.address.is_empty() {
            set_audio_status(
                &state,
                &events,
                "error",
                false,
                0,
                Some("no AirPods device is selected".to_string()),
            )
            .await;
            if !wait_for_change(&mut desired, &mut shutdown).await {
                break;
            }
            continue;
        }

        set_audio_status(
            &state,
            &events,
            "connecting",
            false,
            reconnect_attempt,
            None,
        )
        .await;
        match stream_once(&state, &events, &mut desired, &mut shutdown, target).await {
            AttemptExit::Shutdown => break,
            AttemptExit::Reconfigure => {
                reconnect_attempt = 0;
            }
            AttemptExit::Failed(error) => {
                reconnect_attempt = reconnect_attempt.saturating_add(1);
                set_audio_status(
                    &state,
                    &events,
                    "recovering",
                    false,
                    reconnect_attempt,
                    Some(error),
                )
                .await;
                let seconds = 1_u64 << reconnect_attempt.saturating_sub(1).min(5);
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(seconds)) => {}
                    changed = desired.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        reconnect_attempt = 0;
                    }
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            break;
                        }
                    }
                }
            }
        }
    }

    set_audio_status(&state, &events, "idle", false, 0, None).await;
}

async fn stream_once(
    state: &SharedState,
    events: &mpsc::UnboundedSender<Event>,
    desired: &mut watch::Receiver<DesiredAudio>,
    shutdown: &mut watch::Receiver<bool>,
    mut target: DesiredAudio,
) -> AttemptExit {
    let address: bluer::Address = match target.address.parse().context("invalid Bluetooth address") {
        Ok(address) => address,
        Err(error) => return AttemptExit::Failed(error.to_string()),
    };
    let mut session = match AacpSession::connect(address).await {
        Ok(session) => session,
        Err(error) => return AttemptExit::Failed(error.to_string()),
    };
    if let Err(error) = session.initialize().await {
        return AttemptExit::Failed(error.to_string());
    }

    let mut audio_config = AudioConfig::default();
    audio_config.gain_db = target.gain_db as f32;
    audio_config.limiter_dbfs = target.limiter_db as f32;
    let mut engine = match AudioEngine::new(audio_config) {
        Ok(engine) => engine,
        Err(error) => return AttemptExit::Failed(error.to_string()),
    };
    if let Err(error) = engine.start() {
        return AttemptExit::Failed(error.to_string());
    }
    if let Err(error) = session.start_audio().await {
        let _ = engine.stop();
        return AttemptExit::Failed(error.to_string());
    }

    set_audio_status(state, events, "streaming", true, 0, None).await;
    let mut packet = vec![0_u8; 65_535];
    let outcome = 'stream: loop {
        tokio::select! {
            changed = desired.changed() => {
                if changed.is_err() {
                    break AttemptExit::Shutdown;
                }
                let next = desired.borrow().clone();
                if !next.enabled || !next.address.eq_ignore_ascii_case(&target.address) {
                    break AttemptExit::Reconfigure;
                }
                if next.gain_db != target.gain_db || next.limiter_db != target.limiter_db {
                    if let Err(error) = engine.set_processing(next.gain_db as f32, next.limiter_db as f32) {
                        break AttemptExit::Failed(error.to_string());
                    }
                    target = next;
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break AttemptExit::Shutdown;
                }
            }
            received = session.recv(&mut packet) => {
                let received = match received {
                    Ok(0) => break AttemptExit::Failed("AirPods disconnected".to_string()),
                    Ok(received) => received,
                    Err(error) => break AttemptExit::Failed(error.to_string()),
                };
                let sdu = &packet[..received];
                if !is_audio_sdu(sdu) {
                    continue;
                }
                let access_units = match demux_audio_sdu(sdu) {
                    Ok(access_units) => access_units,
                    Err(error) => break AttemptExit::Failed(error.to_string()),
                };
                for access_unit in access_units {
                    if let Err(error) = engine.push_access_unit(access_unit) {
                        break 'stream AttemptExit::Failed(error.to_string());
                    }
                }
            }
        }
    };

    let _ = session.stop_audio().await;
    let _ = engine.stop();
    set_audio_status(state, events, "stopping", false, 0, None).await;
    outcome
}

async fn wait_for_change(
    desired: &mut watch::Receiver<DesiredAudio>,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    tokio::select! {
        changed = desired.changed() => changed.is_ok(),
        changed = shutdown.changed() => changed.is_ok() && !*shutdown.borrow(),
    }
}

async fn set_audio_status(
    state: &SharedState,
    events: &mpsc::UnboundedSender<Event>,
    name: &str,
    mic_active: bool,
    reconnect_attempt: u32,
    error: Option<String>,
) {
    let status = {
        let mut state = state.write().await;
        state.status.state = name.to_string();
        state.status.mic_active = mic_active;
        state.status.reconnect_attempt = reconnect_attempt;
        if let Some(error) = error {
            state.status.last_error = error;
        } else if mic_active {
            state.status.last_error.clear();
        }
        state.status.clone()
    };
    let _ = events.send(Event::Status(status));
}
