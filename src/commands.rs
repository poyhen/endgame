use regex::Regex;
use std::time::Duration;

use crate::media::request::{ClipRange, DownloadRequest};

const CLIP_USAGE: &str =
    "Usage: /clip <start> <end> <url> (timestamps: SS, MM:SS, or HH:MM:SS[.mmm])";

pub enum MessageAction {
    AddUser(Option<i64>),
    RemoveUser(Option<i64>),
    ListUsers,
    InstagramCookies,
    HealthCheck,
    QueueStatus,
    Cancel(Option<u64>),
    Retry,
    Downloads(Vec<DownloadRequest>),
    Reply(&'static str),
    None,
}

pub fn classify_message(text: &str, url_pattern: &Regex) -> MessageAction {
    match command_name(text) {
        Some(name) if name == "add" => MessageAction::AddUser(
            text.split_whitespace()
                .nth(1)
                .and_then(|value| value.parse().ok())
                .filter(|user_id| *user_id > 0),
        ),
        Some(name) if name == "remove" => MessageAction::RemoveUser(
            text.split_whitespace()
                .nth(1)
                .and_then(|value| value.parse().ok())
                .filter(|user_id| *user_id > 0),
        ),
        Some(name) if name == "users" => MessageAction::ListUsers,
        Some(name) if name == "insta" => MessageAction::InstagramCookies,
        Some(name) if name == "h" || name == "ping" => MessageAction::HealthCheck,
        Some(name) if name == "status" || name == "queue" => MessageAction::QueueStatus,
        Some(name) if name == "help" => MessageAction::Reply(
            "Send one or more links, or use /audio <url>, /video [height] <url>, /best <url>, /clip <start> <end> <url>, /cancel <job-id>, /status, or /ping. You can also reply to a download status with /cancel or /retry. Superusers can use /add <user-id>, /remove <user-id>, and /users.",
        ),
        Some(name) if name == "cancel" => MessageAction::Cancel(
            text.split_whitespace()
                .nth(1)
                .and_then(|value| value.parse().ok()),
        ),
        Some(name) if name == "retry" => MessageAction::Retry,
        Some(name) if name == "clip" => classify_clip(text, url_pattern),
        Some(name) if name == "audio" => {
            let requests: Vec<_> = url_pattern
                .find_iter(text)
                .map(|url| DownloadRequest::audio(url.as_str().to_string()))
                .collect();
            if requests.is_empty() {
                MessageAction::Reply("Usage: /audio <url>")
            } else {
                MessageAction::Downloads(requests)
            }
        }
        Some(name) if name == "video" || name == "best" => {
            let max_height = (name == "video")
                .then(|| text.split_whitespace().nth(1))
                .flatten()
                .filter(|value| !value.starts_with("http://") && !value.starts_with("https://"))
                .and_then(|value| value.trim_end_matches(['p', 'P']).parse::<u32>().ok());
            let requests: Vec<_> = url_pattern
                .find_iter(text)
                .map(|url| DownloadRequest::video(url.as_str().to_string(), max_height))
                .collect();
            if requests.is_empty() {
                MessageAction::Reply("Usage: /video [height] <url> or /best <url>")
            } else {
                MessageAction::Downloads(requests)
            }
        }
        _ => {
            let urls: Vec<DownloadRequest> = url_pattern
                .find_iter(text)
                .map(|found| DownloadRequest::video(found.as_str().to_string(), None))
                .collect();
            if urls.is_empty() {
                MessageAction::None
            } else {
                MessageAction::Downloads(urls)
            }
        }
    }
}

fn classify_clip(text: &str, url_pattern: &Regex) -> MessageAction {
    let mut parts = text.split_whitespace();
    let _command = parts.next();
    let Some(start) = parts.next().and_then(parse_timestamp) else {
        return MessageAction::Reply(CLIP_USAGE);
    };
    let Some(end) = parts.next().and_then(parse_timestamp) else {
        return MessageAction::Reply(CLIP_USAGE);
    };
    let Some(range) = ClipRange::new(start, end) else {
        return MessageAction::Reply(CLIP_USAGE);
    };

    let requests: Vec<_> = url_pattern
        .find_iter(text)
        .map(|url| DownloadRequest::clip(url.as_str().to_string(), range))
        .collect();
    if requests.is_empty() {
        MessageAction::Reply(CLIP_USAGE)
    } else {
        MessageAction::Downloads(requests)
    }
}

fn parse_timestamp(value: &str) -> Option<Duration> {
    let parts: Vec<_> = value.split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }

    let (seconds, milliseconds) = parse_seconds(parts[parts.len() - 1])?;
    let total_seconds = match parts.as_slice() {
        [_] => seconds,
        [minutes, _] if seconds < 60 => parse_whole(minutes)?
            .checked_mul(60)?
            .checked_add(seconds)?,
        [hours, minutes, _] if seconds < 60 => {
            let minutes = parse_whole(minutes)?;
            if minutes >= 60 {
                return None;
            }
            parse_whole(hours)?
                .checked_mul(3_600)?
                .checked_add(minutes.checked_mul(60)?)?
                .checked_add(seconds)?
        }
        _ => return None,
    };
    let total_milliseconds = total_seconds
        .checked_mul(1_000)?
        .checked_add(milliseconds)?;
    Some(Duration::from_millis(total_milliseconds))
}

fn parse_seconds(value: &str) -> Option<(u64, u64)> {
    let mut parts = value.split('.');
    let seconds = parse_whole(parts.next()?)?;
    let milliseconds = match parts.next() {
        None => 0,
        Some(fraction)
            if !fraction.is_empty()
                && fraction.len() <= 3
                && fraction.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            fraction.parse::<u64>().ok()? * 10u64.pow(3 - fraction.len() as u32)
        }
        Some(_) => return None,
    };
    if parts.next().is_some() {
        return None;
    }
    Some((seconds, milliseconds))
}

fn parse_whole(value: &str) -> Option<u64> {
    (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}

fn command_name(text: &str) -> Option<String> {
    let first = text.split_whitespace().next()?;
    let body = first.strip_prefix('/')?;
    let cmd = body.split('@').next()?;
    if cmd.is_empty() {
        return None;
    }
    Some(cmd.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern() -> Regex {
        Regex::new(r"https?://\S+").unwrap()
    }

    #[test]
    fn parses_cancel_command_with_optional_bot_name() {
        assert!(matches!(
            classify_message("/cancel 42", &pattern()),
            MessageAction::Cancel(Some(42))
        ));
        assert!(matches!(
            classify_message("/cancel@endgame 7", &pattern()),
            MessageAction::Cancel(Some(7))
        ));
        assert!(matches!(
            classify_message("/cancel nope", &pattern()),
            MessageAction::Cancel(None)
        ));
    }

    #[test]
    fn parses_reply_commands_without_job_ids() {
        assert!(matches!(
            classify_message("/cancel", &pattern()),
            MessageAction::Cancel(None)
        ));
        assert!(matches!(
            classify_message("/retry", &pattern()),
            MessageAction::Retry
        ));
        assert!(matches!(
            classify_message("/retry@endgame", &pattern()),
            MessageAction::Retry
        ));
    }

    #[test]
    fn parses_users_command() {
        assert!(matches!(
            classify_message("/users", &pattern()),
            MessageAction::ListUsers
        ));
        assert!(matches!(
            classify_message("/users@endgame", &pattern()),
            MessageAction::ListUsers
        ));
    }

    #[test]
    fn parses_add_command_and_rejects_invalid_ids() {
        assert!(matches!(
            classify_message("/add 42", &pattern()),
            MessageAction::AddUser(Some(42))
        ));
        assert!(matches!(
            classify_message("/add@endgame 7", &pattern()),
            MessageAction::AddUser(Some(7))
        ));
        assert!(matches!(
            classify_message("/add nope", &pattern()),
            MessageAction::AddUser(None)
        ));
        assert!(matches!(
            classify_message("/add -1", &pattern()),
            MessageAction::AddUser(None)
        ));
    }

    #[test]
    fn parses_remove_command_and_rejects_invalid_ids() {
        assert!(matches!(
            classify_message("/remove 42", &pattern()),
            MessageAction::RemoveUser(Some(42))
        ));
        assert!(matches!(
            classify_message("/remove@endgame 7", &pattern()),
            MessageAction::RemoveUser(Some(7))
        ));
        assert!(matches!(
            classify_message("/remove nope", &pattern()),
            MessageAction::RemoveUser(None)
        ));
        assert!(matches!(
            classify_message("/remove -1", &pattern()),
            MessageAction::RemoveUser(None)
        ));
    }

    #[test]
    fn extracts_every_url_from_a_message() {
        let MessageAction::Downloads(urls) = classify_message(
            "first https://example.com/a then https://example.org/b",
            &pattern(),
        ) else {
            panic!("expected download action");
        };
        assert_eq!(
            urls.iter()
                .map(|request| request.url.as_str())
                .collect::<Vec<_>>(),
            ["https://example.com/a", "https://example.org/b"]
        );
    }

    #[test]
    fn parses_audio_and_video_format_commands() {
        let MessageAction::Downloads(audio) =
            classify_message("/audio https://example.com/a", &pattern())
        else {
            panic!("expected audio download");
        };
        assert!(matches!(
            audio[0].mode,
            crate::media::request::DownloadMode::Audio
        ));

        let MessageAction::Downloads(video) =
            classify_message("/video 720p https://example.com/v", &pattern())
        else {
            panic!("expected video download");
        };
        assert!(matches!(
            video[0].mode,
            crate::media::request::DownloadMode::Video {
                max_height: Some(720)
            }
        ));
    }

    #[test]
    fn parses_clip_ranges_and_multiple_urls() {
        let MessageAction::Downloads(clips) = classify_message(
            "/clip@endgame 1:02.500 2:03 https://example.com/a https://example.org/b",
            &pattern(),
        ) else {
            panic!("expected clip downloads");
        };
        assert_eq!(clips.len(), 2);
        assert!(clips.iter().all(|request| matches!(
            request.mode,
            crate::media::request::DownloadMode::Clip(range)
                if range.start() == Duration::from_millis(62_500)
                    && range.end() == Duration::from_secs(123)
        )));
    }

    #[test]
    fn parses_clip_timestamp_formats() {
        assert_eq!(parse_timestamp("90"), Some(Duration::from_secs(90)));
        assert_eq!(parse_timestamp("1:02"), Some(Duration::from_secs(62)));
        assert_eq!(
            parse_timestamp("1:02:03.004"),
            Some(Duration::from_millis(3_723_004))
        );
    }

    #[test]
    fn rejects_invalid_clip_ranges_without_downloading_the_url() {
        for text in [
            "/clip 20 10 https://example.com/v",
            "/clip 10 10 https://example.com/v",
            "/clip 1:60 2:00 https://example.com/v",
            "/clip 1:60:00 2:00:00 https://example.com/v",
            "/clip -1 10 https://example.com/v",
            "/clip 1.0000 10 https://example.com/v",
            "/clip 10 20",
        ] {
            assert!(matches!(
                classify_message(text, &pattern()),
                MessageAction::Reply(CLIP_USAGE)
            ));
        }
    }

    #[test]
    fn help_mentions_clip_command() {
        assert!(matches!(
            classify_message("/help", &pattern()),
            MessageAction::Reply(text) if text.contains("/clip <start> <end> <url>")
        ));
    }
}
