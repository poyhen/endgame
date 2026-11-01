use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::cancel::CancellationToken;
use crate::jobs::JobProgress;
use crate::media::command::{self, CommandOutcome};
use crate::media::inspect::{VideoMetadata, probe_video};
use crate::media::progress::read_transcode;
use crate::media::request::DownloadLimits;
use crate::media::workspace::random_file_in;

const UTILITY_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

/// Telegram's photo API rejects formats such as WebP even after the file upload
/// succeeds. Convert the bytes as well as the filename before sending a photo.
pub(crate) async fn prepare_photo(
    path: &Path,
    cancellation: &CancellationToken,
    timeout: Duration,
    operation: &str,
) -> anyhow::Result<PathBuf> {
    if cancellation.is_cancelled() {
        anyhow::bail!("cancelled");
    }
    let mut signature = [0u8; 12];
    tokio::fs::File::open(path)
        .await?
        .read_exact(&mut signature)
        .await
        .context("could not inspect photo")?;
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if (matches!(extension.as_str(), "jpg" | "jpeg") && signature.starts_with(&[0xff, 0xd8, 0xff]))
        || (extension == "png" && signature.starts_with(b"\x89PNG\r\n\x1a\n"))
    {
        return Ok(path.to_path_buf());
    }

    let output = random_file_in(path.parent().unwrap_or_else(|| Path::new(".")), ".jpg");
    let mut command = Command::new("ffmpeg");
    command
        .arg("-y")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-i")
        .arg(path)
        .arg("-map")
        .arg("0:v:0")
        .arg("-frames:v")
        .arg("1")
        .arg("-c:v")
        .arg("mjpeg")
        .arg("-q:v")
        .arg("2")
        .arg("-pix_fmt")
        .arg("yuvj420p")
        .arg("-update")
        .arg("1")
        .arg(&output);
    let outcome = command::run(
        command,
        cancellation,
        timeout,
        operation,
        |mut stdout| async move { command::read_bounded(&mut stdout).await },
    )
    .await;
    let result = match outcome {
        CommandOutcome::Finished { success: true, .. } => {
            match tokio::fs::metadata(&output).await {
                Ok(metadata) if metadata.len() > 0 => Ok(output.clone()),
                _ => Err(anyhow::anyhow!("ffmpeg produced no photo")),
            }
        }
        CommandOutcome::Finished { stderr, .. } => Err(anyhow::anyhow!(
            "ffmpeg could not prepare the photo: {stderr}"
        )),
        CommandOutcome::Cancelled => Err(anyhow::anyhow!("cancelled")),
        CommandOutcome::TimedOut => Err(anyhow::anyhow!(
            "photo conversion timed out after {} seconds",
            timeout.as_secs()
        )),
    };
    if result.is_err() {
        let _ = tokio::fs::remove_file(output).await;
    }
    result
}

pub(crate) struct PreparedVideo {
    path: PathBuf,
    metadata: VideoMetadata,
}

impl PreparedVideo {
    fn original(path: &Path, metadata: VideoMetadata) -> Self {
        Self {
            path: path.to_path_buf(),
            metadata,
        }
    }

    fn temporary(path: PathBuf, metadata: VideoMetadata) -> Self {
        Self { path, metadata }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn metadata(&self) -> &VideoMetadata {
        &self.metadata
    }
}

pub(crate) async fn prepare_video(
    path: &Path,
    cancellation: &CancellationToken,
    progress: &JobProgress,
    limits: DownloadLimits,
) -> anyhow::Result<PreparedVideo> {
    let operation = format!("Job #{}", progress.id());
    progress.inspecting();
    let metadata = probe_video(path, cancellation, UTILITY_COMMAND_TIMEOUT, &operation).await?;
    let file_size = tokio::fs::metadata(path).await?.len();
    let container_is_mp4 = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("mp4"));
    if !requires_video_transcode(
        container_is_mp4,
        &metadata.codec,
        file_size,
        limits.max_upload_bytes,
    ) {
        return Ok(PreparedVideo::original(path, metadata));
    }
    if cancellation.is_cancelled() {
        anyhow::bail!("cancelled");
    }

    progress.transcoding(None);
    let output = random_file_in(path.parent().unwrap_or_else(|| Path::new(".")), ".mp4");
    let mut command = Command::new("ffmpeg");
    command
        .arg("-y")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-i")
        .arg(path)
        .arg("-map")
        .arg("0:v:0")
        .arg("-map")
        .arg("0:a:0?")
        .arg("-sn")
        .arg("-dn")
        .arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("medium")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-vf")
        .arg("scale=min(1920\\,iw):min(1080\\,ih):force_original_aspect_ratio=decrease:force_divisible_by=2,setsar=1")
        .arg("-c:a")
        .arg("aac")
        .arg("-b:a")
        .arg("128k")
        .arg("-ac")
        .arg("2");

    if file_size > limits.max_upload_bytes {
        let bitrate = target_video_bitrate(limits.max_upload_bytes, metadata.duration_secs)?;
        command
            .arg("-b:v")
            .arg(bitrate.to_string())
            .arg("-maxrate")
            .arg(bitrate.to_string())
            .arg("-bufsize")
            .arg(bitrate.saturating_mul(2).to_string());
    } else {
        command.arg("-crf").arg("23");
    }
    command
        .arg("-progress")
        .arg("pipe:1")
        .arg("-nostats")
        .arg("-movflags")
        .arg("+faststart")
        .arg("-tag:v")
        .arg("avc1")
        .arg(&output);

    match command::run(command, cancellation, limits.command_timeout, &operation, {
        let progress = progress.clone();
        let duration_micros = metadata.duration_secs.max(0) as u64 * 1_000_000;
        move |stdout| read_transcode(stdout, progress, duration_micros)
    })
    .await
    {
        CommandOutcome::Cancelled => anyhow::bail!("cancelled"),
        CommandOutcome::TimedOut => anyhow::bail!(
            "ffmpeg timed out after {} seconds",
            limits.command_timeout.as_secs()
        ),
        CommandOutcome::Finished { success: true, .. } => progress.transcoding(Some(100)),
        CommandOutcome::Finished { stderr, .. } => {
            anyhow::bail!("ffmpeg could not prepare the video: {stderr}")
        }
    }

    let output_size = tokio::fs::metadata(&output).await?.len();
    if output_size > limits.max_upload_bytes {
        anyhow::bail!(
            "prepared video is still too large ({} MiB; limit {} MiB)",
            output_size / (1024 * 1024),
            limits.max_upload_bytes / (1024 * 1024)
        );
    }
    progress.inspecting();
    let metadata = probe_video(&output, cancellation, UTILITY_COMMAND_TIMEOUT, &operation).await?;
    Ok(PreparedVideo::temporary(output, metadata))
}

fn requires_video_transcode(
    container_is_mp4: bool,
    codec: &str,
    file_size: u64,
    max_upload_bytes: u64,
) -> bool {
    !container_is_mp4 || codec != "h264" || file_size > max_upload_bytes
}

fn target_video_bitrate(max_upload_bytes: u64, duration_secs: i64) -> anyhow::Result<u64> {
    if duration_secs <= 0 {
        anyhow::bail!("cannot resize a video with unknown duration");
    }
    let target_bits_per_second = (max_upload_bytes as u128)
        .saturating_mul(8)
        .saturating_mul(85)
        / 100
        / duration_secs as u128;
    let video_bits_per_second = target_bits_per_second.saturating_sub(128_000);
    if video_bits_per_second < 200_000 {
        anyhow::bail!("the configured upload limit is too small for this video's duration");
    }
    Ok(video_bits_per_second.min(u64::MAX as u128) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::workspace::JobWorkspace;

    // A lossless 4x2 WebP image. Its real encoding must change, not just its suffix.
    const WEBP: &[u8] = &[
        82, 73, 70, 70, 28, 0, 0, 0, 87, 69, 66, 80, 86, 80, 56, 76, 15, 0, 0, 0, 47, 3, 64, 0, 0,
        7, 16, 245, 143, 254, 7, 34, 162, 255, 1, 0,
    ];

    #[tokio::test]
    async fn converts_webp_photos_including_misleading_jpeg_filenames() {
        let workspace = JobWorkspace::create(1).await.unwrap();
        let cancellation = CancellationToken::new();
        for filename in ["photo.webp", "mislabeled.jpg"] {
            let original = workspace.path().join(filename);
            tokio::fs::write(&original, WEBP).await.unwrap();
            let prepared = prepare_photo(
                &original,
                &cancellation,
                Duration::from_secs(10),
                "Photo conversion test",
            )
            .await
            .unwrap();
            assert_ne!(prepared, original);
            assert_eq!(prepared.extension().unwrap(), "jpg");
            assert!(
                tokio::fs::read(&prepared)
                    .await
                    .unwrap()
                    .starts_with(&[0xff, 0xd8, 0xff])
            );
            let metadata = probe_video(
                &prepared,
                &cancellation,
                Duration::from_secs(10),
                "Prepared photo test",
            )
            .await
            .unwrap();
            assert_eq!(metadata.codec, "mjpeg");
            assert_eq!((metadata.width, metadata.height), (4, 2));
            assert_eq!(tokio::fs::read(&original).await.unwrap(), WEBP);
            // Already compatible JPEGs are kept without another lossy conversion.
            assert_eq!(
                prepare_photo(
                    &prepared,
                    &cancellation,
                    Duration::from_secs(10),
                    "JPEG test"
                )
                .await
                .unwrap(),
                prepared
            );
        }
        workspace.cleanup().await;
    }

    #[tokio::test]
    async fn rejects_corrupt_photos_and_removes_failed_conversion_outputs() {
        let workspace = JobWorkspace::create(2).await.unwrap();
        let original = workspace.path().join("corrupt.webp");
        tokio::fs::write(&original, b"This is not an image.")
            .await
            .unwrap();
        assert!(
            prepare_photo(
                &original,
                &CancellationToken::new(),
                Duration::from_secs(10),
                "Invalid photo test",
            )
            .await
            .is_err()
        );
        let mut entries = tokio::fs::read_dir(workspace.path()).await.unwrap();
        assert_eq!(
            entries.next_entry().await.unwrap().unwrap().path(),
            original
        );
        assert!(entries.next_entry().await.unwrap().is_none());
        workspace.cleanup().await;
    }

    #[tokio::test]
    async fn cancelled_photo_preparation_does_not_start_work() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = prepare_photo(
            Path::new("missing.webp"),
            &cancellation,
            Duration::from_secs(10),
            "Cancelled photo test",
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "cancelled");
    }

    #[test]
    fn transcodes_incompatible_or_oversized_video() {
        assert!(!requires_video_transcode(true, "h264", 99, 100));
        assert!(requires_video_transcode(false, "h264", 99, 100));
        assert!(requires_video_transcode(true, "vp9", 99, 100));
        assert!(requires_video_transcode(true, "h264", 101, 100));
    }

    #[test]
    fn bitrate_math_keeps_headroom_and_rejects_impossible_limits() {
        assert_eq!(target_video_bitrate(100_000_000, 100).unwrap(), 6_672_000);
        assert!(target_video_bitrate(100_000, 3_600).is_err());
        assert!(target_video_bitrate(100_000_000, 0).is_err());
    }
}
