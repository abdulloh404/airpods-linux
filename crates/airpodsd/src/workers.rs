//! ดูแล BlueZ inventory และ battery publishing แยกจาก audio lifecycle
//!
//! inventory worker สำรวจอุปกรณ์เป็นระยะและส่ง connection state ของตัวที่เลือกให้ battery worker
//! ส่วน battery worker ให้ข้อมูล AACP จาก microphone session มีความสำคัญสูงกว่า BLE scan และเขียนผลเดียวกัน
//! ไปยัง shared state, D-Bus event channel และ kernel power bridge

use std::time::{Duration, Instant};

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
/// อายุสูงสุดที่ถือว่าค่าเคสยังสดเมื่อไม่ได้รับ component นี้ซ้ำ
const CASE_FRESH_TIMEOUT: Duration = Duration::from_secs(90);

/// connection snapshot ของอุปกรณ์ที่เลือก ใช้แยกการหลุดชั่วคราวออกจากการเปลี่ยนอุปกรณ์
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SelectedDeviceConnection {
    /// Bluetooth address ของอุปกรณ์ที่เลือก
    address: String,
    /// สถานะ connection ล่าสุดจาก BlueZ
    connected: bool,
}

/// ค่าเคสล่าสุดพร้อมเวลาที่ได้รับข้อมูลจริงจาก AACP หรือ BLE
#[derive(Clone, Copy, Debug)]
struct CachedCase {
    /// เปอร์เซ็นต์ล่าสุดที่เคยได้รับ
    percent: i16,
    /// สถานะชาร์จที่มากับเปอร์เซ็นต์ล่าสุด
    charging: bool,
    /// เวลา monotonic ที่ได้รับค่าจริง ไม่รวมการ publish cache ซ้ำ
    observed_at: Instant,
}

/// การเปลี่ยนแหล่งข้อมูลแบตเตอรี่จาก AACP session ของ microphone
#[derive(Clone, Copy, Debug)]
pub enum AacpBatteryEvent {
    /// battery notification ที่อาจมีข้อมูลเพียง component เดียวจาก AACP session
    Update(AacpBatteryUpdate),
    /// แจ้งว่า AACP session สิ้นสุดและต้องกลับไปใช้ BLE scan หาก device ยังเชื่อมอยู่
    SessionEnded,
}

/// สำรวจ BlueZ ทุกห้าวินาทีและเผยแพร่เฉพาะ inventory ที่เปลี่ยน
pub async fn inventory_loop(
    state: SharedState,
    events: mpsc::UnboundedSender<Event>,
    device_connection: watch::Sender<Option<SelectedDeviceConnection>>,
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
            let connection = SelectedDeviceConnection {
                address: selected.clone(),
                connected: selected_connected,
            };
            if device_connection.borrow().as_ref() != Some(&connection) {
                let _ = device_connection.send(Some(connection));
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
    mut device_connection: watch::Receiver<Option<SelectedDeviceConnection>>,
    mut shutdown: watch::Receiver<bool>,
) {
    // `precise_battery` ระบุว่า AACP เป็นแหล่งหลักอยู่ จึงใช้กั้นผล BLE scan ที่มาถึงภายหลัง
    let mut consecutive_failures = 0_u8;
    let mut precise_battery = None;
    let mut selected_connection = device_connection.borrow_and_update().clone();
    let mut cached_case = None;
    let mut aacp_events_open = true;
    let mut scan_in_progress = false;
    let mut scan_generation = 0_u64;
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
            changed = device_connection.changed() => {
                if changed.is_err() {
                    selected_connection = None;
                } else {
                    let next = device_connection.borrow_and_update().clone();
                    if next == selected_connection {
                        continue;
                    }
                    let target_changed = selected_address(&next) != selected_address(&selected_connection);
                    selected_connection = next;
                    if target_changed {
                        // invalidate scan และ cache ของอุปกรณ์เดิมก่อนเริ่มอ่านเป้าหมายใหม่
                        scan_generation = scan_generation.wrapping_add(1);
                        scan_in_progress = false;
                        precise_battery = None;
                        cached_case = None;
                        consecutive_failures = 0;
                    }
                }

                if selected_connected(&selected_connection) == Some(true) {
                    if precise_battery.is_none() && !scan_in_progress {
                        // เริ่ม fallback scan เพียงงานเดียวจนกว่าจะได้รับผลกลับทาง channel
                        scan_generation = scan_generation.wrapping_add(1);
                        spawn_battery_scan(&scan_results_tx, scan_generation);
                        scan_in_progress = true;
                    }
                } else {
                    precise_battery = None;
                    // ค่าเคสจาก cache ใช้ได้เฉพาะ connection เดิม; เมื่อหลุดต้องลบจาก UPower ทันที
                    cached_case = None;
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
                        if selected_connected(&selected_connection) == Some(false) {
                            continue;
                        }
                        if let Some(connection) = selected_connection.as_mut() {
                            connection.connected = true;
                        }
                        // notification อาจอัปเดต component เดียว จึงเริ่มจาก snapshot ปัจจุบัน
                        let mut battery = match precise_battery {
                            Some(battery) => battery,
                            None => state.read().await.battery,
                        };
                        apply_aacp_update(&mut battery, update, &mut cached_case);
                        refresh_case_freshness(&mut battery, cached_case);
                        precise_battery = Some(battery);
                        consecutive_failures = 0;
                        publish_battery(&state, &events, &bridge, battery).await;
                    }
                    Some(AacpBatteryEvent::SessionEnded) => {
                        // หลัง microphone หยุด ข้อมูล AACP ไม่ถือว่าสดและต้องหา snapshot จาก BLE ใหม่
                        precise_battery = None;
                        consecutive_failures = 0;
                        if selected_connected(&selected_connection) == Some(true) && !scan_in_progress {
                            scan_generation = scan_generation.wrapping_add(1);
                            spawn_battery_scan(&scan_results_tx, scan_generation);
                            scan_in_progress = true;
                        } else if selected_connected(&selected_connection) != Some(true) {
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
                        if selected_connected(&selected_connection) == Some(true) && !scan_in_progress {
                            scan_generation = scan_generation.wrapping_add(1);
                            spawn_battery_scan(&scan_results_tx, scan_generation);
                            scan_in_progress = true;
                        } else if selected_connected(&selected_connection) != Some(true) {
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
            Some((generation, scan)) = scan_results_rx.recv() => {
                if generation != scan_generation {
                    continue;
                }
                scan_in_progress = false;
                // ทิ้งผล scan ที่เก่าแล้วหาก AACP ส่งข้อมูลใหม่หรือ device หลุดระหว่างรอ
                if precise_battery.is_some() || selected_connected(&selected_connection) != Some(true) {
                    continue;
                }
                match scan {
                    Ok(battery) => {
                        consecutive_failures = 0;
                        let battery = battery_status(battery, &mut cached_case);
                        publish_battery(&state, &events, &bridge, battery).await;
                    }
                    Err(_) => {
                        consecutive_failures = consecutive_failures.saturating_add(1);
                        // ยอมให้ความล้มเหลวชั่วคราวหนึ่งครั้งเพื่อไม่ให้ UI กระพริบเป็น unavailable
                        if consecutive_failures >= BATTERY_FAILURES_BEFORE_INVALIDATION {
                            publish_battery(
                                &state,
                                &events,
                                &bridge,
                                cached_case_status(cached_case, true),
                            )
                            .await;
                        }
                    }
                }
            }
            _ = refresh.tick() => {
                if let Some(mut battery) = precise_battery {
                    refresh_case_freshness(&mut battery, cached_case);
                    precise_battery = Some(battery);
                    // ส่งค่าซ้ำเพื่อ refresh อายุข้อมูลใน kernel bridge แม้ D-Bus snapshot ไม่เปลี่ยน
                    publish_battery(&state, &events, &bridge, battery).await;
                } else if selected_connected(&selected_connection) == Some(true) && !scan_in_progress {
                    scan_generation = scan_generation.wrapping_add(1);
                    spawn_battery_scan(&scan_results_tx, scan_generation);
                    scan_in_progress = true;
                }
            }
        }
    }

    // ป้องกัน kernel power_supply ค้างค่าล่าสุดหลัง daemon ปิด
    let _ = bridge.invalidate().await;
}

/// แยก BLE scan เป็น task เพื่อไม่ให้ event loop หยุดรับ AACP หรือ shutdown ระหว่าง scan
fn spawn_battery_scan(
    results: &mpsc::UnboundedSender<(u64, Result<AirPodsBattery, String>)>,
    generation: u64,
) {
    let results = results.clone();
    tokio::spawn(async move {
        let _ = results.send((generation, scan_airpods_battery().await));
    });
}

/// คืน address ของ connection snapshot หรือสตริงว่างเมื่อ inventory ยังไม่พร้อม
fn selected_address(connection: &Option<SelectedDeviceConnection>) -> &str {
    connection
        .as_ref()
        .map(|connection| connection.address.as_str())
        .unwrap_or_default()
}

/// คืน connection state โดยคง `None` ไว้จนกว่า inventory worker จะอ่าน BlueZ สำเร็จ
fn selected_connected(connection: &Option<SelectedDeviceConnection>) -> Option<bool> {
    connection
        .as_ref()
        .map(|connection| connection.connected)
}

/// แปลง battery model จาก BLE เป็น IPC model พร้อมรักษาค่าเคสล่าสุดเมื่อ packet ไม่รายงานเคส
fn battery_status(
    battery: AirPodsBattery,
    cached_case: &mut Option<CachedCase>,
) -> BatteryStatus {
    let mut status = BatteryStatus {
        left_percent: battery.left.percent.map(i16::from).unwrap_or(-1),
        left_charging: battery.left.charging,
        right_percent: battery.right.percent.map(i16::from).unwrap_or(-1),
        right_charging: battery.right.charging,
        ..BatteryStatus::unavailable()
    };
    apply_case_level(&mut status, battery.case, cached_case);
    status
}

/// รวม partial AACP update ลง snapshot เดิมและคง component ที่ไม่ได้มากับ notification
fn apply_aacp_update(
    battery: &mut BatteryStatus,
    update: AacpBatteryUpdate,
    cached_case: &mut Option<CachedCase>,
) {
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
    if let Some(charging_case) = update.case {
        apply_case_level(battery, charging_case, cached_case);
    }
}

/// แปลง battery level หนึ่งข้างเป็นค่า percent และ charging ของ IPC
fn apply_level(percent: &mut i16, charging: &mut bool, level: BatteryLevel) {
    *percent = level.percent.map(i16::from).unwrap_or(-1);
    *charging = level.charging;
}

/// ใช้ค่าจริงของเคสอัปเดต cache หรือคงค่าก่อนหน้าเป็น last known เมื่อเคสรายงาน unavailable
fn apply_case_level(
    battery: &mut BatteryStatus,
    level: BatteryLevel,
    cached_case: &mut Option<CachedCase>,
) {
    if let Some(percent) = level.percent {
        let cached = CachedCase {
            percent: i16::from(percent),
            charging: level.charging,
            observed_at: Instant::now(),
        };
        *cached_case = Some(cached);
        battery.case_percent = cached.percent;
        battery.case_charging = cached.charging;
        battery.case_stale = false;
    } else {
        apply_cached_case(battery, *cached_case, true);
    }
}

/// คืน snapshot ที่ซ่อนหูฟังทั้งสองข้างแต่ยังเผยแพร่ค่าเคสล่าสุดได้
fn cached_case_status(cached_case: Option<CachedCase>, force_stale: bool) -> BatteryStatus {
    let mut battery = BatteryStatus::unavailable();
    apply_cached_case(&mut battery, cached_case, force_stale);
    battery
}

/// เติมค่าเคสจาก cache และทำเครื่องหมาย stale ตามอายุหรือสถานะ connection
fn apply_cached_case(
    battery: &mut BatteryStatus,
    cached_case: Option<CachedCase>,
    force_stale: bool,
) {
    let Some(cached_case) = cached_case else {
        battery.case_percent = -1;
        battery.case_charging = false;
        battery.case_stale = true;
        return;
    };
    battery.case_percent = cached_case.percent;
    battery.case_charging = cached_case.charging;
    battery.case_stale = force_stale || cached_case.observed_at.elapsed() >= CASE_FRESH_TIMEOUT;
}

/// เปลี่ยนค่าเคสเป็น last known เมื่อเลยช่วง freshness โดยไม่ล้างเปอร์เซ็นต์ที่ยังมีประโยชน์
fn refresh_case_freshness(battery: &mut BatteryStatus, cached_case: Option<CachedCase>) {
    let was_stale = battery.case_stale;
    apply_cached_case(battery, cached_case, false);
    battery.case_stale |= was_stale;
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
