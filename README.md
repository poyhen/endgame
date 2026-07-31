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
implicitly authorized and can run administrative commands. On the first start
with an application database, both lists are imported into Turso exactly once;
subsequent user changes are stored in the database. Existing allowed users enter
`default_package`, while superusers receive `superuser_package` as their stored
regular-account package. Changing `allowed_users` after that import has no
effect, so use the Telegram administration commands. Newly configured
superusers are still added on startup. This is an embedded, local database and
does not use Turso Cloud.

`database.path` defaults to `endgame.db`. Relative paths are resolved beside the
selected config file. `packages` can contain any number of named packages, such
as `free`, `pro`, or `enterprise`. Set `default_package` for users added with
`/add` and `superuser_package` for bootstrapped superuser records. Package names
are case-insensitive, must be 1–32 ASCII letters, numbers, `_`, or `-`, and must
start with a letter or number.

Each package can define `max_active_jobs`, `max_queued_jobs`,
`daily_job_limit`, and `max_upload_size_mb`. A `null` or omitted value inherits
the corresponding global capacity, except `daily_job_limit`, where it means
unlimited. Per-user overrides are stored separately and continue to apply if the
user changes packages. All configured numeric limits must be positive;
active-job, queued-job, and upload limits cannot exceed their matching global
safety caps. The old `user_tiers.free`/`user_tiers.paid` shape is accepted for
backward compatibility, but it cannot be combined with `packages`.

On Unix, the database and WAL sidecar are kept owner-only. The application
database persists users, package assignments, overrides, UTC daily usage, and
download outcome statistics. Existing version-1 databases are migrated
automatically from `free`/`paid` tiers to packages. Live download jobs remain in
memory.

## Usage

- Send one or more URLs in a private message. Every URL becomes a separate,
  numbered job.
- Multi-item gallery downloads are returned as Telegram albums in batches of
  at most ten items.
- Use `/cancel <job-id>` to cancel one of your queued or active jobs, or reply
  to its status message with `/cancel` so no job ID is needed.
- Reply to any download status with `/retry` to enqueue the same download
  again. This works regardless of whether the original job failed.
- Use `/audio <url>` for an MP3 extraction. When the source provides it, the
  result includes a clean track name, artist, duration, and embedded cover art;
  Telegram also receives the artwork as the music thumbnail.
- Use `/video 720p <url>` to cap video quality, or `/best <url>` for the
  unrestricted default.
- Use `/clip 1:30 1:45 <url>` to download an accurately cut section. Timestamps
  accept seconds, `MM:SS`, or `HH:MM:SS`, with optional millisecond precision.
- Use `/status` or `/queue` for current queue usage, `/ping` to check whether
  the userbot is alive, and `/help` for the command summary (`/h` remains an
  alias for `/ping`).
- `/limits` shows the caller's effective active-job, queue, daily, and upload
  limits. `/packages` lists all configured packages and their effective limits.
- `/stats` shows the caller's persistent accepted, completed, failed, cancelled,
  interrupted, open, and current UTC-day download counts, plus a success rate.
- Superusers are implicitly authorized and can use `/add <user-id>` to create or
  re-enable a user with `default_package`, and `/remove <user-id>` to disable
  one. Disabled users keep their package, overrides, and statistics.
  `/package <user-id> <name>` changes a package (`/tier` remains an alias);
  `/limit <user-id> <active|queued|daily|upload_mb> <value|default>` manages an
  individual override; and `/limits <user-id>` inspects the result. `/users`
  lists stored users, packages, disabled state, Telegram usernames, and full
  names. `/stats <user-id>` inspects one user's statistics and `/stats all`
  shows application totals.
- Superuser privileges can only be changed in the config file. While configured
  as a superuser, an account uses the unrestricted policy subject to global
  safety caps. Its stored package becomes relevant if that privilege is later
  removed; use `/remove` separately if the regular account should also be
  disabled.
- Superusers can use `/insta <cookie-content>` to replace Instagram cookies.
  The cookie message is deleted and the local file is written with owner-only
  permissions on Unix systems.

Each accepted job maintains one status message as it moves through queued,
downloading, inspection, transcoding, thumbnail preparation, uploading,
finalization, and a terminal state. yt-dlp downloads and transcodes report
progress in five-percent steps. Accepted jobs are counted when admitted to the
queue, and completion, failure, and cancellation outcomes are recorded when the
queue reaches a terminal state. If the process stops with unfinished jobs, they
are reconciled as interrupted on the next startup. Cancellation is owner-scoped:
users cannot cancel or inspect another user's jobs.
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

Videos that are not already MP4/H.264, or that exceed the caller's effective
upload limit, are converted to MP4/H.264/AAC with a streaming-friendly layout.
Temporary media, thumbnails, and cancelled downloads are removed after
processing.
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
- `store.rs` owns the separate Turso application database, schema migration,
  users, overrides, daily usage reservations, and download statistics;
  `policy.rs` validates package names and resolves effective package limits.
- `commands.rs`, `users.rs`, `cookies.rs`, and `telegram/auth.rs` isolate
  Telegram-facing input concerns.
- `jobs/queue.rs` schedules work, `jobs/progress.rs` coordinates progress state, and
  `jobs/status.rs` renders user-facing status text.
- `media/pipeline.rs` orchestrates jobs. Subprocesses, downloader policy, delivery,
  inspection, progress parsing, transcoding, request types, and temporary workspaces
  live in separate modules under `media/`.

Every job downloads and generates derived media inside a unique temporary workspace.
The workspace is explicitly removed asynchronously when the job finishes.
