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
#
# amd64 and arm64 are both supported. Nothing needs passing in: the two
# architecture-dependent steps — which static FFmpeg to fetch, and whether the
# x86 SIMD flags apply to whisper.cpp — read TARGETARCH when BuildKit sets it
# and fall back to the build stage's own dpkg architecture otherwise, so a
# plain native `docker build` is correct on either.
#
#   docker buildx build --platform linux/amd64,linux/arm64 --target runner \
#     -t ghcr.io/fjapm/vidra-runner:<tag> --push .
#
# Cross-building arm64 on an amd64 host works only under QEMU, because several
# stages execute what they just built (cargo, cmake, and the FFmpeg version
# check). Emulating a Rust release build and a whisper.cpp compile is slow
# enough that building natively on an arm64 machine is usually the better
# trade.

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
# These flags are x86-only: passing them on arm64 fails the cmake configure, so
# the default is decided per architecture rather than fixed. Set WHISPER_AVX2
# explicitly to override — OFF for a pre-Haswell x86 host, for instance.
#
# GGML_NATIVE stays OFF on every architecture so a build never bakes in the
# *builder's* CPU features and then dies with SIGILL somewhere else. On arm64
# that means NEON only, no dotprod or i8mm: portable, somewhat slower.
#
# TARGETARCH is set by BuildKit. It is empty under the classic builder and on a
# plain native build, so dpkg is the fallback — it reports the architecture of
# this build stage, which for a native build is the one we want.
ARG TARGETARCH=
ARG WHISPER_AVX2=

RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      build-essential cmake git ca-certificates \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src
RUN git clone --depth 1 --branch "${WHISPER_VERSION}" \
      https://github.com/ggerganov/whisper.cpp.git .

RUN set -eu; \
    arch="${TARGETARCH:-$(dpkg --print-architecture)}"; \
    avx2="${WHISPER_AVX2:-}"; \
    if [ -z "$avx2" ]; then \
      case "$arch" in \
        amd64) avx2=ON ;; \
        arm64) avx2=OFF ;; \
        *) echo "unsupported architecture: $arch" >&2; exit 1 ;; \
      esac; \
    fi; \
    echo "whisper.cpp: arch=$arch x86 SIMD=$avx2"; \
    cmake -B build \
      -DCMAKE_BUILD_TYPE=Release \
      -DGGML_NATIVE=OFF \
      -DGGML_AVX="$avx2" \
      -DGGML_AVX2="$avx2" \
      -DGGML_FMA="$avx2" \
      -DGGML_F16C="$avx2" \
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

# johnvansickle publishes static builds for both amd64 and arm64, under names
# that differ only by that token — so the architecture picks the tarball.
#
# No checksum is pinned here, and that is a documented gap rather than an
# oversight: this URL always serves the *current* release, so any hash
# committed today breaks the build the next time upstream rebuilds. To pin
# properly, take a dated tarball from
# https://johnvansickle.com/ffmpeg/releases/ , host it somewhere you control,
# and add a `sha256sum -c` step here. See README.md ("Pinning FFmpeg").
#
# What is verified is that the extracted binaries run and report a version, so
# a truncated download, an HTML error page, or a tarball for the wrong
# architecture fails the build rather than producing an image whose FFmpeg is
# quietly broken. On a cross-architecture build that check needs emulation; a
# native build on the target architecture needs none.
ARG TARGETARCH=
RUN set -eu; \
    arch="${TARGETARCH:-$(dpkg --print-architecture)}"; \
    case "$arch" in \
      amd64 | arm64) ;; \
      *) echo "no static FFmpeg published for: $arch" >&2; exit 1 ;; \
    esac; \
    echo "ffmpeg: fetching the ${arch} static build"; \
    curl -fsSL --retry 5 --retry-delay 3 \
      "https://johnvansickle.com/ffmpeg/releases/ffmpeg-release-${arch}-static.tar.xz" \
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
