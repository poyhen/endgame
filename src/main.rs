mod cancel;
mod config;
mod download;
mod queue;
mod utils;

use std::future::Future;
use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use grammers_client::client::UpdatesConfiguration;
use grammers_client::session::storages::SqliteSession;
use grammers_client::update::Message as UpdateMessage;
use grammers_client::update::Update;
use grammers_client::{Client, SenderPool, SignInError};
use grammers_session::types::PeerKind;
use regex::Regex;
use simple_logger::SimpleLogger;
use tokio::task::JoinSet;

use config::Config;
use download::DownloadRequest;
use queue::DownloadQueue;

const SESSION_FILE: &str = "userbot.session";
const MAX_HANDLER_TASKS: usize = 64;
const HANDLER_TIMEOUT: Duration = Duration::from_secs(15);

type AnyResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[tokio::main]
async fn main() -> AnyResult<()> {
    let _ = SimpleLogger::new()
        .with_level(log::LevelFilter::Info)
        .init();

    let cfg = Config::from_env()?;

    let session = Arc::new(SqliteSession::open(SESSION_FILE).await?);

    let SenderPool {
        runner,
        handle,
        updates,
    } = SenderPool::new(Arc::clone(&session), cfg.api_id);
    let client = Client::new(handle.clone());
    let _pool_task = tokio::spawn(runner.run());

    if !client.is_authorized().await? {
        println!("Signing in...");
        let phone = prompt("Enter your phone number (international format): ")?;
        let token = client.request_login_code(&phone, &cfg.api_hash).await?;
        let code = prompt("Enter the code you received: ")?;
        match client.sign_in(&token, &code).await {
            Err(SignInError::PasswordRequired(password_token)) => {
                let hint = password_token.hint().unwrap_or("");
                let password = prompt(&format!("Enter the password (hint {hint}): "))?;
                client
                    .check_password(password_token, password.trim())
                    .await?;
            }
            Ok(_) => {}
            Err(e) => return Err(e.into()),
        }
        println!("Signed in!");
    }

    println!("Userbot is running...");

    let download_queue = DownloadQueue::new(
        client.clone(),
        cfg.download_concurrency,
        cfg.download_queue_capacity,
        cfg.max_upload_size_mb,
    );
    let download_queue_handle = download_queue.handle();
    println!(
        "Download queue ready ({} active, {} waiting).",
        cfg.download_concurrency, cfg.download_queue_capacity
    );

    let super_users = Arc::new(cfg.super_users.clone());
    let url_pattern: Regex = cfg.url_pattern;

    // Don't replay updates that arrived while we were offline. Those would be
    // links sent during downtime, and reprocessing them all at once on startup
    // causes massive download/upload spikes. Only handle links that arrive live.
    let mut updates = client
        .stream_updates(
            updates,
            UpdatesConfiguration {
                catch_up: false,
                ..Default::default()
            },
        )
        .await?;

    let mut handler_tasks = JoinSet::new();

    loop {
        // Reap finished handler tasks; surface any panics.
        while let Some(res) = handler_tasks.try_join_next() {
            if let Err(e) = res {
                log::warn!("Handler task ended unexpectedly: {e}");
            }
        }

        let update = tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            update = updates.next() => update,
        };

        let update = match update {
            Ok(u) => u,
            Err(e) => {
                log::error!("Error receiving update: {e}");
                continue;
            }
        };
        let Update::NewMessage(message) = update else {
            continue;
        };
        log::info!(
            "Received message (outgoing={}): {:?}",
            message.outgoing(),
            message.text()
        );

        if message.outgoing() {
            continue;
        }

        // Only handle private (user) conversations. Use peer_id() (always present),
        // not peer() which can return None on a cache miss and silently drop messages.
        let peer_id = message.peer_id();
        if peer_id.kind() != PeerKind::User {
            continue;
        }
        let Some(uid) = peer_id.bare_id() else {
            continue;
        };
        if !cfg.allowed_user_ids.contains(&uid) {
            log::info!("Ignoring unauthorized user {uid}");
            continue;
        }

        log::info!("Dispatching message from user {uid}");
        let text = message.text().to_string();
        match classify_message(&text, &url_pattern) {
            MessageAction::InstagramCookies => {
                let supers = Arc::clone(&super_users);
                spawn_handler(&mut handler_tasks, async move {
                    handle_insta(&message, &text, uid, &supers).await;
                });
            }
            MessageAction::HealthCheck => {
                spawn_handler(&mut handler_tasks, async move {
                    let _ = message.reply("alive").await;
                });
            }
            MessageAction::QueueStatus => {
                let status = download_queue_handle.status_report();
                spawn_handler(&mut handler_tasks, async move {
                    let _ = message.reply(status).await;
                });
            }
            MessageAction::Downloads(urls) => {
                for url in urls {
                    if handler_tasks.len() >= MAX_HANDLER_TASKS {
                        log::warn!("Skipping a URL because no acknowledgement slot is available");
                        break;
                    }
                    let reply = download_queue_handle.try_enqueue(message.clone(), uid, url);
                    if reply.is_accepted() {
                        spawn_definitive_handler(&mut handler_tasks, reply.send());
                    } else {
                        spawn_handler(&mut handler_tasks, reply.send());
                    }
                }
            }
            MessageAction::Cancel(Some(id)) => {
                let reply = download_queue_handle.try_cancel(message, uid, id);
                spawn_handler(&mut handler_tasks, reply.send());
            }
            MessageAction::Cancel(None) => {
                spawn_handler(&mut handler_tasks, async move {
                    let _ = message.reply("Usage: /cancel <job-id>").await;
                });
            }
            MessageAction::Reply(text) => {
                spawn_handler(&mut handler_tasks, async move {
                    let _ = message.reply(text).await;
                });
            }
            MessageAction::None => {}
        }
    }

    // Close admission first, finish bounded acknowledgement tasks, then drain
    // accepted downloads before disconnecting their Telegram client.
    drop(download_queue_handle);
    while handler_tasks.join_next().await.is_some() {}
    println!("Draining download queue...");
    download_queue.shutdown().await;

    println!("Saving session file...");
    let _ = updates.sync_update_state().await;

    println!("Gracefully closing connection...");
    handle.quit();

    Ok(())
}

enum MessageAction {
    InstagramCookies,
    HealthCheck,
    QueueStatus,
    Cancel(Option<u64>),
    Downloads(Vec<DownloadRequest>),
    Reply(&'static str),
    None,
}

fn classify_message(text: &str, url_pattern: &Regex) -> MessageAction {
    match command_name(text) {
        Some(name) if name == "insta" => MessageAction::InstagramCookies,
        Some(name) if name == "h" || name == "ping" => MessageAction::HealthCheck,
        Some(name) if name == "status" || name == "queue" => MessageAction::QueueStatus,
        Some(name) if name == "help" => MessageAction::Reply(
            "Send one or more links, or use /audio <url>, /video [height] <url>, /best <url>, /cancel <job-id>, /status, or /ping.",
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

fn spawn_handler<F>(tasks: &mut JoinSet<()>, future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    if tasks.len() >= MAX_HANDLER_TASKS {
        log::warn!("Dropping a Telegram acknowledgement because the handler limit was reached");
        return;
    }

    tasks.spawn(async move {
        if tokio::time::timeout(HANDLER_TIMEOUT, future).await.is_err() {
            log::warn!("Telegram acknowledgement timed out");
        }
    });
}

fn spawn_definitive_handler<F>(tasks: &mut JoinSet<()>, future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    if tasks.len() >= MAX_HANDLER_TASKS {
        log::warn!("Dropping a Telegram acknowledgement because the handler limit was reached");
        return;
    }
    tasks.spawn(future);
}

async fn handle_insta(message: &UpdateMessage, text: &str, sender_id: i64, super_users: &[i64]) {
    if !super_users.contains(&sender_id) {
        let _ = message
            .reply("You are not authorized to use this command.")
            .await;
        return;
    }

    let content = text
        .split_once(char::is_whitespace)
        .map(|x| x.1)
        .unwrap_or("")
        .trim();
    if content.is_empty() {
        let _ = message
            .reply("Please provide the cookie content after the command.")
            .await;
        return;
    }

    if !valid_cookie_content(content) {
        let _ = message
            .reply("Cookie content does not look like a Netscape cookie file.")
            .await;
        return;
    }

    if let Err(error) = message.delete().await {
        log::warn!("Could not delete the Instagram cookie message: {error}");
    }

    match write_secret_file(Path::new("instacookies.txt"), content) {
        Ok(_) => {
            let _ = message
                .respond("Instagram cookies updated successfully.")
                .await;
        }
        Err(e) => {
            let _ = message
                .respond(format!("Failed to update cookies: {e}"))
                .await;
        }
    }
}

fn valid_cookie_content(content: &str) -> bool {
    content.lines().any(|line| {
        let line = line.trim();
        !line.is_empty() && !line.starts_with('#') && line.split_whitespace().count() >= 6
    })
}

fn write_secret_file(path: &Path, content: &str) -> std::io::Result<()> {
    let temporary = path.with_extension(format!("tmp-{:016x}", rand::random::<u64>()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let write_result = (|| {
        let mut file = options.open(&temporary)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

/// Extracts the command name from a message, mirroring pyrogram's `filters.command`.
/// Returns the lowercase command name (without the `/` or any `@bot` suffix) if present.
fn command_name(text: &str) -> Option<String> {
    let first = text.split_whitespace().next()?;
    let body = first.strip_prefix('/')?;
    let cmd = body.split('@').next()?;
    if cmd.is_empty() {
        return None;
    }
    Some(cmd.to_lowercase())
}

fn prompt(message: &str) -> AnyResult<String> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    stdout.write_all(message.as_bytes())?;
    stdout.flush()?;

    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    let mut line = String::new();
    stdin.read_line(&mut line)?;
    Ok(line.trim().to_string())
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
        assert!(matches!(audio[0].mode, download::DownloadMode::Audio));

        let MessageAction::Downloads(video) =
            classify_message("/video 720p https://example.com/v", &pattern())
        else {
            panic!("expected video download");
        };
        assert!(matches!(
            video[0].mode,
            download::DownloadMode::Video {
                max_height: Some(720)
            }
        ));
    }

    #[test]
    fn validates_cookie_file_shape() {
        assert!(valid_cookie_content(
            ".instagram.com TRUE / TRUE 0 sessionid secret"
        ));
        assert!(!valid_cookie_content("sessionid=secret"));
        assert!(!valid_cookie_content("# Netscape HTTP Cookie File"));
    }
}
