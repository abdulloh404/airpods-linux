//! แยก AAC-ELD access unit ออกจาก AACP type `0x58` SDU

use anyhow::{Result, bail};

/// จำนวน byte ก่อน access unit แรกใน AACP type `0x58`
pub const TYPE58_HEADER_LEN: usize = 22;

/// ตรวจว่า packet เป็น AACP audio SDU ที่รองรับหรือไม่
pub fn is_audio_sdu(data: &[u8]) -> bool {
    data.len() >= 8
        && data[0] == 0x04
        && data[2] == 0x04
        && u16::from_le_bytes([data[4], data[5]]) == 0x0058
        && u16::from_le_bytes([data[6], data[7]]) == 0x0001
}

/// คืน slice ของทุก AAC access unit โดยไม่ copy payload
pub fn demux_audio_sdu(data: &[u8]) -> Result<Vec<&[u8]>> {
    if !is_audio_sdu(data) {
        bail!("not an AACP 0x58 audio SDU");
    }
    if data.len() < TYPE58_HEADER_LEN {
        bail!("truncated AACP 0x58 header");
    }

    let mut access_units = Vec::new();
    let mut offset = TYPE58_HEADER_LEN;
    while offset < data.len() {
        if data.len() - offset < 5 {
            bail!("truncated access-unit header at offset {offset}");
        }

        let length = data[offset + 4] as usize;
        let start = offset + 5;
        let end = start + length;
        if end > data.len() {
            bail!("access unit at offset {offset} exceeds packet boundary");
        }

        access_units.push(&data[start..end]);
        offset = end;
    }

    Ok(access_units)
}
