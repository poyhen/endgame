use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;

use crate::cancel::CancellationToken;
use crate::jobs::JobProgress;
use crate::media::command::{self, CommandOutcome};
use crate::media::inspect::probe_video;
use crate::media::progress::read_transcode;
use crate::media::request::DownloadLimits;
use crate::media::workspace::random_file_in;

const UTILITY_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) struct PreparedVideo {
    path: PathBuf,
}

impl PreparedVideo {
    fn original(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
        }
    }

    fn temporary(path: PathBuf) -> Self {
        Self { path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

pub(crate) async fn prepare_video(
    path: &Path,
    cancellation: &CancellationToken,
    progress: &JobProgress,
    limits: DownloadLimits,
) -> anyhow::Result<PreparedVideo> {
    progress.inspecting();
    let metadata = probe_video(path, cancellation, UTILITY_COMMAND_TIMEOUT).await?;
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
        return Ok(PreparedVideo::original(path));
    }
    if cancellation.is_cancelled() {
        anyhow::bail!("cancelled");
    }

    progress.transcoding(None);
    let output = random_file_in(path.parent().unwrap_or_else(|| Path::new(".")), ".mp4");
    let prepared = PreparedVideo::temporary(output);
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
        .arg(prepared.path());

    let operation = format!("Job #{}", progress.id());
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

    let output_size = tokio::fs::metadata(prepared.path()).await?.len();
    if output_size > limits.max_upload_bytes {
        anyhow::bail!(
            "prepared video is still too large ({} MiB; limit {} MiB)",
            output_size / (1024 * 1024),
            limits.max_upload_bytes / (1024 * 1024)
        );
    }
    Ok(prepared)
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
