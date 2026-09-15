//! ครอบ C++ audio engine และเปิด API แบบ Rust ให้ `airpodsd`
//!
//! Rust layer ตรวจ config และ ownership ก่อนส่งผ่าน C ABI ส่วน native engine รับผิดชอบ
//! AAC-ELD decoder, DSP, SPSC queue และ PipeWire realtime callback

use anyhow::{Result, anyhow, bail};
use std::{
    ffi::{CStr, CString, c_char},
    ptr::NonNull,
};

/// PCM clock ที่ใช้เผยแพร่ virtual microphone
pub const SAMPLE_RATE: u32 = 64_000;
/// จำนวน channel ของ virtual microphone
pub const CHANNELS: u8 = 1;
/// ชื่อภายในของ PipeWire node ที่ client ใช้อ้างอิง source
pub const SOURCE_NAME: &str = "Microphone_Virtual_Abdullohs_AirPods_Pro";
/// ชื่อ PipeWire source ที่แสดงใน audio settings
pub const SOURCE_DESCRIPTION: &str = "Microphone virtual - Abdulloh's AirPods Pro";
/// gain เริ่มต้นก่อนเข้า limiter
pub const DEFAULT_GAIN_DB: f32 = 0.0;
/// limiter เริ่มต้นหน่วย dBFS
pub const DEFAULT_LIMITER_DBFS: f32 = -3.0;

/// ขนาด buffer ที่ C++ ใช้เขียนข้อความ error กลับมายัง Rust
const ERROR_BUFFER_SIZE: usize = 512;

/// opaque type ที่แทน C++ `airpods_audio_engine` โดย Rust ไม่เข้าถึง layout ภายใน
#[repr(C)]
struct NativeEngine {
    /// ป้องกันการสร้างค่าชนิดนี้จากฝั่ง Rust โดยไม่ผ่าน native constructor
    _private: [u8; 0],
}

/// config ที่ส่งข้าม C ABI; pointer ของ string ต้องมีอายุถึง constructor คืนค่า
#[repr(C)]
struct NativeConfig {
    /// pointer ไปยังชื่อ node แบบ null-terminated
    node_name: *const c_char,
    /// pointer ไปยังคำอธิบาย node แบบ null-terminated
    node_description: *const c_char,
    /// gain ก่อน limiter หน่วย dB
    gain_db: f32,
    /// เพดาน limiter หน่วย dBFS
    limiter_dbfs: f32,
    /// ความจุ physical queue หน่วย millisecond
    queue_capacity_ms: u32,
}

/// metric layout ที่ต้องตรงกับ `airpods_audio_metrics` ใน C++ header
#[repr(C)]
#[derive(Default)]
struct NativeMetrics {
    /// จำนวน access unit ที่ native engine รับ
    access_units: u64,
    /// จำนวน frame ที่ decoder คืน PCM สำเร็จ
    decoded_frames: u64,
    /// จำนวน PCM sample ที่ decoder คืนสำเร็จ
    decoded_samples: u64,
    /// จำนวน frame ที่ใส่ queue ไม่ได้
    queue_drops: u64,
    /// จำนวน access unit ที่ decoder ปฏิเสธ
    decode_errors: u64,
    /// จำนวน callback ที่ sample ใน queue ไม่พอ
    underflows: u64,
    /// จำนวน silence sample ที่ส่งออกทั้งหมด
    silence_samples: u64,
    /// จำนวน sample เก่าที่ consumer ทิ้งเพื่อลด latency
    stale_samples_dropped: u64,
    /// จำนวน silence sample ที่ใช้แทน frame เสีย
    concealed_samples: u64,
    /// ช่องว่างมากที่สุดระหว่าง access unit หน่วย microsecond
    maximum_packet_gap_microseconds: u64,
    /// jitter target ล่าสุดหน่วย sample
    target_samples: u64,
    /// จำนวน sample ที่ callback ล่าสุดร้องขอ
    requested_samples: u64,
    /// จำนวน sample สูงสุดที่ callback เคยร้องขอ
    maximum_requested_samples: u64,
    /// clock correction ล่าสุดหน่วย parts per million
    rate_correction_ppm: i64,
    /// จำนวน sample ที่ค้างใน queue ตอนเก็บ snapshot
    queued_samples: u64,
}

// ฟังก์ชันชุดนี้เป็น C ABI; ทุก pointer และ status ถูกแปลงเป็น API ที่ปลอดภัยกว่าใน `AudioEngine`
unsafe extern "C" {
    /// สร้าง native engine และคืน null พร้อมข้อความ error เมื่อสร้างไม่สำเร็จ
    fn airpods_audio_create(
        config: *const NativeConfig,
        error: *mut c_char,
        error_size: usize,
    ) -> *mut NativeEngine;
    /// หยุด resource ที่ยังทำงานและทำลาย native engine
    fn airpods_audio_destroy(engine: *mut NativeEngine);
    /// เริ่ม PipeWire thread โดยคืนศูนย์เมื่อสำเร็จ
    fn airpods_audio_start(engine: *mut NativeEngine, error: *mut c_char, error_size: usize)
    -> i32;
    /// หยุด PipeWire thread โดยคืนศูนย์เมื่อสำเร็จ
    fn airpods_audio_stop(engine: *mut NativeEngine, error: *mut c_char, error_size: usize) -> i32;
    /// ส่ง AAC-ELD access unit ให้ decoder และคืนสถานะ queue/decode
    fn airpods_audio_push(
        engine: *mut NativeEngine,
        data: *const u8,
        size: usize,
        error: *mut c_char,
        error_size: usize,
    ) -> i32;
    /// เปลี่ยน gain และ limiter ที่ native engine ใช้กับ frame ถัดไป
    fn airpods_audio_set_processing(
        engine: *mut NativeEngine,
        gain_db: f32,
        limiter_dbfs: f32,
        error: *mut c_char,
        error_size: usize,
    ) -> i32;
    /// คืนค่าที่ไม่ใช่ศูนย์เมื่อ PipeWire thread ยังทำงาน
    fn airpods_audio_is_running(engine: *const NativeEngine) -> i32;
    /// copy metric snapshot ลง struct ที่ caller จัดเตรียม
    fn airpods_audio_get_metrics(engine: *const NativeEngine, metrics: *mut NativeMetrics);
}

/// ค่าที่ใช้สร้าง C++ audio engine และ PipeWire source
#[derive(Clone, Debug)]
pub struct AudioConfig {
    /// ชื่อคงที่ของ PipeWire node
    pub node_name: String,
    /// ชื่อที่แสดงต่อผู้ใช้ใน audio settings
    pub node_description: String,
    /// gain ก่อนเข้า limiter หน่วย dB
    pub gain_db: f32,
    /// เพดาน limiter หน่วย dBFS
    pub limiter_dbfs: f32,
    /// ความจุสูงสุดของ bounded SPSC physical queue หน่วย millisecond
    pub queue_capacity_ms: u32,
}

impl Default for AudioConfig {
    /// สร้าง config สำหรับ mono source พร้อม physical queue 250 ms
    fn default() -> Self {
        Self {
            node_name: SOURCE_NAME.to_string(),
            node_description: SOURCE_DESCRIPTION.to_string(),
            gain_db: DEFAULT_GAIN_DB,
            limiter_dbfs: DEFAULT_LIMITER_DBFS,
            queue_capacity_ms: 250,
        }
    }
}

/// ผลของการส่ง AAC access unit เข้า audio engine
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PushOutcome {
    /// decode สำเร็จและใส่ PCM ลง queue แล้ว
    Queued,
    /// decode สำเร็จแต่ physical queue เต็ม จึงทิ้งทั้ง frame
    QueueFull,
    /// FDK-AAC ปฏิเสธ access unit นี้และแทน frame ที่เสียด้วย silence
    DecodeError,
}

/// metric สะสมของ decoder, DSP และ PipeWire queue
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AudioMetrics {
    /// จำนวน access unit ที่ได้รับ
    pub access_units: u64,
    /// จำนวน frame ที่ decode สำเร็จ
    pub decoded_frames: u64,
    /// จำนวน PCM sample ที่ decode สำเร็จ
    pub decoded_samples: u64,
    /// จำนวน frame ที่ทิ้งเพราะ queue เต็ม
    pub queue_drops: u64,
    /// จำนวน access unit ที่ decode ไม่สำเร็จ
    pub decode_errors: u64,
    /// จำนวน PipeWire buffer callback ที่เติม silence อย่างน้อยหนึ่ง sample
    pub underflows: u64,
    /// จำนวน silence sample ที่ส่งระหว่างเริ่ม buffer ใหม่หรือเกิด underflow
    pub silence_samples: u64,
    /// จำนวน sample เก่าที่ทิ้งเพื่อจำกัด latency
    pub stale_samples_dropped: u64,
    /// จำนวน silence sample ที่แทน AAC frame ซึ่ง decode ไม่สำเร็จ
    pub concealed_samples: u64,
    /// ช่องว่างระหว่าง access unit ที่มากที่สุด หน่วย microsecond
    pub maximum_packet_gap_microseconds: u64,
    /// jitter target ปัจจุบัน หน่วย sample
    pub target_samples: u64,
    /// จำนวน sample ที่ PipeWire ขอล่าสุด
    pub requested_samples: u64,
    /// จำนวน sample มากที่สุดที่ PipeWire เคยขอใน callback เดียว
    pub maximum_requested_samples: u64,
    /// ค่าชดเชย clock drift ปัจจุบัน หน่วย parts per million
    pub rate_correction_ppm: i64,
    /// จำนวน PCM sample ที่รออยู่ใน queue ขณะอ่าน metric
    pub queued_samples: u64,
}

/// เจ้าของ C++ decoder, DSP, SPSC buffer และ PipeWire source
pub struct AudioEngine {
    native: NonNull<NativeEngine>,
}

// `AudioEngine` ย้าย thread ได้เพราะ C++ object ถือ resource ภายในและ Rust API ใช้ `&mut self`
// สำหรับ operation ที่มี producer เพียงรายเดียว ส่วน PipeWire callback ใช้ SPSC consumer แยกอยู่แล้ว
unsafe impl Send for AudioEngine {}

impl AudioEngine {
    /// สร้าง engine แต่ยังไม่เผยแพร่ PipeWire source
    pub fn new(config: AudioConfig) -> Result<Self> {
        validate_processing(config.gain_db, config.limiter_dbfs)?;
        if !(1..=5_000).contains(&config.queue_capacity_ms) {
            bail!("audio queue capacity must be between 1 and 5000 ms");
        }

        let node_name = CString::new(config.node_name)
            .map_err(|_| anyhow!("PipeWire node name contains a null byte"))?;
        let node_description = CString::new(config.node_description)
            .map_err(|_| anyhow!("PipeWire node description contains a null byte"))?;
        let native_config = NativeConfig {
            node_name: node_name.as_ptr(),
            node_description: node_description.as_ptr(),
            gain_db: config.gain_db,
            limiter_dbfs: config.limiter_dbfs,
            queue_capacity_ms: config.queue_capacity_ms,
        };
        let mut error = error_buffer();
        // C++ constructor copy string ทั้งสองค่าก่อน `CString` ออกจาก scope
        let native =
            unsafe { airpods_audio_create(&native_config, error.as_mut_ptr(), ERROR_BUFFER_SIZE) };
        let native = NonNull::new(native).ok_or_else(|| native_error(&error))?;
        Ok(Self { native })
    }

    /// เริ่ม PipeWire thread และเผยแพร่ `Audio/Source`
    pub fn start(&mut self) -> Result<()> {
        call_status(|error| unsafe {
            airpods_audio_start(self.native.as_ptr(), error, ERROR_BUFFER_SIZE)
        })
    }

    /// decode AAC-ELD access unit, ทำ DSP และส่ง PCM เข้า queue
    pub fn push_access_unit(&mut self, access_unit: &[u8]) -> Result<PushOutcome> {
        if access_unit.is_empty() {
            bail!("cannot process an empty AAC-ELD access unit");
        }
        let mut error = error_buffer();
        let status = unsafe {
            airpods_audio_push(
                self.native.as_ptr(),
                access_unit.as_ptr(),
                access_unit.len(),
                error.as_mut_ptr(),
                ERROR_BUFFER_SIZE,
            )
        };
        match status {
            0 => Ok(PushOutcome::Queued),
            1 => Ok(PushOutcome::QueueFull),
            2 => Ok(PushOutcome::DecodeError),
            _ => Err(native_error(&error)),
        }
    }

    /// เปลี่ยน gain และ limiter สำหรับ frame ถัดไป
    pub fn set_processing(&mut self, gain_db: f32, limiter_dbfs: f32) -> Result<()> {
        validate_processing(gain_db, limiter_dbfs)?;
        call_status(|error| unsafe {
            airpods_audio_set_processing(
                self.native.as_ptr(),
                gain_db,
                limiter_dbfs,
                error,
                ERROR_BUFFER_SIZE,
            )
        })
    }

    /// หยุด PipeWire thread และนำ source ออกจาก graph
    pub fn stop(&mut self) -> Result<()> {
        call_status(|error| unsafe {
            airpods_audio_stop(self.native.as_ptr(), error, ERROR_BUFFER_SIZE)
        })
    }

    /// ระบุว่า PipeWire thread ยังทำงานอยู่หรือไม่
    pub fn is_running(&self) -> bool {
        unsafe { airpods_audio_is_running(self.native.as_ptr()) != 0 }
    }

    /// อ่าน metric snapshot โดยไม่หยุด audio thread
    pub fn metrics(&self) -> AudioMetrics {
        let mut native = NativeMetrics::default();
        unsafe { airpods_audio_get_metrics(self.native.as_ptr(), &mut native) };
        AudioMetrics {
            access_units: native.access_units,
            decoded_frames: native.decoded_frames,
            decoded_samples: native.decoded_samples,
            queue_drops: native.queue_drops,
            decode_errors: native.decode_errors,
            underflows: native.underflows,
            silence_samples: native.silence_samples,
            stale_samples_dropped: native.stale_samples_dropped,
            concealed_samples: native.concealed_samples,
            maximum_packet_gap_microseconds: native.maximum_packet_gap_microseconds,
            target_samples: native.target_samples,
            requested_samples: native.requested_samples,
            maximum_requested_samples: native.maximum_requested_samples,
            rate_correction_ppm: native.rate_correction_ppm,
            queued_samples: native.queued_samples,
        }
    }
}

impl Drop for AudioEngine {
    /// ส่ง ownership คืน C++ เพื่อหยุด thread และปล่อย resource ทั้งหมด
    fn drop(&mut self) {
        unsafe { airpods_audio_destroy(self.native.as_ptr()) };
    }
}

/// ตรวจค่าที่ส่งให้ DSP ให้เป็น finite และอยู่ในช่วงเดียวกับ native engine
fn validate_processing(gain_db: f32, limiter_dbfs: f32) -> Result<()> {
    if !gain_db.is_finite() || !(0.0..=30.0).contains(&gain_db) {
        bail!("microphone gain must be between 0 and 30 dB");
    }
    if !limiter_dbfs.is_finite() || !(-12.0..=0.0).contains(&limiter_dbfs) {
        bail!("limiter must be between -12 and 0 dBFS");
    }
    Ok(())
}

/// แปลง status แบบ C และ error buffer เป็น `anyhow::Result`
fn call_status(call: impl FnOnce(*mut c_char) -> i32) -> Result<()> {
    let mut error = error_buffer();
    let status = call(error.as_mut_ptr());
    if status == 0 {
        Ok(())
    } else {
        Err(native_error(&error))
    }
}

/// สร้าง buffer ที่มี null terminator ตั้งแต่ต้นสำหรับกรณี native ไม่เขียนข้อความ
fn error_buffer() -> [c_char; ERROR_BUFFER_SIZE] {
    [0; ERROR_BUFFER_SIZE]
}

/// อ่าน C string จาก native layer และมี fallback เมื่อไม่มีข้อความ error
fn native_error(error: &[c_char; ERROR_BUFFER_SIZE]) -> anyhow::Error {
    let message = unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy();
    if message.is_empty() {
        anyhow!("native audio engine failed without an error message")
    } else {
        anyhow!(message.into_owned())
    }
}
