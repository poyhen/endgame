mod config;
mod download;
mod utils;

use std::io::{BufRead, Write};
use std::sync::Arc;

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

const SESSION_FILE: &str = "userbot.session";

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
        let client = client.clone();
        let supers = Arc::clone(&super_users);
        let url_re = url_pattern.clone();
        handler_tasks.spawn(handle_message(client, message, uid, text, supers, url_re));
    }

    println!("Saving session file...");
    let _ = updates.sync_update_state().await;

    println!("Gracefully closing connection...");
    handle.quit();
    while handler_tasks.join_next().await.is_some() {}

    Ok(())
}

async fn handle_message(
    client: Client,
    message: UpdateMessage,
    sender_id: i64,
    text: String,
    super_users: Arc<Vec<i64>>,
    url_pattern: Regex,
) {
    match command_name(&text) {
        Some(name) if name == "insta" => {
            handle_insta(&message, &text, sender_id, &super_users).await;
        }
        Some(name) if name == "h" => {
            let _ = message.reply("alive").await;
        }
        _ => {
            if let Some(found) = url_pattern.find(&text) {
                download::download_and_upload(client, message, found.as_str().to_string()).await;
            }
        }
    }
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

    match std::fs::write("instacookies.txt", content) {
        Ok(_) => {
            let _ = message
                .reply("Instagram cookies updated successfully.")
                .await;
        }
        Err(e) => {
            let _ = message
                .reply(format!("Failed to update cookies: {e}"))
                .await;
        }
    }
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
