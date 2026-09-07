//! รัน daemon หลักซึ่งเป็นเจ้าของ BlueZ, AACP, audio engine และ battery bridge

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
use tokio::sync::{mpsc, watch, RwLock};

use airpods_ipc::{BUS_NAME, MANAGER_INTERFACE, OBJECT_PATH};

#[tokio::main]
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

    let service = ManagerService::new(
        state.clone(),
        config,
        config_store,
        desired_tx.clone(),
        event_tx.clone(),
    );
    let connection = zbus::connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, service)?
        .build()
        .await?;

    let event_task = tokio::spawn(emit_events(connection.clone(), event_rx));
    let audio_task = tokio::spawn(audio::lifecycle_loop(
        state.clone(),
        event_tx.clone(),
        desired_rx,
        shutdown_rx.clone(),
    ));
    let inventory_task = tokio::spawn(workers::inventory_loop(
        state.clone(),
        event_tx.clone(),
        shutdown_rx.clone(),
    ));
    let battery_task = tokio::spawn(workers::battery_loop(
        state,
        event_tx,
        power::PowerBridge::default(),
        shutdown_rx,
    ));

    wait_for_shutdown().await?;
    desired_tx.send_modify(|desired| desired.enabled = false);
    shutdown_tx.send_replace(true);

    let _ = audio_task.await;
    let _ = inventory_task.await;
    let _ = battery_task.await;
    drop(connection);
    event_task.abort();
    Ok(())
}

async fn emit_events(
    connection: zbus::Connection,
    mut events: mpsc::UnboundedReceiver<Event>,
) {
    while let Some(event) = events.recv().await {
        let result = match event {
            Event::Status(status) => {
                connection
                    .emit_signal(
                        None::<&str>,
                        OBJECT_PATH,
                        MANAGER_INTERFACE,
                        "StatusChanged",
                        &status,
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
                        &battery,
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
                        &devices,
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
async fn wait_for_shutdown() -> Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result?,
        _ = terminate.recv() => {}
    }
    Ok(())
}

#[cfg(not(unix))]
async fn wait_for_shutdown() -> Result<()> {
    tokio::signal::ctrl_c().await?;
    Ok(())
}
