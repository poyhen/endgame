use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use grammers_client::Client;
use grammers_client::media::{Attribute, InputMedia, Uploaded};
use grammers_client::message::InputMessage;
use grammers_client::update::Message as UpdateMessage;
use tokio::io::{AsyncRead, ReadBuf};

use crate::cancel::CancellationToken;
use crate::jobs::JobProgress;
use crate::media::inspect::extract_thumbnail;
use crate::media::request::{DownloadLimits, DownloadOutcome};
use crate::media::transcode::prepare_video;

const UPLOAD_PROGRESS_INTERVAL: Duration = Duration::from_secs(5);
const TELEGRAM_OPERATION_TIMEOUT: Duration = Duration::from_secs(120);
const UTILITY_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) struct DeliveryPlan {
    pub position: (usize, usize),
    pub replace_status: bool,
}

struct ProgressReader<R> {
    inner: R,
    progress: JobProgress,
    position: (usize, usize),
    bytes_read: u64,
    total_bytes: u64,
    last_reported_bytes: u64,
    last_report: Instant,
}

impl<R> ProgressReader<R> {
    fn new(inner: R, progress: JobProgress, position: (usize, usize), total_bytes: u64) -> Self {
        progress.upload_progress(position.0, position.1, 0, total_bytes);
        Self {
            inner,
            progress,
            position,
            bytes_read: 0,
            total_bytes,
            last_reported_bytes: 0,
            last_report: Instant::now(),
        }
    }

    fn record_read(&mut self, bytes: usize) {
        self.bytes_read = self
            .bytes_read
            .saturating_add(bytes as u64)
            .min(self.total_bytes);
        if self.bytes_read > self.last_reported_bytes
            && self.last_report.elapsed() >= UPLOAD_PROGRESS_INTERVAL
        {
            let reported_bytes = in_flight_upload_bytes(self.bytes_read, self.total_bytes);
            self.progress.upload_progress(
                self.position.0,
                self.position.1,
                reported_bytes,
                self.total_bytes,
            );
            self.last_reported_bytes = self.bytes_read;
            self.last_report = Instant::now();
        }
    }
}

fn in_flight_upload_bytes(bytes_read: u64, total_bytes: u64) -> u64 {
    let bytes_read = bytes_read.min(total_bytes);
    // Grammers can read a few chunks ahead of Telegram's acknowledgements, so
    // reserve 100% for the successful return from `upload_stream`.
    if bytes_read == total_bytes && total_bytes > 0 {
        total_bytes - 1
    } else {
        bytes_read
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for ProgressReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let filled_before = buffer.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(context, buffer);
        if let Poll::Ready(Ok(())) = &result {
            self.record_read(buffer.filled().len().saturating_sub(filled_before));
        }
        result
    }
}

pub(crate) fn media_kind(path: &Path) -> (bool, bool, bool, Option<String>) {
    match mime_guess::from_path(path).first() {
        Some(m) => (
            m.type_() == mime::VIDEO,
            m.type_() == mime::IMAGE,
            m.type_() == mime::AUDIO,
            Some(m.essence_str().to_string()),
        ),
        None => (false, false, false, None),
    }
}

async fn upload_file_with_progress(
    client: &Client,
    path: &Path,
    cancellation: &CancellationToken,
    progress: &JobProgress,
    position: (usize, usize),
    upload_timeout: Duration,
) -> anyhow::Result<Uploaded> {
    let file = tokio::fs::File::open(path).await?;
    let total_bytes = file.metadata().await?.len();
    let size = usize::try_from(total_bytes)
        .map_err(|_| anyhow::anyhow!("file is too large for this platform"))?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow::anyhow!("upload path has no file name"))?;
    let mut reader = ProgressReader::new(file, progress.clone(), position, total_bytes);

    let uploaded = tokio::select! {
        biased;
        _ = cancellation.cancelled() => anyhow::bail!("cancelled"),
        _ = tokio::time::sleep(upload_timeout) => anyhow::bail!(
            "Telegram upload timed out after {} seconds",
            upload_timeout.as_secs()
        ),
        result = client.upload_stream(&mut reader, size, name) => result?,
    };
    progress.upload_progress(position.0, position.1, total_bytes, total_bytes);
    Ok(uploaded)
}

pub(crate) async fn send_gallery_albums(
    client: &Client,
    message: &UpdateMessage,
    downloaded: &[PathBuf],
    cancellation: &CancellationToken,
    progress: &JobProgress,
    info: &str,
    limits: DownloadLimits,
) -> DownloadOutcome {
    let media_paths: Vec<&Path> = downloaded
        .iter()
        .filter_map(|item| {
            let (is_video, is_image, _, _) = media_kind(item);
            (is_video || is_image).then_some(item.as_path())
        })
        .collect();
    let total = media_paths.len();
    let mut sent = 0usize;
    let mut cursor = 0usize;
    let mut last_error: Option<String> = None;

    while cursor < total {
        let remaining = total - cursor;
        // Telegram albums accept at most ten items. Avoid a one-item final
        // batch when possible (e.g. split 11 as 9 + 2).
        let batch_size = if remaining == 11 {
            9
        } else {
            remaining.min(10)
        };
        let mut batch = Vec::with_capacity(batch_size);

        for (offset, path) in media_paths[cursor..cursor + batch_size].iter().enumerate() {
            if cancellation.is_cancelled() {
                return DownloadOutcome::Cancelled;
            }

            let prepared = prepare_album_media(
                client,
                path,
                cancellation,
                progress,
                limits,
                (cursor + offset + 1, total),
            )
            .await;
            if cancellation.is_cancelled() {
                return DownloadOutcome::Cancelled;
            }
            match prepared {
                Ok(media) => {
                    batch.push(media);
                }
                Err(error) => {
                    log::warn!(
                        "{info} | Failed to prepare {} for an album: {error}",
                        path.display()
                    );
                    last_error = Some(format!(
                        "could not prepare {}: {error}",
                        path.file_name()
                            .map(|name| name.to_string_lossy())
                            .unwrap_or_default()
                    ));
                }
            }
        }
        cursor += batch_size;

        if batch.is_empty() {
            continue;
        }
        let batch_count = batch.len();
        progress.finalizing();
        let send_result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return DownloadOutcome::Cancelled,
            _ = tokio::time::sleep(TELEGRAM_OPERATION_TIMEOUT) => {
                Err(anyhow::anyhow!(
                    "Telegram album send timed out after {} seconds",
                    TELEGRAM_OPERATION_TIMEOUT.as_secs()
                ))
            },
            result = message.reply_album(batch) => result.map_err(anyhow::Error::from),
        };
        match send_result {
            Ok(_) => sent += batch_count,
            Err(error) => {
                log::warn!("{info} | Failed to send Telegram album: {error}");
                last_error = Some(format!("could not upload a media album: {error}"));
            }
        }
    }

    if sent > 0 {
        log::info!("{info} | Finished processing gallery. Sent {sent}/{total} item(s) in albums.");
        if let Some(error) = last_error {
            progress.complete_with_warning(error);
        } else {
            progress.delete_status().await;
        }
        DownloadOutcome::Completed
    } else {
        progress.fail(last_error.unwrap_or_else(|| "gallery contained no uploadable media".into()));
        DownloadOutcome::Failed
    }
}

async fn prepare_album_media(
    client: &Client,
    path: &Path,
    cancellation: &CancellationToken,
    progress: &JobProgress,
    limits: DownloadLimits,
    position: (usize, usize),
) -> anyhow::Result<InputMedia> {
    let (is_video, is_image, _, _) = media_kind(path);
    if is_image {
        let photo = upload_file_with_progress(
            client,
            path,
            cancellation,
            progress,
            position,
            limits.upload_timeout,
        )
        .await?;
        return Ok(InputMedia::new().photo(photo));
    }
    if !is_video {
        anyhow::bail!("file is not recognized as an image or video");
    }

    let prepared = prepare_video(path, cancellation, progress, limits).await?;
    let metadata = prepared.metadata();
    progress.thumbnailing();
    let operation = format!("Job #{}", progress.id());
    let thumbnail = extract_thumbnail(
        prepared.path(),
        cancellation,
        UTILITY_COMMAND_TIMEOUT,
        &operation,
    )
    .await
    .ok();
    let result: anyhow::Result<InputMedia> = async {
        let video = upload_file_with_progress(
            client,
            prepared.path(),
            cancellation,
            progress,
            position,
            limits.upload_timeout,
        )
        .await?;
        let mut media = InputMedia::new()
            .document(video)
            .attribute(Attribute::Video {
                round_message: false,
                supports_streaming: true,
                duration: Duration::from_secs(metadata.duration_secs.max(0) as u64),
                w: metadata.width,
                h: metadata.height,
            });
        if let Some(thumbnail) = &thumbnail
            && let Ok(Ok(thumb)) =
                tokio::time::timeout(TELEGRAM_OPERATION_TIMEOUT, client.upload_file(thumbnail))
                    .await
        {
            media = media.thumbnail(thumb);
        }
        Ok(media)
    }
    .await;

    result
}

pub(crate) async fn send_video(
    client: &Client,
    message: &UpdateMessage,
    path: &Path,
    cancellation: &CancellationToken,
    progress: &JobProgress,
    limits: DownloadLimits,
    delivery: DeliveryPlan,
) -> anyhow::Result<()> {
    // Thumbnail extraction is best-effort. Metadata probing remains a hard
    // requirement so we never upload garbage masquerading as video.
    let prepared = prepare_video(path, cancellation, progress, limits).await?;
    let metadata = prepared.metadata();
    progress.thumbnailing();
    let operation = format!("Job #{}", progress.id());
    let thumbnail = extract_thumbnail(
        prepared.path(),
        cancellation,
        UTILITY_COMMAND_TIMEOUT,
        &operation,
    )
    .await
    .ok();

    let result: anyhow::Result<()> = async {
        let video = upload_file_with_progress(
            client,
            prepared.path(),
            cancellation,
            progress,
            delivery.position,
            limits.upload_timeout,
        )
        .await?;
        let mut input = InputMessage::new()
            .document(video)
            .attribute(Attribute::Video {
                round_message: false,
                supports_streaming: true,
                duration: Duration::from_secs(metadata.duration_secs.max(0) as u64),
                w: metadata.width,
                h: metadata.height,
            });
        if let Some(thumbnail) = &thumbnail
            && let Ok(Ok(thumb)) =
                tokio::time::timeout(TELEGRAM_OPERATION_TIMEOUT, client.upload_file(thumbnail))
                    .await
        {
            input = input.thumbnail(thumb);
        }
        deliver_media(message, progress, input, delivery.replace_status).await?;
        Ok(())
    }
    .await;

    result
}

pub(crate) async fn send_image(
    client: &Client,
    message: &UpdateMessage,
    path: &Path,
    cancellation: &CancellationToken,
    progress: &JobProgress,
    limits: DownloadLimits,
    delivery: DeliveryPlan,
) -> anyhow::Result<()> {
    let photo = upload_file_with_progress(
        client,
        path,
        cancellation,
        progress,
        delivery.position,
        limits.upload_timeout,
    )
    .await?;
    deliver_media(
        message,
        progress,
        InputMessage::new().photo(photo),
        delivery.replace_status,
    )
    .await
}

pub(crate) async fn send_audio(
    client: &Client,
    message: &UpdateMessage,
    path: &Path,
    cancellation: &CancellationToken,
    progress: &JobProgress,
    limits: DownloadLimits,
    delivery: DeliveryPlan,
) -> anyhow::Result<()> {
    let audio = upload_file_with_progress(
        client,
        path,
        cancellation,
        progress,
        delivery.position,
        limits.upload_timeout,
    )
    .await?;
    deliver_media(
        message,
        progress,
        InputMessage::new().document(audio),
        delivery.replace_status,
    )
    .await
}

async fn deliver_media(
    source: &UpdateMessage,
    progress: &JobProgress,
    media: InputMessage,
    replace_status: bool,
) -> anyhow::Result<()> {
    progress.finalizing();
    if !replace_status {
        tokio::time::timeout(TELEGRAM_OPERATION_TIMEOUT, source.respond(media))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "Telegram media send timed out after {} seconds",
                    TELEGRAM_OPERATION_TIMEOUT.as_secs()
                )
            })??;
        return Ok(());
    }
    match progress.replace_with_media(media.clone()).await {
        Ok(true) => Ok(()),
        Ok(false) => match timed_fallback_send(source, progress, media).await {
            Ok(_) => {
                progress.delete_status().await;
                Ok(())
            }
            Err(send_error) => Err(anyhow::anyhow!(
                "status was unavailable and fallback media failed: {send_error}"
            )),
        },
        Err(edit_error) => match timed_fallback_send(source, progress, media).await {
            Ok(_) => {
                log::warn!("Could not edit status into media; used fallback send: {edit_error}");
                progress.delete_status().await;
                Ok(())
            }
            Err(send_error) => Err(anyhow::anyhow!(
                "could not edit status ({edit_error}) or send fallback media ({send_error})"
            )),
        },
    }
}

async fn timed_fallback_send(
    source: &UpdateMessage,
    progress: &JobProgress,
    media: InputMessage,
) -> anyhow::Result<()> {
    match tokio::time::timeout(TELEGRAM_OPERATION_TIMEOUT, source.reply(media)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => {
            progress.resume_text_status();
            Err(error.into())
        }
        Err(_) => {
            progress.resume_text_status();
            anyhow::bail!("Telegram fallback media send timed out")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_audio_downloads() {
        let (_, _, is_audio, mime) = media_kind(Path::new("track.mp3"));
        assert!(is_audio);
        assert_eq!(mime.as_deref(), Some("audio/mpeg"));
    }

    #[test]
    fn in_flight_progress_never_claims_completion() {
        assert_eq!(in_flight_upload_bytes(500, 1_000), 500);
        assert_eq!(in_flight_upload_bytes(1_000, 1_000), 999);
        assert_eq!(in_flight_upload_bytes(2_000, 1_000), 999);
    }
}
