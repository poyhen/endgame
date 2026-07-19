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

Telegram API credentials come from environment variables; everything else lives
in a JSON config file. Copy `config.example.json` and fill in your user IDs:

```sh
cp config.example.json config.json
```

| Variable | Required | Default | Description |
| --- | --- | --- | --- |
| `API_ID` | Yes | — | Telegram API ID |
| `API_HASH` | Yes | — | Telegram API hash |
| `CONFIG_FILE` | No | `config.json` | Path to the JSON config file |

`superusers` and `allowed_users` are Telegram user ID lists. Superusers are
implicitly authorized and can run administrative commands. Every other key is
optional and defaults to the value shown in `config.example.json`; all numeric
settings must be positive integers. `/add` and `/remove` rewrite the
`allowed_users` array in place, so edit the file by hand only while the bot is
stopped.

## Usage

- Send one or more URLs in a private message. Every URL becomes a separate,
  numbered job.
- Multi-item gallery downloads are returned as Telegram albums in batches of
  at most ten items.
- Use `/cancel <job-id>` to cancel one of your queued or active jobs, or reply
  to its status message with `/cancel` so no job ID is needed.
- Reply to any download status with `/retry` to enqueue the same download
  again. This works regardless of whether the original job failed.
- Use `/audio <url>` for an MP3 extraction.
- Use `/video 720p <url>` to cap video quality, or `/best <url>` for the
  unrestricted default.
- Use `/clip 1:30 1:45 <url>` to download an accurately cut section. Timestamps
  accept seconds, `MM:SS`, or `HH:MM:SS`, with optional millisecond precision.
- Use `/status` or `/queue` for current queue usage, `/ping` to check whether
  the userbot is alive, and `/help` for the command summary (`/h` remains an
  alias for `/ping`).
- Superusers are implicitly authorized and can use `/add <user-id>` to authorize
  another user or `/remove <user-id>` to revoke access. Both commands update the
  `allowed_users` array in the config file, so changes take effect immediately
  and survive restarts. Superusers can only be changed by editing the config
  file. `/users` lists everyone allowed to download, including Telegram
  usernames and full names when available.
- Superusers can use `/insta <cookie-content>` to replace Instagram cookies.
  The cookie message is deleted and the local file is written with owner-only
  permissions on Unix systems.

Each accepted job maintains one status message as it moves through queued,
downloading, inspection, transcoding, thumbnail preparation, uploading,
finalization, and a terminal state. yt-dlp downloads and transcodes report
progress in five-percent steps. Cancellation
is owner-scoped: users cannot cancel or inspect another user's jobs.
While primary media is uploading, the status includes the current item's
percentage and estimated bytes remaining. Progress edits are throttled to avoid
excessive Telegram requests; thumbnail uploads are not included.

Status edits and other Telegram delivery operations have bounded waits, while
failed status edits are retried without blocking the media worker. `/status`
shows each caller's active job phase and how long it has been in that phase.

For a single image, video, or audio result, that status message is edited into
the final media message instead of sending a second message. For galleries, the
album is sent and the temporary status is deleted. Failures and cancellations
remain visible by editing the same status message.

Videos that are not already MP4/H.264, or that exceed `MAX_UPLOAD_SIZE_MB`, are
converted to MP4/H.264/AAC with a streaming-friendly layout. Temporary media,
thumbnails, and cancelled downloads are removed after processing.
For YouTube, the downloader first prefers an H.264 MP4 format estimated to fit
under the upload ceiling, which avoids needless multi-hour transcodes when a
slightly lower compatible format is available. Playlist expansion is disabled;
each submitted URL remains one job.

External commands, uploads, and whole jobs are watched by configurable
deadlines. Logs include job phase transitions plus command PID and elapsed time,
and a panicking worker is converted into a visible failed job.

On Ctrl-C, new work is rejected and accepted jobs are drained before the
Telegram connection closes.

## Verification

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```

## Code layout

- `main.rs` is the composition root and live-update dispatcher.
- `commands.rs`, `cookies.rs`, and `telegram/auth.rs` isolate Telegram-facing input concerns.
- `jobs/queue.rs` schedules work, `jobs/progress.rs` coordinates progress state, and
  `jobs/status.rs` renders user-facing status text.
- `media/pipeline.rs` orchestrates jobs. Subprocesses, downloader policy, delivery,
  inspection, progress parsing, transcoding, request types, and temporary workspaces
  live in separate modules under `media/`.

Every job downloads and generates derived media inside a unique temporary workspace.
The workspace is explicitly removed asynchronously when the job finishes.
