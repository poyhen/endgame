# endgame

A private Telegram userbot that downloads media links with `yt-dlp` or
`gallery-dl` and uploads the resulting images and videos back to Telegram.

## Requirements

- Rust toolchain
- `yt-dlp`
- `gallery-dl`
- `ffmpeg` and `ffprobe`

The included Nix flake provides these dependencies:

```sh
nix develop
cargo run
```

## Configuration

The bot reads configuration from environment variables.

| Variable | Required | Default | Description |
| --- | --- | --- | --- |
| `API_ID` | Yes | — | Telegram API ID |
| `API_HASH` | Yes | — | Telegram API hash |
| `ALLOWED_USER_IDS` | Yes | — | Comma-separated Telegram user IDs |
| `SUPERUSERS` | No | Empty | Users allowed to update Instagram cookies |
| `DOWNLOAD_CONCURRENCY` | No | `2` | Maximum simultaneous download jobs |
| `DOWNLOAD_QUEUE_CAPACITY` | No | `20` | Maximum jobs waiting in the queue |
| `MAX_UPLOAD_SIZE_MB` | No | `1900` | Video size ceiling before adaptive transcoding |

Both queue settings and the upload limit must be positive integers.

## Usage

- Send one or more URLs in a private message. Every URL becomes a separate,
  numbered job.
- Multi-item gallery downloads are returned as Telegram albums in batches of
  at most ten items.
- Use `/cancel <job-id>` to cancel one of your queued or active jobs.
- Use `/audio <url>` for an MP3 extraction.
- Use `/video 720p <url>` to cap video quality, or `/best <url>` for the
  unrestricted default.
- Use `/status` or `/queue` for current queue usage, `/ping` to check whether
  the userbot is alive, and `/help` for the command summary (`/h` remains an
  alias for `/ping`).
- Superusers can use `/insta <cookie-content>` to replace Instagram cookies.
  The cookie message is deleted and the local file is written with owner-only
  permissions on Unix systems.

Each accepted job maintains one status message as it moves through queued,
downloading, media preparation, uploading, and a terminal state. Cancellation
is owner-scoped: users cannot cancel or inspect another user's jobs.

For a single image, video, or audio result, that status message is edited into
the final media message instead of sending a second message. For galleries, the
album is sent and the temporary status is deleted. Failures and cancellations
remain visible by editing the same status message.

Videos that are not already MP4/H.264, or that exceed `MAX_UPLOAD_SIZE_MB`, are
converted to MP4/H.264/AAC with a streaming-friendly layout. Temporary media,
thumbnails, and cancelled downloads are removed after processing.

On Ctrl-C, new work is rejected and accepted jobs are drained before the
Telegram connection closes.

## Verification

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```
