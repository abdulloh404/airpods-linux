#ifndef AIRPODS_AUDIO_ENGINE_H
#define AIRPODS_AUDIO_ENGINE_H

// ประกาศ C ABI ระหว่าง Rust กับ C++ audio engine โดยซ่อน C++ implementation ไว้หลัง opaque pointer

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

// handle ของ native engine ที่ caller ถือ ownership ผ่าน create/destroy เท่านั้น
typedef struct airpods_audio_engine airpods_audio_engine;

// ค่าเริ่มต้นสำหรับสร้าง decoder, DSP, physical queue และ PipeWire source
typedef struct airpods_audio_config {
    // ชื่อภายในของ PipeWire node แบบ null-terminated
    const char *node_name;
    // ชื่อ PipeWire source ที่แสดงต่อผู้ใช้แบบ null-terminated
    const char *node_description;
    // gain ก่อน limiter หน่วย dB
    float gain_db;
    // เพดาน limiter หน่วย dBFS
    float limiter_dbfs;
    // ความจุสูงสุดของ physical queue หน่วย millisecond
    uint32_t queue_capacity_ms;
} airpods_audio_config;

// snapshot ของ counter และสถานะ realtime pipeline ณ เวลาที่อ่าน
typedef struct airpods_audio_metrics {
    // จำนวน AAC access unit ที่ได้รับ
    uint64_t access_units;
    // จำนวน frame ที่ decode สำเร็จ
    uint64_t decoded_frames;
    // จำนวน PCM sample ที่ decode สำเร็จ
    uint64_t decoded_samples;
    // จำนวน frame ที่ทิ้งเพราะ physical queue เต็ม
    uint64_t queue_drops;
    // จำนวน access unit ที่ decoder ปฏิเสธ
    uint64_t decode_errors;
    // จำนวน PipeWire callback ที่มี sample ไม่พอ
    uint64_t underflows;
    // จำนวน silence sample ที่ส่งระหว่าง buffering หรือ underflow
    uint64_t silence_samples;
    // จำนวน sample เก่าที่ทิ้งเพื่อจำกัด latency
    uint64_t stale_samples_dropped;
    // จำนวน silence sample ที่ใช้แทน AAC frame เสีย
    uint64_t concealed_samples;
    // ช่องว่างสูงสุดระหว่าง access unit หน่วย microsecond
    uint64_t maximum_packet_gap_microseconds;
    // jitter target ล่าสุดหน่วย sample
    uint64_t target_samples;
    // จำนวน sample ที่ PipeWire ขอล่าสุด
    uint64_t requested_samples;
    // จำนวน sample สูงสุดที่ PipeWire เคยขอใน callback เดียว
    uint64_t maximum_requested_samples;
    // ค่าชดเชย clock drift ล่าสุดหน่วย parts per million
    int64_t rate_correction_ppm;
    // จำนวน PCM sample ที่รออยู่ใน physical queue
    uint64_t queued_samples;
} airpods_audio_metrics;

// สร้าง engine จาก config; คืน null และเขียน error เมื่อ validation หรือ allocation ล้มเหลว
airpods_audio_engine *airpods_audio_create(
    const airpods_audio_config *config,
    char *error,
    size_t error_size);
// หยุด resource ที่ยังทำงานและทำลาย engine; รับ null ได้
void airpods_audio_destroy(airpods_audio_engine *engine);
// เริ่ม PipeWire thread และรอจน source พร้อมหรือ startup ล้มเหลว
int airpods_audio_start(airpods_audio_engine *engine, char *error, size_t error_size);
// ขอให้ PipeWire main loop หยุดและรอ thread จบ
int airpods_audio_stop(airpods_audio_engine *engine, char *error, size_t error_size);
// decode และประมวลผล AAC-ELD access unit หนึ่งก้อนก่อนใส่ physical queue
int airpods_audio_push(
    airpods_audio_engine *engine,
    const uint8_t *data,
    size_t size,
    char *error,
    size_t error_size);
// เปลี่ยน gain และ limiter ที่ใช้กับ frame ถัดไป
int airpods_audio_set_processing(
    airpods_audio_engine *engine,
    float gain_db,
    float limiter_dbfs,
    char *error,
    size_t error_size);
// คืน 1 เมื่อ PipeWire thread ทำงาน และคืน 0 เมื่อหยุดหรือ handle ไม่ถูกต้อง
int airpods_audio_is_running(const airpods_audio_engine *engine);
// copy metric snapshot; คืนค่าเป็นศูนย์ทั้งหมดเมื่อไม่มี engine หรือเกิด exception
void airpods_audio_get_metrics(
    const airpods_audio_engine *engine,
    airpods_audio_metrics *metrics);

#ifdef __cplusplus
}
#endif

#endif
