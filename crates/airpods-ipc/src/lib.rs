//! กำหนด D-Bus contract กลางที่ daemon, CLI และ GUI ใช้ร่วมกัน

use serde::{Deserialize, Serialize};
use zvariant::Type;

/// ชื่อ service ของ `airpodsd` บน session bus
pub const BUS_NAME: &str = "io.github.abdulloh404.AirPods";

/// object path หลักของ `airpodsd`
pub const OBJECT_PATH: &str = "/io/github/abdulloh404/AirPods";

/// ชื่อ interface สำหรับจัดการ AirPods
pub const MANAGER_INTERFACE: &str = "io.github.abdulloh404.AirPods.Manager1";

pub const MIN_GAIN_DB: f64 = 0.0;
pub const MAX_GAIN_DB: f64 = 30.0;
pub const DEFAULT_GAIN_DB: f64 = 18.0;
pub const MIN_LIMITER_DB: f64 = -12.0;
pub const MAX_LIMITER_DB: f64 = 0.0;
pub const DEFAULT_LIMITER_DB: f64 = -3.0;

/// สถานะล่าสุดของ daemon และ virtual microphone
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct DaemonStatus {
    pub state: String,
    pub selected_device: String,
    pub mic_active: bool,
    pub reconnect_attempt: u32,
    pub gain_db: f64,
    pub limiter_db: f64,
    pub power_bridge_available: bool,
    pub last_error: String,
}

/// ข้อมูล AirPods ที่ daemon ค้นพบจาก BlueZ
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DeviceInfo {
    pub address: String,
    pub name: String,
    pub connected: bool,
    pub selected: bool,
}

/// ค่าแบตเตอรี่ที่อ่านได้จากหูฟังแต่ละข้าง
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct BatteryStatus {
    /// ใช้ `-1` เมื่อยังไม่มีค่าจากอุปกรณ์
    pub left_percent: i16,
    pub left_charging: bool,
    /// ใช้ `-1` เมื่อยังไม่มีค่าจากอุปกรณ์
    pub right_percent: i16,
    pub right_charging: bool,
}

/// D-Bus proxy สำหรับเรียก `airpodsd` โดยไม่เข้าถึง config หรือ service manager โดยตรง
#[zbus::proxy(
    interface = "io.github.abdulloh404.AirPods.Manager1",
    default_service = "io.github.abdulloh404.AirPods",
    default_path = "/io/github/abdulloh404/AirPods"
)]
pub trait Manager {
    fn status(&self) -> zbus::Result<DaemonStatus>;

    fn list_devices(&self) -> zbus::Result<Vec<DeviceInfo>>;

    fn select_device(&self, address: &str) -> zbus::Result<()>;

    fn start_mic(&self) -> zbus::Result<()>;

    fn stop_mic(&self) -> zbus::Result<()>;

    fn set_gain(&self, gain_db: f64) -> zbus::Result<()>;

    fn set_limiter_db(&self, limiter_db: f64) -> zbus::Result<()>;

    fn battery(&self) -> zbus::Result<BatteryStatus>;

    #[zbus(signal)]
    fn status_changed(&self, status: DaemonStatus) -> zbus::Result<()>;

    #[zbus(signal)]
    fn battery_changed(&self, battery: BatteryStatus) -> zbus::Result<()>;

    #[zbus(signal)]
    fn devices_changed(&self, devices: Vec<DeviceInfo>) -> zbus::Result<()>;
}
