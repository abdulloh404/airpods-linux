//! ดูแล BlueZ inventory และ battery bridge แยกจาก audio lifecycle

use std::time::Duration;

use airpods_core::battery::scan_airpods_battery;
use airpods_ipc::BatteryStatus;
use tokio::sync::{mpsc, watch};

use crate::bluez;
use crate::power::{PowerBridge, UpdateOutcome};
use crate::service::{Event, SharedState};

const BATTERY_FAILURES_BEFORE_INVALIDATION: u8 = 2;

pub async fn inventory_loop(
    state: SharedState,
    events: mpsc::UnboundedSender<Event>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let selected = state.read().await.status.selected_device.clone();
        if let Ok(devices) = bluez::list_airpods(&selected).await {
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
    mut shutdown: watch::Receiver<bool>,
) {
    let mut consecutive_failures = 0_u8;
    loop {
        let scan = tokio::select! {
            result = scan_airpods_battery() => Some(result),
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    None
                } else {
                    continue;
                }
            }
        };
        let Some(scan) = scan else {
            break;
        };

        match scan {
            Ok(battery) => {
                consecutive_failures = 0;
                let battery = BatteryStatus {
                    left_percent: battery.left.percent.map(i16::from).unwrap_or(-1),
                    left_charging: battery.left.charging,
                    right_percent: battery.right.percent.map(i16::from).unwrap_or(-1),
                    right_charging: battery.right.charging,
                };
                publish_battery(&state, &events, &bridge, battery).await;
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

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }

    let _ = bridge.invalidate().await;
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
