//! อ่านรายการ AirPods จาก BlueZ เพื่อให้ daemon เป็นเจ้าของ device inventory เพียงจุดเดียว

use std::collections::HashMap;

use airpods_ipc::DeviceInfo;
use zbus::fdo::ObjectManagerProxy;
use zvariant::OwnedValue;

pub async fn list_airpods(selected: &str) -> zbus::Result<Vec<DeviceInfo>> {
    let connection = zbus::Connection::system().await?;
    let proxy = ObjectManagerProxy::builder(&connection)
        .destination("org.bluez")?
        .path("/")?
        .build()
        .await?;
    let objects = proxy.get_managed_objects().await?;
    let mut devices = Vec::new();

    for interfaces in objects.values() {
        let Some(properties) = interfaces.get("org.bluez.Device1") else {
            continue;
        };
        let Some(address) = string_property(properties, "Address") else {
            continue;
        };
        let name = string_property(properties, "Name")
            .or_else(|| string_property(properties, "Alias"))
            .unwrap_or_default();
        if !name.to_ascii_lowercase().contains("airpods") {
            continue;
        }
        devices.push(DeviceInfo {
            selected: address.eq_ignore_ascii_case(selected),
            address,
            name,
            connected: bool_property(properties, "Connected").unwrap_or(false),
        });
    }

    devices.sort_by(|left, right| left.address.cmp(&right.address));
    Ok(devices)
}

fn string_property(properties: &HashMap<String, OwnedValue>, name: &str) -> Option<String> {
    properties
        .get(name)
        .and_then(|value| String::try_from(value.clone()).ok())
}

fn bool_property(properties: &HashMap<String, OwnedValue>, name: &str) -> Option<bool> {
    properties
        .get(name)
        .and_then(|value| bool::try_from(value.clone()).ok())
}
