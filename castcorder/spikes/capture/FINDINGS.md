# Spike: GStreamer as the native media engine

Written by the lead from the spike agent's report.

## Verdict: go with caveats

One capture, teed to a bit-exact raw WAV branch and a live branch (webrtcdsp echo cancel and noise suppression, then opusenc), works. A stalled live branch cannot harm the recording, and the recording thread can run at real-time priority. The one-command Docker run ends `OVERALL: PASS`.

Run from this directory:

```
docker build -t castcorder-capture-spike . && docker run --rm --ulimit rtprio=95 castcorder-capture-spike
```

The base image is Ubuntu 26.04 (GStreamer 1.28.2). Ubuntu 24.04 ships GStreamer 1.24.2 without webrtcdsp.

Did not pass or not done:

- The PulseAudio pipe-source virtual mic failed (no data in 10 s, cause not diagnosed). Its cases are commented out of the run script.
- In the device-loss test, the first segment captured 0 bytes (see below).
- PipeWire was not attempted. v4l2loopback is not available under Docker Desktop.
- Real hardware and Windows capture were not run.

## Proven by passing automated runs

1. **Tee and priority.** The recording branch uses a never-leaky queue into our own WAV writer; the live branch uses a leaky 200 ms queue. The recording queue's streaming thread was switched to `SCHED_FIFO` priority 50 from a sync bus handler, confirmed in `/proc`. This needs `--ulimit rtprio=95` or `CAP_SYS_NICE`; otherwise it falls back to `nice`.
2. **Bit-exactness with a file source** (5 tests). 48 kHz/24-bit and 44.1 kHz/16-bit stereo: the data chunk is byte-identical. With the live sink stalled (70 of 76 buffers dropped on the live side) the recording was still byte-identical.
3. **Real capture through a PulseAudio null sink monitor.**

   | Case | Frames bit-identical |
   |---|---|
   | 48k/24 | 143603 of 144037 (99.70%) |
   | 44.1k/16 | 131929 of 132337 (99.69%) |

   The differences begin at frame 0, where the capture holds zeros: about the first 9 ms lost when playback starts, not processing. Not verified that every differing frame is in that leading run. This null-sink setup is the recommended virtual mic for CI.
4. **Crash safety** (SIGKILL about 3.0 s in; a process kill, so the page cache survives).

   | Writer | Header claims | On disk | After repair |
   |---|---|---|---|
   | wavenc | 2 GB placeholder | 2.816 s | 2.816 s |
   | Ours, header rewritten every 1 s | 2.027 s | 3.008 s | 3.008 s |
   | Ours, every 100 ms plus fdatasync | 2.987 s | 3.008 s | 3.008 s |

5. **Video** (videotestsrc 720p30, openh264enc 8 Mbps recording and 500 kbps live). Muxer kill test after 4 s:

   | Muxer | Decodable frames |
   |---|---|
   | plain mp4mux | 0 |
   | fragmented MP4 | 102 of 120 |
   | Matroska | 120 of 120 |

6. **Device loss and return.** The device monitor reports removal and return. On removal `pulsesrc` posted no error; data just stopped, so detection needs the monitor plus a no-data watchdog. The program closed the WAV cleanly and opened `segment-002` on return. Caveat: a fresh `pulsesrc` on an idle null-sink monitor delivered nothing for about 2 s, so segment 1 was empty; unexplained.
7. **Clock injection.** A live test source driven by GStreamer's test clock produces buffers only when the test advances the clock.

## Recommendations

- **Keep our own WAV writer**, on the recording thread behind the disk port: unbuffered writes, header rewritten about every 1 s, room reserved for RF64 and a switch to it past 4 GiB, and a startup repair that fixes sizes from the file length. Reject wavenc: its header is useless after a kill and it needs end-of-stream to finalise.
- **Matroska for video recording**, or fragmented MP4 if MP4 is required.
- **Detect device loss** with the device monitor plus a no-data watchdog, not pipeline errors.
- **Ports.** A `CaptureSource` trait returns an element with a "src" pad (real: pulsesrc, wasapi2src, osxaudiosrc; synthetic: a WAV file or test tone); everything downstream is the same real pipeline. A time port installs a clock with `pipeline.use_clock()`.
- **Timing metadata.** The writer logs sample index, buffer timestamps, pipeline clock and system monotonic time once per second to `<track>.timing.jsonl`.

## Researched only, not verified

- **GitHub runners.** `ubuntu-latest` is 24.04. A virtual webcam likely needs `v4l2loopback-dkms` built against the runner's Azure kernel, then `--device /dev/video0`. Untested.
- **Windows.** `wasapi2src` has a low-latency option and, in newer versions, a raw mode that bypasses OS effects where the endpoint supports it. Shared mode often delivers 32-bit float; the writer supports float WAV. A trimmed plugin set is about 30-60 MB.
- **macOS.** `osxaudiosrc` is the raw path; `avfvideosrc` handles the camera. The framework bundles inside the .app.
- **H.264 without GPL.** Hardware encoders per platform (Media Foundation, nvcodec, Quick Sync, AMF, VideoToolbox, VA-API), with openh264enc as the software fallback, downloaded at runtime from Cisco rather than bundled.
- **Risks.** Windows raw-mode behaviour needs a hands-on hardware test; bundle size and plugin trimming; webrtcdsp availability on Linux distros. No showstoppers found.
