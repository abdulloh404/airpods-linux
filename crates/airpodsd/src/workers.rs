//! ดูแล BlueZ inventory และ battery publishing แยกจาก audio lifecycle
//!
//! inventory worker สำรวจอุปกรณ์เป็นระยะและส่ง connection state ของตัวที่เลือกให้ battery worker
//! ส่วน battery worker ให้ข้อมูล AACP จาก microphone session มีความสำคัญสูงกว่า BLE scan และเขียนผลเดียวกัน
//! ไปยัง shared state, D-Bus event channel และ kernel power bridge

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

/// จำนวน BLE scan ที่ล้มเหลวติดกันก่อนล้างค่าที่ client เคยเห็น
const BATTERY_FAILURES_BEFORE_INVALIDATION: u8 = 2;
/// รอบส่งข้อมูลซ้ำไปยัง kernel bridge และเริ่ม fallback scan เมื่อไม่มี AACP data
const BATTERY_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// การเปลี่ยนแหล่งข้อมูลแบตเตอรี่จาก AACP session ของ microphone
#[derive(Clone, Copy, Debug)]
pub enum AacpBatteryEvent {
    /// battery notification ที่อาจมีข้อมูลเพียงข้างเดียวจาก AACP session
    Update(AacpBatteryUpdate),
    /// แจ้งว่า AACP session สิ้นสุดและต้องกลับไปใช้ BLE scan หาก device ยังเชื่อมอยู่
    SessionEnded,
}

/// สำรวจ BlueZ ทุกห้าวินาทีและเผยแพร่เฉพาะ inventory ที่เปลี่ยน
pub async fn inventory_loop(
    state: SharedState,
    events: mpsc::UnboundedSender<Event>,
    device_connected: watch::Sender<Option<bool>>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let selected = state.read().await.status.selected_device.clone();
        // หาก BlueZ query ล้มเหลวให้คง snapshot เดิมและลองใหม่ในรอบถัดไป
        if let Ok(devices) = bluez::list_airpods(&selected).await {
            // connection watch ส่งเฉพาะสถานะของ device ที่เลือก ไม่รวม AirPods ตัวอื่นใน inventory
            let selected_connected = devices
                .iter()
                .any(|device| device.selected && device.connected);
            if *device_connected.borrow() != Some(selected_connected) {
                let _ = device_connected.send(Some(selected_connected));
            }
            // เปรียบเทียบและแทน snapshot ภายใต้ write lock เดียว แต่ส่ง event หลังปล่อย lock
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

/// รวม battery event จาก AACP, fallback BLE scan และ connection state แล้ว publish ค่าที่เชื่อถือได้ล่าสุด
pub async fn battery_loop(
    state: SharedState,
    events: mpsc::UnboundedSender<Event>,
    bridge: PowerBridge,
    mut aacp_events: mpsc::UnboundedReceiver<AacpBatteryEvent>,
    mut device_connected: watch::Receiver<Option<bool>>,
    mut shutdown: watch::Receiver<bool>,
) {
    // `precise_battery` ระบุว่า AACP เป็นแหล่งหลักอยู่ จึงใช้กั้นผล BLE scan ที่มาถึงภายหลัง
    let mut consecutive_failures = 0_u8;
    let mut precise_battery = None;
    let mut is_device_connected = *device_connected.borrow_and_update();
    let mut aacp_events_open = true;
    let mut scan_in_progress = false;
    let (scan_results_tx, mut scan_results_rx) = mpsc::unbounded_channel();
    let mut refresh = tokio::time::interval(BATTERY_REFRESH_INTERVAL);
    // ข้าม tick ที่สะสมเมื่อ task ช้า เพื่อไม่ยิง scan หลายงานติดกันหลังกลับมาทำงาน
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            // จัดการ shutdown และ connection change ก่อน event อื่นเมื่อหลาย branch พร้อมกัน
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
                        // เริ่ม fallback scan เพียงงานเดียวจนกว่าจะได้รับผลกลับทาง channel
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
                        // notification อาจอัปเดตข้างเดียว จึงรวมกับ snapshot AACP ก่อนหน้า
                        let mut battery = precise_battery.unwrap_or_else(BatteryStatus::unavailable);
                        apply_aacp_update(&mut battery, update);
                        precise_battery = Some(battery);
                        consecutive_failures = 0;
                        publish_battery(&state, &events, &bridge, battery).await;
                    }
                    Some(AacpBatteryEvent::SessionEnded) => {
                        // หลัง microphone หยุด ข้อมูล AACP ไม่ถือว่าสดและต้องหา snapshot จาก BLE ใหม่
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
                        // channel ปิดถาวร จึงปิด branch นี้และคง fallback BLE flow ต่อไป
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
                // ทิ้งผล scan ที่เก่าแล้วหาก AACP ส่งข้อมูลใหม่หรือ device หลุดระหว่างรอ
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
                        // ยอมให้ความล้มเหลวชั่วคราวหนึ่งครั้งเพื่อไม่ให้ UI กระพริบเป็น unavailable
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
                    // ส่งค่าซ้ำเพื่อ refresh อายุข้อมูลใน kernel bridge แม้ D-Bus snapshot ไม่เปลี่ยน
                    publish_battery(&state, &events, &bridge, battery).await;
                } else if is_device_connected == Some(true) && !scan_in_progress {
                    spawn_battery_scan(&scan_results_tx);
                    scan_in_progress = true;
                }
            }
        }
    }

    // ป้องกัน kernel power_supply ค้างค่าล่าสุดหลัง daemon ปิด
    let _ = bridge.invalidate().await;
}

/// แยก BLE scan เป็น task เพื่อไม่ให้ event loop หยุดรับ AACP หรือ shutdown ระหว่าง scan
fn spawn_battery_scan(results: &mpsc::UnboundedSender<Result<AirPodsBattery, String>>) {
    let results = results.clone();
    tokio::spawn(async move {
        let _ = results.send(scan_airpods_battery().await);
    });
}

/// แปลง battery model จาก core เป็น IPC model โดยใช้ `-1` แทน percent ที่ไม่มีข้อมูล
fn battery_status(battery: AirPodsBattery) -> BatteryStatus {
    BatteryStatus {
        left_percent: battery.left.percent.map(i16::from).unwrap_or(-1),
        left_charging: battery.left.charging,
        right_percent: battery.right.percent.map(i16::from).unwrap_or(-1),
        right_charging: battery.right.charging,
    }
}

/// รวม partial AACP update ลง snapshot เดิมและคงค่าของข้างที่ไม่ได้มากับ notification
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

/// แปลง battery level หนึ่งข้างเป็นค่า percent และ charging ของ IPC
fn apply_level(percent: &mut i16, charging: &mut bool, level: BatteryLevel) {
    *percent = level.percent.map(i16::from).unwrap_or(-1);
    *charging = level.charging;
}

/// เขียน battery ไป kernel bridge แล้วเผยแพร่เฉพาะ D-Bus snapshots ที่เปลี่ยน
async fn publish_battery(
    state: &SharedState,
    events: &mpsc::UnboundedSender<Event>,
    bridge: &PowerBridge,
    battery: BatteryStatus,
) {
    // ทั้ง device node ที่ไม่มีและ write error หมายถึง bridge ใช้งานไม่ได้ใน status รอบนี้
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
