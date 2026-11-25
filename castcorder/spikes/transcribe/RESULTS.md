# Transcription spike (cast-7hm): results

Goal: pick a local speech-to-text engine for a live, per-participant transcript on the host.
Constraints from the owner: local only, everything bundled in the installer (no runtime downloads),
prefer pure Rust, use the GPU when there is one and fall back to CPU, speed over quality.

## TL;DR

- **whisper.cpp with the Vulkan backend** (Rust route: `whisper-rs` with the `vulkan` feature) is the
  best fit for the constraints today. Vulkan was as fast as CUDA on an RTX 3090 and adds about 40 MB
  to the exe with no extra DLLs; `vulkan-1.dll` ships with every GPU driver. CUDA would add about
  530 MB of cuBLAS DLLs for no speed gain.
- Suggested default model: **small.en-q5_1** (181 MB) on a GPU, and **base.en-q8_0** (78 MB) on
  CPU only. Both run a 5 s chunk well inside real time: 0.11 s on the GPU and 0.4 s on the CPU.
- **Parakeet TDT 0.6B v2** (int8 ONNX, through onnxruntime) is the most accurate engine measured,
  and it was fast **on CPU alone**: 0.43 s per 5 s chunk with 4.3% chunked WER, against 9.6% for
  Whisper small.en. It is not pure Rust (the C++ onnxruntime, ~23 MB of DLLs), it needs ~640 MB of
  model files and about 2 GB of RAM, it is English only, and it is licensed CC-BY-4.0. It is the
  strongest "quality" alternative.
- **Fixed 5 s chunks cost about 4-5 WER points** against whole-clip decoding, because words are cut at
  chunk edges. The live design should cut segments at pauses (VAD) or use overlap, not fixed windows.
- **The pure-Rust engines (candle, burn) were not measured.** The machine's crate proxy was down; see
  "Skipped" below.

## Machine

| item | value |
|---|---|
| CPU | Intel Core i5-12600K, 10 cores (6P+4E), 16 threads |
| RAM | 32 GB |
| GPU | NVIDIA GeForce RTX 3090, 24 GB VRAM (driver 617.42); Intel UHD 770 iGPU |
| CUDA toolkit | installed: v12.6-v13.5 (`CUDA_PATH` = 13.5; nvcc on PATH = 13.2). Built with 13.5. |
| Vulkan SDK | 1.4.363.0 |
| Rust | rustc / cargo 1.100.0 |
| Other | CMake 4.4, MSVC 19.44 (VS 2022 Build Tools), LLVM/clang, uv |

## Method

- **Test clip:** the first 12 utterances of LibriSpeech validation-clean (from
  `hf-internal-testing/librispeech_asr_dummy`), joined with 0.4 s gaps. That gives 135.3 s of
  16 kHz mono audio. The reference transcript is `data/clip.txt`; to rebuild the clip, run
  `scripts/mkclip.py`.
- **Protocol** (`wcpp/bench.cpp`, mirrored by `common/bench.rs` and `onnx_bench.py`), one process
  per run:
  1. Load the model (load s).
  2. Warm up on the first 5 s (first s).
  3. Transcribe the whole clip (RTF = time / 135.3 s).
  4. Transcribe the clip as consecutive 5 s chunks, then as 10 s chunks. Report the median and max
     warm latency per chunk, and the WER of the concatenated chunk outputs.
- **Decoding:** greedy, temperature 0, language en, no context carried between calls, 6 CPU threads.
- **WER:** jiwer, with Whisper's EnglishTextNormalizer applied to both sides (`score.py`).
- **Memory:** peak RAM is the process peak working set. Peak VRAM is the peak of total GPU
  `memory.used`, sampled every 100 ms with nvidia-smi, minus the baseline before the run. That
  includes the CUDA/Vulkan context, and iGPU memory is not visible to it.
- **whisper-rs:** the harness calls the whisper.cpp C API directly (whisper.cpp master adf50b6,
  2026-11-25, statically linked). whisper-rs is a thin binding over this same API, so the numbers
  carry over; the Rust harness (`wrs/`) is written but was not built (see "Skipped").
- **audio_ctx rows:** the encoder context is scaled to the chunk length (50 frames/s + 64), which
  is whisper.cpp's trick for short inputs. Their "full" column is an ordinary whole-clip run.

## Results

All times are in seconds. "5 s chunk med/max" is the latency to transcribe one 5 s chunk with a warm
model. Lower is better everywhere.

| run | load s | RTF full | WER full | 5 s chunk med/max s | WER 5 s | 10 s chunk med/max s | WER 10 s | peak RAM MB | peak VRAM MB |
|---|---|---|---|---|---|---|---|---|---|
| wcpp cpu tiny.en | 0.10 | 0.017 | 6.6 | 0.22/0.24 | 11.9 | 0.27/0.45 | 9.2 | 266 | 0 |
| wcpp cpu tiny.en-q5_1 | 0.14 | 0.016 | 9.9 | 0.22/0.25 | 10.2 | 0.27/0.49 | 10.9 | 223 | 0 |
| wcpp cpu tiny.en-q8_0 | 0.20 | 0.014 | 6.6 | 0.19/0.20 | 11.6 | 0.22/0.41 | 9.9 | 234 | 0 |
| wcpp cpu base.en | 0.48 | 0.035 | 7.9 | 0.44/0.82 | 12.2 | 0.53/0.96 | 8.3 | 356 | 0 |
| wcpp cpu base.en-q5_1 | 0.21 | 0.032 | 8.6 | 0.49/0.52 | 14.5 | 0.53/1.04 | 8.6 | 272 | 0 |
| wcpp cpu base.en-q8_0 | 0.32 | 0.029 | 7.9 | 0.39/0.78 | 11.9 | 0.46/0.81 | 8.3 | 293 | 0 |
| wcpp cpu small.en | 1.50 | 0.117 | 5.0 | 1.66/2.41 | 9.6 | 2.19/2.46 | 6.9 | 752 | 0 |
| wcpp cpu small.en-q5_1 | 0.61 | 0.118 | 5.0 | 1.91/2.12 | 9.2 | 2.00/2.06 | 7.3 | 469 | 0 |
| wcpp cpu small.en-q8_0 | 0.82 | 0.099 | 5.3 | 1.50/1.93 | 10.2 | 1.66/1.92 | 6.9 | 540 | 0 |
| wcpp cpu distil-small.en | 0.77 | 0.079 | 71.9 | 1.76/1.91 | 6.9 | 1.85/2.08 | 5.3 | 514 | 0 |
| wcpp cpu base.en audio_ctx | 0.17 | 0.043 | 7.9 | 0.16/0.22 | 11.6 | 0.32/1.29 | 49.8 | 356 | 0 |
| wcpp cpu small.en audio_ctx | 0.49 | 0.137 | 5.0 | 0.55/0.96 | 23.4 | 1.05/1.61 | 11.9 | 752 | 0 |
| wcpp vulkan tiny.en | 0.72 | 0.008 | 6.6 | 0.05/0.06 | 11.9 | 0.11/0.15 | 9.2 | 223 | 239 |
| wcpp vulkan tiny.en-q5_1 | 0.52 | 0.017 | 8.3 | 0.07/0.11 | 10.2 | 0.17/1.02 | 10.9 | 420 | 205 |
| wcpp vulkan tiny.en-q8_0 | 0.58 | 0.016 | 6.6 | 0.06/0.09 | 11.6 | 0.15/0.21 | 9.9 | 394 | 216 |
| wcpp vulkan base.en | 0.63 | 0.011 | 7.6 | 0.07/0.11 | 12.2 | 0.14/0.22 | 8.6 | 365 | 345 |
| wcpp vulkan base.en-q5_1 | 0.56 | 0.010 | 8.6 | 0.07/0.09 | 14.2 | 0.14/0.21 | 8.3 | 256 | 261 |
| wcpp vulkan base.en-q8_0 | 0.63 | 0.010 | 9.2 | 0.06/0.10 | 12.2 | 0.15/0.21 | 8.3 | 306 | 281 |
| wcpp vulkan small.en | 1.08 | 0.016 | 5.0 | 0.11/0.17 | 9.6 | 0.21/0.22 | 6.9 | 337 | 752 |
| wcpp vulkan small.en-q5_1 | 0.73 | 0.016 | 5.0 | 0.11/0.19 | 9.9 | 0.19/0.24 | 6.9 | 318 | 467 |
| wcpp vulkan small.en-q8_0 | 0.77 | 0.015 | 5.0 | 0.10/0.15 | 9.6 | 0.19/0.22 | 6.9 | 240 | 532 |
| wcpp vulkan distil-small.en | 0.92 | 0.007 | 71.9 | 0.06/0.09 | 6.9 | 0.12/0.15 | 5.3 | 332 | 546 |
| wcpp vulkan large-v3-turbo-q5_0 | 2.07 | 0.051 | 2.6 | 0.11/0.15 | 5.3 | 0.16/0.18 | 4.0 | 491 | 955 |
| wcpp vulkan large-v3-turbo-q8_0 | 2.82 | 0.014 | 3.3 | 0.11/0.12 | 5.0 | 0.15/0.17 | 4.0 | 347 | 1257 |
| wcpp vulkan base.en audio_ctx | 0.66 | 0.010 | 7.6 | 0.06/0.07 | 11.6 | 0.14/0.75 | 49.8 | 311 | 344 |
| wcpp vulkan small.en audio_ctx | 1.00 | 0.015 | 5.0 | 0.11/0.27 | 23.4 | 0.21/0.27 | 11.9 | 400 | 752 |
| wcpp cuda tiny.en | 0.48 | 0.007 | 6.6 | 0.05/0.08 | 11.6 | 0.09/0.13 | 9.2 | 545 | 508 |
| wcpp cuda tiny.en-q5_1 | 0.27 | 0.008 | 9.2 | 0.05/0.09 | 9.9 | 0.09/0.14 | 10.9 | 534 | 450 |
| wcpp cuda tiny.en-q8_0 | 0.30 | 0.008 | 6.9 | 0.04/0.07 | 11.2 | 0.09/0.14 | 9.9 | 529 | 460 |
| wcpp cuda base.en | 0.41 | 0.010 | 9.2 | 0.06/0.09 | 12.2 | 0.11/0.16 | 8.6 | 544 | 598 |
| wcpp cuda base.en-q5_1 | 0.30 | 0.010 | 8.6 | 0.07/0.09 | 13.9 | 0.12/0.17 | 8.3 | 528 | 500 |
| wcpp cuda base.en-q8_0 | 0.29 | 0.011 | 7.9 | 0.07/0.11 | 11.9 | 0.12/0.18 | 9.2 | 530 | 520 |
| wcpp cuda small.en | 0.77 | 0.017 | 5.0 | 0.10/0.15 | 9.6 | 0.18/0.23 | 6.9 | 545 | 988 |
| wcpp cuda small.en-q5_1 | 0.41 | 0.019 | 5.0 | 0.11/0.16 | 9.2 | 0.20/0.24 | 7.6 | 529 | 692 |
| wcpp cuda small.en-q8_0 | 0.53 | 0.020 | 5.3 | 0.12/0.19 | 9.6 | 0.21/0.24 | 6.9 | 530 | 762 |
| wcpp cuda distil-small.en | 0.56 | 0.007 | 71.9 | 0.06/0.08 | 6.9 | 0.11/0.19 | 5.3 | 521 | 784 |
| wcpp cuda large-v3-turbo-q5_0 | 1.24 | 0.013 | 2.3 | 0.11/0.16 | 5.3 | 0.15/0.21 | 4.0 | 534 | 1068 |
| wcpp cuda large-v3-turbo-q8_0 | 1.61 | 0.013 | 3.6 | 0.10/0.13 | 5.3 | 0.14/0.18 | 4.0 | 535 | 1354 |
| wcpp cuda base.en audio_ctx | 0.48 | 0.012 | 9.2 | 0.06/0.09 | 11.6 | 0.11/0.60 | 49.8 | 545 | 598 |
| wcpp cuda small.en audio_ctx | 0.93 | 0.016 | 5.0 | 0.09/0.23 | 23.4 | 0.19/0.28 | 12.2 | 545 | 988 |
| wcpp igpu tiny.en | 0.43 | 0.040 | 6.6 | 0.34/0.38 | 10.9 | 0.45/0.73 | 9.2 | 406 | n/a |
| wcpp igpu tiny.en-q5_1 | 0.59 | 0.027 | 8.3 | 0.29/0.33 | 10.9 | 0.45/1.04 | 10.9 | 382 | n/a |
| wcpp igpu tiny.en-q8_0 | 0.43 | 0.035 | 6.6 | 0.31/0.37 | 11.6 | 0.41/0.77 | 9.9 | 375 | n/a |
| wcpp igpu base.en | 0.51 | 0.054 | 7.9 | 0.61/1.20 | 12.2 | 0.76/1.33 | 8.6 | 490 | n/a |
| wcpp igpu base.en-q5_1 | 0.35 | 0.044 | 8.9 | 0.54/0.61 | 13.9 | 0.66/1.15 | 8.3 | 410 | n/a |
| wcpp igpu base.en-q8_0 | 0.41 | 0.051 | 9.2 | 0.61/1.15 | 12.2 | 0.75/1.33 | 8.3 | 431 | n/a |
| wcpp igpu small.en | 0.79 | 0.144 | 5.0 | 1.96/2.20 | 9.6 | 2.28/2.38 | 6.9 | 903 | n/a |
| wcpp igpu small.en-q5_1 | 0.53 | 0.118 | 5.0 | 1.64/1.82 | 9.6 | 1.89/1.97 | 11.2 | 626 | n/a |
| wcpp igpu small.en-q8_0 | 0.55 | 0.138 | 5.3 | 1.94/2.12 | 9.6 | 2.19/2.28 | 6.9 | 697 | n/a |
| wcpp igpu distil-small.en | 0.61 | 0.104 | 86.5 | 1.72/1.80 | 6.9 | 1.88/1.97 | 5.3 | 685 | n/a |
| wcpp igpu base.en audio_ctx | 0.53 | 0.054 | 7.9 | 0.20/0.26 | 11.6 | 0.40/1.77 | 49.8 | 490 | n/a |
| wcpp igpu small.en audio_ctx | 0.89 | 0.145 | 5.0 | 0.55/1.05 | 23.4 | 1.05/1.59 | 11.9 | 903 | n/a |
| onnx cpu parakeet-tdt-0.6b-v2 int8 | 2.76 | 0.116 | 2.6 | 0.43/0.50 | 4.3 | 0.72/0.76 | 3.0 | 2172 | 0 |
| onnx cpu moonshine-tiny int8 | 0.99 | 0.224 | 215.5 | 0.20/0.30 | 19.1 | 0.31/0.55 | 14.2 | 3700 | 0 |
| onnx cpu moonshine-base int8 | 1.56 | 0.390 | 259.1 | 0.35/0.45 | 10.6 | 0.62/0.83 | 7.9 | 4110 | 0 |

"wcpp" = whisper.cpp; cpu = 6 threads; vulkan and cuda = RTX 3090; igpu = Intel UHD 770 via
Vulkan. "onnx" = sherpa-onnx 1.x Python wheel (onnxruntime CPU, 6 threads); its RAM figures include
the Python interpreter.

## Model file sizes (what the installer would carry)

| model | file | MB |
|---|---|---|
| tiny.en | f16 / q8_0 / q5_1 | 74 / 42 / 31 |
| base.en | f16 / q8_0 / q5_1 | 141 / 78 / 57 |
| small.en | f16 / q8_0 / q5_1 | 465 / 252 / 181 |
| distil-small.en | f16 | 321 |
| large-v3-turbo | q8_0 / q5_0 | 834 / 547 |
| parakeet-tdt-0.6b-v2 | int8 encoder+decoder+joiner | 631 |
| moonshine tiny / base | int8 (4 onnx files) | 118 / 273 |

## Installer cost of each GPU runtime

Measured from the bench exes' import tables (`llvm-readobj --coff-imports`). Every build also needs
the MSVC runtime (MSVCP140, VCRUNTIME140, VCOMP140), which the app already ships or can link
statically.

| build | exe size | extra DLLs to ship | extra MB |
|---|---|---|---|
| whisper.cpp CPU | 1.6 MB | none | 0 |
| whisper.cpp Vulkan | 41.7 MB (embedded SPIR-V shaders) | none; `vulkan-1.dll` comes with the GPU driver | ~40 |
| whisper.cpp CUDA (sm_86 only) | 58.5 MB | `cublas64_13.dll` 52.7 MB + `cublasLt64_13.dll` 476 MB (cudart linked statically) | ~585 |
| onnxruntime CPU (sherpa-onnx) | n/a | `onnxruntime.dll` 18.4 MB + `sherpa-onnx-c-api.dll` 4.6 MB | ~23 |

Shipping CUDA for every NVIDIA generation would need more SM targets in the exe, which makes it
bigger again. It also requires a recent enough NVIDIA driver on the user's machine.

## Observations

- **GPU vs CPU:** on the 3090, every Whisper size up to large-v3-turbo transcribes a 5 s chunk in
  about 0.1 s. On the CPU, tiny/base take 0.2-0.5 s and small takes 1.5-2 s per 5 s chunk, which is
  still real time for one speaker but tight for several participants at once.
- **Vulkan vs CUDA:** the same speed (within noise) for every model. CUDA used more VRAM (context
  overhead) and RAM.
- **The iGPU (UHD 770) was slower than the CPU** for every model. A GPU-or-CPU picker should prefer
  the CPU over a weak integrated GPU, or benchmark both once at startup.
- **First call on Vulkan:** shader pipelines compile on first use. That took 5.8 s for tiny on the
  first-ever run and 8.3 s for the first large-v3-turbo-q5_0 run; later runs were fast because the
  driver caches pipelines. The app should warm the engine up before the call starts.
- **Quantization:** q8_0 and q5_1 cost little or no WER here and cut the file size 2-3x. With
  q5_1, small.en is 181 MB.
- **Chunking hurts accuracy:** for every Whisper model, WER on 5 s chunks is 4-6 points worse than on
  the whole clip; 10 s chunks are in between. Parakeet loses much less (2.6 to 4.3).
- **audio_ctx is not usable as-is.** It cuts latency on CPU (base: 0.44 to 0.16 s per 5 s chunk),
  but WER jumps (small 5 s chunks: 9.6 to 23.4) and it sometimes hallucinates loops (base 10 s:
  49.8).
- **distil-small.en:** excellent on short chunks (the best Whisper chunked WER below
  large-v3-turbo, 6.9 / 5.3), but it breaks down on long-form input (72% WER on the whole clip). It
  is fine for live chunks only.
- **Moonshine:** it is fast on short chunks, but less accurate than Whisper base at similar
  latency, and it fails on long input. Not worth a new runtime.
- **Parakeet TDT 0.6B v2 (int8, CPU):** the best accuracy on every column, and its 5 s chunk
  latency (0.43 s) is similar to Whisper base on CPU. GPU execution through onnxruntime (CUDA or
  DirectML providers) was not measured.

## Skipped and why

- **candle (pure Rust), and whisper-rs built through cargo:** not built. This machine's cargo
  routes crates.io through a local kellnr proxy (192.168.0.65:11102), which was unreachable during
  the spike, and none of the needed crates (whisper-rs, candle-*, hound, tokenizers) were cached.
  Bypassing the proxy was not authorised. `wrs/` (the whisper-rs harness) and `common/bench.rs` are
  written and ready. The safetensors model files for candle are partly downloaded by
  `scripts/getmodels.ps1`, and candle also needs `melfilters.bytes` from the candle repo. When the
  proxy is back, `cargo build --release --features vulkan` in `wrs/` should reproduce the wcpp
  numbers.
- **burn + wgpu whisper:** not attempted, for the same reason (crates), and because of time.
- **Parakeet / Moonshine on GPU:** not measured; CPU only.

## Recommendation

whisper-rs (whisper.cpp) built with the Vulkan feature, with one binary for every GPU vendor plus
the CPU fallback in the same build. Ship small.en-q5_1 for GPU hosts and base.en-q8_0 for CPU-only
hosts, about 260 MB of models in total. Segment audio at pauses (VAD) rather than in fixed 5 s
windows, and warm the engine up before the call starts. This is not pure Rust (it links C++
whisper.cpp), but it is a static link with no runtime DLLs, and it gave the best speed and footprint
measured.

If accuracy matters more than footprint, Parakeet TDT 0.6B v2 on onnxruntime (the `ort` or
`sherpa-rs` crates) gave clearly better transcripts at CPU-real-time speed. The costs are ~650 MB of
model, ~2 GB of RAM, English only, CC-BY-4.0 attribution, and onnxruntime DLLs. It is worth a
follow-up measurement with a GPU execution provider.

candle should still be measured once the crate proxy is back, to see whether pure Rust is within
reach of these numbers.

## Reproduce

1. `scripts/getmodels.ps1` downloads the models. It resolves hosts through 1.1.1.1 because local DNS
   misresolves huggingface.co. Parakeet and Moonshine come from the k2-fsa/sherpa-onnx GitHub release
   `asr-models`.
2. `uv run scripts/mkclip.py data/dummy.parquet data/clip.wav data/clip.txt` builds the test clip.
3. `scripts/buildwcpp.ps1 <cpu|vulkan|cuda> [cmake args]` builds the harness: for Vulkan pass
   `-DGGML_VULKAN=ON`, and for CUDA pass `-DGGML_CUDA=ON -T cuda=13.5 -DCMAKE_CUDA_ARCHITECTURES=86`.
   The build dir is `D:\sttb\<name>` because the Vulkan shader sub-build overflows MAX_PATH under the
   repo. whisper.cpp is cloned into `vendor/whisper.cpp` (git-ignored).
4. `scripts/matrix.ps1 <cpu|vulkan|cuda|igpu>` runs the bench, and `uv run onnx_bench.py ...` runs
   the ONNX models.
5. `uv run score.py results.jsonl data/clip.txt` prints the table.
