use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;

use crate::cancel::CancellationToken;
use crate::media::command::{self, CommandOutcome};
use crate::media::workspace::random_file_in;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioMetadata {
    pub duration: Duration,
    pub title: Option<String>,
    pub performer: Option<String>,
}

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

pub async fn extract_audio_thumbnail(
    audio_file: &Path,
    cancellation: &CancellationToken,
    timeout: Duration,
    operation: &str,
) -> anyhow::Result<PathBuf> {
    let parent = audio_file.parent().unwrap_or_else(|| Path::new("."));
    let thumbnail = random_file_in(parent, ".jpg");
    let mut command = Command::new("ffmpeg");
    command
        .arg("-y")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-i")
        .arg(audio_file)
        .arg("-map")
        .arg("0:v:0")
        .arg("-an")
        .arg("-frames:v")
        .arg("1")
        .arg("-vf")
        .arg("scale=320:320:force_original_aspect_ratio=increase,crop=320:320")
        .arg("-q:v")
        .arg("8")
        .arg(&thumbnail);
    let (success, _, stderr) = run(command, cancellation, timeout, operation).await?;
    if !success {
        let _ = tokio::fs::remove_file(&thumbnail).await;
        anyhow::bail!("failed to extract audio cover: {stderr}");
    }
    match tokio::fs::metadata(&thumbnail).await {
        Ok(metadata) if metadata.len() > 0 => Ok(thumbnail),
        _ => {
            let _ = tokio::fs::remove_file(&thumbnail).await;
            anyhow::bail!("audio does not contain usable cover art");
        }
    }
}

pub async fn probe_audio(
    audio_file: &Path,
    cancellation: &CancellationToken,
    timeout: Duration,
    operation: &str,
) -> anyhow::Result<AudioMetadata> {
    let mut command = Command::new("ffprobe");
    command
        .arg("-v")
        .arg("error")
        .arg("-show_entries")
        .arg("format=duration:format_tags=title,artist,album_artist,performer")
        .arg("-of")
        .arg("json")
        .arg(audio_file);
    let (success, stdout, stderr) = run(command, cancellation, timeout, operation).await?;
    if !success {
        anyhow::bail!("ffprobe could not read audio: {stderr}");
    }
    parse_audio_probe(&stdout)
}

fn parse_audio_probe(output: &str) -> anyhow::Result<AudioMetadata> {
    let output: serde_json::Value = serde_json::from_str(output)?;
    let format = output
        .get("format")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("ffprobe returned no audio format information"))?;
    let duration_secs = format
        .get("duration")
        .and_then(|duration| {
            duration
                .as_str()
                .and_then(|duration| duration.parse::<f64>().ok())
                .or_else(|| duration.as_f64())
        })
        .filter(|duration| duration.is_finite() && *duration > 0.0)
        .unwrap_or(0.0)
        .min(i32::MAX as f64)
        .floor() as u64;
    let tags = format.get("tags").and_then(serde_json::Value::as_object);

    Ok(AudioMetadata {
        duration: Duration::from_secs(duration_secs),
        title: audio_tag(tags, &["title"]),
        performer: audio_tag(tags, &["artist", "album_artist", "performer"]),
    })
}

fn audio_tag(
    tags: Option<&serde_json::Map<String, serde_json::Value>>,
    names: &[&str],
) -> Option<String> {
    let tags = tags?;
    names.iter().find_map(|name| {
        tags.iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .and_then(|(_, value)| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_audio_duration_and_case_insensitive_tags() {
        let metadata = parse_audio_probe(
            r#"{
                "format": {
                    "duration": "187.923000",
                    "tags": {
                        "TITLE": "Good Morning",
                        "ARTIST": "Kanye West"
                    }
                }
            }"#,
        )
        .unwrap();

        assert_eq!(
            metadata,
            AudioMetadata {
                duration: Duration::from_secs(187),
                title: Some("Good Morning".into()),
                performer: Some("Kanye West".into()),
            }
        );
    }

    #[test]
    fn audio_probe_tolerates_missing_optional_metadata() {
        let metadata = parse_audio_probe(r#"{"format": {}}"#).unwrap();

        assert_eq!(
            metadata,
            AudioMetadata {
                duration: Duration::ZERO,
                title: None,
                performer: None,
            }
        );
    }
}
