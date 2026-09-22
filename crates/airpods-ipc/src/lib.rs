//! กำหนด D-Bus contract กลางที่ daemon, CLI และ GUI ใช้ร่วมกัน
//!
//! Module นี้รวมชื่อปลายทาง ค่าขอบเขตของ audio control ชนิดข้อมูลที่ส่งผ่าน bus
//! และ proxy interface ไว้ที่เดียว เพื่อให้ทุก process ตีความ protocol ตรงกัน

use serde::{Deserialize, Serialize};
use zvariant::Type;

/// ชื่อ service ของ `airpodsd` บน session bus
pub const BUS_NAME: &str = "io.github.abdulloh404.AirPods";

/// object path หลักของ `airpodsd`
pub const OBJECT_PATH: &str = "/io/github/abdulloh404/AirPods";

/// ชื่อ interface สำหรับจัดการ AirPods
pub const MANAGER_INTERFACE: &str = "io.github.abdulloh404.AirPods.Manager1";

/// ค่า gain ต่ำสุดที่ daemon ยอมรับ หน่วยเป็น dB
pub const MIN_GAIN_DB: f64 = 0.0;
/// ค่า gain สูงสุดที่ daemon ยอมรับ หน่วยเป็น dB
pub const MAX_GAIN_DB: f64 = 30.0;
/// ค่า gain เริ่มต้นที่ใช้เมื่อ config ยังไม่กำหนด หน่วยเป็น dB
pub const DEFAULT_GAIN_DB: f64 = 18.0;
/// ค่า limiter ceiling ต่ำสุดที่ daemon ยอมรับ หน่วยเป็น dBFS
pub const MIN_LIMITER_DB: f64 = -12.0;
/// ค่า limiter ceiling สูงสุดที่ daemon ยอมรับ หน่วยเป็น dBFS
pub const MAX_LIMITER_DB: f64 = 0.0;
/// ค่า limiter ceiling เริ่มต้นที่ใช้เมื่อ config ยังไม่กำหนด หน่วยเป็น dBFS
pub const DEFAULT_LIMITER_DB: f64 = -3.0;

/// สถานะล่าสุดของ daemon และ virtual microphone
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct DaemonStatus {
    /// ชื่อ state ปัจจุบันของ audio session สำหรับแสดงผลแก่ client
    pub state: String,
    /// Bluetooth address ของอุปกรณ์ที่เลือก หรือสตริงว่างเมื่อยังไม่ได้เลือก
    pub selected_device: String,
    /// ระบุว่า virtual microphone กำลังทำงานอยู่หรือไม่
    pub mic_active: bool,
    /// ลำดับความพยายาม reconnect ของ audio session ปัจจุบัน
    pub reconnect_attempt: u32,
    /// ค่า gain ก่อนเข้า limiter หน่วยเป็น dB
    pub gain_db: f64,
    /// ระดับเพดานของ limiter หน่วยเป็น dBFS
    pub limiter_db: f64,
    /// ระบุว่า kernel power bridge พร้อมเผยแพร่แบตเตอรี่ผ่าน UPower หรือไม่
    pub power_bridge_available: bool,
    /// error ล่าสุดจาก daemon หรือสตริงว่างเมื่อไม่มี error
    pub last_error: String,
}

/// ข้อมูล AirPods ที่ daemon ค้นพบจาก BlueZ
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DeviceInfo {
    /// Bluetooth address ที่ใช้ระบุอุปกรณ์และส่งกลับตอนเลือกอุปกรณ์
    pub address: String,
    /// ชื่ออุปกรณ์ที่ BlueZ รายงาน
    pub name: String,
    /// สถานะการเชื่อมต่อ Bluetooth ล่าสุดจาก BlueZ
    pub connected: bool,
    /// ระบุว่า daemon เลือกอุปกรณ์นี้เป็นเป้าหมายปัจจุบันหรือไม่
    pub selected: bool,
}

/// ค่าแบตเตอรี่ที่อ่านได้จากหูฟังแต่ละข้างและเคสชาร์จ
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct BatteryStatus {
    /// ใช้ `-1` เมื่อยังไม่มีค่าจากอุปกรณ์
    pub left_percent: i16,
    /// ระบุว่า AirPod ข้างซ้ายกำลังชาร์จอยู่หรือไม่
    pub left_charging: bool,
    /// ใช้ `-1` เมื่อยังไม่มีค่าจากอุปกรณ์
    pub right_percent: i16,
    /// ระบุว่า AirPod ข้างขวากำลังชาร์จอยู่หรือไม่
    pub right_charging: bool,
    /// ใช้ `-1` เมื่อยังไม่เคยได้รับค่าแบตเตอรี่เคส
    pub case_percent: i16,
    /// ระบุว่าเคสกำลังชาร์จตามข้อมูลล่าสุดหรือไม่
    pub case_charging: bool,
    /// ระบุว่าค่าเคสเป็นค่าล่าสุดที่ cache ไว้เพราะเคสไม่ได้ส่งข้อมูลอยู่
    pub case_stale: bool,
}

impl BatteryStatus {
    /// สร้างสถานะที่ระบุว่ายังไม่มีค่าแบตเตอรี่ที่เชื่อถือได้
    pub const fn unavailable() -> Self {
        Self {
            left_percent: -1,
            left_charging: false,
            right_percent: -1,
            right_charging: false,
            case_percent: -1,
            case_charging: false,
            case_stale: true,
        }
    }
}

/// D-Bus proxy สำหรับเรียก `airpodsd` โดยไม่เข้าถึง config หรือ service manager โดยตรง
#[zbus::proxy(
    interface = "io.github.abdulloh404.AirPods.Manager1",
    default_service = "io.github.abdulloh404.AirPods",
    default_path = "/io/github/abdulloh404/AirPods"
)]
pub trait Manager {
    /// อ่าน snapshot ของ daemon และ virtual microphone ล่าสุด
    fn status(&self) -> zbus::Result<DaemonStatus>;

    /// อ่านรายการ AirPods ที่ BlueZ รู้จักพร้อมสถานะการเลือกและการเชื่อมต่อ
    fn list_devices(&self) -> zbus::Result<Vec<DeviceInfo>>;

    /// เลือกอุปกรณ์ด้วย Bluetooth address เพื่อใช้กับคำสั่งถัดไป
    fn select_device(&self, address: &str) -> zbus::Result<()>;

    /// ขอให้ daemon เริ่ม virtual microphone สำหรับอุปกรณ์ที่เลือก
    fn start_mic(&self) -> zbus::Result<()>;

    /// ขอให้ daemon หยุด virtual microphone และคืน audio profile
    fn stop_mic(&self) -> zbus::Result<()>;

    /// ตั้งค่า gain ก่อนเข้า limiter หน่วยเป็น dB
    fn set_gain(&self, gain_db: f64) -> zbus::Result<()>;

    /// ตั้งค่า limiter ceiling หน่วยเป็น dBFS
    fn set_limiter_db(&self, limiter_db: f64) -> zbus::Result<()>;

    /// ส่งชื่อ listening mode ให้ daemon แปลงเป็นคำสั่ง AACP
    fn set_listening_mode(&self, mode: &str) -> zbus::Result<()>;

    /// อ่านค่าแบตเตอรี่ล่าสุดของ AirPods ทั้งสองข้างและเคสชาร์จ
    fn battery(&self) -> zbus::Result<BatteryStatus>;

    /// แจ้ง client เมื่อ state หรือ audio setting ของ daemon เปลี่ยน
    #[zbus(signal)]
    fn status_changed(&self, status: DaemonStatus) -> zbus::Result<()>;

    /// แจ้ง client เมื่อ daemon ได้ค่าแบตเตอรี่ชุดใหม่
    #[zbus(signal)]
    fn battery_changed(&self, battery: BatteryStatus) -> zbus::Result<()>;

    /// แจ้ง client เมื่อรายการหรือสถานะอุปกรณ์จาก BlueZ เปลี่ยน
    #[zbus(signal)]
    fn devices_changed(&self, devices: Vec<DeviceInfo>) -> zbus::Result<()>;
}
