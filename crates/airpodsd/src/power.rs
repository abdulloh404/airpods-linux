//! แปลงสถานะแบตเตอรี่เป็น binary protocol ของ kernel power bridge
//!
//! การเปิด `/dev/airpods_power` ใหม่ทุกครั้งทำให้ daemon รองรับ module ที่ถูกโหลดหรือถอดระหว่างทำงาน
//! และรายงานความพร้อมของ bridge กลับไปยัง runtime state ได้จากผลของแต่ละ update

use std::io;
use std::path::PathBuf;

use airpods_ipc::BatteryStatus;
use tokio::io::AsyncWriteExt;

/// ผลการส่งข้อมูลที่แยกกรณีเขียนสำเร็จออกจากกรณี kernel bridge ยังไม่พร้อม
pub enum UpdateOutcome {
    /// เขียน payload ครบลง device node แล้ว
    Written,
    /// ไม่พบ device node จึงไม่มีข้อมูลถูกส่ง
    Missing,
}

#[derive(Debug, Clone)]
/// ตัวส่งสถานะแบตเตอรี่ไปยัง device node ของ kernel module
pub struct PowerBridge {
    /// path ของ device node ที่เปิดใหม่ในแต่ละ update
    path: PathBuf,
}

/// ใช้ device node มาตรฐานที่ kernel module ของโปรเจกต์สร้าง
impl Default for PowerBridge {
    /// สร้าง bridge ที่ชี้ไปยัง `/dev/airpods_power`
    fn default() -> Self {
        Self {
            path: PathBuf::from("/dev/airpods_power"),
        }
    }
}

impl PowerBridge {
    /// ล้างค่าแบตเตอรี่เมื่อ daemon ไม่มีข้อมูลสดหรือกำลังปิดตัว
    pub async fn invalidate(&self) -> io::Result<UpdateOutcome> {
        self.update(BatteryStatus::unavailable()).await
    }

    /// เข้ารหัสสถานะ left, right และ case เป็น payload protocol 2 แล้วเขียนไปยัง kernel bridge
    pub async fn update(&self, battery: BatteryStatus) -> io::Result<UpdateOutcome> {
        let mut device = match tokio::fs::OpenOptions::new().write(true).open(&self.path).await {
            Ok(device) => device,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(UpdateOutcome::Missing)
            }
            Err(error) => return Err(error),
        };
        // byte แรกคือ protocol version ตามด้วย present, percent และ charging ของแต่ละก้อน
        // ส่วนเคสมี stale เพิ่มเพื่อให้ UPower แสดงค่า cache โดยไม่อ้างว่าเป็นสถานะปัจจุบัน
        let payload = [
            2,
            u8::from(battery.left_percent >= 0),
            percent_byte(battery.left_percent),
            u8::from(battery.left_charging),
            u8::from(battery.right_percent >= 0),
            percent_byte(battery.right_percent),
            u8::from(battery.right_charging),
            u8::from(battery.case_percent >= 0),
            percent_byte(battery.case_percent),
            u8::from(battery.case_charging),
            u8::from(battery.case_stale),
        ];
        device.write_all(&payload).await?;
        Ok(UpdateOutcome::Written)
    }
}

/// จำกัด percent ให้อยู่ในช่วงที่ protocol แบบ `u8` รองรับ
fn percent_byte(percent: i16) -> u8 {
    percent.clamp(0, 100) as u8
}
