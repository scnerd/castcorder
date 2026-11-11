# Castcorder capture spike: GStreamer + Rust inside a Linux container.
# Ubuntu 26.04 (GStreamer 1.28): 24.04 (ubuntu-latest) lacks webrtcdsp, see FINDINGS.md.
FROM ubuntu:26.04

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl build-essential pkg-config procps file \
        libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
        gstreamer1.0-tools gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
        gstreamer1.0-plugins-bad gstreamer1.0-libav gstreamer1.0-pulseaudio \
        gstreamer1.0-pipewire gstreamer1.0-plugins-ugly gstreamer1.0-alsa kmod \
        pulseaudio pulseaudio-utils v4l-utils \
    && rm -rf /var/lib/apt/lists/*

# Unprivileged user: PulseAudio refuses to run as root outside system mode.
RUN useradd -m -s /bin/bash spike
USER spike
WORKDIR /home/spike

ENV RUSTUP_HOME=/home/spike/.rustup CARGO_HOME=/home/spike/.cargo PATH=/home/spike/.cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain 1.99.1 \
    && rustc --version

# Dependency cache layer.
COPY --chown=spike:spike Cargo.toml Cargo.lock* /home/spike/app/
WORKDIR /home/spike/app
RUN mkdir -p src tests && echo 'fn main() {}' > src/main.rs && echo '' > src/lib.rs \
    && cargo build --release && rm -rf src tests

COPY --chown=spike:spike . /home/spike/app/
RUN sed -i 's/\r$//' scripts/*.sh pulse/* && chmod +x scripts/*.sh \
    && touch src/main.rs src/lib.rs && cargo build --release && cargo test --release --no-run

CMD ["/home/spike/app/scripts/run-all.sh"]
