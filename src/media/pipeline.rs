use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use grammers_client::Client;
use grammers_client::message::Message;
use tokio::process::Command;

use crate::cancel::CancellationToken;
use crate::cookies;
use crate::jobs::JobProgress;
use crate::media::command::{self, CommandOutcome};
use crate::media::delivery::{
    DeliveryPlan, GalleryOutcome, media_kind, send_audio, send_gallery_albums, send_image,
    send_video,
};
use crate::media::progress::read_download;
use crate::media::request::{
    DownloadLimits, DownloadMode, DownloadOutcome, DownloadRequest, DownloadSettings,
};
use crate::media::transcode::photos_with_audio;
use crate::media::workspace::JobWorkspace;
use crate::media::{downloader, instagram};

enum CommandProgress {
    Download(JobProgress),
}

struct InstagramPhotos<'a> {
    paths: &'a [PathBuf],
    max_height: Option<u32>,
}

const USER_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
static USERNAME_CACHE: OnceLock<Mutex<HashMap<i64, Option<String>>>> = OnceLock::new();

async fn user_info(client: &Client, message: &Message) -> String {
    let id = message.peer_id().bare_id();
    let embedded_username = message
        .sender()
        .or_else(|| message.peer())
        .and_then(|peer| peer.username().map(str::to_owned));
    if let (Some(id), Some(username)) = (id, embedded_username.as_ref()) {
        username_cache().insert(id, Some(username.clone()));
    }

    let username = match (embedded_username, id) {
        (Some(username), _) => Some(username),
        (None, Some(id)) => resolve_username(client, message, id).await,
        (None, None) => None,
    };
    format_user_info(username.as_deref(), id)
}

async fn resolve_username(client: &Client, message: &Message, id: i64) -> Option<String> {
    if let Some(username) = username_cache().get(&id).cloned() {
        return username;
    }

    let lookup = async {
        let Some(sender) = message
            .sender_ref()
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))?
        else {
            anyhow::bail!("sender reference is unavailable");
        };
        let peer = client.resolve_peer(sender).await?;
        Ok::<_, anyhow::Error>(peer.username().map(str::to_owned).or_else(|| {
            peer.usernames()
                .first()
                .map(|username| (*username).to_owned())
        }))
    };
    match tokio::time::timeout(USER_LOOKUP_TIMEOUT, lookup).await {
        Ok(Ok(username)) => {
            username_cache().insert(id, username.clone());
            username
        }
        Ok(Err(error)) => {
            log::warn!("Could not resolve username for user {id}: {error}");
            None
        }
        Err(_) => {
            log::warn!(
                "Timed out resolving username for user {id} after {} seconds",
                USER_LOOKUP_TIMEOUT.as_secs()
            );
            None
        }
    }
}

fn username_cache() -> MutexGuard<'static, HashMap<i64, Option<String>>> {
    USERNAME_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn format_user_info(username: Option<&str>, id: Option<i64>) -> String {
    match (username, id) {
        (Some(username), Some(id)) => format!("User @{username} ({id})"),
        (Some(username), None) => format!("User @{username}"),
        (None, Some(id)) => format!("User {id}"),
        (None, None) => "User (unknown)".to_string(),
    }
}

async fn run_command(
    cmd: Command,
    cancellation: &CancellationToken,
    timeout: Duration,
    job_id: u64,
    command_progress: Option<CommandProgress>,
) -> CommandOutcome {
    let operation = format!("Job #{job_id}");
    command::run(
        cmd,
        cancellation,
        timeout,
        &operation,
        move |mut stdout| async move {
            match command_progress {
                Some(CommandProgress::Download(progress)) => read_download(stdout, progress).await,
                None => command::read_bounded(&mut stdout).await,
            }
        },
    )
    .await
}

async fn download_public_instagram(
    url: &str,
    mode: &DownloadMode,
    workspace: &Path,
    limits: DownloadLimits,
    cancellation: &CancellationToken,
    progress: &JobProgress,
) -> CommandOutcome {
    let source = workspace.join("instagram-source.jsonl");
    let outcome = run_command(
        instagram::metadata_command(url, &source),
        cancellation,
        limits.command_timeout,
        progress.id(),
        None,
    )
    .await;
    if !matches!(outcome, CommandOutcome::Finished { success: true, .. }) {
        return outcome;
    }

    let metadata_path = workspace.join("instagram-public.info.json");
    let prepare = async {
        let source = tokio::fs::read_to_string(source).await?;
        let metadata = instagram::prepare_metadata(&source, mode)?;
        tokio::fs::write(&metadata_path, serde_json::to_vec(&metadata)?).await?;
        Ok::<_, anyhow::Error>(())
    };
    if let Err(error) = prepare.await {
        return CommandOutcome::Finished {
            success: false,
            stdout: String::new(),
            stderr: format!("could not prepare public Instagram media: {error:#}"),
        };
    }

    run_command(
        instagram::download_command(
            url,
            mode,
            limits,
            &metadata_path,
            &workspace.join("instagram-public"),
        ),
        cancellation,
        limits.command_timeout,
        progress.id(),
        Some(CommandProgress::Download(progress.clone())),
    )
    .await
}

async fn collect_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(directory).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            match entry.file_type().await {
                Ok(kind) if kind.is_dir() => directories.push(path),
                Ok(kind) if kind.is_file() => files.push(path),
                _ => {}
            }
        }
    }
    files
}

async fn prepare_instagram_music(
    url: &str,
    cookie_file: &Path,
    photos: InstagramPhotos<'_>,
    workspace: &Path,
    limits: DownloadLimits,
    cancellation: &CancellationToken,
    progress: &JobProgress,
) -> anyhow::Result<PathBuf> {
    progress.downloading();
    let audio = download_instagram_audio(
        url,
        cookie_file,
        workspace,
        limits.command_timeout,
        cancellation,
        progress.id(),
    )
    .await?;
    photos_with_audio(
        photos.paths,
        &audio,
        cancellation,
        Some(progress),
        photos.max_height,
        limits.command_timeout,
        &format!("Job #{} Instagram music video", progress.id()),
    )
    .await
}

async fn download_instagram_audio(
    url: &str,
    cookie_file: &Path,
    workspace: &Path,
    timeout: Duration,
    cancellation: &CancellationToken,
    job_id: u64,
) -> anyhow::Result<PathBuf> {
    let audio_directory = workspace.join("instagram-audio");
    tokio::fs::create_dir_all(&audio_directory).await?;
    let outcome = run_command(
        instagram::audio_command(url, cookie_file, &audio_directory),
        cancellation,
        timeout,
        job_id,
        None,
    )
    .await;
    match outcome {
        CommandOutcome::Finished { success: true, .. } => {}
        CommandOutcome::Finished { .. } => anyhow::bail!("Instagram music download failed"),
        CommandOutcome::Cancelled => anyhow::bail!("cancelled"),
        CommandOutcome::TimedOut => anyhow::bail!("Instagram music download timed out"),
    }
    let mut files = collect_files(&audio_directory).await;
    files.retain(|path| {
        !path.extension().is_some_and(|extension| {
            extension.eq_ignore_ascii_case("json") || extension.eq_ignore_ascii_case("part")
        })
    });
    if files.len() != 1 {
        anyhow::bail!("Instagram did not provide one downloadable soundtrack");
    }
    Ok(files.remove(0))
}

fn add_warning(current: &mut Option<String>, warning: String) {
    match current {
        Some(existing) => {
            existing.push_str("; ");
            existing.push_str(&warning);
        }
        None => *current = Some(warning),
    }
}

fn is_video_ext(p: &str) -> bool {
    let p = p.to_lowercase();
    p.ends_with(".mp4") || p.ends_with(".mkv") || p.ends_with(".webm") || p.ends_with(".mov")
}

fn is_image_ext(p: &str) -> bool {
    let p = p.to_lowercase();
    p.ends_with(".jpg")
        || p.ends_with(".jpeg")
        || p.ends_with(".png")
        || p.ends_with(".gif")
        || p.ends_with(".webp")
}

pub async fn download_and_upload(
    client: Client,
    message: Message,
    request: DownloadRequest,
    cancellation: &CancellationToken,
    progress: &JobProgress,
    settings: &DownloadSettings,
) -> DownloadOutcome {
    let workspace = match JobWorkspace::create(progress.id()).await {
        Ok(workspace) => workspace,
        Err(error) => {
            progress.fail(format!(
                "could not create an isolated job workspace: {error}"
            ));
            return DownloadOutcome::Failed;
        }
    };
    let outcome = download_and_upload_in_workspace(
        client,
        message,
        request,
        cancellation,
        progress,
        settings,
        workspace.path(),
    )
    .await;
    workspace.cleanup().await;
    outcome
}

async fn download_and_upload_in_workspace(
    client: Client,
    message: Message,
    request: DownloadRequest,
    cancellation: &CancellationToken,
    progress: &JobProgress,
    settings: &DownloadSettings,
    workspace: &Path,
) -> DownloadOutcome {
    let limits = settings.limits;
    if cancellation.is_cancelled() {
        return DownloadOutcome::Cancelled;
    }

    let info = format!(
        "Job #{} | {}",
        progress.id(),
        user_info(&client, &message).await
    );
    let DownloadRequest { url, mode } = request;
    let cookie_file = cookies::file_for_url(&url, Path::new(".")).await;
    let download = downloader::build(
        &url,
        &mode,
        &cookie_file,
        workspace,
        limits,
        &settings.yt_dlp_domains,
    );
    let mut downloader_name = download.program;
    let mut gallery_delivery = !download.reports_progress;
    let mut download_directory = workspace.to_path_buf();
    log::info!("{info} | Using {downloader_name} for URL: {url}");

    let mut outcome = run_command(
        download.command,
        cancellation,
        limits.command_timeout,
        progress.id(),
        download
            .reports_progress
            .then(|| CommandProgress::Download(progress.clone())),
    )
    .await;
    let public_fallback = match &outcome {
        CommandOutcome::Finished {
            success: false,
            stdout,
            stderr,
        } => {
            instagram::should_try_public(&url, stderr) || instagram::should_try_public(&url, stdout)
        }
        _ => false,
    };
    if public_fallback {
        log::warn!(
            "{info} | Instagram rejected authenticated access; trying the public post without saved cookies"
        );
        progress.downloading();
        outcome =
            download_public_instagram(&url, &mode, workspace, limits, cancellation, progress).await;
        downloader_name = "yt-dlp (public Instagram)";
        gallery_delivery = true;
        // Keep metadata and any partial files from the first attempt out of delivery.
        download_directory = workspace.join("instagram-public");
    }
    let (success, stdout, stderr) = match outcome {
        CommandOutcome::Finished {
            success,
            stdout,
            stderr,
        } => (success, stdout, stderr),
        CommandOutcome::Cancelled => {
            return DownloadOutcome::Cancelled;
        }
        CommandOutcome::TimedOut => {
            progress.fail(format!(
                "{downloader_name} timed out after {} seconds",
                limits.command_timeout.as_secs()
            ));
            return DownloadOutcome::Failed;
        }
    };

    if !success {
        let mut error_message = stderr;
        if error_message.is_empty() {
            error_message = stdout;
        }
        if error_message.is_empty() {
            error_message = "Unknown error".to_string();
        }
        if public_fallback {
            progress.fail(format!(
                "{} Public download failed: {error_message}",
                instagram::COOKIE_HINT
            ));
        } else {
            progress.fail(format!("{downloader_name} download error: {error_message}"));
        }
        return DownloadOutcome::Failed;
    }

    // Gather downloaded files.
    let mut downloaded = collect_files(&download_directory).await;
    if downloaded.is_empty() {
        progress.fail(format!(
            "{downloader_name} finished without producing a media file"
        ));
        return DownloadOutcome::Failed;
    }
    let instagram_gallery = gallery_delivery
        && !public_fallback
        && instagram::is_post(&url)
        && matches!(&mode, DownloadMode::Video { .. });
    let mut photos = Vec::new();
    let mut has_music = false;
    if instagram_gallery {
        let gallery = instagram::gallery_files(downloaded).await;
        downloaded = gallery.files;
        photos = gallery.photos;
        has_music = gallery.has_music;
    } else if public_fallback {
        downloaded.sort();
    } else if gallery_delivery {
        // Sort to prefer videos, then images.
        downloaded.sort_by_key(|p| {
            let s = p.to_string_lossy().into_owned();
            (!is_video_ext(&s), !is_image_ext(&s), s)
        });
    }
    if downloaded.is_empty() {
        progress.fail(format!(
            "{downloader_name} finished without producing a media file"
        ));
        return DownloadOutcome::Failed;
    }

    let mut any_success = false;
    let mut sent_count = 0usize;
    let mut last_error: Option<String> = None;
    let mut music_video = None;
    if has_music && !photos.is_empty() {
        let max_height = match mode {
            DownloadMode::Video { max_height } => max_height,
            _ => None,
        };
        match prepare_instagram_music(
            &url,
            &cookie_file,
            InstagramPhotos {
                paths: &photos,
                max_height,
            },
            workspace,
            limits,
            cancellation,
            progress,
        )
        .await
        {
            Ok(video) => music_video = Some(video),
            Err(_) if cancellation.is_cancelled() => return DownloadOutcome::Cancelled,
            Err(error) => {
                log::warn!("{info} | Could not prepare Instagram music: {error:#}");
                add_warning(
                    &mut last_error,
                    "could not download or prepare Instagram music".into(),
                );
            }
        }
    }

    let gallery_media_count = downloaded
        .iter()
        .filter(|item| {
            let (is_video, is_image, _, _) = media_kind(item);
            is_video || is_image
        })
        .count();
    let recognized_media_count = downloaded
        .iter()
        .filter(|item| {
            let (is_video, is_image, is_audio, _) = media_kind(item);
            is_video || is_image || is_audio
        })
        .count();
    let replace_status_with_media =
        recognized_media_count == 1 && music_video.is_none() && last_error.is_none();
    if gallery_delivery && gallery_media_count > 1 {
        match send_gallery_albums(
            &client,
            &message,
            &downloaded,
            cancellation,
            progress,
            &info,
            limits,
        )
        .await
        {
            GalleryOutcome::Sent { warning } => {
                any_success = true;
                if let Some(warning) = warning {
                    add_warning(&mut last_error, warning);
                }
            }
            GalleryOutcome::Failed(error) => add_warning(&mut last_error, error),
            GalleryOutcome::Cancelled => return DownloadOutcome::Cancelled,
        }
    }
    let individual_files = if gallery_delivery && gallery_media_count > 1 {
        &[][..]
    } else {
        &downloaded[..]
    };
    for (index, item) in individual_files.iter().enumerate() {
        if cancellation.is_cancelled() {
            return DownloadOutcome::Cancelled;
        }

        if !tokio::fs::try_exists(item).await.unwrap_or(false) {
            if gallery_delivery {
                add_warning(
                    &mut last_error,
                    "a downloaded gallery item disappeared before upload".into(),
                );
                continue;
            } else {
                progress.fail("the downloaded file disappeared before upload");
                return DownloadOutcome::Failed;
            }
        }

        let (is_video, is_image, is_audio, mime_type) = media_kind(item);
        let mut processed = false;

        if is_video {
            let send_result = send_video(
                &client,
                &message,
                item,
                cancellation,
                progress,
                limits,
                DeliveryPlan {
                    position: (index + 1, downloaded.len()),
                    replace_status: replace_status_with_media,
                },
            )
            .await;
            if cancellation.is_cancelled() {
                return DownloadOutcome::Cancelled;
            }
            match send_result {
                Ok(()) => {
                    any_success = true;
                    processed = true;
                    sent_count += 1;
                }
                Err(e) => {
                    let name = item
                        .file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    log::warn!("{info} | Error processing video {name}: {e}");
                    add_warning(
                        &mut last_error,
                        format!("could not process video {name}: {e}"),
                    );
                }
            }
        } else if is_image {
            let send_result = send_image(
                &client,
                &message,
                item,
                cancellation,
                progress,
                limits,
                DeliveryPlan {
                    position: (index + 1, downloaded.len()),
                    replace_status: replace_status_with_media,
                },
            )
            .await;
            if cancellation.is_cancelled() {
                return DownloadOutcome::Cancelled;
            }
            match send_result {
                Ok(()) => {
                    any_success = true;
                    processed = true;
                    sent_count += 1;
                }
                Err(e) => {
                    let name = item
                        .file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    log::warn!("{info} | Error sending image {name}: {e}");
                    add_warning(
                        &mut last_error,
                        format!("could not upload image {name}: {e}"),
                    );
                }
            }
        } else if is_audio {
            let send_result = send_audio(
                &client,
                &message,
                item,
                cancellation,
                progress,
                limits,
                DeliveryPlan {
                    position: (index + 1, downloaded.len()),
                    replace_status: replace_status_with_media,
                },
            )
            .await;
            if cancellation.is_cancelled() {
                return DownloadOutcome::Cancelled;
            }
            match send_result {
                Ok(()) => {
                    any_success = true;
                    processed = true;
                    sent_count += 1;
                }
                Err(error) => {
                    let name = item
                        .file_name()
                        .map(|file| file.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    log::warn!("{info} | Error sending audio {name}: {error}");
                    add_warning(
                        &mut last_error,
                        format!("could not upload audio {name}: {error}"),
                    );
                }
            }
        }

        if !processed && !is_video && !is_image && !is_audio {
            let name = item
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            log::warn!(
                "{info} | Skipping non-media file from {downloader_name}: {name} (MIME: {mime_type:?})"
            );
            add_warning(
                &mut last_error,
                format!("{downloader_name} produced an unsupported file: {name}"),
            );
        }
    }

    if let Some(video) = music_video {
        let send_result = send_video(
            &client,
            &message,
            &video,
            cancellation,
            progress,
            limits,
            DeliveryPlan {
                position: (1, 1),
                replace_status: false,
            },
        )
        .await;
        if cancellation.is_cancelled() {
            return DownloadOutcome::Cancelled;
        }
        match send_result {
            Ok(()) => any_success = true,
            Err(error) => {
                log::warn!("{info} | Could not send Instagram music video: {error:#}");
                add_warning(
                    &mut last_error,
                    "could not upload Instagram music video".into(),
                );
            }
        }
    }

    if !any_success && !downloaded.is_empty() {
        log::warn!(
            "{info} | {downloader_name} downloaded content, but could not process or send any recognized media file."
        );
        progress.fail(
            last_error.clone().unwrap_or_else(|| {
                format!("{downloader_name} produced no supported media to upload")
            }),
        );
    } else if gallery_delivery && sent_count > 0 {
        let total = downloaded.len();
        if sent_count == total {
            log::info!("{info} | Finished processing gallery. Sent all {sent_count} item(s).");
        } else {
            log::info!(
                "{info} | Finished processing gallery. Sent {sent_count} item(s) from {total} downloaded file(s)."
            );
        }
    }

    if any_success && !replace_status_with_media {
        if let Some(error) = last_error {
            progress.complete_with_warning(error);
        } else {
            progress.delete_status().await;
        }
    }

    if any_success {
        DownloadOutcome::Completed
    } else {
        DownloadOutcome::Failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::inspect::{probe_audio_duration, probe_video};

    #[test]
    fn user_info_keeps_the_peer_id_when_entity_details_are_missing() {
        assert_eq!(
            format_user_info(Some("notfurkan"), Some(126_344_924)),
            "User @notfurkan (126344924)"
        );
        assert_eq!(format_user_info(None, Some(126_344_924)), "User 126344924");
        assert_eq!(format_user_info(None, None), "User (unknown)");
    }

    #[tokio::test]
    #[ignore = "requires ENDGAME_LIVE_INSTAGRAM_URL and ENDGAME_LIVE_INSTAGRAM_COOKIES"]
    async fn live_instagram_photo_and_music_use_the_download_pipeline() {
        let url = std::env::var("ENDGAME_LIVE_INSTAGRAM_URL").unwrap();
        let cookies = PathBuf::from(std::env::var_os("ENDGAME_LIVE_INSTAGRAM_COOKIES").unwrap());
        let workspace = JobWorkspace::create(102).await.unwrap();
        let limits = DownloadLimits {
            max_upload_bytes: 100 * 1024 * 1024,
            command_timeout: Duration::from_secs(90),
            upload_timeout: Duration::from_secs(90),
            job_timeout: Duration::from_secs(180),
        };
        let cancel = CancellationToken::new();
        let download = downloader::build(
            &url,
            &DownloadMode::Video { max_height: None },
            &cookies,
            workspace.path(),
            limits,
            &[],
        );
        let outcome =
            run_command(download.command, &cancel, limits.command_timeout, 102, None).await;
        assert!(matches!(
            outcome,
            CommandOutcome::Finished { success: true, .. }
        ));
        let gallery = instagram::gallery_files(collect_files(workspace.path()).await).await;
        let minimum_items = std::env::var("ENDGAME_LIVE_INSTAGRAM_MIN_ITEMS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(1);
        assert!(gallery.files.len() >= minimum_items);
        println!("Downloaded {} original media item(s)", gallery.files.len());
        assert!(gallery.has_music);
        assert!(!gallery.photos.is_empty(), "Instagram returned no photos");
        let audio = download_instagram_audio(
            &url,
            &cookies,
            workspace.path(),
            limits.command_timeout,
            &cancel,
            102,
        )
        .await
        .unwrap();
        let original_duration =
            probe_audio_duration(&audio, &cancel, limits.command_timeout, "Live audio test")
                .await
                .unwrap();
        let video = photos_with_audio(
            &gallery.photos,
            &audio,
            &cancel,
            None,
            None,
            limits.command_timeout,
            "Live music video test",
        )
        .await
        .unwrap();
        let metadata = probe_video(&video, &cancel, limits.command_timeout, "Live video test")
            .await
            .unwrap();
        let output_duration = probe_audio_duration(
            &video,
            &cancel,
            limits.command_timeout,
            "Live video audio test",
        )
        .await
        .unwrap();
        assert_eq!(metadata.codec, "h264");
        assert!((original_duration - output_duration).abs() < 0.5);
        assert!((metadata.duration_secs as f64 - original_duration).abs() < 2.0);
        workspace.cleanup().await;
    }
}
