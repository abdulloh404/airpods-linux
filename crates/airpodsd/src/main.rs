//! ประกอบและรัน daemon ซึ่งเป็นเจ้าของ BlueZ, AACP, audio engine และ battery bridge
//!
//! entrypoint โหลด config, เปิด session D-Bus, สร้าง channel สำหรับสื่อสารระหว่าง service กับ workers
//! แล้วรอ signal ปิดระบบ ก่อนส่ง shutdown และรอให้ audio, inventory และ battery lifecycle จบตามลำดับ

mod audio;
mod bluez;
mod config;
mod power;
mod service;
mod workers;

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use service::{DesiredAudio, Event, ManagerService, RuntimeState};
use tokio::sync::{RwLock, mpsc, watch};

use airpods_ipc::{BUS_NAME, MANAGER_INTERFACE, OBJECT_PATH};

#[tokio::main]
/// เริ่มส่วนประกอบทั้งหมดของ daemon และปิดงานที่ถือ resource เมื่อได้รับ shutdown signal
async fn main() -> Result<()> {
    let config_store = config::ConfigStore::from_xdg()?;
    let (config, config_error) = match config_store.load() {
        Ok(config) => (config, String::new()),
        Err(error) => (
            config::Config::default(),
            format!("failed to load config; using defaults: {error}"),
        ),
    };
    let state = Arc::new(RwLock::new(RuntimeState::new(
        &config,
        Path::new("/dev/airpods_power").exists(),
        config_error,
    )));
    let (desired_tx, desired_rx) = watch::channel(DesiredAudio::from_config(&config));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (mode_tx, mode_rx) = mpsc::channel(8);
    let (aacp_battery_tx, aacp_battery_rx) = mpsc::unbounded_channel();
    let (device_connection_tx, device_connection_rx) =
        watch::channel(None::<workers::SelectedDeviceConnection>);

    // D-Bus service แก้ config และส่ง desired state โดยไม่ถือ hardware resource เอง
    let service = ManagerService::new(
        state.clone(),
        config,
        config_store,
        desired_tx.clone(),
        mode_tx,
        event_tx.clone(),
    );
    let connection = zbus::connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, service)?
        .build()
        .await?;

    // แต่ละ worker รับ channel เฉพาะหน้าที่และแชร์ runtime snapshot ผ่าน RwLock
    let event_task = tokio::spawn(emit_events(connection.clone(), event_rx));
    let audio_task = tokio::spawn(audio::lifecycle_loop(
        state.clone(),
        event_tx.clone(),
        desired_rx,
        mode_rx,
        aacp_battery_tx,
        shutdown_rx.clone(),
    ));
    let inventory_task = tokio::spawn(workers::inventory_loop(
        state.clone(),
        event_tx.clone(),
        device_connection_tx,
        shutdown_rx.clone(),
    ));
    let battery_task = tokio::spawn(workers::battery_loop(
        state,
        event_tx,
        power::PowerBridge::default(),
        aacp_battery_rx,
        device_connection_rx,
        shutdown_rx,
    ));

    wait_for_shutdown().await?;
    // ปิดความต้องการ microphone ก่อนประกาศ shutdown เพื่อให้ audio lifecycle เริ่ม cleanup ทันที
    desired_tx.send_modify(|desired| desired.enabled = false);
    shutdown_tx.send_replace(true);

    let _ = audio_task.await;
    let _ = inventory_task.await;
    let _ = battery_task.await;
    // ปล่อย D-Bus connection ก่อนยกเลิก event forwarder ที่อาจกำลังรอ event ถัดไป
    drop(connection);
    event_task.abort();
    Ok(())
}

/// แปลง internal event เป็น D-Bus signal สำหรับ client ที่ subscribe อยู่
async fn emit_events(connection: zbus::Connection, mut events: mpsc::UnboundedReceiver<Event>) {
    while let Some(event) = events.recv().await {
        let result = match event {
            Event::Status(status) => {
                connection
                    .emit_signal(
                        None::<&str>,
                        OBJECT_PATH,
                        MANAGER_INTERFACE,
                        "StatusChanged",
                        &(status,),
                    )
                    .await
            }
            Event::Battery(battery) => {
                connection
                    .emit_signal(
                        None::<&str>,
                        OBJECT_PATH,
                        MANAGER_INTERFACE,
                        "BatteryChanged",
                        &(battery,),
                    )
                    .await
            }
            Event::Devices(devices) => {
                connection
                    .emit_signal(
                        None::<&str>,
                        OBJECT_PATH,
                        MANAGER_INTERFACE,
                        "DevicesChanged",
                        &(devices,),
                    )
                    .await
            }
        };
        if let Err(error) = result {
            eprintln!("failed to emit D-Bus signal: {error}");
        }
    }
}

#[cfg(unix)]
/// รอ Ctrl-C หรือ SIGTERM เพื่อให้ systemd และ interactive shell ปิด daemon ผ่าน flow เดียวกัน
async fn wait_for_shutdown() -> Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result?,
        _ = terminate.recv() => {}
    }
    Ok(())
}

#[cfg(not(unix))]
/// รอ Ctrl-C บน platform ที่ไม่มี Unix signal API
async fn wait_for_shutdown() -> Result<()> {
    tokio::signal::ctrl_c().await?;
    Ok(())
}
