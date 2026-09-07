//! ส่งค่าแบตเตอรี่ไปยัง kernel bridge เมื่อ `/dev/airpods_power` พร้อมใช้งาน

use std::io;
use std::path::PathBuf;

use airpods_ipc::BatteryStatus;
use tokio::io::AsyncWriteExt;

pub enum UpdateOutcome {
    Written,
    Missing,
}

#[derive(Debug, Clone)]
pub struct PowerBridge {
    path: PathBuf,
}

impl Default for PowerBridge {
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

    pub async fn update(&self, battery: BatteryStatus) -> io::Result<UpdateOutcome> {
        let mut device = match tokio::fs::OpenOptions::new().write(true).open(&self.path).await {
            Ok(device) => device,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(UpdateOutcome::Missing)
            }
            Err(error) => return Err(error),
        };
        let payload = [
            1,
            u8::from(battery.left_percent >= 0),
            percent_byte(battery.left_percent),
            u8::from(battery.left_charging),
            u8::from(battery.right_percent >= 0),
            percent_byte(battery.right_percent),
            u8::from(battery.right_charging),
        ];
        device.write_all(&payload).await?;
        Ok(UpdateOutcome::Written)
    }
}

fn percent_byte(percent: i16) -> u8 {
    percent.clamp(0, 100) as u8
}
