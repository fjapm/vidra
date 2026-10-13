# vidra

A video in, an HLS ladder out.

`vidra` takes one source video and writes a complete, playable HLS package: an
adaptive bitrate ladder, a thumbnail sprite with its WebVTT index, a poster
frame, subtitle tracks, and a `manifest.json` describing all of it.

It is a **one-shot process**, not a daemon. There is no queue client, no
storage client and no HTTP server: the input is a path, the output is a
directory, and whatever scheduled the run decides what those are. That is what
makes it portable — the same image works under `docker run`, a Kubernetes Job,
a CI step, a cron entry, or a queue consumer that shells out to it.

```
        in/clip.mp4  ──▶  vidra  ──▶  out/clip/
                                        master.m3u8
                                        1080p/ 720p/ 480p/
                                        captions/en.vtt + en.m3u8
                                        thumbs.jpg + thumbs.vtt
                                        poster.jpg
                                        manifest.json
```

## Quick start

With Docker directly. Note the build context is the **workspace root**, one
level up: Cargo needs the root manifest and lockfile to build any member, and
the image is one target of a Dockerfile shared with the runner.

```sh
docker build --target transcoder -t vidra:latest ..

cp ~/Videos/clip.mp4 in/

docker run --rm \
  --user "$(id -u):$(id -g)" \
  -v "$PWD/in:/in:ro" \
  -v "$PWD/out:/out" \
  -v vidra-models:/models \
  vidra:latest /in/clip.mp4 --out /out/clip
```

Or through the Makefile, which wraps the same thing:

```sh
make image-build          # build the image
make run file=clip.mp4    # transcode ./in/clip.mp4 into ./out/clip
make help                 # every target
make help-vidra           # the vidra --help
```

From the workspace root these are namespaced — `make transcoder/run
file=clip.mp4`.

`make run` passes `--user "$(id -u):$(id -g)"` so the output is owned by you
rather than by root, and `--force` so repeated local runs overwrite instead of
refusing.

Mount `/models` to something persistent. The default Whisper model is around
550 MB, and without a mount it is re-downloaded on every single run.

## Output layout

```
out/clip/
  master.m3u8               multivariant playlist — the URL a player opens
  1080p/
    index.m3u8              media playlist for this rendition
    init.mp4                fMP4 initialisation segment
    seg_00001.m4s …         6-second media segments
  720p/ 480p/ …             one directory per rendition in the ladder
  captions/
    en.vtt                  the subtitle track itself
    en.m3u8                 its media playlist (Safari ignores a bare .vtt)
  poster.jpg                single frame, taken 10% into the video
  thumbs.jpg                scrub-preview sprite sheet
  thumbs.vtt                maps playback time to a sprite tile
  manifest.json             what was produced — written last
```

The ladder is derived from the source: `vidra` never upscales, so a 480p input
produces a 480p-and-below ladder rather than a blurry 1080p rendition.
Keyframes are aligned across renditions so a player can switch bitrate mid-
stream without a visible stall.

## The readiness contract

`manifest.json` is written **last**, after every file it references is on disk.
This is the whole signalling mechanism, and it holds in both directions:

| State | Meaning |
|---|---|
| exit `0`, `manifest.json` says `"ready"` | Complete and playable. |
| exit non-zero, `manifest.json` says `"failed"` | Failed; the `error` field says why. |
| no `manifest.json` | The run was killed partway. Output is incomplete. |

So a consumer never has to guess. If the manifest is absent the directory is
garbage; if it is present it is accurate.

### Exit codes

Non-zero splits two ways, so a caller can tell whether retrying is worthwhile:

| Code | Meaning |
|---|---|
| `1` | Transient — a full disk, a killed process. Another attempt might work. |
| `2` | Permanent — missing, empty, corrupt or unreadable input; a non-empty output directory without `--force`. Retrying will fail identically. |

`2` rather than a private number because clap already exits `2` on a bad
argument list, and that is the same category of problem: the request is wrong,
so sending it again will not help. The classification is deliberately
conservative — a failure is transient unless something specifically marks it
otherwise, since retrying a permanent failure wastes time while *not* retrying
a transient one loses the video.

A `ready` manifest, verbatim from a real run:

```json
{
  "video_id": "clip",
  "status": "ready",
  "duration_seconds": 12.0,
  "source": { "width": 1280, "height": 720, "fps": 30.0 },
  "renditions": [
    { "height": 720, "width": 1280, "bitrate_kbps": 2800, "playlist": "720p/index.m3u8" },
    { "height": 480, "width": 854,  "bitrate_kbps": 1400, "playlist": "480p/index.m3u8" },
    { "height": 360, "width": 640,  "bitrate_kbps": 800,  "playlist": "360p/index.m3u8" }
  ],
  "captions": [
    { "language": "en", "path": "captions/en.vtt", "auto_generated": false }
  ],
  "poster": "poster.jpg",
  "thumbnails": {
    "sprite": "thumbs.jpg",
    "vtt": "thumbs.vtt",
    "interval_seconds": 10,
    "tile_width": 160,
    "tile_height": 90,
    "columns": 5,
    "count": 6
  },
  "master_playlist": "master.m3u8",
  "processed_at": "2026-10-12T17:11:43.153119277Z"
}
```

Renditions are ordered highest-bandwidth first, matching the master playlist.
A `failed` manifest carries `video_id`, `status`, `processed_at` and `error`,
and omits everything that was never produced.

## Command line

```
vidra [OPTIONS] --out <DIR> <INPUT>
```

| Flag | Environment | Default | Meaning |
|---|---|---|---|
| `<INPUT>` | `VIDRA_INPUT` | — | Source video file. |
| `-o, --out <DIR>` | `VIDRA_OUT` | — | Output directory. Created if absent. |
| `--id <ID>` | `VIDRA_ID` | input filename stem | Recorded as `video_id`. No path separators. |
| `--captions-dir <DIR>` | `VIDRA_CAPTIONS_DIR` | — | Directory of `{language}.vtt` files. |
| `--no-transcribe` | `VIDRA_NO_TRANSCRIBE` | off | Never run Whisper. |
| `--thumbnail-interval <S>` | `VIDRA_THUMBNAIL_INTERVAL` | `10` | Seconds between sprite tiles. |
| `--scratch-dir <DIR>` | `VIDRA_SCRATCH_DIR` | system temp (`/scratch` in the image) | Intermediates. |
| `--whisper-model <FILE>` | `VIDRA_WHISPER_MODEL` | `ggml-large-v3-turbo-q5_0.bin` | Model filename. |
| `--whisper-model-dir <DIR>` | `VIDRA_WHISPER_MODEL_DIR` | `/models` | Model cache. Mount this. |
| `-f, --force` | `VIDRA_FORCE` | off | Clear a non-empty output directory. |
| `--log-pretty` | `VIDRA_LOG_PRETTY` | off | Human-readable logs instead of JSON. |

Flags win over the environment. `RUST_LOG` controls log filtering
(`RUST_LOG=debug` to see every FFmpeg invocation).

Two environment footguns, both inherited from how the libraries behave:
`RUST_LOG=` (set but empty) installs an empty filter and silences *all* logs,
and `VIDRA_LOG_PRETTY=` (set but empty) is a hard parse error rather than
"false". `docker-compose.yml` therefore uses the value-less `- RUST_LOG` form,
which passes a variable through only when it is actually set on the host.

### Why a non-empty output directory is refused

Re-encoding into a directory that already holds a previous run would leave
orphaned segments that the new playlists never reference — they cost storage
and, if served, can be played by a stale playlist. `--force` clears the
directory contents first. The directory itself is kept, because it is usually a
mount point.

## Captions

Captions resolve in this order, and the first one that yields a track wins:

1. **Supplied files.** With `--captions-dir`, every `{language}.vtt` in that
   directory becomes a subtitle rendition. The language comes from the
   filename — `en.vtt`, `pt-BR.vtt`. A file that is not valid WebVTT is
   skipped with a warning rather than failing the run: a playable video with
   one missing subtitle track beats no video.
2. **Transcription.** With nothing supplied, `whisper.cpp` transcribes the
   audio and the resulting track is marked `"auto_generated": true`. The model
   is downloaded on first use and checksum-verified.
3. **Nothing.** A source with no audio, `--no-transcribe`, a missing
   `whisper-cli`, or a failed transcription all produce a video with no
   captions. None of these fail the run.

The model is fetched lazily, immediately before transcription — not at
startup — so supplying captions or passing `--no-transcribe` costs no
download at all.

## Development

The test suite needs a local Rust toolchain; running the service does not.

```sh
make check        # fmt-check + clippy -D warnings + cargo test
make test
make lint
```

These run against the whole workspace from the root; `make test` here is
`cargo test -p vidra`.

169 tests for the transcoder, of which 6 are integration tests that generate a
real clip with FFmpeg, encode it, and assert on the actual output — that
advertised resolutions match encoded ones, that keyframes align across
renditions, that the master playlist only references files that exist, and that
a small source is not upscaled. Those 6 need `ffmpeg` and `ffprobe` on `PATH`.

Everything else is a pure function of its inputs and runs without a video
file. That split is deliberate: `probe`, `ladder`, `hls`, `thumbnails`,
`manifest` and `process` take paths and return bytes or FFmpeg argument
vectors, so the FFmpeg command line itself is unit-testable.

### Module layout

| Module | Responsibility |
|---|---|
| `cli` | Argument and environment parsing, validation, derived defaults. |
| `pipeline` | Orchestrates one run; the only module that knows the output layout. |
| `probe` | `ffprobe` → `SourceInfo`. |
| `ladder` | Chooses the rendition set and builds the FFmpeg argument vector. |
| `hls` | Generates the master, media and subtitle playlists, and `thumbs.vtt`. |
| `thumbnails` | Sprite planning, poster and sprite generation. |
| `captions` | WebVTT validation, Whisper model management, transcription. |
| `manifest` | The `manifest.json` schema. Also consumed by the runner, so the contract has one definition. |
| `process` | Child-process helper: timeouts, captured stderr, closed stdin. |
| `error` | Permanent-vs-transient classification, which drives the exit code. |

### Pinning FFmpeg

The Dockerfile fetches `ffmpeg-release-amd64-static.tar.xz`, which always
serves the *current* release, so no checksum is pinned — any hash committed
today would break the build the next time upstream rebuilds. The build does
verify that the extracted binaries run and report a version, so a truncated
download or an HTML error page fails the build rather than producing an image
whose FFmpeg is quietly broken.

To pin properly: take a dated tarball from
<https://johnvansickle.com/ffmpeg/releases/>, host it somewhere you control,
and add a `sha256sum -c` step to the `ffmpeg-fetch` stage.

### Building for older or non-x86 hardware

`whisper.cpp` is compiled with AVX2 by default, which roughly doubles
transcription throughput and is available on every x86-64 CPU since Haswell
(2013). For older hardware or arm64:

```sh
docker build --target transcoder --build-arg WHISPER_AVX2=OFF -t vidra:latest ..
```

`GGML_NATIVE` is always `OFF`, so a build never bakes in the *builder's* CPU
features and then dies with `SIGILL` somewhere else.

## Provenance

This started as `worker-transcode` in a private video-hosting infrastructure
repo, where it ran on a single Hetzner VPS as a long-lived worker: it polled a
Cloudflare Queue for R2 object-create events, downloaded each source from R2,
and uploaded the output back to R2.

The encoding core came across unchanged. What was removed is everything that
assumed that one deployment:

- the Cloudflare Queues poll loop, lease/ack/retry handling and dead-letter
  accounting;
- the R2/S3 client, and the event-notification parsing that turned an object
  key into a job;
- the HMAC-signed completion webhook;
- the `Cache-Control` and `Content-Type` tables, which were properties of
  serving the output over HTTP rather than of producing it.

All of that now lives in [`../runner`](../runner/), which is the piece that
knows about infrastructure — and it is provider-agnostic there, rather than
assuming Cloudflare. The transcoder keeps only what is true of transcoding
anywhere.

Concurrency went the same way: it bounded how many *videos* a worker handled at
once, which a one-shot process does not decide. Renditions are still encoded
sequentially, exactly as before. To transcode several videos at once, run
several containers — which is what the runner does.
