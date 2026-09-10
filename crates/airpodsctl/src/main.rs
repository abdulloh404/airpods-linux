//! ให้คำสั่ง CLI สำหรับควบคุม `airpodsd` ผ่าน session D-Bus เท่านั้น

use airpods_ipc::{BatteryStatus, DaemonStatus, ManagerProxy};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "airpodsctl", version, about = "Control AirPods on Linux")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show daemon and microphone status.
    Status,
    /// List or select an AirPods device.
    Device {
        #[command(subcommand)]
        command: DeviceCommand,
    },
    /// Control the virtual microphone.
    Mic {
        #[command(subcommand)]
        command: MicCommand,
    },
    /// Set the AirPods listening mode.
    #[command(
        after_help = "Suggestions:\n  adaptive      Everyday use on supported models\n  anc           Noisy environments\n  transparency  Conversations or environmental awareness\n  off           Disable Noise Control"
    )]
    Mode {
        #[command(subcommand)]
        command: ModeCommand,
    },
    /// Control stereo and spatial processing on the existing AirPods output.
    Sound {
        #[command(subcommand)]
        command: SoundCommand,
    },
    /// Show left and right battery values.
    Battery,
}

#[derive(Clone, Copy, Debug, Subcommand)]
enum ModeCommand {
    /// Turn off both ANC and Transparency.
    Off,
    /// Reduce outside noise; recommended in noisy environments.
    Anc,
    /// Let surrounding sound in for conversations or environmental awareness.
    Transparency,
    /// Automatically blend ANC and Transparency on supported AirPods models.
    Adaptive,
}

impl ModeCommand {
    fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Anc => "anc",
            Self::Transparency => "transparency",
            Self::Adaptive => "adaptive",
        }
    }
}

#[derive(Clone, Copy, Debug, Subcommand)]
enum SoundModeCommand {
    /// Preserve the original PipeWire stereo signal.
    Off,
    /// Widen the stereo image without head tracking.
    Wide,
    /// Keep a stationary binaural sound stage.
    #[command(alias = "fixed")]
    Fix,
    /// Use the binaural graph and accept head-pose updates.
    Spatial,
}

impl SoundModeCommand {
    fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Wide => "wide",
            Self::Fix => "fix",
            Self::Spatial => "spatial",
        }
    }
}

#[derive(Debug, Subcommand)]
enum SoundCommand {
    /// Select one output processing mode.
    #[command(
        after_help = "Suggestions:\n  off      Original stereo and lowest processing\n  wide     Wider music playback without head tracking\n  fix      Stationary binaural sound stage\n  spatial  Binaural sound stage with head-pose control"
    )]
    Mode {
        #[command(subcommand)]
        mode: SoundModeCommand,
    },
    /// Show the requested output processing mode.
    Status,
}

#[derive(Debug, Subcommand)]
enum DeviceCommand {
    /// List AirPods known to BlueZ.
    List,
    /// Select the AirPods used by the microphone.
    Select { address: String },
}

#[derive(Debug, Subcommand)]
enum MicCommand {
    /// Start the virtual microphone.
    Start,
    /// Stop the virtual microphone.
    Stop,
    /// Show microphone status.
    Status,
    /// Set pre-limiter gain in dB.
    Gain { db: f64 },
    /// Set the limiter ceiling in dBFS.
    Limiter { db: f64 },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let connection = zbus::Connection::session()
        .await
        .context("failed to connect to the session D-Bus")?;
    let proxy = ManagerProxy::new(&connection)
        .await
        .context("airpodsd is unavailable on the session D-Bus")?;

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
        Command::Sound { command } => match command {
            SoundCommand::Mode { mode } => {
                proxy.set_sound_mode(mode.as_str()).await?;
                println!("Sound mode set to {}.", mode.as_str());
            }
            SoundCommand::Status => print_sound_status(&proxy.status().await?),
        },
        Command::Battery => print_battery(proxy.battery().await?),
    }
    Ok(())
}

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
    println!("Sound mode: {}", status.sound_mode);
    println!(
        "UPower bridge: {}",
        if status.power_bridge_available {
            "available"
        } else {
            "unavailable"
        }
    );
    if !status.last_error.is_empty() {
        println!("Last error: {}", status.last_error);
    }
    if !status.sound_error.is_empty() {
        println!("Sound error: {}", status.sound_error);
    }
}

fn print_sound_status(status: &DaemonStatus) {
    println!("Sound mode: {}", status.sound_mode);
    if !status.sound_error.is_empty() {
        println!("Last error: {}", status.sound_error);
    }
}

fn print_mic_status(status: &DaemonStatus) {
    println!("Microphone: {}", on_off(status.mic_active));
    println!("State: {}", status.state);
    println!("Gain: {:.1} dB", status.gain_db);
    println!("Limiter: {:.1} dBFS", status.limiter_db);
    println!("Reconnect attempt: {}", status.reconnect_attempt);
    if !status.last_error.is_empty() {
        println!("Last error: {}", status.last_error);
    }
}

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

fn percent(value: i16) -> String {
    if value < 0 {
        "unknown".to_string()
    } else {
        format!("{value}%")
    }
}

fn charging(value: bool) -> &'static str {
    if value { " (charging)" } else { "" }
}

fn on_off(value: bool) -> &'static str {
    if value { "active" } else { "inactive" }
}
