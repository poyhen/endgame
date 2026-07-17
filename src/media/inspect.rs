use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;

use crate::cancel::CancellationToken;
use crate::media::command::{self, CommandOutcome};
use crate::media::workspace::random_file_in;

pub struct VideoMetadata {
    pub duration_secs: i64,
    pub width: i32,
    pub height: i32,
    pub codec: String,
}

pub async fn extract_thumbnail(
    video_file: &Path,
    cancellation: &CancellationToken,
    timeout: Duration,
    operation: &str,
) -> anyhow::Result<PathBuf> {
    let parent = video_file.parent().unwrap_or_else(|| Path::new("."));
    let thumbnail = random_file_in(parent, ".jpg");
    let mut command = Command::new("ffmpeg");
    command
        .arg("-y")
        .arg("-i")
        .arg(video_file)
        .arg("-ss")
        .arg("00:00:01.000")
        .arg("-vframes")
        .arg("1")
        .arg("-vf")
        .arg("scale=320:-1")
        .arg(&thumbnail);
    let (success, _, stderr) = run(command, cancellation, timeout, operation).await?;
    if !success {
        let _ = tokio::fs::remove_file(&thumbnail).await;
        anyhow::bail!("failed to extract thumbnail: {stderr}");
    }
    Ok(thumbnail)
}

/// Probe the metadata Telegram needs. Missing individual fields default to zero,
/// but an ffprobe rejection remains a hard error so HTML or corrupt files are
/// never uploaded as videos.
pub async fn probe_video(
    video_file: &Path,
    cancellation: &CancellationToken,
    timeout: Duration,
    operation: &str,
) -> anyhow::Result<VideoMetadata> {
    let mut command = Command::new("ffprobe");
    command
        .arg("-v")
        .arg("error")
        .arg("-select_streams")
        .arg("v:0")
        .arg("-show_entries")
        .arg("stream=width,height,codec_name:format=duration")
        .arg("-of")
        .arg("default=noprint_wrappers=1")
        .arg(video_file);
    let (success, stdout, stderr) = run(command, cancellation, timeout, operation).await?;
    if !success {
        anyhow::bail!("ffprobe could not read video: {stderr}");
    }

    let mut metadata = VideoMetadata {
        duration_secs: 0,
        width: 0,
        height: 0,
        codec: String::new(),
    };
    for line in stdout.lines() {
        if let Some(value) = line.trim().strip_prefix("width=") {
            metadata.width = value.parse().unwrap_or(0);
        } else if let Some(value) = line.trim().strip_prefix("height=") {
            metadata.height = value.parse().unwrap_or(0);
        } else if let Some(value) = line.trim().strip_prefix("duration=") {
            metadata.duration_secs = value.parse::<f64>().map(|v| v as i64).unwrap_or(0);
        } else if let Some(value) = line.trim().strip_prefix("codec_name=") {
            metadata.codec = value.to_string();
        }
    }
    Ok(metadata)
}

async fn run(
    command_to_run: Command,
    cancellation: &CancellationToken,
    timeout: Duration,
    operation: &str,
) -> anyhow::Result<(bool, String, String)> {
    match command::run(
        command_to_run,
        cancellation,
        timeout,
        operation,
        |mut stdout| async move { command::read_bounded(&mut stdout).await },
    )
    .await
    {
        CommandOutcome::Finished {
            success,
            stdout,
            stderr,
        } => Ok((success, stdout, stderr)),
        CommandOutcome::Cancelled => anyhow::bail!("cancelled"),
        CommandOutcome::TimedOut => {
            anyhow::bail!("command timed out after {} seconds", timeout.as_secs())
        }
    }
}
