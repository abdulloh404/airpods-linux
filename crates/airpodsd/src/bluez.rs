//! อ่านรายการและติดตามสถานะ AirPods ผ่าน BlueZ โดยไม่สั่งเชื่อมอุปกรณ์

use std::collections::HashMap;

use airpods_ipc::DeviceInfo;
use anyhow::{Context, bail};
use bluer::{Address, DeviceEvent, DeviceProperty, Session};
use futures_util::{StreamExt, pin_mut};
use zbus::fdo::ObjectManagerProxy;
use zvariant::OwnedValue;

/// อ่านสถานะการเชื่อมต่อปัจจุบันจาก BlueZ โดยไม่สั่งเชื่อมอุปกรณ์
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

/// รอจน BlueZ รายงานว่าอุปกรณ์เชื่อมแล้ว โดยไม่สั่งเชื่อมอุปกรณ์เอง
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
        .and_then(|value| <&str>::try_from(value).ok())
        .map(str::to_owned)
}

fn bool_property(properties: &HashMap<String, OwnedValue>, name: &str) -> Option<bool> {
    properties
        .get(name)
        .and_then(|value| bool::try_from(value).ok())
}
