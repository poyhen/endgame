use regex::Regex;

use crate::media::request::DownloadRequest;

pub enum MessageAction {
    AddUser(Option<i64>),
    InstagramCookies,
    HealthCheck,
    QueueStatus,
    Cancel(Option<u64>),
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
        Some(name) if name == "insta" => MessageAction::InstagramCookies,
        Some(name) if name == "h" || name == "ping" => MessageAction::HealthCheck,
        Some(name) if name == "status" || name == "queue" => MessageAction::QueueStatus,
        Some(name) if name == "help" => MessageAction::Reply(
            "Send one or more links, or use /audio <url>, /video [height] <url>, /best <url>, /cancel <job-id>, /status, or /ping. Superusers can use /add <user-id>.",
        ),
        Some(name) if name == "cancel" => MessageAction::Cancel(
            text.split_whitespace()
                .nth(1)
                .and_then(|value| value.parse().ok()),
        ),
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
}
