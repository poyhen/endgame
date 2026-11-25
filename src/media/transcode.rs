use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::cancel::CancellationToken;
use crate::jobs::JobProgress;
use crate::media::command::{self, CommandOutcome};
use crate::media::inspect::{VideoMetadata, probe_audio_duration, probe_video};
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

/// Make a Telegram-compatible video that displays one photo for the complete
/// soundtrack. Explicitly cap the duration: ffmpeg's `-shortest` can leave a
/// long silent tail when a looped image has a low frame rate.
pub(crate) async fn photo_with_audio(
    photo: &Path,
    audio: &Path,
    cancellation: &CancellationToken,
    progress: Option<&JobProgress>,
    max_height: Option<u32>,
    timeout: Duration,
    operation: &str,
) -> anyhow::Result<PathBuf> {
    if let Some(progress) = progress {
        progress.inspecting();
    }
    let duration =
        probe_audio_duration(audio, cancellation, UTILITY_COMMAND_TIMEOUT, operation).await?;
    let photo = prepare_photo(photo, cancellation, timeout, operation).await?;
    let max_height = max_height.unwrap_or(1920).min(1920);
    if max_height < 2 {
        anyhow::bail!("music video height must be at least 2 pixels");
    }
    let scale = format!(
        "scale=min(1920\\,iw):min({max_height}\\,ih):force_original_aspect_ratio=decrease:force_divisible_by=2,setsar=1"
    );
    let output = random_file_in(photo.parent().unwrap_or_else(|| Path::new(".")), ".mp4");
    let mut command = Command::new("ffmpeg");
    command
        .arg("-y")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-loop")
        .arg("1")
        .arg("-framerate")
        .arg("5")
        .arg("-i")
        .arg(photo)
        .arg("-i")
        .arg(audio);
    configure_music_video_output(&mut command, &output, duration, &scale);
    run_music_video(
        command,
        output,
        duration,
        cancellation,
        progress,
        timeout,
        operation,
    )
    .await
}

/// Show the photos in order for equal portions of one continuous soundtrack.
pub(crate) async fn photos_with_audio(
    photos: &[PathBuf],
    audio: &Path,
    cancellation: &CancellationToken,
    progress: Option<&JobProgress>,
    max_height: Option<u32>,
    timeout: Duration,
    operation: &str,
) -> anyhow::Result<PathBuf> {
    let Some(first) = photos.first() else {
        anyhow::bail!("music video has no photos");
    };
    if photos.len() == 1 {
        return photo_with_audio(
            first,
            audio,
            cancellation,
            progress,
            max_height,
            timeout,
            operation,
        )
        .await;
    }
    if let Some(progress) = progress {
        progress.inspecting();
    }
    let duration =
        probe_audio_duration(audio, cancellation, UTILITY_COMMAND_TIMEOUT, operation).await?;
    // Keep at least two video frames per photo, including for short soundtracks.
    let fps = (photos.len() as f64 * 2.0 / duration).ceil().max(5.0);
    if fps > 60.0 {
        anyhow::bail!("the soundtrack is too short to show every photo");
    }
    let first = prepare_photo(first, cancellation, timeout, operation).await?;
    let metadata = probe_video(&first, cancellation, UTILITY_COMMAND_TIMEOUT, operation).await?;
    let (width, height) = slideshow_dimensions(metadata.width, metadata.height, max_height)?;
    let parent = first.parent().unwrap_or_else(|| Path::new("."));
    let directory = random_file_in(parent, "-slideshow");
    tokio::fs::create_dir(&directory).await?;
    let result = async {
        let mut manifest = String::from("ffconcat version 1.0\n");
        let per_photo = duration / photos.len() as f64;
        for (index, photo) in photos.iter().enumerate() {
            let prepared = if index == 0 {
                first.clone()
            } else {
                prepare_photo(photo, cancellation, timeout, operation).await?
            };
            let name = format!("slide-{index:04}.jpg");
            normalize_slideshow_photo(
                &prepared,
                &directory.join(&name),
                (width, height),
                cancellation,
                timeout,
                operation,
            )
            .await?;
            manifest.push_str(&format!(
                "file {name}\noption framerate 1000\nduration {per_photo:.9}\n"
            ));
        }
        // The final duplicate supplies a timestamp at the end of the last slide.
        manifest.push_str(&format!(
            "file slide-{:04}.jpg\noption framerate 1000\n",
            photos.len() - 1
        ));
        let manifest_path = directory.join("slides.ffconcat");
        tokio::fs::write(&manifest_path, manifest).await?;
        let output = random_file_in(parent, ".mp4");
        let mut command = Command::new("ffmpeg");
        command
            .arg("-y")
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-f")
            .arg("concat")
            // The manifest contains only generated relative filenames and
            // per-file framerate options; no user-supplied paths are included.
            .arg("-safe")
            .arg("0")
            .arg("-i")
            .arg(manifest_path)
            .arg("-i")
            .arg(audio);
        configure_music_video_output(&mut command, &output, duration, &format!("fps={fps:.0}"));
        run_music_video(
            command,
            output,
            duration,
            cancellation,
            progress,
            timeout,
            operation,
        )
        .await
    }
    .await;
    let _ = tokio::fs::remove_dir_all(directory).await;
    result
}

fn slideshow_dimensions(
    width: i32,
    height: i32,
    max_height: Option<u32>,
) -> anyhow::Result<(u32, u32)> {
    let max_height = max_height.unwrap_or(1920).min(1920);
    if width <= 0 || height <= 0 || max_height < 2 {
        anyhow::bail!("music video needs valid photo dimensions and a height of at least 2 pixels");
    }
    let width = f64::from(width);
    let height = f64::from(height);
    let scale = 1.0_f64
        .min(1920.0 / width)
        .min(f64::from(max_height) / height);
    let even = |size: f64| ((size as u32) / 2 * 2).max(2);
    Ok((even(width * scale), even(height * scale)))
}

async fn normalize_slideshow_photo(
    photo: &Path,
    output: &Path,
    (width, height): (u32, u32),
    cancellation: &CancellationToken,
    timeout: Duration,
    operation: &str,
) -> anyhow::Result<()> {
    let filter = format!(
        "scale={width}:{height}:force_original_aspect_ratio=decrease:force_divisible_by=2,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2:black,setsar=1"
    );
    let mut command = Command::new("ffmpeg");
    command
        .arg("-y")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-i")
        .arg(photo)
        .arg("-vf")
        .arg(filter)
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
        .arg(output);
    match command::run(
        command,
        cancellation,
        timeout,
        operation,
        |mut stdout| async move { command::read_bounded(&mut stdout).await },
    )
    .await
    {
        CommandOutcome::Finished { success: true, .. } => Ok(()),
        CommandOutcome::Finished { stderr, .. } => {
            anyhow::bail!("ffmpeg could not prepare a slideshow photo: {stderr}")
        }
        CommandOutcome::Cancelled => anyhow::bail!("cancelled"),
        CommandOutcome::TimedOut => anyhow::bail!("slideshow photo preparation timed out"),
    }
}

fn configure_music_video_output(
    command: &mut Command,
    output: &Path,
    duration: f64,
    video_filter: &str,
) {
    command
        .arg("-t")
        .arg(format!("{duration:.6}"))
        .arg("-map")
        .arg("0:v:0")
        .arg("-map")
        .arg("1:a:0")
        .arg("-vf")
        .arg(video_filter)
        .arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("veryfast")
        .arg("-tune")
        .arg("stillimage")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-c:a")
        .arg("aac")
        .arg("-b:a")
        .arg("128k")
        .arg("-ac")
        .arg("2")
        .arg("-progress")
        .arg("pipe:1")
        .arg("-nostats")
        .arg("-movflags")
        .arg("+faststart")
        .arg(output);
}

async fn run_music_video(
    command: Command,
    output: PathBuf,
    duration: f64,
    cancellation: &CancellationToken,
    progress: Option<&JobProgress>,
    timeout: Duration,
    operation: &str,
) -> anyhow::Result<PathBuf> {
    if let Some(progress) = progress {
        progress.transcoding(None);
    }
    let duration_micros = (duration * 1_000_000.0).min(u64::MAX as f64) as u64;
    let progress_reader = progress.cloned();
    let outcome = command::run(
        command,
        cancellation,
        timeout,
        operation,
        move |mut stdout| async move {
            match progress_reader {
                Some(progress) => read_transcode(stdout, progress, duration_micros).await,
                None => command::read_bounded(&mut stdout).await,
            }
        },
    )
    .await;
    let result = match outcome {
        CommandOutcome::Finished { success: true, .. } => {
            match tokio::fs::metadata(&output).await {
                Ok(metadata) if metadata.len() > 0 => Ok(output.clone()),
                _ => Err(anyhow::anyhow!("ffmpeg produced no music video")),
            }
        }
        CommandOutcome::Finished { stderr, .. } => Err(anyhow::anyhow!(
            "ffmpeg could not combine the photo and music: {stderr}"
        )),
        CommandOutcome::Cancelled => Err(anyhow::anyhow!("cancelled")),
        CommandOutcome::TimedOut => Err(anyhow::anyhow!(
            "music video conversion timed out after {} seconds",
            timeout.as_secs()
        )),
    };
    if result.is_err() {
        let _ = tokio::fs::remove_file(&output).await;
    } else if let Some(progress) = progress {
        progress.transcoding(Some(100));
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

    async fn sample_color(video: &Path, timestamp: f64) -> [u8; 3] {
        let frame = Command::new("ffmpeg")
            .args(["-v", "error", "-ss", &format!("{timestamp}"), "-i"])
            .arg(video)
            .args([
                "-frames:v",
                "1",
                "-vf",
                "scale=1:1",
                "-pix_fmt",
                "rgb24",
                "-f",
                "rawvideo",
                "pipe:1",
            ])
            .output()
            .await
            .unwrap();
        assert!(frame.status.success());
        frame.stdout.try_into().expect("expected one RGB pixel")
    }

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

    #[tokio::test]
    async fn combines_a_photo_with_an_audio_only_mp4_for_the_full_duration() {
        let workspace = JobWorkspace::create(3).await.unwrap();
        let photo = workspace.path().join("photo.webp");
        let audio = workspace.path().join("soundtrack.mp4");
        tokio::fs::write(&photo, WEBP).await.unwrap();
        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2.3",
                "-c:a",
                "aac",
            ])
            .arg(&audio)
            .status()
            .await
            .unwrap();
        assert!(status.success());

        let output = photo_with_audio(
            &photo,
            &audio,
            &CancellationToken::new(),
            None,
            None,
            Duration::from_secs(30),
            "Photo with music test",
        )
        .await
        .unwrap();
        let probe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_name,codec_type:format=duration",
                "-of",
                "json",
            ])
            .arg(&output)
            .output()
            .await
            .unwrap();
        assert!(probe.status.success());
        let info: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
        let streams = info["streams"].as_array().unwrap();
        assert!(
            streams
                .iter()
                .any(|s| s["codec_type"] == "video" && s["codec_name"] == "h264")
        );
        assert!(
            streams
                .iter()
                .any(|s| s["codec_type"] == "audio" && s["codec_name"] == "aac")
        );
        let duration: f64 = info["format"]["duration"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            (2.2..3.0).contains(&duration),
            "unexpected duration: {duration}"
        );
        assert_eq!(tokio::fs::read(&photo).await.unwrap(), WEBP);

        let larger_photo = workspace.path().join("larger.jpg");
        let status = Command::new("ffmpeg")
            .args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
            .arg(&photo)
            .args(["-vf", "scale=80:40", "-frames:v", "1", "-update", "1"])
            .arg(&larger_photo)
            .status()
            .await
            .unwrap();
        assert!(status.success());
        let capped = photo_with_audio(
            &larger_photo,
            &audio,
            &CancellationToken::new(),
            None,
            Some(20),
            Duration::from_secs(30),
            "Photo with capped music video test",
        )
        .await
        .unwrap();
        let metadata = probe_video(
            &capped,
            &CancellationToken::new(),
            Duration::from_secs(10),
            "Capped music video test",
        )
        .await
        .unwrap();
        assert_eq!((metadata.width, metadata.height), (40, 20));
        workspace.cleanup().await;
    }

    #[tokio::test]
    async fn slideshow_shows_each_photo_in_order_for_equal_time_with_continuous_audio() {
        let workspace = JobWorkspace::create(5).await.unwrap();
        let mut photos = Vec::new();
        for (index, (width, height, color)) in [
            (32, 24, [255, 0, 0]),
            (24, 32, [0, 255, 0]),
            (32, 24, [0, 0, 255]),
        ]
        .into_iter()
        .enumerate()
        {
            let photo = workspace.path().join(format!("photo-{index}.ppm"));
            let mut image = format!("P6\n{width} {height}\n255\n").into_bytes();
            for _ in 0..width * height {
                image.extend_from_slice(&color);
            }
            tokio::fs::write(&photo, image).await.unwrap();
            photos.push(photo);
        }
        let audio = workspace.path().join("soundtrack.mp4");
        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=6",
                "-c:a",
                "aac",
            ])
            .arg(&audio)
            .status()
            .await
            .unwrap();
        assert!(status.success());
        let output = photos_with_audio(
            &photos,
            &audio,
            &CancellationToken::new(),
            None,
            None,
            Duration::from_secs(30),
            "Slideshow test",
        )
        .await
        .unwrap();
        let metadata = probe_video(
            &output,
            &CancellationToken::new(),
            Duration::from_secs(10),
            "Slideshow video test",
        )
        .await
        .unwrap();
        assert_eq!(metadata.codec, "h264");
        assert_eq!((metadata.width, metadata.height), (32, 24));
        assert!((metadata.duration_secs - 6).abs() <= 1);
        let audio_duration = probe_audio_duration(
            &output,
            &CancellationToken::new(),
            Duration::from_secs(10),
            "Slideshow audio test",
        )
        .await
        .unwrap();
        assert!((audio_duration - 6.0).abs() < 0.2);

        for (timestamp, color) in [(0.5, 0), (1.7, 0), (2.3, 1), (3.7, 1), (4.3, 2), (5.5, 2)] {
            let frame = sample_color(&output, timestamp).await;
            assert!(
                frame[color] > 100
                    && (0..3)
                        .filter(|channel| *channel != color)
                        .all(|channel| frame[channel] < 30),
                "unexpected photo at {timestamp}s: {:?}",
                frame
            );
        }

        let decoded_audio = Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&output)
            .args([
                "-map", "0:a:0", "-f", "s16le", "-ac", "1", "-ar", "8000", "pipe:1",
            ])
            .output()
            .await
            .unwrap();
        assert!(decoded_audio.status.success());
        for second in [2, 4] {
            let start = (second * 8_000 - 400) * 2;
            let end = (second * 8_000 + 400) * 2;
            let non_silent = decoded_audio.stdout[start..end]
                .as_chunks::<2>()
                .0
                .iter()
                .filter(|sample| i16::from_le_bytes(**sample).abs() > 100)
                .count();
            assert!(non_silent > 600, "audio dropped out at {second}s");
        }

        let short_audio = workspace.path().join("short.mp4");
        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=0.6",
                "-c:a",
                "aac",
            ])
            .arg(&short_audio)
            .status()
            .await
            .unwrap();
        assert!(status.success());
        let short_video = photos_with_audio(
            &photos,
            &short_audio,
            &CancellationToken::new(),
            None,
            Some(20),
            Duration::from_secs(30),
            "Short slideshow test",
        )
        .await
        .unwrap();
        for (timestamp, color) in [(0.1, 0), (0.3, 1), (0.5, 2)] {
            let frame = sample_color(&short_video, timestamp).await;
            assert!(frame[color] > 100, "photo {color} was not displayed");
        }
        let capped = probe_video(
            &short_video,
            &CancellationToken::new(),
            Duration::from_secs(10),
            "Capped slideshow test",
        )
        .await
        .unwrap();
        assert!(capped.height <= 20);
        workspace.cleanup().await;
    }

    #[tokio::test]
    async fn refuses_to_create_a_music_video_without_an_audio_stream() {
        let workspace = JobWorkspace::create(4).await.unwrap();
        let photo = workspace.path().join("photo.webp");
        tokio::fs::write(&photo, WEBP).await.unwrap();
        let result = photo_with_audio(
            &photo,
            &photo,
            &CancellationToken::new(),
            None,
            None,
            Duration::from_secs(10),
            "Missing music test",
        )
        .await;
        assert!(result.is_err());
        assert_eq!(tokio::fs::read(&photo).await.unwrap(), WEBP);
        workspace.cleanup().await;
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
