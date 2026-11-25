use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde_json::{Value, json};
use tokio::process::Command;
use url::Url;

use crate::media::downloader::{configure_yt_dlp, gallery_dl_command, yt_dlp_command};
use crate::media::request::{DownloadLimits, DownloadMode};

pub const COOKIE_HINT: &str = "Instagram may require account verification. Open Instagram in your browser, complete any checkpoint, then refresh the cookies with /insta.";

pub fn is_post(url: &str) -> bool {
    let Ok(url) = Url::parse(url) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(url.host_str(), Some("instagram.com" | "www.instagram.com"))
    {
        return false;
    }
    let mut segments = url.path_segments().into_iter().flatten();
    segments.next() == Some("p") && segments.next().is_some_and(|id| !id.is_empty())
}

pub(crate) struct GalleryFiles {
    pub files: Vec<PathBuf>,
    pub photos: Vec<PathBuf>,
    pub has_music: bool,
}

/// gallery-dl writes metadata as `<media filename>.json`. The `num` field
/// reflects Instagram's order, including for carousels mixing photos and video.
pub(crate) async fn gallery_files(paths: Vec<PathBuf>) -> GalleryFiles {
    let mut metadata = HashMap::new();
    for path in &paths {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(media_name) = name.strip_suffix(".json") else {
            continue;
        };
        let Ok(bytes) = tokio::fs::read(path).await else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        let Some(object) = value.as_object() else {
            continue;
        };
        let has_music = object.contains_key("audio_title")
            || object.contains_key("audio_artist")
            || object.contains_key("audio_duration");
        metadata.insert(
            path.with_file_name(media_name),
            (object.get("num").and_then(Value::as_u64), has_music),
        );
    }

    let mut files: Vec<_> = paths
        .into_iter()
        .filter(|path| {
            !path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        })
        .collect();
    files.sort_by(|a, b| {
        let order = |path: &PathBuf| {
            metadata
                .get(path)
                .and_then(|(num, _)| *num)
                .unwrap_or(u64::MAX)
        };
        order(a).cmp(&order(b)).then_with(|| a.cmp(b))
    });
    let photos = files
        .iter()
        .filter(|path| {
            mime_guess::from_path(path)
                .first()
                .is_some_and(|mime| mime.type_() == mime::IMAGE)
        })
        .cloned()
        .collect();
    let has_music = files
        .iter()
        .any(|path| metadata.get(path).is_some_and(|(_, has_music)| *has_music));
    GalleryFiles {
        files,
        photos,
        has_music,
    }
}

/// Download only the post-level soundtrack. Instagram commonly serves it in
/// an audio-only MP4, so the containing extension is not used to identify it.
pub(crate) fn audio_command(url: &str, cookie_file: &Path, workspace: &Path) -> Command {
    let mut command = gallery_dl_command(cookie_file, workspace);
    command
        .arg("--no-postprocessors")
        .arg("-o")
        .arg("extractor.instagram.audio=merged")
        .arg("-o")
        .arg("extractor.instagram.previews=false")
        .arg("--filter")
        .arg("audio_url")
        .arg(url);
    command
}

pub fn should_try_public(url: &str, error: &str) -> bool {
    let Ok(url) = Url::parse(url) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(url.host_str(), Some("instagram.com" | "www.instagram.com"))
    {
        return false;
    }
    let mut segments = url.path_segments().into_iter().flatten();
    if !matches!(
        segments.next(),
        Some("p" | "reel" | "reels" | "tv" | "share")
    ) || segments.next().is_none_or(str::is_empty)
    {
        return false;
    }

    let error = error.to_ascii_lowercase();
    [
        "400 bad request",
        "401 unauthorized",
        "403 forbidden",
        "http error 400",
        "http error 401",
        "http error 403",
        "checkpoint_required",
        "login_required",
        "redirect to login page",
        "redirect to challenge page",
        "cookies are no longer valid",
    ]
    .iter()
    .any(|message| error.contains(message))
}

pub fn metadata_command(url: &str, path: &Path) -> Command {
    let mut command = yt_dlp_command();
    command
        .arg("--no-playlist")
        .arg("--skip-download")
        .arg("--ignore-no-formats-error")
        // A file avoids truncating large carousels at the command output limit.
        // yt-dlp emits one complete JSON record per carousel item.
        .arg("--print-to-file")
        .arg("%()j")
        .arg(path)
        .arg(url);
    command
}

pub fn download_command(
    url: &str,
    mode: &DownloadMode,
    limits: DownloadLimits,
    metadata: &Path,
    workspace: &Path,
) -> Command {
    let mut command = yt_dlp_command();
    configure_yt_dlp(&mut command, url, mode, limits);
    if let DownloadMode::Video {
        max_height: Some(height),
    } = mode
    {
        // A video height cap must not discard photos in a mixed carousel.
        command.arg("-f").arg(format!(
            "{}/best[ext=jpg]/best[ext=jpeg]/best[ext=png]/best[ext=webp]/best[ext=gif]",
            crate::media::downloader::video_format_selector(*height)
        ));
    }
    command
        .arg("-o")
        .arg(workspace.join("media-%(autonumber)03d.%(ext)s"))
        .arg("--load-info-json")
        .arg(metadata);
    command
}

pub fn prepare_metadata(source: &str, mode: &DownloadMode) -> anyhow::Result<Value> {
    let mut entries = Vec::new();
    for item in serde_json::Deserializer::from_str(source).into_iter::<Value>() {
        let mut item = item.context("could not parse public Instagram metadata")?;
        let object = item
            .as_object_mut()
            .context("public Instagram metadata was not an object")?;
        for field in [
            "requested_formats",
            "requested_downloads",
            "filepath",
            "_filename",
        ] {
            object.remove(field);
        }

        let has_video = object
            .get("formats")
            .and_then(Value::as_array)
            .is_some_and(|formats| !formats.is_empty());
        if !has_video {
            if object
                .get("duration")
                .is_some_and(|duration| !duration.is_null())
            {
                bail!("Instagram returned a video without downloadable formats");
            }
            if !matches!(mode, DownloadMode::Video { .. }) {
                continue;
            }
            // Instagram exposes the photo's image_versions2 through yt-dlp's
            // thumbnail list. These are the post images, not video previews.
            let image_url = object
                .get("thumbnails")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .rev()
                .filter_map(|thumbnail| thumbnail.get("url")?.as_str())
                .filter_map(|url| Url::parse(url).ok())
                .find(|url| matches!(url.scheme(), "http" | "https"))
                .context("public Instagram metadata contained no downloadable photo or video")?;
            let extension = image_url
                .path()
                .rsplit_once('.')
                .map(|(_, extension)| extension.to_ascii_lowercase())
                .filter(|extension| {
                    matches!(extension.as_str(), "jpg" | "jpeg" | "png" | "webp" | "gif")
                })
                .unwrap_or_else(|| "jpg".into());
            object.remove("formats");
            object.insert("url".into(), json!(image_url.as_str()));
            object.insert("ext".into(), json!(extension));
        }
        entries.push(item);
    }
    if entries.is_empty() {
        bail!("Instagram returned no public media for the requested mode");
    }
    // --load-info-json accepts an array and processes every item in order.
    // A synthetic playlist would have its entries stripped by yt-dlp cleanup.
    Ok(Value::Array(entries))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::request::ClipRange;
    use crate::media::workspace::JobWorkspace;
    use std::time::Duration;

    const PHOTO: &str = r#"{"id":"photo","formats":[],"thumbnails":[{"url":"https://cdn.example/preview.jpg"},{"url":"https://cdn.example/original.jpg?size=large"}]}"#;

    #[test]
    fn recognizes_only_instagram_post_urls() {
        assert!(is_post("https://www.instagram.com/p/post/?query=value"));
        assert!(is_post("https://instagram.com/p/post/"));
        for url in [
            "https://instagram.com/reel/post/",
            "https://instagram.com/p/",
            "https://instagram.com.example/p/post/",
            "https://example.com/?url=instagram.com/p/post/",
            "file://instagram.com/p/post/",
        ] {
            assert!(!is_post(url), "{url}");
        }
    }

    #[tokio::test]
    async fn orders_instagram_carousel_by_metadata_and_detects_music() {
        let workspace = JobWorkspace::create(100).await.unwrap();
        let image = workspace.path().join("z-first.jpg");
        let video = workspace.path().join("a-second.mp4");
        let last = workspace.path().join("b-third.jpg");
        let mut paths = Vec::new();
        for (path, number) in [(&image, 1), (&video, 2), (&last, 3)] {
            tokio::fs::write(path, b"media").await.unwrap();
            let sidecar = PathBuf::from(format!("{}.json", path.display()));
            tokio::fs::write(
                &sidecar,
                json!({"num": number, "audio_title": "Music"}).to_string(),
            )
            .await
            .unwrap();
            paths.push(sidecar);
            paths.push(path.to_path_buf());
        }
        paths.reverse();
        let gallery = gallery_files(paths).await;
        assert_eq!(gallery.files, vec![image.clone(), video, last.clone()]);
        assert_eq!(gallery.photos, vec![image, last]);
        assert!(gallery.has_music);
        workspace.cleanup().await;
    }

    #[tokio::test]
    async fn handles_posts_without_music_or_readable_sidecars() {
        let workspace = JobWorkspace::create(101).await.unwrap();
        let photo = workspace.path().join("photo.jpg");
        let sidecar = workspace.path().join("photo.jpg.json");
        tokio::fs::write(&photo, b"photo").await.unwrap();
        tokio::fs::write(&sidecar, b"not json").await.unwrap();
        let gallery = gallery_files(vec![sidecar, photo.clone()]).await;
        assert_eq!(gallery.files, vec![photo.clone()]);
        assert_eq!(gallery.photos, vec![photo]);
        assert!(!gallery.has_music);
        workspace.cleanup().await;
    }

    #[test]
    fn audio_download_targets_only_the_post_soundtrack() {
        let command = audio_command(
            "https://instagram.com/p/example/",
            Path::new("instagram-cookies.txt"),
            Path::new("/tmp/instagram-audio"),
        );
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.windows(2)
                .any(|args| args == ["--filter", "audio_url"])
        );
        assert!(
            args.iter()
                .any(|arg| arg == "extractor.instagram.audio=merged")
        );
    }

    #[test]
    fn retries_only_instagram_posts_with_access_errors() {
        let url = "https://www.instagram.com/p/DeYolYNTpaO/?cplk=MTNtN3p4NTVxZWJraQ==";
        for error in [
            "HttpError: '400 Bad Request'",
            "HTTP Error 401: Unauthorized",
            "HTTP redirect to challenge page",
            "checkpoint_required",
        ] {
            assert!(should_try_public(url, error), "{error}");
        }
        for error in [
            "429 Too Many Requests",
            "404 Not Found",
            "Connection timed out",
        ] {
            assert!(!should_try_public(url, error), "{error}");
        }
        for url in [
            "https://www.instagram.com/profile/",
            "https://www.instagram.com/p/",
            "https://instagram.com.example/p/post/",
            "https://example.com/?url=instagram.com/p/post/",
            "file://instagram.com/p/post/",
        ] {
            assert!(!should_try_public(url, "400 Bad Request"), "{url}");
        }
        assert!(should_try_public(
            "https://instagram.com/reel/post/",
            "403 Forbidden"
        ));
    }

    #[test]
    fn downloads_the_best_photo_instead_of_reporting_no_video() {
        let metadata = prepare_metadata(PHOTO, &DownloadMode::Video { max_height: None }).unwrap();
        let photo = &metadata[0];
        assert_eq!(photo["url"], "https://cdn.example/original.jpg?size=large");
        assert_eq!(photo["ext"], "jpg");
        assert!(photo.get("formats").is_none());
    }

    #[test]
    fn preserves_every_item_and_video_format_in_a_mixed_carousel() {
        let video = json!({
            "id": "video",
            "formats": [{"url": "https://cdn.example/video.mp4", "height": 1080}],
            "thumbnails": [{"url": "https://cdn.example/video-preview.jpg"}],
            "requested_formats": [{"height": 1080}],
        });
        let source = format!("{PHOTO}\n{video}\n{PHOTO}\n");
        let metadata = prepare_metadata(
            &source,
            &DownloadMode::Video {
                max_height: Some(720),
            },
        )
        .unwrap();
        let items = metadata.as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["id"], "photo");
        assert_eq!(items[1]["id"], "video");
        assert_eq!(items[2]["id"], "photo");
        assert_eq!(items[1]["formats"], video["formats"]);
        assert!(items[1].get("requested_formats").is_none());
        assert!(items[1].get("url").is_none());
    }

    #[test]
    fn photos_cannot_satisfy_audio_or_clip_requests() {
        let clip = ClipRange::new(Duration::from_secs(1), Duration::from_secs(2)).unwrap();
        for mode in [DownloadMode::Audio, DownloadMode::Clip(clip)] {
            assert!(prepare_metadata(PHOTO, &mode).is_err());
        }
    }

    #[test]
    fn audio_and_clip_requests_keep_videos_in_mixed_carousels() {
        let source = format!(
            "{PHOTO}\n{}",
            json!({"id":"video","formats":[{"url":"https://cdn.example/video.mp4"}]})
        );
        let clip = ClipRange::new(Duration::from_secs(1), Duration::from_secs(2)).unwrap();
        for mode in [DownloadMode::Audio, DownloadMode::Clip(clip)] {
            let metadata = prepare_metadata(&source, &mode).unwrap();
            assert_eq!(metadata.as_array().unwrap().len(), 1);
            assert_eq!(metadata[0]["id"], "video");
        }
    }

    #[test]
    fn rejects_empty_invalid_and_missing_media_metadata() {
        for source in [
            "",
            "not JSON",
            "[]",
            r#"{"id":"empty","formats":[],"thumbnails":[]}"#,
            r#"{"id":"unsafe","thumbnails":[{"url":"file:///etc/passwd"}]}"#,
            r#"{"id":"unavailable-video","duration":5,"formats":[],"thumbnails":[{"url":"https://cdn.example/poster.jpg"}]}"#,
        ] {
            assert!(prepare_metadata(source, &DownloadMode::Video { max_height: None }).is_err());
        }
    }
}
