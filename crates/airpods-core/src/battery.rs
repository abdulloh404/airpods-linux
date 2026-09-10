//! อ่านและแปลงสถานะแบตเตอรี่จาก Apple BLE manufacturer data และ AACP

use crate::{AirPodsBattery, BatteryLevel};
use bluer::{Adapter, AdapterEvent, DiscoveryFilter, DiscoveryTransport, Session};
use futures_util::{StreamExt, pin_mut};
use std::time::Duration;
use tokio::time::timeout;

const APPLE_COMPANY_ID: u16 = 0x004c;
const SCAN_TIMEOUT: Duration = Duration::from_secs(10);
const AACP_BATTERY_HEADER: [u8; 6] = [0x04, 0x00, 0x04, 0x00, 0x04, 0x00];
const AACP_BATTERY_ITEM_SIZE: usize = 5;
const AACP_MAX_BATTERY_ITEMS: usize = 3;

/// ค่าแบตเตอรี่เฉพาะ component ที่มากับ AACP notification หนึ่ง packet
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AacpBatteryUpdate {
    pub left: Option<BatteryLevel>,
    pub right: Option<BatteryLevel>,
    pub case: Option<BatteryLevel>,
}

/// สแกน BLE ชั่วคราวจนพบ AirPods advertisement ที่แปลงได้
pub async fn scan_airpods_battery() -> Result<AirPodsBattery, String> {
    let session = Session::new()
        .await
        .map_err(|error| format!("failed to connect to BlueZ: {error}"))?;
    let adapter = session
        .default_adapter()
        .await
        .map_err(|error| format!("failed to get the Bluetooth adapter: {error}"))?;
    let previous_filter = adapter.discovery_filter().await;
    adapter
        .set_discovery_filter(DiscoveryFilter {
            transport: DiscoveryTransport::Le,
            duplicate_data: true,
            ..Default::default()
        })
        .await
        .map_err(|error| format!("failed to configure BLE scan: {error}"))?;

    let result = scan_once(&adapter).await;
    let _ = adapter.set_discovery_filter(previous_filter).await;
    result
}

async fn scan_once(adapter: &Adapter) -> Result<AirPodsBattery, String> {
    let events = adapter
        .discover_devices_with_changes()
        .await
        .map_err(|error| format!("failed to start BLE scan: {error}"))?;
    pin_mut!(events);

    timeout(SCAN_TIMEOUT, async {
        while let Some(event) = events.next().await {
            let AdapterEvent::DeviceAdded(address) = event else {
                continue;
            };
            let Ok(device) = adapter.device(address) else {
                continue;
            };
            let Ok(Some(rssi)) = device.rssi().await else {
                continue;
            };
            let Ok(Some(manufacturer_data)) = device.manufacturer_data().await else {
                continue;
            };
            let Some(data) = manufacturer_data.get(&APPLE_COMPANY_ID) else {
                continue;
            };
            if let Some(battery) = parse_advertisement(data, address.to_string(), rssi) {
                return Ok(battery);
            }
        }
        Err("BLE scan ended before an AirPods advertisement was received".to_string())
    })
    .await
    .map_err(|_| {
        "no AirPods battery advertisement found; open the case near this computer and try again"
            .to_string()
    })?
}

/// แปลง Apple manufacturer data เป็นสถานะแบตเตอรี่เมื่อเป็นรุ่นที่รองรับ
pub fn parse_advertisement(
    data: &[u8],
    ble_address: impl Into<String>,
    rssi: i16,
) -> Option<AirPodsBattery> {
    if data.len() < 11 || data[0] != 0x07 || data[1] != 0x19 || data[2] != 0x01 {
        return None;
    }

    let model_id = u16::from_be_bytes([data[3], data[4]]);
    if !is_airpods_model(model_id) {
        return None;
    }

    let values_flipped = data[5] & 0x20 == 0;
    let pods = data[6];
    let left_nibble = if values_flipped {
        pods >> 4
    } else {
        pods & 0x0f
    };
    let right_nibble = if values_flipped {
        pods & 0x0f
    } else {
        pods >> 4
    };
    let flags = data[7] >> 4;

    Some(AirPodsBattery {
        left: BatteryLevel {
            percent: battery_percent(left_nibble),
            charging: flags & if values_flipped { 0x02 } else { 0x01 } != 0,
        },
        right: BatteryLevel {
            percent: battery_percent(right_nibble),
            charging: flags & if values_flipped { 0x01 } else { 0x02 } != 0,
        },
        case: BatteryLevel {
            percent: battery_percent(data[7] & 0x0f),
            charging: flags & 0x04 != 0,
        },
        model_id,
        ble_address: ble_address.into(),
        rssi,
    })
}

/// แปลง AACP battery notification เป็นค่า 0–100 โดยไม่ลดความละเอียด
pub fn parse_aacp_battery(data: &[u8]) -> Option<AacpBatteryUpdate> {
    if data.len() < 7 || !data.starts_with(&AACP_BATTERY_HEADER) {
        return None;
    }

    let item_count = usize::from(data[6]);
    if item_count == 0
        || item_count > AACP_MAX_BATTERY_ITEMS
        || data.len() != 7 + item_count * AACP_BATTERY_ITEM_SIZE
    {
        return None;
    }

    let mut update = AacpBatteryUpdate::default();
    for item in data[7..].chunks_exact(AACP_BATTERY_ITEM_SIZE) {
        if item[1] != 0x01 || item[4] != 0x01 {
            return None;
        }

        let level = match item[3] {
            0x01 => BatteryLevel {
                percent: Some(valid_percent(item[2])?),
                charging: true,
            },
            0x02 => BatteryLevel {
                percent: Some(valid_percent(item[2])?),
                charging: false,
            },
            0x04 => BatteryLevel {
                percent: None,
                charging: false,
            },
            _ => return None,
        };

        match item[0] {
            0x02 => update.right = Some(level),
            0x04 => update.left = Some(level),
            0x08 => update.case = Some(level),
            _ => {}
        }
    }

    (update.left.is_some() || update.right.is_some() || update.case.is_some()).then_some(update)
}

/// คืนชื่อรุ่นที่ตรงกับ model id ใน advertisement
pub fn model_name(model_id: u16) -> &'static str {
    match model_id {
        0x0220 => "AirPods 1",
        0x0f20 => "AirPods 2",
        0x1320 => "AirPods 3",
        0x0e20 => "AirPods Pro",
        0x1420 => "AirPods Pro 2 (Lightning)",
        0x2420 => "AirPods Pro 2 (USB-C)",
        0x0a20 => "AirPods Max (Lightning)",
        0x1f20 => "AirPods Max (USB-C)",
        0x1920 => "AirPods 4",
        0x1b20 => "AirPods 4 (ANC)",
        0x2720 => "AirPods Pro (model 2720)",
        _ => "Unknown AirPods",
    }
}

fn is_airpods_model(model_id: u16) -> bool {
    matches!(
        model_id,
        0x0220
            | 0x0f20
            | 0x1320
            | 0x0e20
            | 0x1420
            | 0x2420
            | 0x0a20
            | 0x1f20
            | 0x1920
            | 0x1b20
            | 0x2720
    )
}

fn battery_percent(nibble: u8) -> Option<u8> {
    match nibble {
        0x00..=0x0a => Some(nibble * 10),
        _ => None,
    }
}

fn valid_percent(percent: u8) -> Option<u8> {
    (percent <= 100).then_some(percent)
}
