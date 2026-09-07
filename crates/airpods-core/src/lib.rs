//! รวบรวม protocol, framing และ domain model ที่ไม่ผูกกับ UI

pub mod aacp;
pub mod battery;
pub mod domain;
pub mod framing;

pub use domain::{AirPodsBattery, BatteryLevel, MicSettings};
