//! รวบรวม protocol, framing และ domain model ที่ไม่ผูกกับ UI
//!
//! crate นี้เป็นชั้นกลางที่ daemon ใช้คุยกับ AirPods และแปลงข้อมูลดิบให้ CLI,
//! GUI และ audio engine ใช้ต่อได้โดยไม่ต้องรู้รูปแบบ AACP หรือ BLE packet

pub mod aacp;
pub mod battery;
pub mod domain;
pub mod framing;

pub use domain::{AirPodsBattery, BatteryLevel, MicSettings};
