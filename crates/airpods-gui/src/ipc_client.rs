//! เชื่อม GTK main loop กับ async D-Bus client โดยไม่ย้าย GTK widget ข้าม thread

use std::sync::mpsc;
use std::time::Duration;

use airpods_ipc::{BatteryStatus, DaemonStatus, DeviceInfo, ManagerProxy};
use futures_util::StreamExt;
use tokio::sync::mpsc as tokio_mpsc;

/// คำสั่งจาก UI ที่ต้องส่งให้ `airpodsd`
#[derive(Debug)]
pub enum Command {
    Refresh,
    SelectDevice(String),
    StartMic,
    StopMic,
    SetGain(f64),
    SetLimiter(f64),
}

/// ข้อมูลจาก daemon ที่ UI นำไปแสดงผล
#[derive(Debug)]
pub enum Event {
    Snapshot {
        status: DaemonStatus,
        devices: Vec<DeviceInfo>,
        battery: BatteryStatus,
    },
    Status(DaemonStatus),
    Battery(BatteryStatus),
    Devices(Vec<DeviceInfo>),
    Error(String),
}

/// Handle สำหรับส่งคำสั่งไปยัง D-Bus worker
#[derive(Clone)]
pub struct Client {
    commands: tokio_mpsc::UnboundedSender<Command>,
}

impl Client {
    /// เริ่ม D-Bus worker และคืน event receiver สำหรับ GTK main loop
    pub fn start() -> (Self, mpsc::Receiver<Event>) {
        let (command_tx, command_rx) = tokio_mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::channel();

        std::thread::Builder::new()
            .name("airpods-gui-dbus".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to create D-Bus runtime");
                runtime.block_on(run(command_rx, event_tx));
            })
            .expect("failed to start D-Bus worker");

        (
            Self {
                commands: command_tx,
            },
            event_rx,
        )
    }

    /// ส่งคำสั่งโดยไม่ block GTK main loop
    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }
}

async fn run(mut commands: tokio_mpsc::UnboundedReceiver<Command>, events: mpsc::Sender<Event>) {
    loop {
        let connection = match zbus::Connection::session().await {
            Ok(connection) => connection,
            Err(error) => {
                send_error(&events, "Cannot connect to the session bus", &error);
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        let proxy = match ManagerProxy::new(&connection).await {
            Ok(proxy) => proxy,
            Err(error) => {
                send_error(&events, "airpodsd is not available", &error);
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        let mut status_signals = match proxy.receive_status_changed().await {
            Ok(signals) => signals,
            Err(error) => {
                send_error(&events, "Cannot subscribe to daemon state", &error);
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        let mut battery_signals = match proxy.receive_battery_changed().await {
            Ok(signals) => signals,
            Err(error) => {
                send_error(&events, "Cannot subscribe to battery state", &error);
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        let mut device_signals = match proxy.receive_devices_changed().await {
            Ok(signals) => signals,
            Err(error) => {
                send_error(&events, "Cannot subscribe to device state", &error);
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        if let Err(error) = send_snapshot(&proxy, &events).await {
            send_error(&events, "airpodsd is not available", &error);
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }

        loop {
            tokio::select! {
                command = commands.recv() => {
                    let Some(command) = command else {
                        return;
                    };
                    if let Err(error) = handle_command(&proxy, command, &events).await {
                        send_error(&events, "The requested action failed", &error);
                    }
                }
                signal = status_signals.next() => {
                    let Some(signal) = signal else {
                        let _ = events.send(Event::Error("Lost the airpodsd connection. Reconnecting…".into()));
                        break;
                    };
                    match signal.args() {
                        Ok(args) => {
                            let _ = events.send(Event::Status(args.status().clone()));
                        }
                        Err(error) => send_error(&events, "Cannot read daemon update", &error),
                    }
                }
                signal = battery_signals.next() => {
                    let Some(signal) = signal else {
                        let _ = events.send(Event::Error("Lost the airpodsd connection. Reconnecting…".into()));
                        break;
                    };
                    match signal.args() {
                        Ok(args) => {
                            let _ = events.send(Event::Battery(*args.battery()));
                        }
                        Err(error) => send_error(&events, "Cannot read battery update", &error),
                    }
                }
                signal = device_signals.next() => {
                    let Some(signal) = signal else {
                        let _ = events.send(Event::Error("Lost the airpodsd connection. Reconnecting…".into()));
                        break;
                    };
                    match signal.args() {
                        Ok(args) => {
                            let _ = events.send(Event::Devices(args.devices().to_vec()));
                        }
                        Err(error) => send_error(&events, "Cannot read device update", &error),
                    }
                }
            }
        }
    }
}

async fn send_snapshot(proxy: &ManagerProxy<'_>, events: &mpsc::Sender<Event>) -> zbus::Result<()> {
    let status = proxy.status().await?;
    let devices = proxy.list_devices().await?;
    let battery = proxy.battery().await?;
    let _ = events.send(Event::Snapshot {
        status,
        devices,
        battery,
    });
    Ok(())
}

async fn handle_command(
    proxy: &ManagerProxy<'_>,
    command: Command,
    events: &mpsc::Sender<Event>,
) -> zbus::Result<()> {
    match command {
        Command::Refresh => send_snapshot(proxy, events).await,
        Command::SelectDevice(address) => {
            proxy.select_device(&address).await?;
            send_snapshot(proxy, events).await
        }
        Command::StartMic => {
            proxy.start_mic().await?;
            send_snapshot(proxy, events).await
        }
        Command::StopMic => {
            proxy.stop_mic().await?;
            send_snapshot(proxy, events).await
        }
        Command::SetGain(gain_db) => {
            proxy.set_gain(gain_db).await?;
            Ok(())
        }
        Command::SetLimiter(limiter_db) => {
            proxy.set_limiter_db(limiter_db).await?;
            Ok(())
        }
    }
}

fn send_error(events: &mpsc::Sender<Event>, message: &str, error: &dyn std::fmt::Display) {
    let _ = events.send(Event::Error(format!("{message}: {error}")));
}
