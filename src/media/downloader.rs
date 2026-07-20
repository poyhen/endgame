use std::path::Path;
use std::time::Duration;

use tokio::process::Command;

use crate::media::request::{DownloadLimits, DownloadMode};

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:131.0) Gecko/20100101 Firefox/131.0";
const DEFAULT_FORMAT: &str =
    "bestvideo[vcodec!*=av01][vcodec!*=vp9][ext=mp4]+bestaudio[ext=m4a]/best[ext=mp4]/best";

pub struct DownloadCommand {
    pub command: Command,
    pub program: &'static str,
    pub reports_progress: bool,
}

pub fn build(
    url: &str,
    mode: &DownloadMode,
    cookie_file: &Path,
    workspace: &Path,
    limits: DownloadLimits,
) -> DownloadCommand {
    let use_yt_dlp =
        matches!(mode, DownloadMode::Audio | DownloadMode::Clip(_)) || should_use_yt_dlp(url);
    let program = if use_yt_dlp { "yt-dlp" } else { "gallery-dl" };
    let mut command = Command::new(program);

    if use_yt_dlp {
        command
            .arg("-o")
            .arg(workspace.join("media.%(ext)s"))
            .arg("--cookies")
            .arg(cookie_file)
            .arg("--user-agent")
            .arg(USER_AGENT)
            .arg("--newline")
            .arg("--progress-template")
            .arg("download:download:%(progress._percent_str)s");

        match mode {
            DownloadMode::Audio => {
                command
                    .arg("--extract-audio")
                    .arg("--audio-format")
                    .arg("mp3")
                    .arg("--audio-quality")
                    .arg("0");
            }
            DownloadMode::Clip(range) => {
                // Full-source filesize estimates would unnecessarily downgrade a short
                // clip from a long video. Enforce the upload limit on the finished clip.
                command
                    .arg("-f")
                    .arg(unrestricted_video_format(url))
                    .arg("--download-sections")
                    .arg(format!(
                        "*{}-{}",
                        format_clip_timestamp(range.start()),
                        format_clip_timestamp(range.end())
                    ))
                    .arg("--force-keyframes-at-cuts");
            }
            DownloadMode::Video { max_height }
                if url.contains("youtube.com/") || url.contains("youtu.be/") =>
            {
                command.arg("-f").arg(youtube_format_selector(
                    limits.max_upload_bytes,
                    *max_height,
                ));
            }
            DownloadMode::Video {
                max_height: Some(height),
            } => {
                command.arg("-f").arg(video_format_selector(*height));
            }
            DownloadMode::Video { max_height: None } => {
                command.arg("-f").arg(unrestricted_video_format(url));
            }
        }
        command.arg("--no-playlist").arg(url);
    } else {
        command
            .arg("--cookies")
            .arg(cookie_file)
            .arg("--user-agent")
            .arg(USER_AGENT)
            .arg("-D")
            .arg(workspace)
            .arg(url);
    }
    DownloadCommand {
        command,
        program,
        reports_progress: use_yt_dlp,
    }
}

fn unrestricted_video_format(url: &str) -> &'static str {
    if url.contains("tiktok.com") {
        "bestvideo[ext=mp4][vcodec=h264]+bestaudio[ext=m4a]/best[ext=mp4][vcodec=h264]/best"
    } else {
        DEFAULT_FORMAT
    }
}

fn format_clip_timestamp(timestamp: Duration) -> String {
    let total_milliseconds = timestamp.as_millis();
    let milliseconds = total_milliseconds % 1_000;
    let total_seconds = total_milliseconds / 1_000;
    let seconds = total_seconds % 60;
    let total_minutes = total_seconds / 60;
    let minutes = total_minutes % 60;
    let hours = total_minutes / 60;

    let mut formatted = if hours == 0 {
        format!("{total_minutes}:{seconds:02}")
    } else {
        format!("{hours}:{minutes:02}:{seconds:02}")
    };
    if milliseconds != 0 {
        formatted.push_str(&format!(".{milliseconds:03}"));
    }
    formatted
}

fn should_use_yt_dlp(url: &str) -> bool {
    [
        "youtube.com/",
        "youtu.be/",
        "instagram.com/share/",
        "instagram.com/reel/",
        "instagram.com/reels/",
        "instagram.com/tv/",
        "x.com/i/broadcasts/",
        "tiktok.com/",
    ]
    .iter()
    .any(|domain| url.contains(domain))
}

pub(crate) fn video_format_selector(max_height: u32) -> String {
    format!(
        "bestvideo[height<={max_height}][vcodec!*=av01][vcodec!*=vp9][ext=mp4]+bestaudio[ext=m4a]/best[height<={max_height}][ext=mp4]/best[height<={max_height}]"
    )
}

pub(crate) fn youtube_format_selector(max_upload_bytes: u64, max_height: Option<u32>) -> String {
    let video_budget = max_upload_bytes.saturating_mul(90) / 100;
    let height = max_height
        .map(|height| format!("[height<={height}]"))
        .unwrap_or_default();
    let fallback = max_height.map_or_else(|| DEFAULT_FORMAT.to_string(), video_format_selector);
    format!(
        "bestvideo[ext=mp4][vcodec^=avc1]{height}[filesize<{video_budget}]+bestaudio[ext=m4a]/\
         bestvideo[ext=mp4][vcodec^=avc1]{height}[filesize_approx<{video_budget}]+bestaudio[ext=m4a]/\
         best[ext=mp4][vcodec^=avc1]{height}[filesize<{max_upload_bytes}]/\
         best[ext=mp4][vcodec^=avc1]{height}[filesize_approx<{max_upload_bytes}]/{fallback}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::request::ClipRange;

    fn limits() -> DownloadLimits {
        DownloadLimits {
            max_upload_bytes: 1_000_000,
            command_timeout: Duration::from_secs(60),
            upload_timeout: Duration::from_secs(60),
            job_timeout: Duration::from_secs(60),
        }
    }

    fn arguments(download: &DownloadCommand) -> Vec<String> {
        download
            .command
            .as_std()
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn quality_selector_caps_every_fallback() {
        assert_eq!(video_format_selector(720).matches("height<=720").count(), 3);
    }

    #[test]
    fn youtube_selector_prefers_compatible_media_within_budget() {
        let selector = youtube_format_selector(1_000_000, Some(720));
        assert!(selector.contains("[vcodec^=avc1][height<=720][filesize<900000]"));
        assert!(selector.contains("[filesize_approx<900000]"));
        assert!(selector.ends_with(&video_format_selector(720)));
    }

    #[test]
    fn instagram_reel_urls_use_yt_dlp() {
        for url in [
            "https://www.instagram.com/reel/DbAFv_jMyOy/?igsh=Zmk3cGl5eTNmNGk4",
            "https://www.instagram.com/reels/DbAFv_jMyOy/",
        ] {
            let download = build(
                url,
                &DownloadMode::Video { max_height: None },
                Path::new("instacookies.txt"),
                Path::new("/tmp/endgame-test"),
                limits(),
            );

            assert_eq!(download.program, "yt-dlp", "unexpected downloader for {url}");
            assert!(download.reports_progress);
        }
    }

    #[test]
    fn clip_forces_yt_dlp_with_an_accurate_time_range() {
        let range = ClipRange::new(
            Duration::from_millis(62_500),
            Duration::from_millis(125_250),
        )
        .unwrap();
        let download = build(
            "https://example.com/video",
            &DownloadMode::Clip(range),
            Path::new("cookies.txt"),
            Path::new("/tmp/endgame-test"),
            limits(),
        );
        let args = arguments(&download);

        assert_eq!(download.program, "yt-dlp");
        assert!(download.reports_progress);
        assert!(
            args.windows(2).any(|pair| {
                pair[0] == "--download-sections" && pair[1] == "*1:02.500-2:05.250"
            })
        );
        assert!(
            args.iter()
                .any(|argument| argument == "--force-keyframes-at-cuts")
        );
        assert!(args.iter().any(|argument| argument == "--no-playlist"));
        assert!(!args.iter().any(|argument| argument == "--extract-audio"));
    }

    #[test]
    fn youtube_clips_do_not_use_full_video_filesize_estimates() {
        let range = ClipRange::new(Duration::from_secs(10), Duration::from_secs(20)).unwrap();
        let clip = build(
            "https://youtube.com/watch?v=example",
            &DownloadMode::Clip(range),
            Path::new("cookies.txt"),
            Path::new("/tmp/endgame-test"),
            limits(),
        );
        let clip_args = arguments(&clip);
        assert!(clip_args.iter().any(|argument| argument == DEFAULT_FORMAT));
        assert!(
            !clip_args
                .iter()
                .any(|argument| argument.contains("filesize<"))
        );

        let full_video = build(
            "https://youtube.com/watch?v=example",
            &DownloadMode::Video { max_height: None },
            Path::new("cookies.txt"),
            Path::new("/tmp/endgame-test"),
            limits(),
        );
        assert!(
            arguments(&full_video)
                .iter()
                .any(|argument| argument.contains("filesize<"))
        );
    }
}
