# vidra-runner

Claims transcode jobs from a queue, runs [vidra](../transcoder/) on each one,
and stores the output.

This is the half that knows about infrastructure. It is a long-lived daemon:
it polls, and for each job it downloads the source, invokes the transcoder as a
subprocess, uploads the result, and only then acknowledges the job.

```
queue ─claim─→ download ─→ vidra (subprocess) ─→ upload ─→ acknowledge
                                                              │
                        ← release for retry ──── on failure ──┤
                        ← record failure + ack ── if permanent┘
```

## Provider support

**Object storage is provider-agnostic.** It speaks the S3 API, so it works
against R2, AWS S3, MinIO, Backblaze B2, DigitalOcean Spaces, Wasabi and Ceph.
Three settings differ:

| | R2 | AWS S3 | MinIO |
|---|---|---|---|
| `RUNNER_S3_ENDPOINT` | account URL | unset | your URL |
| `AWS_DEFAULT_REGION` | `auto` | a real region | `us-east-1` |
| `RUNNER_S3_PATH_STYLE` | `true` | `false` | `true` |

There is deliberately no trait for this. One implementation covers every
provider worth supporting, and a trait with a single permanent implementation
is ceremony.

**The queue is the real provider dependency**, and that is where the seam is.
`JobSource` ([`src/source.rs`](src/source.rs)) is the whole surface:

```rust
async fn claim(&self) -> Result<Claimed>;
async fn complete(&self, job: &Job) -> Result<()>;
async fn release(&self, job: &Job, delay: Option<Duration>) -> Result<()>;
fn max_attempts(&self) -> u32;
```

Today there is one implementation, `CloudflareQueues`. Everything
Cloudflare-shaped is sealed inside it — the HTTP pull protocol, the lease
model, and the R2 event-notification payload. A `JobSource` yields an
*already-normalised* `Job`, so adding SQS or a Postgres job table means writing
one file and touching nothing else.

## Configuration

Everything has a `RUNNER_*` environment variable; `--help` documents the full
surface. Copy the template and fill it in:

```sh
make env-example     # writes .env.local, which is gitignored
make help-runner     # every flag and variable
```

Required: `RUNNER_CF_ACCOUNT_ID`, `RUNNER_QUEUE_ID`, `RUNNER_QUEUE_API_TOKEN`,
`RUNNER_BUCKET`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`.

The queue API token needs **Queues Read *and* Write** — acknowledging is a
write. A 403 on the first pull is almost always this, and the error message
says so.

### Key layout

```
{RUNNER_SOURCE_PREFIX}{id}/source.{ext}         the video to transcode
{RUNNER_SOURCE_PREFIX}{id}/captions/{lang}.vtt  optional supplied captions
{RUNNER_OUTPUT_PREFIX}{id}/…                    everything vidra produced
```

Both prefixes are configurable, defaulting to `source/` and `media/`. They may
not nest — output under the source prefix would be re-ingested as new input and
transcoded forever, so that is rejected at startup.

The shape *below* the prefix is fixed rather than templated, which is a
deliberate limit: a template language here would be a lot of machinery for a
layout nobody has needed to vary. The video ID convention is likewise fixed at
`^[a-z0-9][a-z0-9-]{2,63}$`, because it has to agree with whatever serves the
output — an ID that passes here but not there produces files nobody can fetch.

## What it does with each message

Most messages are not jobs, and filtering is strict:

- **Only create actions** — `PutObject`, `CompleteMultipartUpload`,
  `CopyObject`. A delete event must never start a transcode. All three matter:
  `aws s3 cp` switches to multipart above 8 MiB, so real videos arrive as
  `CompleteMultipartUpload` while captions arrive as `PutObject`.
- **Only `{source_prefix}{id}/source.{ext}`.** Event prefixes are coarse, so
  `meta.json` and `captions/*.vtt` arrive too — and they are uploaded *before*
  the video by design, so acting on one would start transcoding a file that is
  still uploading.
- Anything malformed or uninteresting is **acknowledged and dropped**, so it
  cannot cycle until it reaches the dead-letter queue.

## Retry semantics

Retry is not implemented here — the queue owns it. A job that is not
acknowledged comes back on its own; releasing it explicitly only shortens the
wait from the visibility timeout (two hours, by default) to 30 seconds.

| Situation | Action |
|---|---|
| Success | Acknowledge, *then* fire the webhook. A webhook failure must not undo a finished job. |
| vidra exit `1`, attempts remain | Release for another attempt. |
| vidra exit `2` (permanent) | Record a `failed` manifest and acknowledge — the remaining attempts would fail identically. |
| Final attempt | Same: record the failure, then acknowledge. |
| Cannot record the failure | Leave it unacknowledged, so it reappears or lands in the dead-letter queue rather than vanishing with no record. |

Acknowledging is the point of no return, so it happens only once the output and
the manifest are durably stored. If the acknowledgement itself fails the job is
redelivered, which is wasteful but harmless: processing is idempotent, because
the output prefix is cleared and rewritten.

## Operating it

```sh
make up          # start polling
make logs-f      # follow
make down        # SIGTERM, 600s grace to finish the batch in flight
make ps
```

Mount `/models` somewhere persistent, or every job re-downloads the Whisper
model. `/scratch` needs real space — a 1080p source can be several gigabytes,
and the runner stages both the download and the output there.

`RUNNER_CONCURRENCY` bounds jobs in flight, each running its own vidra process
with its own scratch directory. Each one wants a CPU core and its own share of
the scratch volume, so raise it with both in mind.
