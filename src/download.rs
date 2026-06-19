use std::path::{Path, PathBuf};
use std::time::Duration;

use grammers_client::Client;
use grammers_client::media::Attribute;
use grammers_client::message::InputMessage;
use grammers_client::update::Message as UpdateMessage;
use tokio::process::Command;

use crate::utils::{
    clean_cookie_file, extract_thumbnail, generate_random_filename, probe_video,
};

const GALLERY_DL_DOWNLOAD_PATH: &str = "gallery_dl_downloads";

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:131.0) Gecko/20100101 Firefox/131.0";

const DEFAULT_YT_DLP_FORMAT_SELECTOR: &str =
    "bestvideo[vcodec!*=av01][vcodec!*=vp9][ext=mp4]+bestaudio[ext=m4a]/best[ext=mp4]/best";

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

fn should_use_yt_dlp(url: &str) -> bool {
    [
        "youtube.com/",
        "youtu.be/",
        "instagram.com/share/",
        "instagram.com/reels/",
        "instagram.com/tv/",
        "x.com/i/broadcasts/",
        "tiktok.com/",
    ]
    .iter()
    .any(|d| url.contains(d))
}

async fn run_command(mut cmd: Command) -> (bool, String, String) {
    match cmd.output().await {
        Ok(out) => (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ),
        Err(e) => (false, String::new(), e.to_string()),
    }
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
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

fn media_kind(path: &Path) -> (bool, bool, Option<String>) {
    match mime_guess::from_path(path).first() {
        Some(m) => (
            m.type_() == mime::VIDEO,
            m.type_() == mime::IMAGE,
            Some(m.essence_str().to_string()),
        ),
        None => (false, false, None),
    }
}

pub async fn download_and_upload(client: Client, message: UpdateMessage, url: String) {
    let info = user_info(&message);
    let use_yt_dlp = should_use_yt_dlp(&url);

    // Cookies selection.
    let mut cookies_file = "cookies.txt".to_string();
    if url.contains("instagram.com/") && Path::new("instacookies.txt").exists() {
        cookies_file = "instacookies.txt".to_string();
        clean_cookie_file(&cookies_file);
    }

    let mut cleanup_path: Option<PathBuf> = None;
    let mut yt_dlp_base: Option<String> = None;

    let downloader_name = if use_yt_dlp { "yt-dlp" } else { "gallery-dl" };

    let mut cmd = Command::new(downloader_name);

    if use_yt_dlp {
        let base = generate_random_filename("");
        yt_dlp_base = Some(base.clone());
        let output_template = format!("{base}.%(ext)s");
        cmd.arg("-o")
            .arg(&output_template)
            .arg("--cookies")
            .arg(&cookies_file)
            .arg("--user-agent")
            .arg(USER_AGENT);

        if url.contains("youtube.com/") || url.contains("youtu.be/") {
            cmd.arg("-t").arg("mp4");
        } else {
            let mut chosen = DEFAULT_YT_DLP_FORMAT_SELECTOR.to_string();
            for (domain, fmt) in [(
                "tiktok.com",
                "bestvideo[ext=mp4][vcodec=h264]+bestaudio[ext=m4a]/best[ext=mp4][vcodec=h264]/best",
            )] {
                if url.contains(domain) {
                    chosen = fmt.to_string();
                    break;
                }
            }
            cmd.arg("-f").arg(&chosen);
        }
        cmd.arg(&url);
        println!("{info} | Using yt-dlp for URL: {url}");
    } else {
        let instance_path = Path::new(GALLERY_DL_DOWNLOAD_PATH).join(generate_random_filename(""));
        if let Err(e) = std::fs::create_dir_all(&instance_path) {
            let _ = message
                .reply(format!("Failed to create download directory: {e}"))
                .await;
            return;
        }
        cleanup_path = Some(instance_path.clone());
        cmd.arg("--cookies")
            .arg(&cookies_file)
            .arg("--user-agent")
            .arg(USER_AGENT)
            .arg("-D")
            .arg(&instance_path)
            .arg(&url);
        println!("{info} | Using gallery-dl for URL: {url}");
    }

    let (success, stdout, stderr) = run_command(cmd).await;

    if !success {
        let mut error_message = stderr;
        if error_message.is_empty() {
            error_message = stdout;
        }
        if error_message.is_empty() {
            error_message = "Unknown error".to_string();
        }
        let _ = message
            .reply(format!(
                "{downloader_name} failed to download. Error: {error_message}"
            ))
            .await;
        perform_cleanup(&cleanup_path, &yt_dlp_base, use_yt_dlp, &info);
        return;
    }

    // Gather downloaded files.
    let mut downloaded: Vec<PathBuf> = Vec::new();
    if use_yt_dlp {
        let base = yt_dlp_base.clone().unwrap_or_default();
        if let Ok(entries) = std::fs::read_dir(".") {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(&base) {
                    downloaded.push(entry.path());
                }
            }
        }
        if let Some(first) = downloaded.first() {
            cleanup_path = Some(first.clone());
        } else {
            let _ = message
                .reply(format!(
                    "{downloader_name} finished, but the downloaded file was not found (expected prefix: {base})."
                ))
                .await;
            perform_cleanup(&cleanup_path, &yt_dlp_base, use_yt_dlp, &info);
            return;
        }
    } else {
        let dir = cleanup_path.clone().unwrap_or_default();
        collect_files(&dir, &mut downloaded);
        if downloaded.is_empty() {
            let _ = message
                .reply(format!(
                    "{downloader_name} downloaded successfully, but no files found in {}. Output: {stdout}",
                    dir.display()
                ))
                .await;
            perform_cleanup(&cleanup_path, &yt_dlp_base, use_yt_dlp, &info);
            return;
        }
        // Sort to prefer videos, then images.
        downloaded.sort_by_key(|p| {
            let s = p.to_string_lossy().into_owned();
            (!is_video_ext(&s), !is_image_ext(&s), s)
        });
    }

    let mut any_success = false;
    let mut sent_count = 0usize;

    for item in &downloaded {
        if !item.exists() {
            if !use_yt_dlp {
                let _ = message
                    .reply(format!(
                        "Skipping a downloaded file ({}) as it seems to have disappeared before processing.",
                        item.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default()
                    ))
                    .await;
                continue;
            } else {
                let _ = message
                    .reply(format!(
                        "Downloaded file {} seems to have disappeared before processing.",
                        item.display()
                    ))
                    .await;
                perform_cleanup(&cleanup_path, &yt_dlp_base, use_yt_dlp, &info);
                return;
            }
        }

        let (is_video, is_image, mime_type) = media_kind(item);
        let mut processed = false;

        if is_video {
            match send_video(&client, &message, item).await {
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
                    let _ = message
                        .reply(format!("Error processing video {name}: {e}"))
                        .await;
                }
            }
        } else if is_image {
            match send_image(&client, &message, item).await {
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
                    let _ = message
                        .reply(format!("Error sending image {name}: {e}"))
                        .await;
                }
            }
        }

        if !processed && !is_video && !is_image {
            let name = item
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !use_yt_dlp {
                println!(
                    "{info} | Skipping non-media file from gallery-dl: {name} (MIME: {mime_type:?})"
                );
            } else {
                let _ = message
                    .reply(format!(
                        "Downloaded a file ({name}) with {downloader_name} that is not a recognized video or image (MIME: {mime_type:?})."
                    ))
                    .await;
            }
        }
    }

    if !any_success && !downloaded.is_empty() {
        println!(
            "{info} | {downloader_name} downloaded content, but could not process or send any recognized media file."
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

    perform_cleanup(&cleanup_path, &yt_dlp_base, use_yt_dlp, &info);
}

async fn send_video(client: &Client, message: &UpdateMessage, path: &Path) -> anyhow::Result<()> {
    // Metadata and thumbnail are best-effort: a valid video must still be sent
    // even if probing or thumbnail extraction hiccups. `probe_video` only fails
    // when the file is not a real/decodable video, which we treat as a hard
    // error so we never upload garbage masquerading as media.
    let (duration, width, height) = probe_video(path).await?;
    let thumb_name = extract_thumbnail(path).await.ok();

    let result: anyhow::Result<()> = async {
        let video = client.upload_file(path).await?;
        let mut input = InputMessage::new()
            .document(video)
            .attribute(Attribute::Video {
                round_message: false,
                supports_streaming: true,
                duration: Duration::from_secs(duration.max(0) as u64),
                w: width,
                h: height,
            });
        if let Some(thumb_name) = &thumb_name
            && let Ok(thumb) = client.upload_file(Path::new(thumb_name)).await
        {
            input = input.thumbnail(thumb);
        }
        message.respond(input).await?;
        Ok(())
    }
    .await;

    if let Some(thumb_name) = &thumb_name
        && Path::new(thumb_name).exists()
    {
        let _ = std::fs::remove_file(thumb_name);
    }

    result
}

async fn send_image(client: &Client, message: &UpdateMessage, path: &Path) -> anyhow::Result<()> {
    let photo = client.upload_file(path).await?;
    message.respond(InputMessage::new().photo(photo)).await?;
    Ok(())
}

fn perform_cleanup(
    cleanup_path: &Option<PathBuf>,
    yt_dlp_base: &Option<String>,
    is_yt_dlp: bool,
    info: &str,
) {
    if let Some(path) = cleanup_path
        && path.exists()
    {
        if is_yt_dlp && path.is_file() {
            match std::fs::remove_file(path) {
                Ok(()) => println!("{info} | Cleaned up yt-dlp file: {}", path.display()),
                Err(e) => println!("{info} | Error during cleanup of {}: {e}", path.display()),
            }
            return;
        }
        if !is_yt_dlp && path.is_dir() {
            match std::fs::remove_dir_all(path) {
                Ok(()) => println!(
                    "{info} | Cleaned up gallery-dl directory: {}",
                    path.display()
                ),
                Err(e) => println!("{info} | Error during cleanup of {}: {e}", path.display()),
            }
            return;
        }
    }

    // Orphaned yt-dlp files (prefix-based) cleanup.
    if is_yt_dlp
        && let Some(base) = yt_dlp_base
        && let Ok(entries) = std::fs::read_dir(".")
    {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(base) {
                match std::fs::remove_file(entry.path()) {
                    Ok(()) => println!("{info} | Cleaned up orphaned yt-dlp file: {name}"),
                    Err(e) => {
                        println!("{info} | Error cleaning up orphaned yt-dlp file {name}: {e}")
                    }
                }
            }
        }
    }
}
