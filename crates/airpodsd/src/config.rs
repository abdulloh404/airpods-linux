//! จัดเก็บ daemon config ภายใต้ XDG config directory
//!
//! การอ่านจะ validate ค่าที่มีผลต่อ audio processing ก่อนส่งต่อให้ runtime ส่วนการเขียนใช้
//! temporary file, `fsync` และ atomic rename เพื่อลดโอกาสเหลือไฟล์ครึ่งหนึ่งเมื่อ daemon หยุดกะทันหัน

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use airpods_ipc::{
    DEFAULT_GAIN_DB, DEFAULT_LIMITER_DB, MAX_GAIN_DB, MAX_LIMITER_DB, MIN_GAIN_DB, MIN_LIMITER_DB,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
/// ค่าที่บันทึกข้ามการเริ่ม daemon และใช้สร้างสถานะ audio ที่ต้องการ
pub struct Config {
    /// Bluetooth address ของ AirPods ที่ผู้ใช้เลือก หรือค่าว่างเมื่อยังไม่ได้เลือก
    pub selected_device: String,
    /// ระบุว่าควรเริ่ม microphone lifecycle หลัง daemon พร้อมทำงานหรือไม่
    pub mic_enabled: bool,
    /// gain ที่ส่งให้ audio engine มีหน่วยเป็น dB
    pub gain_db: f64,
    /// threshold ของ limiter ที่ส่งให้ audio engine มีหน่วยเป็น dBFS
    pub limiter_db: f64,
}

/// กำหนดค่าเริ่มต้นที่ปิด microphone และใช้ขอบเขต audio processing กลางของโปรเจกต์
impl Default for Config {
    /// สร้าง config เริ่มต้นที่ยังไม่เลือกอุปกรณ์และปิด microphone
    fn default() -> Self {
        Self {
            selected_device: String::new(),
            mic_enabled: false,
            gain_db: DEFAULT_GAIN_DB,
            limiter_db: DEFAULT_LIMITER_DB,
        }
    }
}

#[derive(Debug, Clone)]
/// ผูกการอ่านและเขียน config เข้ากับ path เดียวตลอดอายุ daemon
pub struct ConfigStore {
    /// ตำแหน่ง `config.toml` ที่ resolve จาก XDG config directory แล้ว
    path: PathBuf,
}

impl ConfigStore {
    /// สร้าง store ที่ชี้ไปยัง `$XDG_CONFIG_HOME/airpods-linux/config.toml`
    pub fn from_xdg() -> io::Result<Self> {
        let base = dirs::config_dir().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "XDG config directory is unavailable")
        })?;
        Ok(Self {
            path: base.join("airpods-linux").join("config.toml"),
        })
    }

    /// อ่านและ validate config หรือคืนค่าเริ่มต้นเมื่อยังไม่มีไฟล์
    pub fn load(&self) -> anyhow::Result<Config> {
        match fs::read_to_string(&self.path) {
            Ok(contents) => {
                let config: Config = toml::from_str(&contents)?;
                config.validate()?;
                Ok(config)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
            Err(error) => Err(error.into()),
        }
    }

    /// บันทึก config แบบ atomic และพยายามยืนยันข้อมูลถึง filesystem ก่อนคืนผลสำเร็จ
    pub fn save(&self, config: &Config) -> anyhow::Result<()> {
        let parent = self.path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "config path has no parent")
        })?;
        fs::create_dir_all(parent)?;

        let temporary = temporary_path(&self.path);
        let encoded = toml::to_string_pretty(config)?;
        // `create_new` ป้องกันการเขียนทับ temporary file ที่อาจชนชื่อจาก process อื่น
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        if let Err(error) = (|| -> io::Result<()> {
            file.write_all(encoded.as_bytes())?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            // sync directory เพื่อยืนยัน metadata ของ rename บน Unix
            sync_directory(parent)?;
            Ok(())
        })() {
            // ลบเศษ temporary file เมื่อขั้นตอนใดขั้นตอนหนึ่งไม่สำเร็จ โดยคง error เดิมไว้
            let _ = fs::remove_file(&temporary);
            return Err(error.into());
        }
        Ok(())
    }
}

impl Config {
    /// ปฏิเสธค่าที่ไม่เป็นจำนวนจริงหรืออยู่นอกช่วงที่ IPC และ audio engine รองรับ
    fn validate(&self) -> anyhow::Result<()> {
        if !self.gain_db.is_finite() || !(MIN_GAIN_DB..=MAX_GAIN_DB).contains(&self.gain_db) {
            anyhow::bail!("configured gain is outside the supported range");
        }
        if !self.limiter_db.is_finite()
            || !(MIN_LIMITER_DB..=MAX_LIMITER_DB).contains(&self.limiter_db)
        {
            anyhow::bail!("configured limiter is outside the supported range");
        }
        Ok(())
    }
}

/// สร้างชื่อ temporary file ที่ลดโอกาสชนกันด้วย process id และเวลาระดับ nanosecond
fn temporary_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.with_file_name(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        nonce
    ))
}

#[cfg(unix)]
/// flush metadata ของ directory หลัง atomic rename
fn sync_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
/// รักษา API เดียวกันบนระบบที่ไม่มีวิธี sync directory แบบ Unix
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}
