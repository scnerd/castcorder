// whisper.cpp bench, same protocol as common/bench.rs (whisper-rs wraps this exact C API).
// Usage: wbench <model.bin> <clip.wav> <label> [--gpu N] [--threads N] [--audio-ctx]
// Prints one line: RESULT {json}
#include "whisper.h"
#define NOMINMAX
#include <windows.h>
#include <psapi.h>
#include <algorithm>
#include <chrono>
#include <cstdio>
#include <cstring>
#include <fstream>
#include <string>
#include <vector>

static std::vector<float> load_wav(const char *path) {
    std::ifstream f(path, std::ios::binary);
    std::vector<char> b((std::istreambuf_iterator<char>(f)), {});
    size_t i = 12;
    while (i + 8 <= b.size()) {
        uint32_t sz;
        memcpy(&sz, &b[i + 4], 4);
        if (memcmp(&b[i], "data", 4) == 0) {
            std::vector<float> out(sz / 2);
            for (size_t k = 0; k < out.size(); k++) {
                int16_t s;
                memcpy(&s, &b[i + 8 + 2 * k], 2);
                out[k] = s / 32768.0f;
            }
            return out;
        }
        i += 8 + sz;
    }
    fprintf(stderr, "no data chunk\n");
    exit(1);
}

static std::string esc(const std::string &s) {
    std::string o;
    for (unsigned char c : s) {
        if (c < 32 || c > 126) continue;
        if (c == '"' || c == '\\') o += '\\';
        o += (char)c;
    }
    return o;
}

static double now() {
    return std::chrono::duration<double>(std::chrono::steady_clock::now().time_since_epoch()).count();
}

struct Eng {
    whisper_context *ctx;
    int threads;
    bool scale_ctx;
    std::string run(const float *pcm, size_t n, float chunk_secs) {
        whisper_full_params p = whisper_full_default_params(WHISPER_SAMPLING_GREEDY);
        p.n_threads = threads;
        p.language = "en";
        p.translate = false;
        p.no_context = true;
        p.temperature = 0.0f;
        p.temperature_inc = 0.0f;
        p.print_progress = p.print_realtime = p.print_special = p.print_timestamps = false;
        if (scale_ctx && chunk_secs > 0) p.audio_ctx = std::min(1500, (int)(chunk_secs * 50) + 64);
        if (whisper_full(ctx, p, pcm, (int)n) != 0) { fprintf(stderr, "whisper_full failed\n"); exit(1); }
        std::string s;
        for (int i = 0; i < whisper_full_n_segments(ctx); i++) s += whisper_full_get_segment_text(ctx, i);
        return s;
    }
};

int main(int argc, char **argv) {
    const char *model = argv[1], *wav = argv[2], *label = argv[3];
    int gpu = -1, threads = 6;
    bool scale_ctx = false;
    for (int i = 4; i < argc; i++) {
        if (!strcmp(argv[i], "--gpu")) gpu = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--threads")) threads = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--audio-ctx")) scale_ctx = true;
    }
    auto pcm = load_wav(wav);
    double dur = pcm.size() / 16000.0;
    double t = now();
    whisper_context_params cp = whisper_context_default_params();
    cp.use_gpu = gpu >= 0;
    cp.gpu_device = gpu >= 0 ? gpu : 0;
    whisper_context *ctx = whisper_init_from_file_with_params(model, cp);
    if (!ctx) { fprintf(stderr, "load failed\n"); return 1; }
    double load_s = now() - t;
    Eng e{ctx, threads, scale_ctx};
    t = now();
    e.run(pcm.data(), 16000 * 5, 5.0f);
    double first_s = now() - t;
    t = now();
    std::string full = e.run(pcm.data(), pcm.size(), 0);
    double full_s = now() - t;
    printf("RESULT {\"label\":\"%s\",\"audio_s\":%.2f,\"load_s\":%.3f,\"first_s\":%.3f,\"full_s\":%.3f,\"rtf\":%.4f,\"full_text\":\"%s\"",
           esc(label).c_str(), dur, load_s, first_s, full_s, full_s / dur, esc(full).c_str());
    for (int secs : {5, 10}) {
        size_t n = 16000 * (size_t)secs;
        std::vector<double> lat;
        std::string text;
        for (size_t off = 0; off < pcm.size(); off += n) {
            size_t len = std::min(n, pcm.size() - off);
            if (len < 16000) break;
            t = now();
            std::string s = e.run(pcm.data() + off, len, len / 16000.0f);
            lat.push_back(now() - t);
            text += s + " ";
        }
        double mx = *std::max_element(lat.begin(), lat.end());
        std::sort(lat.begin(), lat.end());
        printf(",\"c%d_med_s\":%.3f,\"c%d_max_s\":%.3f,\"c%d_text\":\"%s\"", secs, lat[lat.size() / 2], secs, mx, secs, esc(text).c_str());
    }
    PROCESS_MEMORY_COUNTERS pmc{};
    pmc.cb = sizeof(pmc);
    GetProcessMemoryInfo(GetCurrentProcess(), &pmc, sizeof(pmc));
    printf(",\"peak_rss_mb\":%.0f}\n", pmc.PeakWorkingSetSize / 1048576.0);
    whisper_free(ctx);
    return 0;
}
