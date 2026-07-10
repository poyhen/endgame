use std::path::{Path, PathBuf};
use std::time::Duration;

use grammers_client::Client;
use grammers_client::update::Message as UpdateMessage;
use tokio::process::Command;

use crate::cancel::CancellationToken;
use crate::jobs::JobProgress;
use crate::media::command::{self, CommandOutcome};
use crate::media::delivery::{
    DeliveryPlan, media_kind, send_audio, send_gallery_albums, send_image, send_video,
};
use crate::media::downloader;
use crate::media::progress::read_download;
use crate::media::request::{DownloadLimits, DownloadOutcome, DownloadRequest};
use crate::media::workspace::JobWorkspace;

enum CommandProgress {
    Download(JobProgress),
}

fn user_info(message: &UpdateMessage) -> String {
    let peer = message.sender().or_else(|| message.peer());
    let Some(peer) = peer else {
        return "User".to_string();
    };
    let Some(id) = peer.id().bare_id() else {
        return "User".to_string();
    };
    match peer.username() {
        Some(u) => format!("User @{u} ({id})"),
        None => format!("User {id}"),
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
    message: UpdateMessage,
    request: DownloadRequest,
    cancellation: &CancellationToken,
    progress: &JobProgress,
    limits: DownloadLimits,
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
        limits,
        workspace.path(),
    )
    .await;
    workspace.cleanup().await;
    outcome
}

async fn download_and_upload_in_workspace(
    client: Client,
    message: UpdateMessage,
    request: DownloadRequest,
    cancellation: &CancellationToken,
    progress: &JobProgress,
    limits: DownloadLimits,
    workspace: &Path,
) -> DownloadOutcome {
    if cancellation.is_cancelled() {
        return DownloadOutcome::Cancelled;
    }

    let info = user_info(&message);
    let DownloadRequest { url, mode } = request;
    let cookie_file = if url.contains("instagram.com/")
        && tokio::fs::try_exists("instacookies.txt")
            .await
            .unwrap_or(false)
    {
        Path::new("instacookies.txt")
    } else {
        Path::new("cookies.txt")
    };
    let download = downloader::build(&url, &mode, cookie_file, workspace, limits);
    let downloader_name = download.program;
    let use_yt_dlp = download.reports_progress;
    log::info!("{info} | Using {downloader_name} for URL: {url}");

    let (success, stdout, stderr) = match run_command(
        download.command,
        cancellation,
        limits.command_timeout,
        progress.id(),
        use_yt_dlp.then(|| CommandProgress::Download(progress.clone())),
    )
    .await
    {
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
        progress.fail(format!("{downloader_name} download error: {error_message}"));
        return DownloadOutcome::Failed;
    }

    // Gather downloaded files.
    let mut downloaded = collect_files(workspace).await;
    if use_yt_dlp {
        if downloaded.is_empty() {
            progress.fail(format!(
                "{downloader_name} finished, but its output file was not found"
            ));
            return DownloadOutcome::Failed;
        }
    } else {
        if downloaded.is_empty() {
            progress.fail(format!(
                "{downloader_name} finished without producing a media file"
            ));
            return DownloadOutcome::Failed;
        }
        // Sort to prefer videos, then images.
        downloaded.sort_by_key(|p| {
            let s = p.to_string_lossy().into_owned();
            (!is_video_ext(&s), !is_image_ext(&s), s)
        });
    }

    let mut any_success = false;
    let mut sent_count = 0usize;
    let mut last_error: Option<String> = None;

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
    let replace_status_with_media = recognized_media_count == 1;
    if !use_yt_dlp && gallery_media_count > 1 {
        let outcome = send_gallery_albums(
            &client,
            &message,
            &downloaded,
            cancellation,
            progress,
            &info,
            limits,
        )
        .await;
        return outcome;
    }

    for (index, item) in downloaded.iter().enumerate() {
        if cancellation.is_cancelled() {
            return DownloadOutcome::Cancelled;
        }
        progress.uploading(index + 1, downloaded.len());

        if !tokio::fs::try_exists(item).await.unwrap_or(false) {
            if !use_yt_dlp {
                last_error = Some("a downloaded gallery item disappeared before upload".into());
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
                    println!("{info} | Error processing video {name}: {e}");
                    last_error = Some(format!("could not process video {name}: {e}"));
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
                    println!("{info} | Error sending image {name}: {e}");
                    last_error = Some(format!("could not upload image {name}: {e}"));
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
                    last_error = Some(format!("could not upload audio {name}: {error}"));
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
            last_error = Some(format!(
                "{downloader_name} produced an unsupported file: {name}"
            ));
        }
    }

    if !any_success && !downloaded.is_empty() {
        println!(
            "{info} | {downloader_name} downloaded content, but could not process or send any recognized media file."
        );
        progress.fail(
            last_error.clone().unwrap_or_else(|| {
                format!("{downloader_name} produced no supported media to upload")
            }),
        );
    } else if !use_yt_dlp && sent_count > 0 {
        let total = downloaded.len();
        if sent_count == total {
            println!("{info} | Finished processing gallery. Sent all {sent_count} item(s).");
        } else {
            println!(
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
