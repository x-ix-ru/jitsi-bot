FROM rust:1.88-bookworm AS builder
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential clang cmake git libclang-dev libgstreamer1.0-dev \
    libgstreamer-plugins-base1.0-dev libnice-dev libssl-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /upstream
RUN git clone https://github.com/jbg/gst-meet.git . \
    && git checkout 2947267f8d2ca780b010cd4f4c38e0f41cda0009 \
    && cargo build --locked --release -p gst-meet
WORKDIR /bridge
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libnice10 gstreamer1.0-tools gstreamer1.0-plugins-base \
    gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
    gstreamer1.0-nice gstreamer1.0-plugins-ugly \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /upstream/target/release/gst-meet /usr/local/bin/gst-meet
COPY --from=builder /bridge/target/release/hermes-jitsi-bridge /usr/local/bin/hermes-jitsi-bridge
RUN useradd --uid 10001 --create-home bridge && mkdir -p /data/audio && chown bridge:bridge /data/audio
USER bridge
ENV AUDIO_OUTPUT_DIR=/data/audio
ENTRYPOINT ["/usr/local/bin/hermes-jitsi-bridge"]
CMD ["run"]
