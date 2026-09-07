#include "audio_engine.h"

#include <aacdecoder_lib.h>
#include <pipewire/pipewire.h>
#include <spa/param/audio/format-utils.h>

#include <algorithm>
#include <array>
#include <atomic>
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
constexpr size_t kPcmBufferSamples = 8'192;
constexpr float kMinGainDb = 0.0F;
constexpr float kMaxGainDb = 30.0F;
constexpr float kMinLimiterDbfs = -12.0F;
constexpr float kMaxLimiterDbfs = 0.0F;
constexpr float kLimiterReleaseSeconds = 0.1F;
constexpr std::array<UCHAR, 4> kEldAsc = {0xF8, 0xE6, 0x30, 0x00};

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
};

class Engine final {
  public:
    explicit Engine(const airpods_audio_config &config)
        : node_name_(required_string(config.node_name, "PipeWire node name")),
          node_description_(required_string(config.node_description,
                                            "PipeWire node description")),
          queue_(queue_capacity(config.queue_capacity_ms)), decoder_() {
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

        int16_t *samples = nullptr;
        size_t count = 0;
        try {
            std::tie(samples, count) = decoder_.decode(data, size);
        } catch (const std::exception &) {
            metrics_.decode_errors.fetch_add(1, std::memory_order_relaxed);
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

    void process_pipewire_buffer() noexcept {
        pw_buffer *buffer = pw_stream_dequeue_buffer(stream_);
        if (buffer == nullptr || buffer->buffer == nullptr ||
            buffer->buffer->n_datas == 0) {
            return;
        }

        spa_data &data = buffer->buffer->datas[0];
        if (data.data == nullptr || data.chunk == nullptr) {
            pw_stream_queue_buffer(stream_, buffer);
            return;
        }

        uint32_t bytes = data.maxsize - (data.maxsize % sizeof(int16_t));
        if (buffer->requested > 0) {
            const uint64_t requested_bytes =
                static_cast<uint64_t>(buffer->requested) * sizeof(int16_t);
            bytes = static_cast<uint32_t>(
                std::min<uint64_t>(bytes, requested_bytes));
        }
        auto *output = static_cast<int16_t *>(data.data);
        const size_t requested_samples = bytes / sizeof(int16_t);
        const size_t copied = queue_.pop(output, requested_samples);
        std::fill(output + copied, output + requested_samples, 0);
        if (copied < requested_samples) {
            metrics_.underflows.fetch_add(1, std::memory_order_relaxed);
        }

        data.chunk->offset = 0;
        data.chunk->stride = sizeof(int16_t);
        data.chunk->size = bytes;
        pw_stream_queue_buffer(stream_, buffer);
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
                PW_KEY_NODE_PAUSE_ON_IDLE, "false", nullptr);
            if (properties == nullptr) {
                throw std::runtime_error("failed to create PipeWire properties");
            }

            static const pw_stream_events events = {
                .version = PW_VERSION_STREAM_EVENTS,
                .destroy = nullptr,
                .state_changed = on_state_changed,
                .control_info = nullptr,
                .io_changed = nullptr,
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

            std::array<uint8_t, 1'024> pod_buffer{};
            spa_pod_builder builder =
                SPA_POD_BUILDER_INIT(pod_buffer.data(), pod_buffer.size());
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

            {
                std::lock_guard<std::mutex> lock(lifecycle_mutex_);
                main_loop_ = loop;
                stream_ = stream;
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

        {
            std::lock_guard<std::mutex> lock(lifecycle_mutex_);
            stream_ = nullptr;
            main_loop_ = nullptr;
        }
        if (stream != nullptr) {
            pw_stream_destroy(stream);
        }
        if (loop != nullptr) {
            pw_main_loop_destroy(loop);
        }
        running_.store(false, std::memory_order_release);
    }

    std::string node_name_;
    std::string node_description_;
    SpscBuffer queue_;
    Decoder decoder_;
    Metrics metrics_;
    float gain_linear_{1.0F};
    float limit_sample_{static_cast<float>(std::numeric_limits<int16_t>::max())};
    float limiter_gain_{1.0F};
    std::atomic<bool> running_{false};
    std::mutex lifecycle_mutex_;
    pw_main_loop *main_loop_{nullptr};
    pw_stream *stream_{nullptr};
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
