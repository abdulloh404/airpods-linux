#include "audio_engine.h"

#include <aacdecoder_lib.h>
#include <pipewire/pipewire.h>
#include <spa/node/io.h>
#include <spa/param/audio/format-utils.h>

#include <algorithm>
#include <array>
#include <atomic>
#include <chrono>
#include <cmath>
#include <cstdint>
#include <cstring>
#include <exception>
#include <future>
#include <limits>
#include <mutex>
#include <stdexcept>
#include <string>
#include <thread>
#include <tuple>
#include <utility>
#include <vector>

namespace {

constexpr uint32_t kSampleRate = 64'000;
constexpr uint32_t kChannels = 1;
constexpr size_t kAacFrameSamples = 480;
constexpr size_t kPcmBufferSamples = 8'192;
constexpr uint32_t kInitialJitterTargetMs = 60;
constexpr uint32_t kMinimumJitterTargetMs = 40;
constexpr uint32_t kMaximumJitterTargetMs = 80;
constexpr uint32_t kHardJitterLimitMs = 150;
constexpr uint64_t kJitterWindowMicroseconds = 2'000'000;
constexpr uint64_t kJitterTargetStepMicroseconds = 20'000;
constexpr double kRateControlPeriodSeconds = 1.0;
constexpr double kMaximumRateCorrection = 0.005;
constexpr char kNodeLatency[] = "640/64000";
constexpr float kMinGainDb = 0.0F;
constexpr float kMaxGainDb = 30.0F;
constexpr float kMinLimiterDbfs = -12.0F;
constexpr float kMaxLimiterDbfs = 0.0F;
constexpr float kLimiterReleaseSeconds = 0.1F;
constexpr std::array<UCHAR, 4> kEldAsc = {0xF8, 0xE6, 0x30, 0x00};
constexpr std::array<int16_t, kAacFrameSamples> kConcealmentSilence{};

constexpr size_t samples_for_milliseconds(uint32_t milliseconds) noexcept {
    return static_cast<size_t>(kSampleRate) * milliseconds / 1'000;
}

void update_maximum(std::atomic<uint64_t> &value, uint64_t candidate) noexcept {
    uint64_t current = value.load(std::memory_order_relaxed);
    while (current < candidate &&
           !value.compare_exchange_weak(current, candidate, std::memory_order_relaxed,
                                        std::memory_order_relaxed)) {
    }
}

void write_error(char *buffer, size_t size, const std::string &message) noexcept {
    if (buffer == nullptr || size == 0) {
        return;
    }
    const size_t length = std::min(size - 1, message.size());
    std::memcpy(buffer, message.data(), length);
    buffer[length] = '\0';
}

void validate_processing(float gain_db, float limiter_dbfs) {
    if (!std::isfinite(gain_db) || gain_db < kMinGainDb || gain_db > kMaxGainDb) {
        throw std::invalid_argument("microphone gain must be between 0 and 30 dB");
    }
    if (!std::isfinite(limiter_dbfs) || limiter_dbfs < kMinLimiterDbfs ||
        limiter_dbfs > kMaxLimiterDbfs) {
        throw std::invalid_argument("limiter must be between -12 and 0 dBFS");
    }
}

class SpscBuffer final {
  public:
    explicit SpscBuffer(size_t capacity) : samples_(capacity + 1) {
        if (capacity == 0) {
            throw std::invalid_argument("audio queue capacity must not be zero");
        }
    }

    bool push(const int16_t *samples, size_t count) noexcept {
        const size_t head = head_.load(std::memory_order_relaxed);
        const size_t tail = tail_.load(std::memory_order_acquire);
        if (count > free_space(head, tail)) {
            return false;
        }
        for (size_t index = 0; index < count; ++index) {
            samples_[(head + index) % samples_.size()] = samples[index];
        }
        head_.store((head + count) % samples_.size(), std::memory_order_release);
        return true;
    }

    size_t pop(int16_t *output, size_t count) noexcept {
        const size_t tail = tail_.load(std::memory_order_relaxed);
        const size_t head = head_.load(std::memory_order_acquire);
        const size_t available = size_from(head, tail);
        const size_t copied = std::min(count, available);
        for (size_t index = 0; index < copied; ++index) {
            output[index] = samples_[(tail + index) % samples_.size()];
        }
        tail_.store((tail + copied) % samples_.size(), std::memory_order_release);
        return copied;
    }

    size_t discard(size_t count) noexcept {
        const size_t tail = tail_.load(std::memory_order_relaxed);
        const size_t head = head_.load(std::memory_order_acquire);
        const size_t discarded = std::min(count, size_from(head, tail));
        tail_.store((tail + discarded) % samples_.size(), std::memory_order_release);
        return discarded;
    }

    size_t size() const noexcept {
        return size_from(head_.load(std::memory_order_acquire),
                         tail_.load(std::memory_order_acquire));
    }

    void clear() noexcept {
        tail_.store(0, std::memory_order_relaxed);
        head_.store(0, std::memory_order_relaxed);
    }

  private:
    size_t size_from(size_t head, size_t tail) const noexcept {
        return head >= tail ? head - tail : samples_.size() - (tail - head);
    }

    size_t free_space(size_t head, size_t tail) const noexcept {
        return samples_.size() - size_from(head, tail) - 1;
    }

    std::vector<int16_t> samples_;
    alignas(64) std::atomic<size_t> head_{0};
    alignas(64) std::atomic<size_t> tail_{0};
};

class Decoder final {
  public:
    Decoder() {
        handle_ = aacDecoder_Open(TT_MP4_RAW, 1);
        if (handle_ == nullptr) {
            throw std::runtime_error("FDK-AAC decoder allocation failed");
        }

        auto config = kEldAsc;
        UCHAR *config_pointer = config.data();
        const UINT config_length = static_cast<UINT>(config.size());
        const AAC_DECODER_ERROR status =
            aacDecoder_ConfigRaw(handle_, &config_pointer, &config_length);
        if (status != AAC_DEC_OK) {
            aacDecoder_Close(handle_);
            handle_ = nullptr;
            throw std::runtime_error("FDK-AAC AAC-ELD configuration failed");
        }
    }

    ~Decoder() {
        if (handle_ != nullptr) {
            aacDecoder_Close(handle_);
        }
    }

    Decoder(const Decoder &) = delete;
    Decoder &operator=(const Decoder &) = delete;

    std::pair<int16_t *, size_t> decode(const uint8_t *data, size_t size) {
        if (data == nullptr || size == 0 || size > std::numeric_limits<UINT>::max()) {
            throw std::invalid_argument("invalid AAC-ELD access unit");
        }

        UCHAR *input = const_cast<UCHAR *>(reinterpret_cast<const UCHAR *>(data));
        const UINT input_size = static_cast<UINT>(size);
        UINT bytes_valid = input_size;
        AAC_DECODER_ERROR status =
            aacDecoder_Fill(handle_, &input, &input_size, &bytes_valid);
        if (status != AAC_DEC_OK || bytes_valid != 0) {
            throw std::runtime_error("FDK-AAC rejected the AAC-ELD access unit");
        }

        status = aacDecoder_DecodeFrame(handle_, pcm_.data(),
                                        static_cast<INT>(pcm_.size()), 0);
        if (status != AAC_DEC_OK) {
            throw std::runtime_error("FDK-AAC failed to decode the AAC-ELD access unit");
        }

        const CStreamInfo *info = aacDecoder_GetStreamInfo(handle_);
        if (info == nullptr || info->frameSize <= 0 || info->numChannels != 1) {
            throw std::runtime_error("FDK-AAC returned an invalid mono PCM format");
        }
        const size_t sample_count = static_cast<size_t>(info->frameSize);
        if (sample_count > pcm_.size()) {
            throw std::runtime_error("decoded PCM frame exceeds the output buffer");
        }
        return {pcm_.data(), sample_count};
    }

  private:
    HANDLE_AACDECODER handle_{nullptr};
    std::array<int16_t, kPcmBufferSamples> pcm_{};
};

struct Metrics final {
    std::atomic<uint64_t> access_units{0};
    std::atomic<uint64_t> decoded_frames{0};
    std::atomic<uint64_t> decoded_samples{0};
    std::atomic<uint64_t> queue_drops{0};
    std::atomic<uint64_t> decode_errors{0};
    std::atomic<uint64_t> underflows{0};
    std::atomic<uint64_t> silence_samples{0};
    std::atomic<uint64_t> stale_samples_dropped{0};
    std::atomic<uint64_t> concealed_samples{0};
    std::atomic<uint64_t> maximum_packet_gap_microseconds{0};
    std::atomic<uint64_t> requested_samples{0};
    std::atomic<uint64_t> maximum_requested_samples{0};
    std::atomic<int64_t> rate_correction_ppm{0};
};

class Engine final {
  public:
    explicit Engine(const airpods_audio_config &config)
        : node_name_(required_string(config.node_name, "PipeWire node name")),
          node_description_(required_string(config.node_description,
                                            "PipeWire node description")),
          queue_capacity_samples_(queue_capacity(config.queue_capacity_ms)),
          hard_limit_samples_(std::min(queue_capacity_samples_,
                                       samples_for_milliseconds(kHardJitterLimitMs))),
          queue_(queue_capacity_samples_),
          target_samples_(std::min(hard_limit_samples_,
                                   samples_for_milliseconds(kInitialJitterTargetMs))),
          decoder_() {
        set_processing(config.gain_db, config.limiter_dbfs);
        static std::once_flag initialized;
        std::call_once(initialized, [] { pw_init(nullptr, nullptr); });
    }

    ~Engine() noexcept {
        try {
            stop();
        } catch (...) {
        }
    }

    Engine(const Engine &) = delete;
    Engine &operator=(const Engine &) = delete;

    void start() {
        if (running_.load(std::memory_order_acquire)) {
            return;
        }
        if (thread_.joinable()) {
            thread_.join();
        }
        queue_.clear();
        target_samples_.store(
            std::min(hard_limit_samples_,
                     samples_for_milliseconds(kInitialJitterTargetMs)),
            std::memory_order_release);
        rate_match_.store(nullptr, std::memory_order_release);
        position_.store(nullptr, std::memory_order_release);
        buffering_ = true;
        fill_average_seconds_ =
            static_cast<double>(target_samples_.load(std::memory_order_relaxed)) /
            static_cast<double>(kSampleRate);
        rate_correction_ = 1.0;
        position_remainder_ = 0;
        position_rate_numerator_ = 0;
        position_rate_denominator_ = 0;
        last_access_unit_ = {};
        jitter_window_started_ = {};
        jitter_window_peak_gap_microseconds_ = 0;
        metrics_.requested_samples.store(0, std::memory_order_relaxed);
        metrics_.rate_correction_ppm.store(0, std::memory_order_relaxed);

        std::promise<std::string> ready;
        auto future = ready.get_future();
        thread_ = std::thread([this, ready = std::move(ready)]() mutable {
            thread_main(std::move(ready));
        });
        const std::string startup_error = future.get();
        if (!startup_error.empty()) {
            thread_.join();
            throw std::runtime_error(startup_error);
        }
    }

    void stop() {
        running_.store(false, std::memory_order_release);
        {
            std::lock_guard<std::mutex> lock(lifecycle_mutex_);
            if (main_loop_ != nullptr) {
                pw_main_loop_quit(main_loop_);
            }
        }
        if (thread_.joinable()) {
            thread_.join();
        }
    }

    int push(const uint8_t *data, size_t size) {
        if (!running_.load(std::memory_order_acquire)) {
            throw std::runtime_error("PipeWire virtual microphone is not running");
        }
        metrics_.access_units.fetch_add(1, std::memory_order_relaxed);
        observe_access_unit_arrival();

        int16_t *samples = nullptr;
        size_t count = 0;
        try {
            std::tie(samples, count) = decoder_.decode(data, size);
        } catch (const std::exception &) {
            metrics_.decode_errors.fetch_add(1, std::memory_order_relaxed);
            if (queue_.push(kConcealmentSilence.data(),
                            kConcealmentSilence.size())) {
                metrics_.concealed_samples.fetch_add(kConcealmentSilence.size(),
                                                     std::memory_order_relaxed);
            } else {
                metrics_.queue_drops.fetch_add(1, std::memory_order_relaxed);
            }
            return 2;
        }

        process(samples, count);
        metrics_.decoded_frames.fetch_add(1, std::memory_order_relaxed);
        metrics_.decoded_samples.fetch_add(count, std::memory_order_relaxed);
        if (!queue_.push(samples, count)) {
            metrics_.queue_drops.fetch_add(1, std::memory_order_relaxed);
            return 1;
        }
        return 0;
    }

    void set_processing(float gain_db, float limiter_dbfs) {
        validate_processing(gain_db, limiter_dbfs);
        gain_linear_ = std::pow(10.0F, gain_db / 20.0F);
        limit_sample_ = static_cast<float>(std::numeric_limits<int16_t>::max()) *
                        std::pow(10.0F, limiter_dbfs / 20.0F);
        limiter_gain_ = 1.0F;
    }

    bool running() const noexcept { return running_.load(std::memory_order_acquire); }

    void metrics(airpods_audio_metrics &output) const noexcept {
        output.access_units = metrics_.access_units.load(std::memory_order_relaxed);
        output.decoded_frames = metrics_.decoded_frames.load(std::memory_order_relaxed);
        output.decoded_samples = metrics_.decoded_samples.load(std::memory_order_relaxed);
        output.queue_drops = metrics_.queue_drops.load(std::memory_order_relaxed);
        output.decode_errors = metrics_.decode_errors.load(std::memory_order_relaxed);
        output.underflows = metrics_.underflows.load(std::memory_order_relaxed);
        output.silence_samples =
            metrics_.silence_samples.load(std::memory_order_relaxed);
        output.stale_samples_dropped =
            metrics_.stale_samples_dropped.load(std::memory_order_relaxed);
        output.concealed_samples =
            metrics_.concealed_samples.load(std::memory_order_relaxed);
        output.maximum_packet_gap_microseconds =
            metrics_.maximum_packet_gap_microseconds.load(std::memory_order_relaxed);
        output.target_samples =
            static_cast<uint64_t>(target_samples_.load(std::memory_order_relaxed));
        output.requested_samples =
            metrics_.requested_samples.load(std::memory_order_relaxed);
        output.maximum_requested_samples =
            metrics_.maximum_requested_samples.load(std::memory_order_relaxed);
        output.rate_correction_ppm =
            metrics_.rate_correction_ppm.load(std::memory_order_relaxed);
        output.queued_samples = static_cast<uint64_t>(queue_.size());
    }

  private:
    static std::string required_string(const char *value, const char *field) {
        if (value == nullptr || value[0] == '\0') {
            throw std::invalid_argument(std::string(field) + " must not be empty");
        }
        return value;
    }

    static size_t queue_capacity(uint32_t milliseconds) {
        if (milliseconds == 0 || milliseconds > 5'000) {
            throw std::invalid_argument("audio queue capacity must be between 1 and 5000 ms");
        }
        return static_cast<size_t>(kSampleRate) * milliseconds / 1'000;
    }

    void observe_access_unit_arrival() noexcept {
        const auto now = std::chrono::steady_clock::now();
        if (jitter_window_started_ == std::chrono::steady_clock::time_point{}) {
            jitter_window_started_ = now;
        }
        if (last_access_unit_ != std::chrono::steady_clock::time_point{}) {
            const auto gap = std::chrono::duration_cast<std::chrono::microseconds>(
                now - last_access_unit_);
            const uint64_t gap_microseconds =
                gap.count() > 0 ? static_cast<uint64_t>(gap.count()) : 0;
            jitter_window_peak_gap_microseconds_ =
                std::max(jitter_window_peak_gap_microseconds_, gap_microseconds);
            update_maximum(metrics_.maximum_packet_gap_microseconds,
                           gap_microseconds);
        }
        last_access_unit_ = now;

        if (std::chrono::duration_cast<std::chrono::microseconds>(
                now - jitter_window_started_)
                .count() < static_cast<int64_t>(kJitterWindowMicroseconds)) {
            return;
        }

        uint64_t target_microseconds =
            jitter_window_peak_gap_microseconds_ +
            jitter_window_peak_gap_microseconds_ / 2;
        // ใช้ peak 1.5 เท่าแล้วปัดเป็นช่วง 20 ms เพื่อรับ packet burst โดยไม่ตรึง latency สูงตลอดเวลา
        target_microseconds =
            std::max(target_microseconds,
                     static_cast<uint64_t>(kMinimumJitterTargetMs) * 1'000);
        target_microseconds =
            ((target_microseconds + kJitterTargetStepMicroseconds - 1) /
             kJitterTargetStepMicroseconds) *
            kJitterTargetStepMicroseconds;
        target_microseconds =
            std::min(target_microseconds,
                     static_cast<uint64_t>(kMaximumJitterTargetMs) * 1'000);
        const size_t target = static_cast<size_t>(
            static_cast<uint64_t>(kSampleRate) * target_microseconds / 1'000'000);
        target_samples_.store(std::min(target, hard_limit_samples_),
                              std::memory_order_release);
        jitter_window_started_ = now;
        jitter_window_peak_gap_microseconds_ = 0;
    }

    void process(int16_t *samples, size_t count) noexcept {
        const float release =
            1.0F - std::exp(-1.0F / (static_cast<float>(kSampleRate) *
                                    kLimiterReleaseSeconds));
        for (size_t index = 0; index < count; ++index) {
            const float amplified = static_cast<float>(samples[index]) * gain_linear_;
            const float absolute = std::abs(amplified);
            const float required_gain =
                absolute > limit_sample_ ? limit_sample_ / absolute : 1.0F;
            if (required_gain < limiter_gain_) {
                limiter_gain_ = required_gain;
            } else {
                limiter_gain_ += (1.0F - limiter_gain_) * release;
            }
            samples[index] = static_cast<int16_t>(std::lround(amplified * limiter_gain_));
        }
    }

    static void on_process(void *data) noexcept {
        static_cast<Engine *>(data)->process_pipewire_buffer();
    }

    static void on_io_changed(void *data, uint32_t id, void *area,
                              uint32_t size) noexcept {
        auto *engine = static_cast<Engine *>(data);
        if (id == SPA_IO_RateMatch) {
            auto *rate_match =
                area != nullptr && size >= sizeof(spa_io_rate_match)
                    ? static_cast<spa_io_rate_match *>(area)
                    : nullptr;
            engine->rate_match_.store(rate_match, std::memory_order_release);
        } else if (id == SPA_IO_Position) {
            auto *position =
                area != nullptr && size >= sizeof(spa_io_position)
                    ? static_cast<spa_io_position *>(area)
                    : nullptr;
            engine->position_.store(position, std::memory_order_release);
        }
    }

    static void on_state_changed(void *data, enum pw_stream_state,
                                 enum pw_stream_state state,
                                 const char *) noexcept {
        if (state == PW_STREAM_STATE_ERROR) {
            auto *engine = static_cast<Engine *>(data);
            engine->running_.store(false, std::memory_order_release);
            std::lock_guard<std::mutex> lock(engine->lifecycle_mutex_);
            if (engine->main_loop_ != nullptr) {
                pw_main_loop_quit(engine->main_loop_);
            }
        }
    }

    size_t requested_sample_count(size_t capacity) noexcept {
        if (capacity == 0) {
            return 0;
        }

        spa_io_rate_match *rate_match =
            rate_match_.load(std::memory_order_acquire);
        if (rate_match != nullptr && rate_match->size > 0) {
            return std::min(capacity, static_cast<size_t>(rate_match->size));
        }

        spa_io_position *position = position_.load(std::memory_order_acquire);
        if (position != nullptr && position->clock.duration > 0 &&
            position->clock.rate.num > 0 && position->clock.rate.denom > 0) {
            const uint32_t numerator = position->clock.rate.num;
            const uint32_t denominator = position->clock.rate.denom;
            if (numerator != position_rate_numerator_ ||
                denominator != position_rate_denominator_) {
                position_remainder_ = 0;
                position_rate_numerator_ = numerator;
                position_rate_denominator_ = denominator;
            }
            const __uint128_t scaled =
                static_cast<__uint128_t>(position->clock.duration) * kSampleRate *
                    numerator +
                position_remainder_;
            const __uint128_t frames = scaled / denominator;
            position_remainder_ = static_cast<uint64_t>(scaled % denominator);
            if (frames > 0) {
                return frames > capacity ? capacity : static_cast<size_t>(frames);
            }
        }

        return std::min(capacity, samples_for_milliseconds(10));
    }

    void reset_rate_control(spa_io_rate_match *rate_match,
                            size_t target_samples) noexcept {
        fill_average_seconds_ = static_cast<double>(target_samples) /
                                static_cast<double>(kSampleRate);
        rate_correction_ = 1.0;
        metrics_.rate_correction_ppm.store(0, std::memory_order_relaxed);
        if (rate_match != nullptr) {
            rate_match->rate = 1.0;
            rate_match->flags &= ~SPA_IO_RATE_MATCH_FLAG_ACTIVE;
        }
    }

    void update_rate_control(spa_io_rate_match *rate_match, size_t queued_samples,
                             size_t target_samples,
                             size_t requested_samples) noexcept {
        if (rate_match == nullptr || requested_samples == 0) {
            metrics_.rate_correction_ppm.store(0, std::memory_order_relaxed);
            return;
        }

        const double cycle_seconds = static_cast<double>(requested_samples) /
                                     static_cast<double>(kSampleRate);
        const double target_seconds = static_cast<double>(target_samples) /
                                      static_cast<double>(kSampleRate);
        const double level_seconds = static_cast<double>(queued_samples) /
                                     static_cast<double>(kSampleRate);
        const double beta =
            std::clamp(cycle_seconds / kRateControlPeriodSeconds, 0.0, 1.0);
        const double previous_average = fill_average_seconds_;
        fill_average_seconds_ =
            (1.0 - beta) * previous_average + beta * level_seconds;
        rate_correction_ +=
            (fill_average_seconds_ - previous_average) /
                (3.0 * kRateControlPeriodSeconds) +
            beta * (previous_average - target_seconds) /
                (27.0 * kRateControlPeriodSeconds);
        rate_correction_ =
            std::clamp(rate_correction_, 1.0 - kMaximumRateCorrection,
                       1.0 + kMaximumRateCorrection);
        rate_match->rate = 1.0 / rate_correction_;
        rate_match->flags |= SPA_IO_RATE_MATCH_FLAG_ACTIVE;
        metrics_.rate_correction_ppm.store(
            static_cast<int64_t>(std::llround((rate_correction_ - 1.0) * 1'000'000.0)),
            std::memory_order_relaxed);
    }

    void process_pipewire_buffer() noexcept {
        pw_stream *stream = stream_.load(std::memory_order_acquire);
        if (stream == nullptr) {
            return;
        }
        pw_buffer *buffer = pw_stream_dequeue_buffer(stream);
        if (buffer == nullptr) {
            return;
        }
        if (buffer->buffer == nullptr || buffer->buffer->n_datas == 0) {
            pw_stream_queue_buffer(stream, buffer);
            return;
        }

        spa_data &data = buffer->buffer->datas[0];
        if (data.data == nullptr || data.chunk == nullptr) {
            pw_stream_queue_buffer(stream, buffer);
            return;
        }

        const size_t capacity = data.maxsize / sizeof(int16_t);
        const size_t requested_samples = requested_sample_count(capacity);
        metrics_.requested_samples.store(requested_samples,
                                         std::memory_order_relaxed);
        update_maximum(metrics_.maximum_requested_samples, requested_samples);

        auto *output = static_cast<int16_t *>(data.data);
        spa_io_rate_match *rate_match =
            rate_match_.load(std::memory_order_acquire);
        const size_t target_samples =
            std::min(target_samples_.load(std::memory_order_acquire),
                     hard_limit_samples_);
        if (rate_match != active_rate_match_) {
            active_rate_match_ = rate_match;
            reset_rate_control(rate_match, target_samples);
        }

        size_t queued_samples = queue_.size();
        if (queued_samples > hard_limit_samples_) {
            // ฝั่ง consumer ทิ้ง sample เก่าเพื่อกลับสู่ target โดยไม่แย่งสิทธิ์เขียนของ producer
            const size_t discarded =
                queue_.discard(queued_samples - target_samples);
            metrics_.stale_samples_dropped.fetch_add(discarded,
                                                     std::memory_order_relaxed);
            queued_samples -= discarded;
            reset_rate_control(rate_match, target_samples);
        }

        const size_t start_threshold =
            std::min(queue_capacity_samples_,
                     std::max(target_samples, requested_samples));
        if (buffering_ && queued_samples < start_threshold) {
            std::fill(output, output + requested_samples, 0);
            metrics_.silence_samples.fetch_add(requested_samples,
                                               std::memory_order_relaxed);
            reset_rate_control(rate_match, target_samples);
            data.chunk->offset = 0;
            data.chunk->stride = sizeof(int16_t);
            data.chunk->size =
                static_cast<uint32_t>(requested_samples * sizeof(int16_t));
            buffer->size = requested_samples;
            pw_stream_queue_buffer(stream, buffer);
            return;
        }
        if (buffering_) {
            buffering_ = false;
            reset_rate_control(rate_match, target_samples);
        }

        update_rate_control(rate_match, queued_samples, target_samples,
                            requested_samples);
        const size_t copied = queue_.pop(output, requested_samples);
        std::fill(output + copied, output + requested_samples, 0);
        if (copied < requested_samples) {
            metrics_.underflows.fetch_add(1, std::memory_order_relaxed);
            metrics_.silence_samples.fetch_add(requested_samples - copied,
                                               std::memory_order_relaxed);
            buffering_ = true;
            reset_rate_control(rate_match, target_samples);
        }

        data.chunk->offset = 0;
        data.chunk->stride = sizeof(int16_t);
        data.chunk->size =
            static_cast<uint32_t>(requested_samples * sizeof(int16_t));
        buffer->size = requested_samples;
        pw_stream_queue_buffer(stream, buffer);
    }

    void thread_main(std::promise<std::string> ready) noexcept {
        pw_main_loop *loop = nullptr;
        pw_stream *stream = nullptr;
        try {
            loop = pw_main_loop_new(nullptr);
            if (loop == nullptr) {
                throw std::runtime_error("failed to create the PipeWire main loop");
            }

            pw_properties *properties = pw_properties_new(
                PW_KEY_NODE_NAME, node_name_.c_str(), PW_KEY_NODE_NICK,
                node_description_.c_str(), PW_KEY_NODE_DESCRIPTION,
                node_description_.c_str(), PW_KEY_DEVICE_DESCRIPTION,
                node_description_.c_str(), PW_KEY_MEDIA_CLASS, "Audio/Source",
                PW_KEY_MEDIA_TYPE, "Audio", PW_KEY_NODE_VIRTUAL, "true",
                PW_KEY_NODE_AUTOCONNECT, "false", PW_KEY_NODE_ALWAYS_PROCESS, "true",
                PW_KEY_NODE_PAUSE_ON_IDLE, "false", PW_KEY_NODE_LATENCY,
                kNodeLatency, nullptr);
            if (properties == nullptr) {
                throw std::runtime_error("failed to create PipeWire properties");
            }

            static const pw_stream_events events = {
                .version = PW_VERSION_STREAM_EVENTS,
                .destroy = nullptr,
                .state_changed = on_state_changed,
                .control_info = nullptr,
                .io_changed = on_io_changed,
                .param_changed = nullptr,
                .add_buffer = nullptr,
                .remove_buffer = nullptr,
                .process = on_process,
                .drained = nullptr,
                .command = nullptr,
                .trigger_done = nullptr,
            };
            stream = pw_stream_new_simple(pw_main_loop_get_loop(loop), node_name_.c_str(),
                                          properties, &events, this);
            if (stream == nullptr) {
                throw std::runtime_error("failed to create the PipeWire source stream");
            }

            {
                std::lock_guard<std::mutex> lock(lifecycle_mutex_);
                main_loop_ = loop;
            }
            stream_.store(stream, std::memory_order_release);

            std::array<uint8_t, 1'024> pod_buffer{};
            spa_pod_builder builder{};
            spa_pod_builder_init(&builder, pod_buffer.data(), pod_buffer.size());
            spa_audio_info_raw format{};
            format.format = SPA_AUDIO_FORMAT_S16_LE;
            format.rate = kSampleRate;
            format.channels = kChannels;
            format.position[0] = SPA_AUDIO_CHANNEL_MONO;
            const spa_pod *params[] = {
                spa_format_audio_raw_build(&builder, SPA_PARAM_EnumFormat, &format)};
            const pw_stream_flags flags = static_cast<pw_stream_flags>(
                PW_STREAM_FLAG_MAP_BUFFERS | PW_STREAM_FLAG_RT_PROCESS);
            const int status = pw_stream_connect(stream, PW_DIRECTION_OUTPUT, PW_ID_ANY,
                                                 flags, params, 1);
            if (status < 0) {
                throw std::runtime_error("failed to connect the PipeWire source stream");
            }

            running_.store(true, std::memory_order_release);
            ready.set_value({});
            pw_main_loop_run(loop);
        } catch (const std::exception &error) {
            running_.store(false, std::memory_order_release);
            try {
                ready.set_value(error.what());
            } catch (...) {
            }
        } catch (...) {
            running_.store(false, std::memory_order_release);
            try {
                ready.set_value("unknown PipeWire thread error");
            } catch (...) {
            }
        }

        if (stream != nullptr) {
            pw_stream_destroy(stream);
        }
        stream_.store(nullptr, std::memory_order_release);
        rate_match_.store(nullptr, std::memory_order_release);
        position_.store(nullptr, std::memory_order_release);
        active_rate_match_ = nullptr;
        {
            std::lock_guard<std::mutex> lock(lifecycle_mutex_);
            main_loop_ = nullptr;
        }
        if (loop != nullptr) {
            pw_main_loop_destroy(loop);
        }
        running_.store(false, std::memory_order_release);
    }

    std::string node_name_;
    std::string node_description_;
    const size_t queue_capacity_samples_;
    const size_t hard_limit_samples_;
    SpscBuffer queue_;
    std::atomic<size_t> target_samples_;
    Decoder decoder_;
    Metrics metrics_;
    std::atomic<spa_io_rate_match *> rate_match_{nullptr};
    std::atomic<spa_io_position *> position_{nullptr};
    spa_io_rate_match *active_rate_match_{nullptr};
    bool buffering_{true};
    double fill_average_seconds_{0.0};
    double rate_correction_{1.0};
    uint64_t position_remainder_{0};
    uint32_t position_rate_numerator_{0};
    uint32_t position_rate_denominator_{0};
    std::chrono::steady_clock::time_point last_access_unit_{};
    std::chrono::steady_clock::time_point jitter_window_started_{};
    uint64_t jitter_window_peak_gap_microseconds_{0};
    float gain_linear_{1.0F};
    float limit_sample_{static_cast<float>(std::numeric_limits<int16_t>::max())};
    float limiter_gain_{1.0F};
    std::atomic<bool> running_{false};
    std::mutex lifecycle_mutex_;
    pw_main_loop *main_loop_{nullptr};
    std::atomic<pw_stream *> stream_{nullptr};
    std::thread thread_;
};

}

struct airpods_audio_engine {
    explicit airpods_audio_engine(const airpods_audio_config &config) : value(config) {}
    Engine value;
};

extern "C" airpods_audio_engine *airpods_audio_create(
    const airpods_audio_config *config, char *error, size_t error_size) {
    try {
        if (config == nullptr) {
            throw std::invalid_argument("audio config must not be null");
        }
        return new airpods_audio_engine(*config);
    } catch (const std::exception &exception) {
        write_error(error, error_size, exception.what());
        return nullptr;
    } catch (...) {
        write_error(error, error_size, "unknown audio engine initialization error");
        return nullptr;
    }
}

extern "C" void airpods_audio_destroy(airpods_audio_engine *engine) {
    try {
        delete engine;
    } catch (...) {
    }
}

extern "C" int airpods_audio_start(airpods_audio_engine *engine, char *error,
                                    size_t error_size) {
    try {
        if (engine == nullptr) {
            throw std::invalid_argument("audio engine must not be null");
        }
        engine->value.start();
        return 0;
    } catch (const std::exception &exception) {
        write_error(error, error_size, exception.what());
        return -1;
    } catch (...) {
        write_error(error, error_size, "unknown PipeWire startup error");
        return -1;
    }
}

extern "C" int airpods_audio_stop(airpods_audio_engine *engine, char *error,
                                   size_t error_size) {
    try {
        if (engine == nullptr) {
            throw std::invalid_argument("audio engine must not be null");
        }
        engine->value.stop();
        return 0;
    } catch (const std::exception &exception) {
        write_error(error, error_size, exception.what());
        return -1;
    } catch (...) {
        write_error(error, error_size, "unknown PipeWire shutdown error");
        return -1;
    }
}

extern "C" int airpods_audio_push(airpods_audio_engine *engine, const uint8_t *data,
                                   size_t size, char *error, size_t error_size) {
    try {
        if (engine == nullptr) {
            throw std::invalid_argument("audio engine must not be null");
        }
        return engine->value.push(data, size);
    } catch (const std::exception &exception) {
        write_error(error, error_size, exception.what());
        return -1;
    } catch (...) {
        write_error(error, error_size, "unknown AAC-ELD processing error");
        return -1;
    }
}

extern "C" int airpods_audio_set_processing(airpods_audio_engine *engine,
                                              float gain_db, float limiter_dbfs,
                                              char *error, size_t error_size) {
    try {
        if (engine == nullptr) {
            throw std::invalid_argument("audio engine must not be null");
        }
        engine->value.set_processing(gain_db, limiter_dbfs);
        return 0;
    } catch (const std::exception &exception) {
        write_error(error, error_size, exception.what());
        return -1;
    } catch (...) {
        write_error(error, error_size, "unknown DSP configuration error");
        return -1;
    }
}

extern "C" int airpods_audio_is_running(const airpods_audio_engine *engine) {
    try {
        return engine != nullptr && engine->value.running() ? 1 : 0;
    } catch (...) {
        return 0;
    }
}

extern "C" void airpods_audio_get_metrics(const airpods_audio_engine *engine,
                                            airpods_audio_metrics *metrics) {
    try {
        if (metrics == nullptr) {
            return;
        }
        *metrics = {};
        if (engine != nullptr) {
            engine->value.metrics(*metrics);
        }
    } catch (...) {
        if (metrics != nullptr) {
            *metrics = {};
        }
    }
}
