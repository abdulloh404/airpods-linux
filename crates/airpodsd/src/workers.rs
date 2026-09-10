//! ดูแล BlueZ inventory และ battery bridge แยกจาก audio lifecycle

use std::time::Duration;

use airpods_core::{
    AirPodsBattery, BatteryLevel,
    battery::{AacpBatteryUpdate, scan_airpods_battery},
};
use airpods_ipc::BatteryStatus;
use tokio::sync::{mpsc, watch};

use crate::bluez;
use crate::power::{PowerBridge, UpdateOutcome};
use crate::service::{Event, SharedState};

const BATTERY_FAILURES_BEFORE_INVALIDATION: u8 = 2;
const BATTERY_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// การเปลี่ยนแหล่งข้อมูลแบตเตอรี่จาก AACP session ของ microphone
#[derive(Clone, Copy, Debug)]
pub enum AacpBatteryEvent {
    Update(AacpBatteryUpdate),
    SessionEnded,
}

pub async fn inventory_loop(
    state: SharedState,
    events: mpsc::UnboundedSender<Event>,
    device_connected: watch::Sender<Option<bool>>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let selected = state.read().await.status.selected_device.clone();
        if let Ok(devices) = bluez::list_airpods(&selected).await {
            let selected_connected = devices
                .iter()
                .any(|device| device.selected && device.connected);
            if *device_connected.borrow() != Some(selected_connected) {
                let _ = device_connected.send(Some(selected_connected));
            }
            let changed = {
                let mut state = state.write().await;
                if state.devices == devices {
                    false
                } else {
                    state.devices = devices.clone();
                    true
                }
            };
            if changed {
                let _ = events.send(Event::Devices(devices));
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }
}

pub async fn battery_loop(
    state: SharedState,
    events: mpsc::UnboundedSender<Event>,
    bridge: PowerBridge,
    mut aacp_events: mpsc::UnboundedReceiver<AacpBatteryEvent>,
    mut device_connected: watch::Receiver<Option<bool>>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut consecutive_failures = 0_u8;
    let mut precise_battery = None;
    let mut is_device_connected = *device_connected.borrow_and_update();
    let mut aacp_events_open = true;
    let mut scan_in_progress = false;
    let (scan_results_tx, mut scan_results_rx) = mpsc::unbounded_channel();
    let mut refresh = tokio::time::interval(BATTERY_REFRESH_INTERVAL);
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            changed = device_connected.changed() => {
                if changed.is_err() {
                    is_device_connected = Some(false);
                } else {
                    let next = *device_connected.borrow_and_update();
                    if next == is_device_connected {
                        continue;
                    }
                    is_device_connected = next;
                }

                if is_device_connected == Some(true) {
                    if precise_battery.is_none() && !scan_in_progress {
                        spawn_battery_scan(&scan_results_tx);
                        scan_in_progress = true;
                    }
                } else {
                    precise_battery = None;
                    consecutive_failures = 0;
                    publish_battery(
                        &state,
                        &events,
                        &bridge,
                        BatteryStatus::unavailable(),
                    )
                    .await;
                }
            }
            event = aacp_events.recv(), if aacp_events_open => {
                match event {
                    Some(AacpBatteryEvent::Update(update)) => {
                        if is_device_connected == Some(false) {
                            continue;
                        }
                        if update.left.is_none() && update.right.is_none() {
                            continue;
                        }
                        is_device_connected = Some(true);
                        let mut battery = precise_battery.unwrap_or_else(BatteryStatus::unavailable);
                        apply_aacp_update(&mut battery, update);
                        precise_battery = Some(battery);
                        consecutive_failures = 0;
                        publish_battery(&state, &events, &bridge, battery).await;
                    }
                    Some(AacpBatteryEvent::SessionEnded) => {
                        precise_battery = None;
                        consecutive_failures = 0;
                        if is_device_connected == Some(true) && !scan_in_progress {
                            spawn_battery_scan(&scan_results_tx);
                            scan_in_progress = true;
                        } else if is_device_connected != Some(true) {
                            publish_battery(
                                &state,
                                &events,
                                &bridge,
                                BatteryStatus::unavailable(),
                            )
                            .await;
                        }
                    }
                    None => {
                        aacp_events_open = false;
                        precise_battery = None;
                        if is_device_connected == Some(true) && !scan_in_progress {
                            spawn_battery_scan(&scan_results_tx);
                            scan_in_progress = true;
                        } else if is_device_connected != Some(true) {
                            publish_battery(
                                &state,
                                &events,
                                &bridge,
                                BatteryStatus::unavailable(),
                            )
                            .await;
                        }
                    }
                }
            }
            Some(scan) = scan_results_rx.recv(), if scan_in_progress => {
                scan_in_progress = false;
                if precise_battery.is_some() || is_device_connected != Some(true) {
                    continue;
                }
                match scan {
                    Ok(battery) => {
                        consecutive_failures = 0;
                        publish_battery(&state, &events, &bridge, battery_status(battery)).await;
                    }
                    Err(_) => {
                        consecutive_failures = consecutive_failures.saturating_add(1);
                        if consecutive_failures >= BATTERY_FAILURES_BEFORE_INVALIDATION {
                            publish_battery(
                                &state,
                                &events,
                                &bridge,
                                BatteryStatus::unavailable(),
                            )
                            .await;
                        }
                    }
                }
            }
            _ = refresh.tick() => {
                if let Some(battery) = precise_battery {
                    publish_battery(&state, &events, &bridge, battery).await;
                } else if is_device_connected == Some(true) && !scan_in_progress {
                    spawn_battery_scan(&scan_results_tx);
                    scan_in_progress = true;
                }
            }
        }
    }

    let _ = bridge.invalidate().await;
}

fn spawn_battery_scan(results: &mpsc::UnboundedSender<Result<AirPodsBattery, String>>) {
    let results = results.clone();
    tokio::spawn(async move {
        let _ = results.send(scan_airpods_battery().await);
    });
}

fn battery_status(battery: AirPodsBattery) -> BatteryStatus {
    BatteryStatus {
        left_percent: battery.left.percent.map(i16::from).unwrap_or(-1),
        left_charging: battery.left.charging,
        right_percent: battery.right.percent.map(i16::from).unwrap_or(-1),
        right_charging: battery.right.charging,
    }
}

fn apply_aacp_update(battery: &mut BatteryStatus, update: AacpBatteryUpdate) {
    if let Some(left) = update.left {
        apply_level(&mut battery.left_percent, &mut battery.left_charging, left);
    }
    if let Some(right) = update.right {
        apply_level(
            &mut battery.right_percent,
            &mut battery.right_charging,
            right,
        );
    }
}

fn apply_level(percent: &mut i16, charging: &mut bool, level: BatteryLevel) {
    *percent = level.percent.map(i16::from).unwrap_or(-1);
    *charging = level.charging;
}

async fn publish_battery(
    state: &SharedState,
    events: &mpsc::UnboundedSender<Event>,
    bridge: &PowerBridge,
    battery: BatteryStatus,
) {
    let bridge_available = matches!(bridge.update(battery).await, Ok(UpdateOutcome::Written));
    let (battery_changed, status_changed, status) = {
        let mut state = state.write().await;
        let battery_changed = state.battery != battery;
        let status_changed = state.status.power_bridge_available != bridge_available;
        state.battery = battery;
        state.status.power_bridge_available = bridge_available;
        (battery_changed, status_changed, state.status.clone())
    };
    if battery_changed {
        let _ = events.send(Event::Battery(battery));
    }
    if status_changed {
        let _ = events.send(Event::Status(status));
    }
}
