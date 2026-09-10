//! ส่งค่า sound mode จาก daemon ไปยัง metadata ที่ WirePlumber Lua เฝ้าดู

use std::ffi::OsString;
use std::time::Duration;

use airpods_ipc::SoundMode;
use anyhow::{Context, bail};
use tokio::process::Command;

const METADATA_NAME: &str = "airpods-linux";
const MODE_KEY: &str = "sound.mode";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

/// อัปเดต mode แบบ event-driven โดยไม่ต้อง poll PipeWire
pub async fn publish_mode(mode: SoundMode) -> anyhow::Result<()> {
    let executable = std::env::var_os("AIRPODS_PW_METADATA")
        .unwrap_or_else(|| OsString::from("pw-metadata"));
    let value = format!("\"{}\"", mode.as_str());
    let child = Command::new(&executable)
        .args([
            "--name",
            METADATA_NAME,
            "0",
            MODE_KEY,
            &value,
            "Spa:String:JSON",
        ])
        .output();
    let output = tokio::time::timeout(COMMAND_TIMEOUT, child)
        .await
        .context("pw-metadata timed out")?
        .with_context(|| format!("failed to run {}", executable.to_string_lossy()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            bail!("pw-metadata exited with {}", output.status);
        }
        bail!("pw-metadata failed: {stderr}");
    }
    Ok(())
}
