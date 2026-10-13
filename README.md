# vidra

Video transcoding, in two pieces.

| Package | What it is | Shape |
|---|---|---|
| [`transcoder/`](transcoder/) | A video in, an HLS ladder out. | One-shot CLI |
| [`runner/`](runner/) | Claims jobs from a queue, calls the transcoder, stores the result. | Long-lived daemon |

The split is the point. The transcoder knows nothing about queues, buckets or
cloud providers — it reads a path and writes a directory, so it runs anywhere:
a laptop, a CI step, a Kubernetes Job, a cron entry. The runner is the piece
that knows about infrastructure, and it drives the transcoder as a subprocess.

```
       ┌────────── runner ──────────────────────────────┐
       │                                                │
 queue ┤  claim → download → ┌─ vidra ─┐ → upload → ack │
       │                     └─────────┘                │
       └────────────────────────────────────────────────┘
                                 ▲
                   also usable entirely on its own:
                   vidra in/clip.mp4 --out out/clip
```

## Why one repo

The two share a contract: vidra's CLI flags, its exit codes, and the
`manifest.json` schema. Across two repos every change to that contract is a
four-step dance — edit, publish, bump the pin, update the consumer. Here it is
one commit, and the runner depends on the transcoder as a library for the
manifest type, so the schema is a single definition rather than two that drift.

The process boundary is still the CLI. Only the *type* is shared.

## Layout

```
vidra/
  Cargo.toml          workspace: transcoder + runner, one lockfile
  Dockerfile          both images, two targets, shared heavy layers
  Makefile            mkpm monorepo fan-out
  transcoder/         crate "vidra"        → image vidra:latest
  runner/             crate "vidra-runner" → image vidra-runner:latest
```

## Getting started

```sh
make check                              # fmt + clippy + every test
make images                             # build both container images

# transcode something by hand
cp ~/Videos/clip.mp4 transcoder/in/
make transcoder/run file=clip.mp4       # → transcoder/out/clip/

# run the daemon
cd runner && make env-example           # then fill in .env.local
make runner/up
make runner/logs-f
```

Per-package targets are namespaced: `make transcoder/<target>` and
`make runner/<target>`. Each package documents its own with
`make transcoder/help` / `make runner/help`.

`make help` at the root lists only the root targets — the monorepo package
ships a hook meant to aggregate the sub-package help, but the bundled oras
plugin registers a `help_hook` too and the last one loaded wins.

## The readiness contract

Both halves preserve one invariant: **`manifest.json` is written last**, after
everything it references exists.

The transcoder writes it last on local disk. The runner uploads the whole tree
*except* the manifest, and only then uploads the manifest on its own — so the
guarantee survives the network. A naive recursive sync would put it somewhere
in the middle and a reader could see a `ready` manifest pointing at segments
still in flight.

So, for any consumer, in local or remote storage:

| State | Meaning |
|---|---|
| `manifest.json` says `"ready"` | Complete and playable. |
| `manifest.json` says `"failed"` | Failed; the `error` field says why. |
| no `manifest.json` | Interrupted partway. Output is incomplete. |

## Exit codes

The transcoder distinguishes failures the caller can do something about:

| Code | Meaning | What the runner does |
|---|---|---|
| `0` | Ready | Upload, then acknowledge the job. |
| `1` | Transient — a full disk, a killed process | Release the job for another attempt. |
| `2` | Permanent — corrupt or unreadable input | Record the failure and acknowledge immediately. |

Without that distinction a corrupt upload would fail identically on all three
attempts, consuming the whole retry budget and delaying the failure report by
hours. `2` rather than a private number because clap already exits `2` on a
bad argument list, and that is the same category: the request is wrong.

## Development

A local Rust toolchain is needed for the tests; running either service is not.

```sh
make test        # whole workspace
make lint        # clippy -D warnings
make fmt
```

243 tests. Six are integration tests that generate a real clip with FFmpeg,
encode it and assert on the actual output — advertised resolutions match
encoded ones, keyframes align across renditions, the master playlist only
references files that exist, a small source is not upscaled. Those six need
`ffmpeg` and `ffprobe` on `PATH`.

Everything else runs without a video file, which is deliberate: the modules
that matter take paths and return bytes or argument vectors, so the FFmpeg
command line itself is unit-testable.

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

### Older or non-x86 hardware

whisper.cpp is compiled with AVX2 by default — every x86-64 CPU since Haswell
(2013) has it. For older hardware or arm64:

```sh
docker build --target runner --build-arg WHISPER_AVX2=OFF -t vidra-runner:latest .
```

`GGML_NATIVE` is always `OFF`, so a build never bakes in the *builder's* CPU
features and then dies with `SIGILL` somewhere else.
