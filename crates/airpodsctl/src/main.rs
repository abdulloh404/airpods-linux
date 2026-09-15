//! ให้คำสั่ง CLI สำหรับควบคุม `airpodsd` ผ่าน session D-Bus
//!
//! CLI แปลง subcommand เป็น method call ตาม D-Bus contract แล้วพิมพ์ snapshot
//! หรือผลยืนยันกลับทาง standard output โดยไม่ถือครอง Bluetooth และ audio device เอง

use airpods_ipc::{BatteryStatus, DaemonStatus, ManagerProxy};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "airpodsctl", version, about = "Control AirPods on Linux")]
/// รับและแยกคำสั่งระดับบนสุดของ `airpodsctl`
struct Cli {
    /// คำสั่งที่ผู้ใช้เลือกให้ส่งต่อไปยัง daemon
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
/// กลุ่มงานหลักที่ CLI รองรับ
enum Command {
    /// แสดงภาพรวมของ daemon และ virtual microphone
    #[command(about = "Show daemon and microphone status", long_about = None)]
    Status,
    /// จัดการ AirPods ที่ daemon มองเห็นผ่าน BlueZ
    #[command(about = "List or select an AirPods device", long_about = None)]
    Device {
        /// งานย่อยสำหรับอ่านหรือเปลี่ยนอุปกรณ์เป้าหมาย
        #[command(subcommand)]
        command: DeviceCommand,
    },
    /// ควบคุม lifecycle และระดับเสียงของ virtual microphone
    #[command(about = "Control the virtual microphone", long_about = None)]
    Mic {
        /// งานย่อยสำหรับควบคุม virtual microphone
        #[command(subcommand)]
        command: MicCommand,
    },
    /// ตั้งค่า listening mode ของ AirPods ผ่าน AACP
    #[command(
        about = "Set the AirPods listening mode",
        long_about = None,
        after_help = "Suggestions:\n  adaptive      Everyday use on supported models\n  anc           Noisy environments\n  transparency  Conversations or environmental awareness\n  off           Disable Noise Control"
    )]
    Mode {
        /// listening mode ที่ต้องการส่งให้ daemon
        #[command(subcommand)]
        command: ModeCommand,
    },
    /// แสดงค่าแบตเตอรี่ล่าสุดของ AirPods ทั้งสองข้าง
    #[command(about = "Show left and right battery values", long_about = None)]
    Battery,
}

#[derive(Clone, Copy, Debug, Subcommand)]
/// listening mode ที่แปลงเป็นค่าตาม D-Bus contract ได้โดยตรง
enum ModeCommand {
    /// ปิดทั้ง ANC และ Transparency
    #[command(about = "Turn off both ANC and Transparency", long_about = None)]
    Off,
    /// ลดเสียงจากภายนอกด้วย ANC
    #[command(
        about = "Reduce outside noise; recommended in noisy environments",
        long_about = None
    )]
    Anc,
    /// เปิดเสียงรอบข้างเพื่อสนทนาหรือรับรู้สภาพแวดล้อม
    #[command(
        about = "Let surrounding sound in for conversations or environmental awareness",
        long_about = None
    )]
    Transparency,
    /// ให้ AirPods รุ่นที่รองรับผสม ANC และ Transparency โดยอัตโนมัติ
    #[command(
        about = "Automatically blend ANC and Transparency on supported AirPods models",
        long_about = None
    )]
    Adaptive,
}

impl ModeCommand {
    /// คืนค่าชื่อ mode ที่ D-Bus API กำหนดไว้
    fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Anc => "anc",
            Self::Transparency => "transparency",
            Self::Adaptive => "adaptive",
        }
    }
}

#[derive(Debug, Subcommand)]
/// งานที่เกี่ยวกับการค้นหาและเลือก AirPods
enum DeviceCommand {
    /// แสดง AirPods ที่ BlueZ รู้จัก
    #[command(about = "List AirPods known to BlueZ", long_about = None)]
    List,
    /// เลือก AirPods ที่ virtual microphone จะใช้งาน
    #[command(
        about = "Select the AirPods used by the microphone",
        long_about = None
    )]
    Select { address: String },
}

#[derive(Debug, Subcommand)]
/// งานที่ควบคุม virtual microphone และ DSP setting
enum MicCommand {
    /// เริ่ม virtual microphone
    #[command(about = "Start the virtual microphone", long_about = None)]
    Start,
    /// หยุด virtual microphone
    #[command(about = "Stop the virtual microphone", long_about = None)]
    Stop,
    /// แสดงสถานะของ virtual microphone
    #[command(about = "Show microphone status", long_about = None)]
    Status,
    /// ตั้งค่า gain ก่อนเข้า limiter หน่วยเป็น dB
    #[command(about = "Set pre-limiter gain in dB", long_about = None)]
    Gain { db: f64 },
    /// ตั้งค่า limiter ceiling หน่วยเป็น dBFS
    #[command(about = "Set the limiter ceiling in dBFS", long_about = None)]
    Limiter { db: f64 },
}

/// เชื่อมต่อ session bus สร้าง proxy และ dispatch subcommand ที่ผู้ใช้ระบุ
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let connection = zbus::Connection::session()
        .await
        .context("failed to connect to the session D-Bus")?;
    let proxy = ManagerProxy::new(&connection)
        .await
        .context("airpodsd is unavailable on the session D-Bus")?;

    // แต่ละ branch เรียก D-Bus เพียงเท่าที่คำสั่งต้องใช้ แล้วจัดรูปผลสำหรับ terminal
    match cli.command {
        Command::Status => print_status(&proxy.status().await?),
        Command::Device { command } => match command {
            DeviceCommand::List => {
                let devices = proxy.list_devices().await?;
                if devices.is_empty() {
                    println!("No AirPods found by BlueZ.");
                } else {
                    for device in devices {
                        let selected = if device.selected { "*" } else { " " };
                        let connected = if device.connected {
                            "connected"
                        } else {
                            "disconnected"
                        };
                        println!(
                            "{selected} {}  {}  {connected}",
                            device.address, device.name
                        );
                    }
                }
            }
            DeviceCommand::Select { address } => {
                proxy.select_device(&address).await?;
                println!("Selected {address}.");
            }
        },
        Command::Mic { command } => match command {
            MicCommand::Start => {
                proxy.start_mic().await?;
                println!("Microphone start requested.");
            }
            MicCommand::Stop => {
                proxy.stop_mic().await?;
                println!("Microphone stop requested.");
            }
            MicCommand::Status => print_mic_status(&proxy.status().await?),
            MicCommand::Gain { db } => {
                proxy.set_gain(db).await?;
                println!("Gain set to {db:.1} dB.");
            }
            MicCommand::Limiter { db } => {
                proxy.set_limiter_db(db).await?;
                println!("Limiter set to {db:.1} dBFS.");
            }
        },
        Command::Mode { command } => {
            proxy.set_listening_mode(command.as_str()).await?;
            println!("Listening mode command sent: {}.", command.as_str());
        }
        Command::Battery => print_battery(proxy.battery().await?),
    }
    Ok(())
}

/// พิมพ์สถานะรวม พร้อมซ่อนรายละเอียดที่ยังไม่มีค่าจาก daemon
fn print_status(status: &DaemonStatus) {
    println!("State: {}", status.state);
    println!(
        "Device: {}",
        if status.selected_device.is_empty() {
            "not selected"
        } else {
            &status.selected_device
        }
    );
    println!("Microphone: {}", on_off(status.mic_active));
    println!("Gain: {:.1} dB", status.gain_db);
    println!("Limiter: {:.1} dBFS", status.limiter_db);
    println!(
        "UPower bridge: {}",
        if status.power_bridge_available {
            "available"
        } else {
            "unavailable"
        }
    );
    // สตริงว่างหมายถึง daemon ไม่มี error ที่ต้องรายงานใน snapshot นี้
    if !status.last_error.is_empty() {
        println!("Last error: {}", status.last_error);
    }
}

/// พิมพ์เฉพาะข้อมูลที่เกี่ยวกับ virtual microphone และการ reconnect
fn print_mic_status(status: &DaemonStatus) {
    println!("Microphone: {}", on_off(status.mic_active));
    println!("State: {}", status.state);
    println!("Gain: {:.1} dB", status.gain_db);
    println!("Limiter: {:.1} dBFS", status.limiter_db);
    println!("Reconnect attempt: {}", status.reconnect_attempt);
    // แสดง error เฉพาะเมื่อ daemon ส่งรายละเอียดมา เพื่อให้ output ปกติกระชับ
    if !status.last_error.is_empty() {
        println!("Last error: {}", status.last_error);
    }
}

/// พิมพ์แบตเตอรี่สองข้างด้วยรูปแบบเดียวกัน รวมเครื่องหมายชาร์จเมื่อมีข้อมูล
fn print_battery(battery: BatteryStatus) {
    println!(
        "Left: {}{}",
        percent(battery.left_percent),
        charging(battery.left_charging)
    );
    println!(
        "Right: {}{}",
        percent(battery.right_percent),
        charging(battery.right_charging)
    );
}

/// แปลงค่าร้อยละจาก D-Bus เป็นข้อความ โดยตีความค่าติดลบว่าไม่มีข้อมูล
fn percent(value: i16) -> String {
    if value < 0 {
        "unknown".to_string()
    } else {
        format!("{value}%")
    }
}

/// คืนส่วนต่อท้ายที่ระบุสถานะชาร์จสำหรับบรรทัดแบตเตอรี่
fn charging(value: bool) -> &'static str {
    if value { " (charging)" } else { "" }
}

/// แปลง boolean ของ daemon เป็นคำสถานะที่ CLI ใช้ร่วมกัน
fn on_off(value: bool) -> &'static str {
    if value { "active" } else { "inactive" }
}
