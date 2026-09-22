//! เชื่อม GTK main loop กับ async D-Bus client โดยไม่ย้าย GTK widget ข้าม thread
//!
//! UI ส่ง `Command` ผ่าน async channel ไปยัง worker thread ส่วน worker ส่ง `Event`
//! กลับผ่าน standard channel ให้ GTK ดึงไปใช้บน main thread ของตัวเอง

use std::sync::mpsc;
use std::time::Duration;

use airpods_ipc::{BatteryStatus, DaemonStatus, DeviceInfo, ManagerProxy};
use futures_util::StreamExt;
use tokio::sync::mpsc as tokio_mpsc;

/// คำสั่งจาก UI ที่ต้องส่งให้ `airpodsd`
#[derive(Debug)]
pub enum Command {
    /// ขอ snapshot ล่าสุดของ status, devices และ battery
    Refresh,
    /// เลือกอุปกรณ์ด้วย Bluetooth address แล้วอ่าน snapshot ใหม่
    SelectDevice(String),
    /// ขอให้ daemon เริ่ม virtual microphone
    StartMic,
    /// ขอให้ daemon หยุด virtual microphone
    StopMic,
    /// ตั้งค่า gain ก่อนเข้า limiter หน่วยเป็น dB
    SetGain(f64),
    /// ตั้งค่า limiter ceiling หน่วยเป็น dBFS
    SetLimiter(f64),
}

/// ข้อมูลจาก daemon ที่ UI นำไปแสดงผล
#[derive(Debug)]
pub enum Event {
    /// snapshot ครบชุดสำหรับสร้าง state ของ UI ให้สอดคล้องกันในครั้งเดียว
    Snapshot {
        /// สถานะ daemon และ virtual microphone ล่าสุด
        status: DaemonStatus,
        /// รายการ AirPods ที่ BlueZ รู้จัก
        devices: Vec<DeviceInfo>,
        /// ค่าแบตเตอรี่ล่าสุดของ AirPods ทั้งสองข้างและเคสชาร์จ
        battery: BatteryStatus,
    },
    /// status update ที่มาจาก D-Bus signal
    Status(DaemonStatus),
    /// battery update ที่มาจาก D-Bus signal
    Battery(BatteryStatus),
    /// device update ที่มาจาก D-Bus signal
    Devices(Vec<DeviceInfo>),
    /// ข้อผิดพลาดที่ UI ควรแสดงแก่ผู้ใช้
    Error(String),
}

/// Handle สำหรับส่งคำสั่งไปยัง D-Bus worker
#[derive(Clone)]
pub struct Client {
    /// ฝั่งส่งของ channel ที่มี worker thread เป็นผู้รับเพียงรายเดียว
    commands: tokio_mpsc::UnboundedSender<Command>,
}

impl Client {
    /// เริ่ม D-Bus worker และคืน event receiver สำหรับ GTK main loop
    ///
    /// worker ใช้ Tokio runtime แบบ single-thread เพราะงานหลักรอ I/O จาก D-Bus
    /// ส่วน widget ทั้งหมดยังคงถูกอ่านและแก้เฉพาะบน GTK main thread
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
    ///
    /// หาก worker ปิดไปแล้ว channel จะคืน error ซึ่ง retry ผ่าน sender เดิมไม่ได้
    /// และ worker จะจบไปพร้อม lifecycle ของ application
    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }
}

/// รักษาการเชื่อมต่อ D-Bus รับ signal และ dispatch คำสั่งจนกว่า UI จะปิด channel
async fn run(mut commands: tokio_mpsc::UnboundedReceiver<Command>, events: mpsc::Sender<Event>) {
    // วงรอบชั้นนอกสร้าง connection ใหม่หลัง bus, proxy หรือ signal stream ใช้งานไม่ได้
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

        // สมัคร signal ก่อนอ่าน snapshot เพื่อไม่ให้ update ที่เกิดระหว่าง setup สูญหาย
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

        // รอทั้งคำสั่งจาก UI และ D-Bus signal บน runtime เดียวกัน
        loop {
            tokio::select! {
                command = commands.recv() => {
                    // การปิด sender หมายถึง GTK application จบแล้ว จึงหยุด worker ได้ทันที
                    let Some(command) = command else {
                        return;
                    };
                    if let Err(error) = handle_command(&proxy, command, &events).await {
                        send_error(&events, "The requested action failed", &error);
                    }
                }
                signal = status_signals.next() => {
                    // stream ที่จบลงหมายถึง connection ใช้งานต่อไม่ได้และต้องสร้างใหม่
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
                    // ใช้เส้นทาง reconnect เดียวกันเมื่อ battery signal stream หยุด
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
                    // รายการอุปกรณ์ต้อง reconnect เช่นกันเพื่อให้ subscription กลับมาครบ
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

/// อ่าน state ทั้งสามส่วนตามลำดับแล้วส่งเป็น event เดียวให้ UI
async fn send_snapshot(proxy: &ManagerProxy<'_>, events: &mpsc::Sender<Event>) -> zbus::Result<()> {
    let status = proxy.status().await?;
    let devices = proxy.list_devices().await?;
    let battery = proxy.battery().await?;
    // receiver อาจปิดระหว่าง D-Bus call; ไม่มีงานฟื้นฟูที่ worker ต้องทำในกรณีนั้น
    let _ = events.send(Event::Snapshot {
        status,
        devices,
        battery,
    });
    Ok(())
}

/// แปลงคำสั่ง UI เป็น D-Bus method call และ refresh state เมื่อ action เปลี่ยน lifecycle
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
        // daemon ส่ง status signal หลังปรับค่า จึงไม่ต้องอ่าน snapshot ซ้ำทุกครั้งที่ spin เปลี่ยน
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

/// รวมบริบทที่ผู้ใช้เข้าใจกับรายละเอียด error แล้วส่งให้ GTK แสดงใน banner
fn send_error(events: &mpsc::Sender<Event>, message: &str, error: &dyn std::fmt::Display) {
    let _ = events.send(Event::Error(format!("{message}: {error}")));
}
