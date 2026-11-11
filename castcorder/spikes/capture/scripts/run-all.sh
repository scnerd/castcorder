#!/bin/bash
# Runs every capture-spike scenario inside the container and exits non-zero
# if any required step fails. Measurement-only steps report but never fail.
set -u
APP=/home/spike/app
BIN=$APP/target/release/capture-spike
OUT=${OUT_DIR:-/tmp/spike-out}
rm -rf "$OUT"; mkdir -p "$OUT"
cd "$APP"

FAILED=()
PASSED=()
step() {
    # step <required|measure> <name> <command...>
    local kind=$1 name=$2; shift 2
    echo
    echo "=================================================================="
    echo "=== [$kind] $name"
    echo "=================================================================="
    if "$@"; then
        echo "--- PASS: $name"; PASSED+=("$name")
    else
        if [ "$kind" = required ]; then
            echo "--- FAIL: $name"; FAILED+=("$name")
        else
            echo "--- (measurement step returned non-zero: $name)"; PASSED+=("$name (measure, rc!=0)")
        fi
    fi
}

echo "capture spike run on $(uname -srm), $(nproc) cpus"
rustc --version
echo "ulimit -r (rtprio): $(ulimit -r)"

# --- 0. what is installed ---------------------------------------------------
step measure "probe elements and devices (before sound server)" $BIN probe

# --- 1+2. pipeline and bit-exactness with a synthetic source ---------------
step required "cargo test (unit, bit-exact file source, stalled live branch, TestClock)" \
    cargo test --release -q -- --nocapture --test-threads=1

$BIN gen-wav --rate 48000 --bits 24 --out "$OUT/ref48k24.wav"
$BIN gen-wav --rate 44100 --bits 16 --out "$OUT/ref44k16.wav"
step required "CLI record-file 48k/24 (thread priority report)" \
    $BIN record-file --in "$OUT/ref48k24.wav" --out "$OUT/file48k24.wav"
step required "CLI record-file 44.1k/16 with wavenc writer" \
    $BIN record-file --in "$OUT/ref44k16.wav" --out "$OUT/file44k16.wav" --writer wavenc
step required "CLI record-file 48k/24 with live branch stalled 50 ms/packet" \
    $BIN record-file --in "$OUT/ref48k24.wav" --out "$OUT/stall48k24.wav" --stall-ms 50

# --- 3. real capture path through PulseAudio virtual devices ---------------
mkdir -p ~/.config/pulse
cp $APP/pulse/daemon.conf ~/.config/pulse/daemon.conf
pulseaudio --daemonize=yes -n -F $APP/pulse/default.pa --log-target=file:$OUT/pulseaudio.log
for i in $(seq 50); do pactl info >/dev/null 2>&1 && break; sleep 0.1; done
pactl info | grep -E "Server (Name|Version)|Default Sample"

pipe_case() { # name format rate ref
    local name=$1 fmt=$2 rate=$3 ref=$4
    rm -f "$OUT/$name.fifo"
    local id
    id=$(pactl load-module module-pipe-source source_name=$name file="$OUT/$name.fifo" \
        format=$fmt rate=$rate channels=2)
    $BIN pulse-roundtrip --mode pipe --in "$ref" --device $name --fifo "$OUT/$name.fifo" --out "$OUT/cap-$name.wav"
    local rc=$?
    pactl unload-module "$id"
    return $rc
}
monitor_case() { # name format rate ref [capture-caps]
    local name=$1 fmt=$2 rate=$3 ref=$4 caps=${5:-}
    local id
    id=$(pactl load-module module-null-sink sink_name=$name format=$fmt rate=$rate channels=2)
    pactl set-sink-volume $name 100%
    pactl set-source-volume $name.monitor 100%
    if [ -n "$caps" ]; then
        $BIN pulse-roundtrip --mode monitor --in "$ref" --sink $name --device $name.monitor --caps "$caps" --out "$OUT/cap-$name.wav"
    else
        $BIN pulse-roundtrip --mode monitor --in "$ref" --sink $name --device $name.monitor --out "$OUT/cap-$name.wav"
    fi
    local rc=$?
    pactl unload-module "$id"
    return $rc
}
# Not run: module-pipe-source delivered no data to pulsesrc and left PulseAudio 17 unusable (see FINDINGS.md).
# step measure "pulse pipe-source virtual mic, 48k/24" pipe_case vmic48 s24le 48000 "$OUT/ref48k24.wav"
# step measure "pulse pipe-source virtual mic, 44.1k/16" pipe_case vmic44 s16le 44100 "$OUT/ref44k16.wav"
step measure "pulse null-sink monitor (playback mixed by server), 48k/24" monitor_case vsink48 s24le 48000 "$OUT/ref48k24.wav"
step measure "pulse null-sink monitor, 44.1k/16" monitor_case vsink44 s16le 44100 "$OUT/ref44k16.wav"
step measure "pulse mismatch: 44.1k/16 file into a 48k/24 device, captured at 44.1k/16" \
    monitor_case vsinkmis s24le 48000 "$OUT/ref44k16.wav" "audio/x-raw,format=S16LE,rate=44100,channels=2"

# --- 4. crash safety --------------------------------------------------------
step measure "SIGKILL mid-recording: wavenc writer" \
    $BIN kill-audio --out "$OUT/kill-wavenc.wav" --after-ms 3000 -- --source tone --writer wavenc
step measure "SIGKILL mid-recording: app writer, header every 1000 ms" \
    $BIN kill-audio --out "$OUT/kill-app1000.wav" --after-ms 3000 -- --source tone --writer app --header-ms 1000
step measure "SIGKILL mid-recording: app writer, header every 100 ms + fdatasync" \
    $BIN kill-audio --out "$OUT/kill-app100.wav" --after-ms 3000 -- --source tone --writer app --header-ms 100 --fsync yes
id=$(pactl load-module module-null-sink sink_name=vkill format=s24le rate=48000 channels=2)
gst-launch-1.0 -q audiotestsrc is-live=true wave=sine ! audio/x-raw,format=S24LE,rate=48000,channels=2 ! pulsesink device=vkill &
FEED=$!
step measure "SIGKILL mid-recording: app writer, real pulsesrc capture" \
    $BIN kill-audio --out "$OUT/kill-pulse.wav" --after-ms 3000 -- --source pulse:vkill.monitor \
        --caps "audio/x-raw,format=S24LE,rate=48000,channels=2" --writer app
kill $FEED; pactl unload-module "$id"

# --- 5. video ---------------------------------------------------------------
echo; echo "v4l2loopback check:"; ls -l /dev/video* 2>&1 | head; (modprobe v4l2loopback 2>&1 || true) | head -2
step required "video tee: clean recording (Matroska) + live branch" \
    $BIN record-video --mux mkv --out "$OUT/clean.mkv" --seconds 4
step required "video tee: clean recording (fragmented MP4)" \
    $BIN record-video --mux mp4frag --out "$OUT/clean-frag.mp4" --seconds 4
for m in mp4 mp4frag mkv; do
    step measure "video SIGKILL survival: $m" $BIN kill-video --mux $m --out "$OUT/kill-video.$m" --after-ms 4000
done

# --- 6. device handling / segments -----------------------------------------
segments_case() {
    local id
    id=$(pactl load-module module-null-sink sink_name=vseg format=s24le rate=48000 channels=2)
    $BIN segments --device vseg.monitor --dir "$OUT/segments" --seconds 8 --expect-segments 2 \
        --during "sleep 2; pactl unload-module $id; sleep 2; pactl load-module module-null-sink sink_name=vseg format=s24le rate=48000 channels=2 >/dev/null"
    local rc=$?
    pactl unload-module module-null-sink 2>/dev/null; pactl load-module module-null-sink sink_name=placeholder >/dev/null
    return $rc
}
step measure "probe devices with PulseAudio running" $BIN probe
step required "device removed mid-capture -> segment closed, new segment on return" segments_case

pulseaudio --kill 2>/dev/null

echo
echo "=================================================================="
echo "SUMMARY"
for p in "${PASSED[@]}"; do echo "  PASS  $p"; done
for f in "${FAILED[@]+"${FAILED[@]}"}"; do echo "  FAIL  $f"; done
if [ ${#FAILED[@]} -gt 0 ]; then
    echo "OVERALL: FAIL (${#FAILED[@]} required step(s) failed)"
    exit 1
fi
echo "OVERALL: PASS"
