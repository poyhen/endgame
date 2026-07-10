use std::path::Path;

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
    let use_yt_dlp = matches!(mode, DownloadMode::Audio) || should_use_yt_dlp(url);
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
                let format = if url.contains("tiktok.com") {
                    "bestvideo[ext=mp4][vcodec=h264]+bestaudio[ext=m4a]/best[ext=mp4][vcodec=h264]/best"
                } else {
                    DEFAULT_FORMAT
                };
                command.arg("-f").arg(format);
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
}
