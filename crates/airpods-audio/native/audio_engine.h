#ifndef AIRPODS_AUDIO_ENGINE_H
#define AIRPODS_AUDIO_ENGINE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct airpods_audio_engine airpods_audio_engine;

typedef struct airpods_audio_config {
    const char *node_name;
    const char *node_description;
    float gain_db;
    float limiter_dbfs;
    uint32_t queue_capacity_ms;
} airpods_audio_config;

typedef struct airpods_audio_metrics {
    uint64_t access_units;
    uint64_t decoded_frames;
    uint64_t decoded_samples;
    uint64_t queue_drops;
    uint64_t decode_errors;
    uint64_t underflows;
    uint64_t silence_samples;
    uint64_t stale_samples_dropped;
    uint64_t concealed_samples;
    uint64_t maximum_packet_gap_microseconds;
    uint64_t target_samples;
    uint64_t requested_samples;
    uint64_t maximum_requested_samples;
    int64_t rate_correction_ppm;
    uint64_t queued_samples;
} airpods_audio_metrics;

airpods_audio_engine *airpods_audio_create(
    const airpods_audio_config *config,
    char *error,
    size_t error_size);
void airpods_audio_destroy(airpods_audio_engine *engine);
int airpods_audio_start(airpods_audio_engine *engine, char *error, size_t error_size);
int airpods_audio_stop(airpods_audio_engine *engine, char *error, size_t error_size);
int airpods_audio_push(
    airpods_audio_engine *engine,
    const uint8_t *data,
    size_t size,
    char *error,
    size_t error_size);
int airpods_audio_set_processing(
    airpods_audio_engine *engine,
    float gain_db,
    float limiter_dbfs,
    char *error,
    size_t error_size);
int airpods_audio_is_running(const airpods_audio_engine *engine);
void airpods_audio_get_metrics(
    const airpods_audio_engine *engine,
    airpods_audio_metrics *metrics);

#ifdef __cplusplus
}
#endif

#endif
