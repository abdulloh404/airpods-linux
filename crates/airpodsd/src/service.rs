//! ให้บริการ D-Bus methods และส่งคำสั่งไปยัง lifecycle worker ของ daemon

use std::sync::Arc;

use airpods_ipc::{
    BatteryStatus, DaemonStatus, DeviceInfo, MAX_GAIN_DB, MAX_LIMITER_DB, MIN_GAIN_DB,
    MIN_LIMITER_DB,
};
use tokio::sync::{mpsc, watch, Mutex, RwLock};

use crate::config::{Config, ConfigStore};

#[derive(Debug, Clone, PartialEq)]
pub struct DesiredAudio {
    pub enabled: bool,
    pub address: String,
    pub gain_db: f64,
    pub limiter_db: f64,
}

impl DesiredAudio {
    pub fn from_config(config: &Config) -> Self {
        Self {
            enabled: false,
            address: config.selected_device.clone(),
            gain_db: config.gain_db,
            limiter_db: config.limiter_db,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    Status(DaemonStatus),
    Battery(BatteryStatus),
    Devices(Vec<DeviceInfo>),
}

#[derive(Debug)]
pub struct RuntimeState {
    pub status: DaemonStatus,
    pub devices: Vec<DeviceInfo>,
    pub battery: BatteryStatus,
}

impl RuntimeState {
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

pub type SharedState = Arc<RwLock<RuntimeState>>;

pub struct ManagerService {
    state: SharedState,
    config: Arc<Mutex<Config>>,
    config_store: ConfigStore,
    desired: watch::Sender<DesiredAudio>,
    events: mpsc::UnboundedSender<Event>,
}

impl ManagerService {
    pub fn new(
        state: SharedState,
        config: Config,
        config_store: ConfigStore,
        desired: watch::Sender<DesiredAudio>,
        events: mpsc::UnboundedSender<Event>,
    ) -> Self {
        Self {
            state,
            config: Arc::new(Mutex::new(config)),
            config_store,
            desired,
            events,
        }
    }

    async fn publish_status(&self) {
        let status = self.state.read().await.status.clone();
        let _ = self.events.send(Event::Status(status));
    }
}

#[zbus::interface(name = "io.github.abdulloh404.AirPods.Manager1")]
impl ManagerService {
    async fn status(&self) -> DaemonStatus {
        self.state.read().await.status.clone()
    }

    async fn list_devices(&self) -> Vec<DeviceInfo> {
        self.state.read().await.devices.clone()
    }

    async fn select_device(&self, address: &str) -> zbus::fdo::Result<()> {
        if !valid_bluetooth_address(address) {
            return Err(zbus::fdo::Error::InvalidArgs(
                "expected Bluetooth address in XX:XX:XX:XX:XX:XX format".to_string(),
            ));
        }

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

        let devices = {
            let mut state = self.state.write().await;
            state.status.selected_device = next.selected_device.clone();
            for device in &mut state.devices {
                device.selected = device
                    .address
                    .eq_ignore_ascii_case(&next.selected_device);
            }
            state.devices.clone()
        };
        self.desired
            .send_modify(|desired| desired.address = next.selected_device.clone());
        let _ = self.events.send(Event::Devices(devices));
        self.publish_status().await;
        Ok(())
    }

    async fn start_mic(&self) -> zbus::fdo::Result<()> {
        self.desired.send_modify(|desired| desired.enabled = true);
        Ok(())
    }

    async fn stop_mic(&self) -> zbus::fdo::Result<()> {
        self.desired.send_modify(|desired| desired.enabled = false);
        Ok(())
    }

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

    async fn battery(&self) -> BatteryStatus {
        self.state.read().await.battery
    }

    #[zbus(signal)]
    async fn status_changed(
        signal_emitter: &zbus::object_server::SignalEmitter<'_>,
        status: DaemonStatus,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn battery_changed(
        signal_emitter: &zbus::object_server::SignalEmitter<'_>,
        battery: BatteryStatus,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn devices_changed(
        signal_emitter: &zbus::object_server::SignalEmitter<'_>,
        devices: Vec<DeviceInfo>,
    ) -> zbus::Result<()>;
}

fn valid_bluetooth_address(address: &str) -> bool {
    let parts: Vec<_> = address.split(':').collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|part| part.len() == 2 && part.chars().all(|character| character.is_ascii_hexdigit()))
}
