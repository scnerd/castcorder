# /// script
# dependencies = ["sherpa-onnx", "soundfile", "numpy", "psutil"]
# ///
# Same protocol as common/bench.rs, for ONNX models via sherpa-onnx (onnxruntime, CPU).
# Usage: uv run onnx_bench.py <parakeet|moonshine> <model dir> <clip.wav> <label> [threads]
import json, sys, time
import numpy as np, psutil, sherpa_onnx, soundfile as sf

kind, d, wav, label = sys.argv[1:5]
threads = int(sys.argv[5]) if len(sys.argv) > 5 else 6
pcm, sr = sf.read(wav, dtype="float32")
t = time.perf_counter()
if kind == "parakeet":
    rec = sherpa_onnx.OfflineRecognizer.from_transducer(
        encoder=f"{d}/encoder.int8.onnx", decoder=f"{d}/decoder.int8.onnx", joiner=f"{d}/joiner.int8.onnx",
        tokens=f"{d}/tokens.txt", num_threads=threads, model_type="nemo_transducer", decoding_method="greedy_search")
else:
    rec = sherpa_onnx.OfflineRecognizer.from_moonshine(
        preprocessor=f"{d}/preprocess.onnx", encoder=f"{d}/encode.int8.onnx",
        uncached_decoder=f"{d}/uncached_decode.int8.onnx", cached_decoder=f"{d}/cached_decode.int8.onnx",
        tokens=f"{d}/tokens.txt", num_threads=threads)
load_s = time.perf_counter() - t


def run(x):
    s = rec.create_stream()
    s.accept_waveform(16000, x)
    rec.decode_stream(s)
    return s.result.text


t = time.perf_counter(); run(pcm[: 16000 * 5]); first_s = time.perf_counter() - t
t = time.perf_counter(); full = run(pcm); full_s = time.perf_counter() - t
r = dict(label=label, audio_s=len(pcm) / 16000, load_s=load_s, first_s=first_s, full_s=full_s,
         rtf=full_s / (len(pcm) / 16000), full_text=full)
for secs in (5, 10):
    n, lat, text = 16000 * secs, [], []
    for off in range(0, len(pcm), n):
        c = pcm[off: off + n]
        if len(c) < 16000:
            break
        t = time.perf_counter(); text.append(run(c)); lat.append(time.perf_counter() - t)
    r[f"c{secs}_med_s"] = sorted(lat)[len(lat) // 2]
    r[f"c{secs}_max_s"] = max(lat)
    r[f"c{secs}_text"] = " ".join(text)
r["peak_rss_mb"] = psutil.Process().memory_info().peak_wset / 1048576
r["vram_mb"] = 0
print("RESULT " + json.dumps(r, ensure_ascii=True))
