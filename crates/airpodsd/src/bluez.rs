//! อ่านรายการและติดตามสถานะ AirPods ผ่าน BlueZ
//!
//! module นี้เปิด session ไปยัง system BlueZ เพื่ออ่าน inventory และ connection event เท่านั้น
//! audio lifecycle จึงรออุปกรณ์ที่ผู้ใช้เชื่อมไว้แล้วโดยไม่แย่งหน้าที่ pairing หรือ connect จากระบบ

use std::collections::HashMap;

use airpods_ipc::DeviceInfo;
use anyhow::{Context, bail};
use bluer::{Address, DeviceEvent, DeviceProperty, Session};
use futures_util::{StreamExt, pin_mut};
use zbus::fdo::ObjectManagerProxy;
use zvariant::OwnedValue;

/// อ่านสถานะการเชื่อมต่อปัจจุบันของ address จาก default Bluetooth adapter
pub async fn is_connected(address: Address) -> anyhow::Result<bool> {
    let session = Session::new()
        .await
        .context("failed to connect to BlueZ while checking AirPods")?;
    let adapter = session
        .default_adapter()
        .await
        .context("failed to get the Bluetooth adapter while checking AirPods")?;
    let device = adapter
        .device(address)
        .context("failed to access the selected AirPods in BlueZ")?;
    device
        .is_connected()
        .await
        .context("failed to read the selected AirPods connection state")
}

/// รอ connection event ของ address จนเชื่อมสำเร็จหรือ device event stream สิ้นสุด
pub async fn wait_until_connected(address: Address) -> anyhow::Result<()> {
    let session = Session::new()
        .await
        .context("failed to connect to BlueZ while waiting for AirPods")?;
    let adapter = session
        .default_adapter()
        .await
        .context("failed to get the Bluetooth adapter while waiting for AirPods")?;
    let device = adapter
        .device(address)
        .context("failed to access the selected AirPods in BlueZ")?;
    let events = device
        .events()
        .await
        .context("failed to monitor the selected AirPods connection")?;
    pin_mut!(events);

    // ตรวจสถานะหลัง subscribe เพื่อไม่พลาดการเชื่อมที่เกิดระหว่างเตรียม event stream
    if device
        .is_connected()
        .await
        .context("failed to read the selected AirPods connection state")?
    {
        return Ok(());
    }

    while let Some(event) = events.next().await {
        if matches!(
            event,
            DeviceEvent::PropertyChanged(DeviceProperty::Connected(true))
        ) {
            return Ok(());
        }
    }

    bail!("selected AirPods disappeared from BlueZ while waiting for connection")
}

/// สร้างรายการ AirPods จาก BlueZ ObjectManager พร้อมสถานะ selected และ connected
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
        // object อื่นของ BlueZ ไม่มี Device1 และไม่ใช่อุปกรณ์ที่แสดงในรายการ
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

    // ลำดับ address ทำให้ผลลัพธ์และ D-Bus signal คงที่แม้ ObjectManager เปลี่ยนลำดับ
    devices.sort_by(|left, right| left.address.cmp(&right.address));
    Ok(devices)
}

/// แปลง dynamic D-Bus property เป็น owned string เมื่อชนิดตรงกัน
fn string_property(properties: &HashMap<String, OwnedValue>, name: &str) -> Option<String> {
    properties
        .get(name)
        .and_then(|value| <&str>::try_from(value).ok())
        .map(str::to_owned)
}

/// แปลง dynamic D-Bus property เป็น boolean เมื่อชนิดตรงกัน
fn bool_property(properties: &HashMap<String, OwnedValue>, name: &str) -> Option<bool> {
    properties
        .get(name)
        .and_then(|value| bool::try_from(value).ok())
}
