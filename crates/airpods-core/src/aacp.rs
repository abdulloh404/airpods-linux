//! จัดการ AACP session ผ่าน Bluetooth L2CAP แบบ `SeqPacket`
//!
//! flow หลักคือเชื่อมต่อ PSM เฉพาะของ AACP รอให้ kernel ยืนยัน peer แล้วส่ง
//! handshake และ feature command ก่อนรับ notification หรือสั่ง microphone stream

use anyhow::{Context, Result, bail};
use bluer::{
    Address, AddressType,
    l2cap::{SeqPacket, Socket, SocketAddr},
};
use log::{debug, info};
use std::{sync::Arc, time::Duration};

/// PSM ที่ AirPods เปิดไว้สำหรับ AACP บน Bluetooth BR/EDR
const AACP_PSM: u16 = 0x1001;
/// เวลาสูงสุดสำหรับสร้าง connection และรอ peer พร้อมใช้งาน
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// เวลาสูงสุดของการส่ง packet หนึ่งครั้งรวมช่วง retry
const IO_TIMEOUT: Duration = Duration::from_secs(3);
/// ช่วงพักระหว่างตรวจ connection หรือ retry เมื่อ socket ยังไม่พร้อม
const CONNECTION_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// จำนวนครั้งสูงสุดที่ยอม retry หลัง kernel คืน `ENOTCONN`
const SEND_RETRY_LIMIT: usize = 10;

/// packet เริ่มต้นที่เปิด AACP session
const AACP_HANDSHAKE: [u8; 16] = [
    0x00, 0x00, 0x04, 0x00, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];
/// packet เลือก feature ที่จำเป็นต่อ notification ของ session
const AACP_SET_SPECIFIC_FEATURES: [u8; 14] = [
    0x04, 0x00, 0x04, 0x00, 0x4d, 0x00, 0xd7, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];
/// packet สมัครรับ AACP notifications ทุกชนิดที่อุปกรณ์รองรับ
const AACP_REQUEST_NOTIFICATIONS: [u8; 10] = [
    0x04, 0x00, 0x04, 0x00, 0x0f, 0x00, 0xff, 0xff, 0xff, 0xff,
];
/// prefix ที่ใช้ยืนยันว่า AirPods ตอบ handshake แล้ว
const AACP_HANDSHAKE_ACK_PREFIX: [u8; 4] = [0x01, 0x00, 0x04, 0x00];
/// prefix ที่ใช้ยืนยันว่า AirPods รับ feature setup แล้ว
const AACP_FEATURES_ACK_PREFIX: [u8; 6] = [0x04, 0x00, 0x04, 0x00, 0x2b, 0x00];
/// packet เปิด AAC-ELD microphone stream
const AACP_START_AUDIO: [u8; 19] = [
    0x04, 0x00, 0x04, 0x00, 0x58, 0x00, 0x00, 0x00, 0x09, 0x00, 0x00, 0x01, 0x82, 0x00, 0x00, 0x00,
    0x04, 0x96, 0x00,
];
/// packet ปิด microphone stream ที่เปิดผ่าน AACP
const AACP_STOP_AUDIO: [u8; 12] = [
    0x04, 0x00, 0x04, 0x00, 0x58, 0x00, 0x00, 0x00, 0x02, 0x00, 0x03, 0x01,
];
/// template ของ control command ที่แทนค่า listening mode ที่ index 7
const AACP_SET_LISTENING_MODE: [u8; 11] = [
    0x04, 0x00, 0x04, 0x00, 0x09, 0x00, 0x0D, 0x00, 0x00, 0x00, 0x00,
];

/// listening mode ที่ AirPods รองรับผ่าน AACP control command `0x0D`
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ListeningMode {
    /// ปิดทั้ง noise cancellation และ transparency
    Off = 0x01,
    /// เปิด active noise cancellation
    NoiseCancellation = 0x02,
    /// เปิด transparency เพื่อรับเสียงภายนอก
    Transparency = 0x03,
    /// ให้อุปกรณ์ปรับระดับการตัดเสียงตามสภาพแวดล้อม
    Adaptive = 0x04,
}

impl ListeningMode {
    /// แปลงชื่อที่ใช้ใน CLI และ D-Bus เป็น listening mode
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "anc" => Some(Self::NoiseCancellation),
            "transparency" => Some(Self::Transparency),
            "adaptive" => Some(Self::Adaptive),
            _ => None,
        }
    }

    /// คืนชื่อคงที่ที่ใช้แสดงใน CLI และ log
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::NoiseCancellation => "anc",
            Self::Transparency => "transparency",
            Self::Adaptive => "adaptive",
        }
    }
}

/// AACP transport หนึ่ง session สำหรับ AirPods หนึ่งคู่
pub struct AacpSession {
    /// socket ใช้ `Arc` เพื่อให้ future รับและส่งยืม transport เดียวกันได้อย่างปลอดภัย
    socket: Arc<SeqPacket>,
    /// บันทึกว่า START สำเร็จแล้ว เพื่อไม่ส่งคำสั่งซ้ำและใช้ตัดสินใจส่ง STOP
    started: bool,
}

impl AacpSession {
    /// เชื่อมต่อ AACP PSM ของ AirPods ที่ระบุ
    pub async fn connect(address: Address) -> Result<Self> {
        let socket_address = SocketAddr::new(address, AddressType::BrEdr, AACP_PSM);
        let socket = Socket::new_seq_packet().context("failed to create AACP L2CAP socket")?;
        let socket = tokio::time::timeout(CONNECT_TIMEOUT, socket.connect(socket_address))
            .await
            .context("AACP L2CAP connection timed out")?
            .context("AACP L2CAP connection failed")?;

        let socket = Arc::new(socket);
        // `connect` อาจคืนก่อน L2CAP peer มี CID จึง poll จนพร้อมหรือหมดเวลา
        tokio::time::timeout(CONNECT_TIMEOUT, async {
            loop {
                match socket.peer_addr() {
                    Ok(peer) if peer.cid != 0 => break Ok(()),
                    Ok(_) => {}
                    Err(error) if error.raw_os_error() == Some(libc::ENOTCONN) => {}
                    Err(error) => break Err(error),
                }
                tokio::time::sleep(CONNECTION_POLL_INTERVAL).await;
            }
        })
        .await
        .context("AACP L2CAP peer did not become ready within 10 seconds")?
        .context("failed to verify AACP L2CAP peer")?;

        info!("[bt] connected {address} on PSM 0x{AACP_PSM:04X}");
        Ok(Self {
            socket,
            started: false,
        })
    }

    /// ส่ง handshake เริ่มต้นของ AACP session
    pub async fn initialize(&self) -> Result<()> {
        self.send(&AACP_HANDSHAKE)
            .await
            .context("AACP handshake failed")?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        info!("[aacp] session initialized");
        Ok(())
    }

    /// ระบุว่า packet เป็นการตอบรับ AACP handshake
    pub fn is_handshake_ack(packet: &[u8]) -> bool {
        packet.starts_with(&AACP_HANDSHAKE_ACK_PREFIX)
    }

    /// ระบุว่า packet เป็นการตอบรับการตั้งค่า AACP features
    pub fn is_features_ack(packet: &[u8]) -> bool {
        packet.starts_with(&AACP_FEATURES_ACK_PREFIX)
    }

    /// ส่ง feature setup หลังได้รับ handshake ACK
    pub async fn set_specific_features(&self) -> Result<()> {
        self.send(&AACP_SET_SPECIFIC_FEATURES)
            .await
            .context("failed to configure AACP notification features")
    }

    /// ขอให้ AirPods ส่ง AACP notifications รวมถึงค่าแบตเตอรี่
    pub async fn request_notifications(&self) -> Result<()> {
        self.send(&AACP_REQUEST_NOTIFICATIONS)
            .await
            .context("failed to request AACP notifications")
    }

    /// สั่ง AirPods ให้เริ่มส่ง AAC-ELD microphone stream
    pub async fn start_audio(&mut self) -> Result<()> {
        if self.started {
            return Ok(());
        }
        // ตั้งค่าก่อน `await` เพื่อให้ caller ที่ cancel งานยังส่ง STOP แบบ best-effort ได้
        self.started = true;
        if let Err(error) = self.send(&AACP_START_AUDIO).await {
            self.started = false;
            return Err(error).context("AACP microphone START failed");
        }
        info!("[aacp] hi-res microphone START sent");
        Ok(())
    }

    /// สั่ง AirPods ให้หยุด microphone stream หากเคยเริ่มไว้
    pub async fn stop_audio(&mut self) -> Result<()> {
        if !self.started {
            return Ok(());
        }
        self.send(&AACP_STOP_AUDIO)
            .await
            .context("AACP microphone STOP failed")?;
        self.started = false;
        info!("[aacp] hi-res microphone STOP sent");
        Ok(())
    }

    /// ส่ง AACP control command เพื่อเปลี่ยน listening mode ของ AirPods
    pub async fn set_listening_mode(&self, mode: ListeningMode) -> Result<()> {
        let mut packet = AACP_SET_LISTENING_MODE;
        packet[7] = mode as u8;
        self.send(&packet)
            .await
            .context("failed to set AirPods listening mode")?;
        info!("[aacp] listening mode command sent: {}", mode.as_str());
        Ok(())
    }

    /// รับ AACP SDU หนึ่ง packet ลง buffer ที่กำหนด
    pub async fn recv(&self, buffer: &mut [u8]) -> Result<usize> {
        self.socket
            .recv(buffer)
            .await
            .context("Bluetooth receive failed")
    }

    /// ระบุว่า session ได้ส่งคำสั่ง START แล้วหรือไม่
    pub fn is_audio_started(&self) -> bool {
        self.started
    }

    /// ส่ง packet ให้ครบทั้งก้อน เพราะ AACP ใช้ขอบเขต SDU ของ `SeqPacket`
    async fn send(&self, packet: &[u8]) -> Result<()> {
        let send = async {
            let mut attempts = 0;
            loop {
                match self.socket.send(packet).await {
                    // kernel อาจยังรายงาน `ENOTCONN` ชั่วคราวหลังสร้าง L2CAP socket สำเร็จ
                    Err(error)
                        if error.raw_os_error() == Some(libc::ENOTCONN)
                            && attempts < SEND_RETRY_LIMIT =>
                    {
                        attempts += 1;
                        debug!(
                            "[aacp] socket not ready; retrying send ({attempts}/{SEND_RETRY_LIMIT})"
                        );
                        tokio::time::sleep(CONNECTION_POLL_INTERVAL).await;
                    }
                    result => break result,
                }
            }
        };

        let written = tokio::time::timeout(IO_TIMEOUT, send)
            .await
            .context("Bluetooth send timed out")?
            .context("Bluetooth send failed")?;
        if written != packet.len() {
            bail!(
                "short Bluetooth packet write: {written}/{} bytes",
                packet.len()
            );
        }
        debug!("[aacp] sent {}-byte packet", packet.len());
        Ok(())
    }
}
