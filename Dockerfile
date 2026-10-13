# Both images, from one build.
#
#   builder        compiles the whole workspace in release mode
#   whisper-build  builds whisper.cpp
#   ffmpeg-fetch   fetches a pinned static FFmpeg
#   base           runtime deps + ffmpeg + whisper-cli
#   transcoder     base + vidra              — one video in, an HLS ladder out
#   runner         base + vidra + the runner — claims jobs, calls vidra
#
# A single Dockerfile with two targets rather than one per package: whisper.cpp
# and FFmpeg are expensive and identical for both, so they are built once and
# shared. The build context is the workspace root either way, because Cargo
# needs the root manifest and the lockfile to build any member.
#
#   docker build --target transcoder -t vidra:latest .
#   docker build --target runner     -t vidra-runner:latest .

# ---------------------------------------------------------------------------
# Rust builder
# ---------------------------------------------------------------------------
# rust:1-bookworm tracks the latest stable 1.x rather than a pinned patch
# version, which would drift out of step with Cargo.lock as dependencies move
# their MSRV forward.
FROM rust:1-bookworm AS builder

WORKDIR /build

# Manifests alone first, so the dependency build is cached and only re-runs
# when a manifest changes rather than on every source edit.
COPY Cargo.toml Cargo.lock ./
COPY transcoder/Cargo.toml transcoder/
COPY runner/Cargo.toml runner/

# Stub sources let cargo compile the dependency graph with no real code
# present. The runner depends on the transcoder by path, so the transcoder
# needs a stub lib too or the graph will not resolve. The artifacts are then
# removed so the real build cannot silently reuse them.
RUN mkdir -p transcoder/src runner/src \
 && echo 'fn main() {}' > transcoder/src/main.rs \
 && echo '' > transcoder/src/lib.rs \
 && echo 'fn main() {}' > runner/src/main.rs \
 && echo '' > runner/src/lib.rs \
 && cargo build --release --locked 2>/dev/null || cargo build --release \
 && rm -rf transcoder/src runner/src

COPY transcoder/src transcoder/src
COPY transcoder/tests transcoder/tests
COPY runner/src runner/src

# touch so cargo sees the real sources as newer than the stub artifacts. Both
# binaries are already stripped by `profile.release.strip`.
RUN touch transcoder/src/main.rs transcoder/src/lib.rs \
          runner/src/main.rs runner/src/lib.rs \
 && cargo build --release

# ---------------------------------------------------------------------------
# whisper.cpp
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS whisper-build

ARG WHISPER_VERSION=v1.7.4

# AVX2 roughly doubles transcription throughput over the portable build, and
# every x86-64 CPU since Haswell (2013) has it — along with FMA and F16C, which
# landed in the same generation, hence the single switch.
#
# For older hardware or arm64, pass --build-arg WHISPER_AVX2=OFF. GGML_NATIVE
# stays OFF either way so a build never bakes in the *builder's* CPU features
# and then dies with SIGILL somewhere else.
ARG WHISPER_AVX2=ON

RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      build-essential cmake git ca-certificates \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src
RUN git clone --depth 1 --branch "${WHISPER_VERSION}" \
      https://github.com/ggerganov/whisper.cpp.git .

RUN cmake -B build \
      -DCMAKE_BUILD_TYPE=Release \
      -DGGML_NATIVE=OFF \
      -DGGML_AVX="${WHISPER_AVX2}" \
      -DGGML_AVX2="${WHISPER_AVX2}" \
      -DGGML_FMA="${WHISPER_AVX2}" \
      -DGGML_F16C="${WHISPER_AVX2}" \
      -DWHISPER_BUILD_TESTS=OFF \
      -DWHISPER_BUILD_EXAMPLES=ON \
 && cmake --build build --config Release -j "$(nproc)" \
 && mkdir -p /out/bin /out/lib \
 && cp build/bin/whisper-cli /out/bin/ \
 && find build -name '*.so*' -exec cp -P {} /out/lib/ \; || true

# ---------------------------------------------------------------------------
# Static FFmpeg
# ---------------------------------------------------------------------------
# A pinned static build rather than Debian's ffmpeg: the distro version lags,
# and pinning means an encode is reproducible across image rebuilds.
FROM debian:bookworm-slim AS ffmpeg-fetch

RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      ca-certificates curl xz-utils \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /ffmpeg

# johnvansickle's builds are the long-standing static x86-64 distribution.
#
# No checksum is pinned here, and that is a documented gap rather than an
# oversight: this URL always serves the *current* release, so any hash
# committed today breaks the build the next time upstream rebuilds. To pin
# properly, take a dated tarball from
# https://johnvansickle.com/ffmpeg/releases/ , host it somewhere you control,
# and add a `sha256sum -c` step here. See README.md ("Pinning FFmpeg").
#
# What is verified is that the extracted binaries run and report a version, so
# a truncated download or an HTML error page fails the build rather than
# producing an image whose FFmpeg is quietly broken.
RUN curl -fsSL --retry 5 --retry-delay 3 \
      "https://johnvansickle.com/ffmpeg/releases/ffmpeg-release-amd64-static.tar.xz" \
      -o ffmpeg.tar.xz \
 && tar -xJf ffmpeg.tar.xz --strip-components=1 \
 && mkdir -p /out/bin \
 && cp ffmpeg ffprobe /out/bin/ \
 && /out/bin/ffmpeg -hide_banner -version | grep -q "^ffmpeg version" \
 && /out/bin/ffprobe -hide_banner -version | grep -q "^ffprobe version"

# ---------------------------------------------------------------------------
# Shared runtime
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS base

# ca-certificates for TLS to the model host (and, in the runner, to object
# storage and the queue API). libgomp1 is whisper.cpp's OpenMP runtime.
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      ca-certificates libgomp1 libstdc++6 \
 && rm -rf /var/lib/apt/lists/*

COPY --from=ffmpeg-fetch /out/bin/ffmpeg  /usr/local/bin/ffmpeg
COPY --from=ffmpeg-fetch /out/bin/ffprobe /usr/local/bin/ffprobe
COPY --from=whisper-build /out/bin/whisper-cli /usr/local/bin/whisper-cli
COPY --from=whisper-build /out/lib/ /usr/local/lib/
COPY --from=builder /build/target/release/vidra /usr/local/bin/vidra

# Fail the build, not the first run, if a copied binary cannot actually run.
RUN ldconfig \
 && ffmpeg -version > /dev/null \
 && ffprobe -version > /dev/null \
 && whisper-cli --help > /dev/null 2>&1 \
 && vidra --version > /dev/null

# World-writable rather than owned by a baked-in user: these containers are
# meant to run as whoever owns the host directory being written to
# (`docker run --user "$(id -u):$(id -g)"`), and that uid is not known at
# build time.
#
#   /in       a source video, usually mounted read-only  (transcoder)
#   /out      where the HLS tree is written              (transcoder)
#   /models   the Whisper model cache; mount it to download only once
#   /scratch  intermediates, removed at the end of every run
RUN mkdir -p /in /out /models /scratch \
 && chmod 0777 /out /models /scratch

ENV VIDRA_SCRATCH_DIR=/scratch \
    VIDRA_WHISPER_MODEL_DIR=/models \
    RUST_BACKTRACE=1

# ---------------------------------------------------------------------------
# transcoder: one video in, an HLS ladder out
# ---------------------------------------------------------------------------
FROM base AS transcoder

VOLUME ["/models", "/scratch"]
WORKDIR /out

# No HEALTHCHECK and no supervisor: vidra processes one video and exits. The
# exit status is the result, so whatever started the container decides whether
# to retry.
ENTRYPOINT ["/usr/local/bin/vidra"]

# ---------------------------------------------------------------------------
# runner: claims jobs from a queue, calls vidra, stores the output
# ---------------------------------------------------------------------------
FROM base AS runner

COPY --from=builder /build/target/release/vidra-runner /usr/local/bin/vidra-runner
RUN vidra-runner --version > /dev/null

# The runner stages downloads and output here, so it needs real space — a
# volume, not the container's writable layer.
VOLUME ["/models", "/scratch"]
WORKDIR /scratch

ENV RUNNER_SCRATCH_DIR=/scratch \
    RUNNER_WHISPER_MODEL_DIR=/models \
    RUNNER_VIDRA_BIN=/usr/local/bin/vidra

# No HEALTHCHECK: the runner makes no inbound connections, so there is nothing
# to probe. Whatever supervises the container restarts it if it exits.
#
# SIGTERM lets it finish the batch in flight rather than orphaning a job, so
# give it a long stop timeout: `docker stop --time=600`.
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/vidra-runner"]
