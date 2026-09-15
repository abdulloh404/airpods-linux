//! ควบคุม AACP microphone session, audio engine และ recovery lifecycle
//!
//! worker นี้รอ AirPods ที่เลือกให้เชื่อม, เปิด AACP และ PipeWire virtual microphone แล้วส่ง AAC-ELD
//! access units เข้า audio engine ระหว่าง stream จะรับ config, listening mode, battery notification และ shutdown
//! พร้อม watchdog สำหรับเสียงเงียบ, decoder failure และ queue congestion ก่อน cleanup แล้ว reconnect ด้วย backoff

use std::time::Duration;

use airpods_audio::{AudioConfig, AudioEngine, PushOutcome};
use airpods_core::aacp::{AacpSession, ListeningMode};
use airpods_core::battery::parse_aacp_battery;
use airpods_core::framing::{demux_audio_sdu, is_audio_sdu};
use anyhow::{Context, bail};
use tokio::sync::{Mutex, mpsc, oneshot, watch};

use crate::{
    bluez,
    service::{DesiredAudio, Event, ModeRequest, SharedState},
    workers::AacpBatteryEvent,
};

/// เวลาสูงสุดตั้งแต่เริ่ม stream จน audio engine queue รับ access unit แรกได้
const FIRST_AUDIO_TIMEOUT: Duration = Duration::from_secs(3);
/// เวลาสูงสุดที่ไม่มี access unit ถูก queue หลัง stream เคยมีเสียงแล้ว
const AUDIO_STALL_TIMEOUT: Duration = Duration::from_secs(3);
/// จำนวน decode error ติดต่อกันที่ถือว่า AAC-ELD stream เสียและควร reconnect
const DECODE_ERROR_LIMIT: u32 = 64;
/// จำนวน queue-full ติดต่อกันที่ถือว่า PipeWire consumer ไม่ระบายข้อมูล
const QUEUE_FULL_LIMIT: u32 = 256;
/// เวลารอหลังสั่งเริ่ม microphone ก่อนสลับ A2DP profile เพื่อฟื้น playback route
const A2DP_RESET_DELAY: Duration = Duration::from_millis(800);
/// เวลารอให้ AirPods จัดการ STOP packet ก่อน reset A2DP profile
const AACP_STOP_SETTLE_DELAY: Duration = Duration::from_millis(200);
/// เวลารอให้ Bluetooth stack ส่ง listening-mode packet ก่อนปิด session ชั่วคราว
const AACP_CONTROL_SETTLE_DELAY: Duration = Duration::from_millis(100);
/// ระยะห่างของการขอ AACP battery notification ซ้ำ
const AACP_NOTIFICATION_RETRY_DELAY: Duration = Duration::from_secs(2);
/// จำนวนครั้งสูงสุดที่ขอ battery notification ซ้ำหลัง feature negotiation
const AACP_NOTIFICATION_RETRY_LIMIT: u8 = 2;
/// เวลาสูงสุดของแต่ละคำสั่ง `pactl` เพื่อไม่ให้ audio lifecycle ค้าง
const PACTL_TIMEOUT: Duration = Duration::from_secs(2);
/// จำนวนครั้งที่พยายามคืน A2DP profile เดิมหลังปิด profile ชั่วคราว
const A2DP_RESTORE_ATTEMPTS: usize = 3;
/// ระยะรอระหว่างความพยายามคืน A2DP profile
const A2DP_RESTORE_RETRY_DELAY: Duration = Duration::from_millis(100);

/// profile ที่ปิดแล้วแต่ยังคืนไม่สำเร็จ ใช้ serialize A2DP reset และ recovery ข้ามรอบ
static A2DP_PENDING_RESTORE: Mutex<Option<(String, String)>> = Mutex::const_new(None);

/// เหตุผลที่การเชื่อมหรือ stream หนึ่งรอบสิ้นสุด
enum AttemptExit {
    /// desired audio state เปลี่ยนและต้องเริ่มรอบใหม่โดยไม่เพิ่ม reconnect counter
    Reconfigure,
    /// daemon กำลังปิดหรือ channel สำคัญถูกปิด
    Shutdown,
    /// hardware, protocol หรือ audio pipeline ล้มเหลวและควรเข้าสู่ backoff
    Failed(String),
}

/// ผลของขั้นตอน START ที่แยก protocol completion ออกจาก cancellation ระหว่างรอ
enum StartAudioExit {
    /// AirPods ตอบผลของ START ตามปกติ
    Completed(anyhow::Result<()>),
    /// config หรือ shutdown เปลี่ยนก่อน START เสร็จและต้อง cleanup session
    Cancelled(AttemptExit),
}

/// ทำ desired audio state ให้เป็นจริงต่อเนื่องและ reconnect เมื่อ stream ล้มเหลว
pub async fn lifecycle_loop(
    state: SharedState,
    events: mpsc::UnboundedSender<Event>,
    mut desired: watch::Receiver<DesiredAudio>,
    mut mode_requests: mpsc::Receiver<ModeRequest>,
    aacp_battery_events: mpsc::UnboundedSender<AacpBatteryEvent>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut reconnect_attempt = 0_u32;

    loop {
        if *shutdown.borrow() {
            break;
        }
        // clone ค่า watch ล่าสุดเป็นเป้าหมายคงที่ของหนึ่ง connection attempt
        let target = desired.borrow().clone();
        if !target.enabled {
            set_audio_status(&state, &events, "idle", false, 0, None).await;
            reconnect_attempt = 0;
            if !wait_for_action(&mut desired, &mut mode_requests, &mut shutdown).await {
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
            if !wait_for_action(&mut desired, &mut mode_requests, &mut shutdown).await {
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
        match stream_once(
            &state,
            &events,
            &mut desired,
            &mut mode_requests,
            &aacp_battery_events,
            &mut shutdown,
            target,
        )
        .await
        {
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
                // exponential backoff เพิ่มจาก 1 ถึง 32 วินาทีและรีเซ็ตทันทีเมื่อมีคำสั่งใหม่
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
                    Some(request) = mode_requests.recv() => {
                        send_mode_without_audio(request).await;
                        reconnect_attempt = 0;
                    }
                }
            }
        }
    }

    set_audio_status(&state, &events, "idle", false, 0, None).await;
}

/// เปิดและดูแล AACP กับ audio engine หนึ่งรอบจน reconfigure, shutdown หรือ failure
async fn stream_once(
    state: &SharedState,
    events: &mpsc::UnboundedSender<Event>,
    desired: &mut watch::Receiver<DesiredAudio>,
    mode_requests: &mut mpsc::Receiver<ModeRequest>,
    aacp_battery_events: &mpsc::UnboundedSender<AacpBatteryEvent>,
    shutdown: &mut watch::Receiver<bool>,
    mut target: DesiredAudio,
) -> AttemptExit {
    let address: bluer::Address = match target.address.parse().context("invalid Bluetooth address")
    {
        Ok(address) => address,
        Err(error) => return AttemptExit::Failed(error.to_string()),
    };
    // ทุกขั้นตอน startup ที่อาจรอนานเปิดทางให้ desired state, mode request และ shutdown ยกเลิกได้
    let connected = tokio::select! {
        biased;
        result = bluez::wait_until_connected(address) => result,
        changed = desired.changed() => {
            return if changed.is_err() {
                AttemptExit::Shutdown
            } else {
                AttemptExit::Reconfigure
            };
        }
        _ = shutdown.changed() => return AttemptExit::Shutdown,
        Some(request) = mode_requests.recv() => {
            send_mode_without_audio(request).await;
            return AttemptExit::Reconfigure;
        }
    };
    if let Err(error) = connected {
        return AttemptExit::Failed(error.to_string());
    }
    if let Some(exit) = stale_startup(desired, shutdown, &target) {
        return exit;
    }
    let connection = tokio::select! {
        biased;
        result = AacpSession::connect(address) => result,
        changed = desired.changed() => {
            return if changed.is_err() {
                AttemptExit::Shutdown
            } else {
                AttemptExit::Reconfigure
            };
        }
        _ = shutdown.changed() => return AttemptExit::Shutdown,
        Some(request) = mode_requests.recv() => {
            send_mode_without_audio(request).await;
            return AttemptExit::Reconfigure;
        }
    };
    let mut session = match connection {
        Ok(session) => session,
        Err(error) => return AttemptExit::Failed(error.to_string()),
    };
    let initialization = tokio::select! {
        biased;
        result = session.initialize() => result,
        changed = desired.changed() => {
            return if changed.is_err() {
                AttemptExit::Shutdown
            } else {
                AttemptExit::Reconfigure
            };
        }
        _ = shutdown.changed() => return AttemptExit::Shutdown,
        Some(request) = mode_requests.recv() => {
            send_mode_without_audio(request).await;
            return AttemptExit::Reconfigure;
        }
    };
    if let Err(error) = initialization {
        return AttemptExit::Failed(error.to_string());
    }
    if let Some(exit) = stale_startup(desired, shutdown, &target) {
        return exit;
    }

    // สร้าง engine ด้วย processing values ของ target เดียวกับที่ตรวจ stale แล้ว
    let mut audio_config = AudioConfig::default();
    audio_config.gain_db = target.gain_db as f32;
    audio_config.limiter_dbfs = target.limiter_db as f32;
    let mut engine = match AudioEngine::new(audio_config) {
        Ok(engine) => engine,
        Err(error) => return AttemptExit::Failed(error.to_string()),
    };
    if let Some(exit) = stale_startup(desired, shutdown, &target) {
        return exit;
    }
    if let Err(error) = engine.start() {
        return AttemptExit::Failed(error.to_string());
    }
    if let Some(exit) = stale_startup(desired, shutdown, &target) {
        let _ = engine.stop();
        return exit;
    }
    // START อาจส่งถึง AirPods แล้วแม้ caller ยกเลิก จึงรวมทุกทางออกไว้ให้ cleanup เดียวกัน
    let start = tokio::select! {
        result = session.start_audio() => StartAudioExit::Completed(result),
        changed = desired.changed() => {
            let exit = if changed.is_err() {
                AttemptExit::Shutdown
            } else {
                AttemptExit::Reconfigure
            };
            StartAudioExit::Cancelled(exit)
        }
        _ = shutdown.changed() => StartAudioExit::Cancelled(AttemptExit::Shutdown),
    };
    match start {
        StartAudioExit::Completed(Ok(())) => {}
        StartAudioExit::Completed(Err(error)) => {
            stop_stream(&mut session, &mut engine, &target.address).await;
            return AttemptExit::Failed(error.to_string());
        }
        StartAudioExit::Cancelled(exit) => {
            stop_stream(&mut session, &mut engine, &target.address).await;
            return exit;
        }
    }
    if let Some(exit) = stale_startup(desired, shutdown, &target) {
        stop_stream(&mut session, &mut engine, &target.address).await;
        return exit;
    }

    // reset A2DP แบบหน่วงเวลาใน task แยกเพื่อเริ่มรับ AACP packet ได้ทันที
    let reset_address = target.address.clone();
    let (cancel_reset, reset_cancelled) = oneshot::channel();
    let start_reset = tokio::spawn(async move {
        tokio::select! {
            _ = tokio::time::sleep(A2DP_RESET_DELAY) => {
                if let Err(error) = reset_a2dp(&reset_address).await {
                    eprintln!("failed to reset A2DP after microphone START: {error}");
                }
            }
            _ = reset_cancelled => {}
        }
    });

    set_audio_status(state, events, "streaming", true, 0, None).await;
    // buffer เดียวรองรับ L2CAP SDU ขนาดสูงสุดและนำกลับมาใช้ทุก receive
    let mut packet = vec![0_u8; 65_535];
    let stream_started = tokio::time::Instant::now();
    let mut last_queued_audio: Option<tokio::time::Instant> = None;
    let mut decode_errors = 0_u32;
    let mut queue_full = 0_u32;
    let mut watchdog = tokio::time::interval(Duration::from_millis(500));
    let notification_retry = tokio::time::sleep(AACP_NOTIFICATION_RETRY_DELAY);
    tokio::pin!(notification_retry);
    let mut notification_retry_pending = false;
    let mut notification_retries = 0_u8;
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
                    // gain และ limiter เปลี่ยนใน engine เดิมได้โดยไม่ตัด AACP stream
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
            Some(request) = mode_requests.recv() => {
                // ใช้ session ปัจจุบันเฉพาะเมื่อ address ยังตรงกับ snapshot ตอนรับ D-Bus request
                let result = if request.address.eq_ignore_ascii_case(&target.address) {
                    session
                        .set_listening_mode(request.mode)
                        .await
                        .map_err(|error| error.to_string())
                } else {
                    Err("selected AirPods changed before the mode command was sent".to_string())
                };
                request.respond(result);
            }
            _ = watchdog.tick() => {
                // ก่อน audio แรกให้นับจาก START และหลังจากนั้นให้นับจาก access unit ล่าสุดที่ queue สำเร็จ
                let timed_out = match last_queued_audio {
                    Some(last_audio) => last_audio.elapsed() >= AUDIO_STALL_TIMEOUT,
                    None => stream_started.elapsed() >= FIRST_AUDIO_TIMEOUT,
                };
                if timed_out {
                    break AttemptExit::Failed("AirPods microphone stream produced no usable audio for 3 seconds".to_string());
                }
            }
            _ = &mut notification_retry, if notification_retry_pending => {
                // บาง firmware ไม่ส่ง battery หลัง request แรก จึง retry แบบจำกัดโดยไม่หยุด audio
                notification_retries = notification_retries.saturating_add(1);
                if let Err(error) = session.request_notifications().await {
                    eprintln!("failed to retry AACP notifications: {error}");
                }
                if notification_retries < AACP_NOTIFICATION_RETRY_LIMIT {
                    notification_retry
                        .as_mut()
                        .reset(tokio::time::Instant::now() + AACP_NOTIFICATION_RETRY_DELAY);
                } else {
                    notification_retry_pending = false;
                }
            }
            received = session.recv(&mut packet) => {
                let received = match received {
                    Ok(0) => break AttemptExit::Failed("AirPods disconnected".to_string()),
                    Ok(received) => received,
                    Err(error) => break AttemptExit::Failed(error.to_string()),
                };
                let sdu = &packet[..received];
                if AacpSession::is_handshake_ack(sdu) {
                    // handshake ack เปิดขั้นตอนตั้ง feature ที่ใช้รับ battery notification
                    if let Err(error) = session.set_specific_features().await {
                        eprintln!("failed to configure AACP battery notifications: {error}");
                    }
                    continue;
                }
                if AacpSession::is_features_ack(sdu) {
                    // หลัง AirPods ยอมรับ feature แล้วจึงสมัคร notification และติดตั้ง retry timer
                    if let Err(error) = session.request_notifications().await {
                        eprintln!("failed to request AACP battery notifications: {error}");
                    }
                    notification_retries = 0;
                    notification_retry_pending = true;
                    notification_retry
                        .as_mut()
                        .reset(tokio::time::Instant::now() + AACP_NOTIFICATION_RETRY_DELAY);
                    continue;
                }
                if let Some(update) = parse_aacp_battery(sdu) {
                    // battery worker รวม partial update และเลือกแหล่งข้อมูลแทน audio loop
                    let _ = aacp_battery_events.send(AacpBatteryEvent::Update(update));
                    notification_retry_pending = false;
                    continue;
                }
                if !is_audio_sdu(sdu) {
                    continue;
                }
                let access_units = match demux_audio_sdu(sdu) {
                    Ok(access_units) => access_units,
                    Err(error) => break AttemptExit::Failed(error.to_string()),
                };
                for access_unit in access_units {
                    match engine.push_access_unit(access_unit) {
                        Ok(PushOutcome::Queued) => {
                            // ความสำเร็จตัดลำดับ error ทั้งสองชนิดและเลื่อน watchdog ไปที่ packet นี้
                            last_queued_audio = Some(tokio::time::Instant::now());
                            decode_errors = 0;
                            queue_full = 0;
                        }
                        Ok(PushOutcome::DecodeError) => {
                            // QueueFull ไม่ได้นับต่อเป็น decoder failure ติดต่อกัน
                            decode_errors = decode_errors.saturating_add(1);
                            queue_full = 0;
                            if decode_errors >= DECODE_ERROR_LIMIT {
                                break 'stream AttemptExit::Failed(
                                    "AAC-ELD decoder rejected 64 consecutive access units".to_string(),
                                );
                            }
                        }
                        Ok(PushOutcome::QueueFull) => {
                            // DecodeError ไม่ได้นับต่อเป็น queue congestion ติดต่อกัน
                            decode_errors = 0;
                            queue_full = queue_full.saturating_add(1);
                            if queue_full >= QUEUE_FULL_LIMIT {
                                break 'stream AttemptExit::Failed(
                                    "PipeWire audio queue remained full for 256 access units".to_string(),
                                );
                            }
                        }
                        Err(error) => break 'stream AttemptExit::Failed(error.to_string()),
                    }
                }
            }
        }
    };

    // ยกเลิก delayed reset ถ้ายังไม่เริ่ม แล้วรอ task จบก่อน cleanup เพื่อไม่ให้ pactl ทำงานซ้อนกัน
    let _ = cancel_reset.send(());
    let _ = start_reset.await;
    stop_stream(&mut session, &mut engine, &target.address).await;
    let _ = aacp_battery_events.send(AacpBatteryEvent::SessionEnded);
    set_audio_status(state, events, "stopping", false, 0, None).await;
    outcome
}

/// หยุด AACP stream, คืน A2DP route และปิด audio engine สำหรับทุกทางออกหลัง START
async fn stop_stream(session: &mut AacpSession, engine: &mut AudioEngine, address: &str) {
    let was_started = session.is_audio_started();
    let stopped = match session.stop_audio().await {
        Ok(()) => true,
        Err(error) => {
            eprintln!("failed to stop AACP microphone stream cleanly: {error}");
            false
        }
    };
    // reset profile เฉพาะ session ที่เคย START เพื่อไม่รบกวน playback จาก startup failure ก่อนหน้านั้น
    if was_started {
        if stopped {
            tokio::time::sleep(AACP_STOP_SETTLE_DELAY).await;
        }
        if let Err(error) = reset_a2dp(address).await {
            eprintln!("failed to reset A2DP after microphone STOP: {error}");
        }
    }
    let _ = engine.stop();
}

/// สลับ active A2DP profile ผ่าน `off` แล้วคืนค่าเดิมเพื่อให้ PipeWire สร้าง playback route ใหม่
async fn reset_a2dp(address: &str) -> anyhow::Result<()> {
    let mut pending_restore = A2DP_PENDING_RESTORE.lock().await;
    // คืน profile ที่ค้างจากรอบก่อนให้สำเร็จก่อนเริ่ม reset ใหม่ ขณะถือ mutex เพื่อกัน task ซ้อน
    if let Some((card, profile)) = pending_restore.as_ref() {
        restore_card_profile(card, profile)
            .await
            .context("failed to recover a pending A2DP profile restoration")?;
        *pending_restore = None;
    }

    let card = format!("bluez_card.{}", address.replace(':', "_"));
    let output = run_pactl(&["list", "cards"])
        .await
        .context("failed to inspect PipeWire cards with pactl")?;
    if !output.status.success() {
        bail!(
            "pactl list cards failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    // `LC_ALL=C` ใน `run_pactl` ทำให้ prefix ที่ parse ด้านล่างคงรูปภาษาอังกฤษ
    let mut in_card = false;
    let mut active_profile = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim();
        if line.starts_with("Card #") {
            in_card = false;
        } else if let Some(name) = line.strip_prefix("Name: ") {
            in_card = name == card;
        } else if in_card {
            if let Some(profile) = line.strip_prefix("Active Profile: ") {
                active_profile = Some(profile.to_owned());
                break;
            }
        }
    }

    // profile ชนิดอื่นไม่ใช่ playback A2DP ที่ flow นี้ควรสลับ
    let Some(profile) = active_profile.filter(|profile| profile.starts_with("a2dp-sink")) else {
        return Ok(());
    };
    // บันทึก recovery target ก่อนสั่ง off เพื่อให้รอบถัดไปคืน profile ได้หากคำสั่ง restore ล้มเหลว
    *pending_restore = Some((card.clone(), profile.clone()));
    let off_result = set_card_profile(&card, "off").await;
    let restore_result = restore_card_profile(&card, &profile).await;
    if restore_result.is_ok() {
        *pending_restore = None;
    }
    restore_result?;
    off_result
}

/// คืน card profile ด้วย retry ระยะสั้นเพื่อรองรับ BlueZ/PipeWire ที่ยังสร้าง route ไม่พร้อม
async fn restore_card_profile(card: &str, profile: &str) -> anyhow::Result<()> {
    let mut restore_error = None;
    for attempt in 0..A2DP_RESTORE_ATTEMPTS {
        match set_card_profile(card, profile).await {
            Ok(()) => return Ok(()),
            Err(error) => restore_error = Some(error),
        }
        if attempt + 1 < A2DP_RESTORE_ATTEMPTS {
            tokio::time::sleep(A2DP_RESTORE_RETRY_DELAY).await;
        }
    }

    Err(restore_error.expect("at least one A2DP restore attempt must run"))
}

/// เรียก `pactl set-card-profile` และแปลง exit status กับ stderr เป็น error
async fn set_card_profile(card: &str, profile: &str) -> anyhow::Result<()> {
    let output = run_pactl(&["set-card-profile", card, profile])
        .await
        .with_context(|| format!("failed to set {card} profile to {profile}"))?;
    if output.status.success() {
        Ok(())
    } else {
        bail!(
            "failed to set {card} profile to {profile}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
}

/// รัน `pactl` ด้วย locale คงที่และ timeout ที่ยกเลิก child process เมื่อ future ถูก drop
async fn run_pactl(args: &[&str]) -> anyhow::Result<std::process::Output> {
    let mut command = tokio::process::Command::new("pactl");
    command.env("LC_ALL", "C").args(args).kill_on_drop(true);
    tokio::time::timeout(PACTL_TIMEOUT, command.output())
        .await
        .context("pactl command timed out")?
        .context("failed to run pactl")
}

/// ตรวจว่าค่าเป้าหมายหรือ shutdown เปลี่ยนระหว่างจุด startup ที่ไม่สามารถ cancel กลาง call ได้
fn stale_startup(
    desired: &mut watch::Receiver<DesiredAudio>,
    shutdown: &watch::Receiver<bool>,
    target: &DesiredAudio,
) -> Option<AttemptExit> {
    if *shutdown.borrow() {
        return Some(AttemptExit::Shutdown);
    }
    if &*desired.borrow_and_update() != target {
        return Some(AttemptExit::Reconfigure);
    }
    None
}

/// รอ config, listening mode หรือ shutdown ขณะ microphone ไม่มี active session
async fn wait_for_action(
    desired: &mut watch::Receiver<DesiredAudio>,
    mode_requests: &mut mpsc::Receiver<ModeRequest>,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    tokio::select! {
        changed = desired.changed() => changed.is_ok(),
        changed = shutdown.changed() => changed.is_ok() && !*shutdown.borrow(),
        Some(request) = mode_requests.recv() => {
            send_mode_without_audio(request).await;
            true
        }
    }
}

/// เปิด AACP control session ชั่วคราวสำหรับ listening mode แล้วตอบผลกลับไปยัง caller
async fn send_mode_without_audio(request: ModeRequest) {
    let result = send_mode_once(&request.address, request.mode)
        .await
        .map_err(|error| error.to_string());
    request.respond(result);
}

/// ตรวจ connection แล้วส่ง listening mode ผ่าน AACP session แบบครั้งเดียว
async fn send_mode_once(address: &str, mode: ListeningMode) -> anyhow::Result<()> {
    let address = address
        .parse()
        .context("invalid selected Bluetooth address")?;
    if !bluez::is_connected(address).await? {
        bail!("selected AirPods are not connected");
    }

    let session = AacpSession::connect(address).await?;
    session.initialize().await?;
    session.set_listening_mode(mode).await?;
    // เปิดเวลาให้ Bluetooth stack ส่ง packet ก่อนปิด AACP session ชั่วคราว
    tokio::time::sleep(AACP_CONTROL_SETTLE_DELAY).await;
    Ok(())
}

/// อัปเดต audio fields ใน shared state และส่ง status snapshot ไปยัง D-Bus event forwarder
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
        // เก็บ error ล่าสุดระหว่าง recovery และล้างเมื่อกลับมา streaming สำเร็จ
        if let Some(error) = error {
            state.status.last_error = error;
        } else if mic_active {
            state.status.last_error.clear();
        }
        state.status.clone()
    };
    let _ = events.send(Event::Status(status));
}
