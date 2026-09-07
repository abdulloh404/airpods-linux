//! กำหนด domain model ที่ daemon, CLI และ GUI ใช้ร่วมกัน

/// ระดับแบตเตอรี่หนึ่งก้อนพร้อมสถานะการชาร์จ
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BatteryLevel {
    /// เปอร์เซ็นต์แบตเตอรี่ หรือ `None` เมื่ออุปกรณ์ไม่ได้รายงานค่า
    pub percent: Option<u8>,
    /// ระบุว่าก้อนนี้กำลังชาร์จอยู่หรือไม่
    pub charging: bool,
}

/// สถานะแบตเตอรี่ที่อ่านจาก AirPods BLE advertisement
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AirPodsBattery {
    /// แบตเตอรี่หูฟังข้างซ้าย
    pub left: BatteryLevel,
    /// แบตเตอรี่หูฟังข้างขวา
    pub right: BatteryLevel,
    /// แบตเตอรี่เคส
    pub case: BatteryLevel,
    /// รหัสรุ่นจาก Apple manufacturer data
    pub model_id: u16,
    /// BLE address ที่ส่ง advertisement
    pub ble_address: String,
    /// ความแรงสัญญาณของ advertisement
    pub rssi: i16,
}

/// ค่า DSP สำหรับ virtual microphone
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MicSettings {
    /// gain ก่อนเข้า limiter หน่วย dB
    pub gain_db: f32,
    /// เพดาน limiter หน่วย dBFS
    pub limiter_dbfs: f32,
}

impl Default for MicSettings {
    fn default() -> Self {
        Self {
            gain_db: 18.0,
            limiter_dbfs: -3.0,
        }
    }
}
