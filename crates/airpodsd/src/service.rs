//! ให้บริการ D-Bus API และประสาน config กับ daemon workers
//!
//! `ManagerService` รับคำสั่งจาก client, validate ค่า และประสาน persisted config กับ desired audio state
//! ส่วน listening mode ใช้ request/response channel เพื่อให้ worker ที่ถือ AACP session เป็นผู้ดำเนินการจริง

use std::sync::Arc;

use airpods_core::aacp::ListeningMode;
use airpods_ipc::{
    BatteryStatus, DaemonStatus, DeviceInfo, MAX_GAIN_DB, MAX_LIMITER_DB, MIN_GAIN_DB,
    MIN_LIMITER_DB,
};
use tokio::sync::{Mutex, RwLock, mpsc, oneshot, watch};

use crate::config::{Config, ConfigStore};

#[derive(Debug, Clone, PartialEq)]
/// snapshot ของ audio state ที่ lifecycle worker ต้องทำให้เป็นจริง
pub struct DesiredAudio {
    /// ระบุว่าผู้ใช้ต้องการให้ virtual microphone ทำงานหรือไม่
    pub enabled: bool,
    /// Bluetooth address ของ AirPods เป้าหมาย
    pub address: String,
    /// gain ปัจจุบันที่ audio engine ต้องใช้
    pub gain_db: f64,
    /// limiter threshold ปัจจุบันที่ audio engine ต้องใช้
    pub limiter_db: f64,
}

impl DesiredAudio {
    /// แปลง persisted config เป็นค่าเริ่มต้นของ watch channel
    pub fn from_config(config: &Config) -> Self {
        Self {
            enabled: config.mic_enabled,
            address: config.selected_device.clone(),
            gain_db: config.gain_db,
            limiter_db: config.limiter_db,
        }
    }
}

/// คำขอเปลี่ยน listening mode ที่ D-Bus ส่งให้ AACP lifecycle worker
#[derive(Debug)]
pub struct ModeRequest {
    /// Bluetooth address ที่ถูกเลือกขณะรับ D-Bus method
    pub address: String,
    /// listening mode ที่ parse เป็น protocol value แล้ว
    pub mode: ListeningMode,
    /// ช่องตอบผลลัพธ์เพียงครั้งเดียวกลับไปยัง D-Bus caller
    response: oneshot::Sender<Result<(), String>>,
}

impl ModeRequest {
    /// รวมคำสั่งและ response channel สำหรับส่งข้าม task ไปยัง AACP lifecycle worker
    pub fn new(
        address: String,
        mode: ListeningMode,
        response: oneshot::Sender<Result<(), String>>,
    ) -> Self {
        Self {
            address,
            mode,
            response,
        }
    }

    /// ส่งผลลัพธ์กลับไปยัง D-Bus method ที่รออยู่
    pub fn respond(self, result: Result<(), String>) {
        let _ = self.response.send(result);
    }
}

#[derive(Debug, Clone)]
/// เหตุการณ์ภายในที่ event forwarder แปลงเป็น D-Bus signal
pub enum Event {
    /// daemon status เปลี่ยน เช่น state, error หรือ audio processing value
    Status(DaemonStatus),
    /// ค่าแบตเตอรี่ left หรือ right เปลี่ยน
    Battery(BatteryStatus),
    /// inventory หรือ connection state ของ AirPods เปลี่ยน
    Devices(Vec<DeviceInfo>),
}

#[derive(Debug)]
/// snapshot กลางที่ D-Bus service และ background workers อ่านหรือแก้ร่วมกัน
pub struct RuntimeState {
    /// สถานะ lifecycle และ config ที่ client อ่านได้
    pub status: DaemonStatus,
    /// รายการ AirPods ล่าสุดจาก BlueZ inventory worker
    pub devices: Vec<DeviceInfo>,
    /// ค่าแบตเตอรี่ล่าสุด หรือค่า unavailable เมื่อข้อมูลหมดอายุ
    pub battery: BatteryStatus,
}

impl RuntimeState {
    /// สร้าง runtime snapshot จาก config พร้อมสถานะ availability ที่ตรวจได้ตอนเริ่ม daemon
    pub fn new(config: &Config, power_bridge_available: bool, config_error: String) -> Self {
        Self {
            status: DaemonStatus {
                state: "idle".to_string(),
                selected_device: config.selected_device.clone(),
                mic_active: false,
                reconnect_attempt: 0,
                gain_db: config.gain_db,
                limiter_db: config.limiter_db,
                power_bridge_available,
                last_error: config_error,
            },
            devices: Vec::new(),
            battery: BatteryStatus::unavailable(),
        }
    }
}

/// runtime snapshot ที่ clone handle ไปให้หลาย async task และล็อกแบบหลาย reader ได้
pub type SharedState = Arc<RwLock<RuntimeState>>;

/// implementation ของ D-Bus Manager1 ที่จัดการ config และส่งงานไป background workers
pub struct ManagerService {
    /// runtime snapshot สำหรับตอบ query และอัปเดตค่าที่ client มองเห็น
    state: SharedState,
    /// config ปัจจุบันที่ serialize การแก้ไขจาก D-Bus methods หลายคำขอ
    config: Arc<Mutex<Config>>,
    /// ตัวเขียน config ลง disk แบบ atomic
    config_store: ConfigStore,
    /// watch channel ที่แจ้ง audio lifecycle เฉพาะ desired state ล่าสุด
    desired: watch::Sender<DesiredAudio>,
    /// bounded channel ของ listening mode request เพื่อไม่สะสมคำสั่งโดยไม่จำกัด
    mode_requests: mpsc::Sender<ModeRequest>,
    /// unbounded channel สำหรับส่ง snapshot ไปยัง D-Bus signal forwarder
    events: mpsc::UnboundedSender<Event>,
}

impl ManagerService {
    /// ประกอบ D-Bus service จาก shared state, persisted config และ worker channels
    pub fn new(
        state: SharedState,
        config: Config,
        config_store: ConfigStore,
        desired: watch::Sender<DesiredAudio>,
        mode_requests: mpsc::Sender<ModeRequest>,
        events: mpsc::UnboundedSender<Event>,
    ) -> Self {
        Self {
            state,
            config: Arc::new(Mutex::new(config)),
            config_store,
            desired,
            mode_requests,
            events,
        }
    }

    /// clone status ภายใต้ read lock แล้วส่ง event หลังปล่อย lock
    async fn publish_status(&self) {
        let status = self.state.read().await.status.clone();
        let _ = self.events.send(Event::Status(status));
    }
}

#[zbus::interface(name = "io.github.abdulloh404.AirPods.Manager1")]
impl ManagerService {
    /// คืน daemon status snapshot ล่าสุด
    async fn status(&self) -> DaemonStatus {
        self.state.read().await.status.clone()
    }

    /// คืน BlueZ inventory snapshot ล่าสุด
    async fn list_devices(&self) -> Vec<DeviceInfo> {
        self.state.read().await.devices.clone()
    }

    /// เลือก AirPods ด้วย Bluetooth address และแจ้งทั้ง inventory client กับ audio lifecycle
    async fn select_device(&self, address: &str) -> zbus::fdo::Result<()> {
        if !valid_bluetooth_address(address) {
            return Err(zbus::fdo::Error::InvalidArgs(
                "expected Bluetooth address in XX:XX:XX:XX:XX:XX format".to_string(),
            ));
        }

        // serialize การแก้ config และ commit ลง disk ก่อนเปลี่ยน runtime state
        let next = {
            let mut config = self.config.lock().await;
            let mut next = config.clone();
            next.selected_device = address.to_ascii_uppercase();
            self.config_store
                .save(&next)
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
            *config = next.clone();
            next
        };

        // อัปเดต selected flag ทั้งชุดใน write lock เดียวเพื่อให้ client ไม่เห็น state ครึ่งทาง
        let devices = {
            let mut state = self.state.write().await;
            state.status.selected_device = next.selected_device.clone();
            for device in &mut state.devices {
                device.selected = device.address.eq_ignore_ascii_case(&next.selected_device);
            }
            state.devices.clone()
        };
        self.desired
            .send_modify(|desired| desired.address = next.selected_device.clone());
        let _ = self.events.send(Event::Devices(devices));
        self.publish_status().await;
        Ok(())
    }

    /// เปิด microphone ใน persisted config และปลุก audio lifecycle ผ่าน watch channel
    async fn start_mic(&self) -> zbus::fdo::Result<()> {
        let persisted = {
            let mut config = self.config.lock().await;
            let mut next = config.clone();
            next.mic_enabled = true;
            let result = self.config_store.save(&next);
            *config = next;
            result
        };
        // ให้คำสั่งมีผลกับ runtime แม้การ persist ล้มเหลว และส่ง error ให้ caller รับรู้ว่าไม่คงข้าม restart
        self.desired.send_modify(|desired| desired.enabled = true);
        persisted.map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }

    /// ปิด microphone ใน persisted config และแจ้ง audio lifecycle ให้ cleanup session
    async fn stop_mic(&self) -> zbus::fdo::Result<()> {
        let persisted = {
            let mut config = self.config.lock().await;
            let mut next = config.clone();
            next.mic_enabled = false;
            let result = self.config_store.save(&next);
            *config = next;
            result
        };
        // ปิด runtime ต่อไปแม้ disk write ล้มเหลว เพื่อไม่ปล่อย microphone ทำงานสวนคำสั่งผู้ใช้
        self.desired.send_modify(|desired| desired.enabled = false);
        persisted.map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }

    /// validate และบันทึก gain ก่อนอัปเดต runtime status กับ audio engine ที่กำลังทำงาน
    async fn set_gain(&self, gain_db: f64) -> zbus::fdo::Result<()> {
        if !gain_db.is_finite() || !(MIN_GAIN_DB..=MAX_GAIN_DB).contains(&gain_db) {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "gain must be between {MIN_GAIN_DB} and {MAX_GAIN_DB} dB"
            )));
        }
        {
            let mut config = self.config.lock().await;
            let mut next = config.clone();
            next.gain_db = gain_db;
            self.config_store
                .save(&next)
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
            *config = next;
        }
        self.state.write().await.status.gain_db = gain_db;
        self.desired
            .send_modify(|desired| desired.gain_db = gain_db);
        self.publish_status().await;
        Ok(())
    }

    /// validate และบันทึก limiter threshold ก่อนอัปเดต runtime status กับ audio engine
    async fn set_limiter_db(&self, limiter_db: f64) -> zbus::fdo::Result<()> {
        if !limiter_db.is_finite() || !(MIN_LIMITER_DB..=MAX_LIMITER_DB).contains(&limiter_db) {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "limiter must be between {MIN_LIMITER_DB} and {MAX_LIMITER_DB} dBFS"
            )));
        }
        {
            let mut config = self.config.lock().await;
            let mut next = config.clone();
            next.limiter_db = limiter_db;
            self.config_store
                .save(&next)
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
            *config = next;
        }
        self.state.write().await.status.limiter_db = limiter_db;
        self.desired
            .send_modify(|desired| desired.limiter_db = limiter_db);
        self.publish_status().await;
        Ok(())
    }

    /// ส่ง listening mode ไปยัง worker ที่ถือหรือสร้าง AACP session แล้วรอผลตอบกลับ
    async fn set_listening_mode(&self, mode: &str) -> zbus::fdo::Result<()> {
        let mode = ListeningMode::parse(mode).ok_or_else(|| {
            zbus::fdo::Error::InvalidArgs(
                "mode must be one of: off, anc, transparency, adaptive".to_string(),
            )
        })?;
        let address = self.config.lock().await.selected_device.clone();
        if address.is_empty() {
            return Err(zbus::fdo::Error::Failed(
                "no AirPods device is selected".to_string(),
            ));
        }

        // oneshot ผูก D-Bus call นี้กับผลของคำสั่งเดียวโดยไม่แชร์ mutable response state
        let (response_tx, response_rx) = oneshot::channel();
        self.mode_requests
            .send(ModeRequest::new(address, mode, response_tx))
            .await
            .map_err(|_| {
                zbus::fdo::Error::Failed("AACP lifecycle worker is unavailable".to_string())
            })?;
        response_rx
            .await
            .map_err(|_| {
                zbus::fdo::Error::Failed("AACP lifecycle worker stopped responding".to_string())
            })?
            .map_err(zbus::fdo::Error::Failed)
    }

    /// คืน battery snapshot ล่าสุดโดยไม่สั่ง scan ใหม่
    async fn battery(&self) -> BatteryStatus {
        self.state.read().await.battery
    }

    #[zbus(signal)]
    /// D-Bus signal ที่แจ้ง daemon status snapshot ใหม่
    async fn status_changed(
        signal_emitter: &zbus::object_server::SignalEmitter<'_>,
        status: DaemonStatus,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    /// D-Bus signal ที่แจ้ง battery snapshot ใหม่
    async fn battery_changed(
        signal_emitter: &zbus::object_server::SignalEmitter<'_>,
        battery: BatteryStatus,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    /// D-Bus signal ที่แจ้ง AirPods inventory snapshot ใหม่
    async fn devices_changed(
        signal_emitter: &zbus::object_server::SignalEmitter<'_>,
        devices: Vec<DeviceInfo>,
    ) -> zbus::Result<()>;
}

/// ตรวจรูปแบบ Bluetooth address เป็นหกกลุ่มของเลขฐานสิบหกกลุ่มละสองหลัก
fn valid_bluetooth_address(address: &str) -> bool {
    let parts: Vec<_> = address.split(':').collect();
    parts.len() == 6
        && parts.iter().all(|part| {
            part.len() == 2 && part.chars().all(|character| character.is_ascii_hexdigit())
        })
}
