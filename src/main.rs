mod cancel;
mod commands;
mod config;
mod cookies;
mod jobs;
mod media;
mod telegram;
mod users;

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use grammers_client::sender::{ConnectionParams, UpdatesConfiguration};
use grammers_client::session::storages::SqliteSession;
use grammers_client::update::Update;
use grammers_client::{Client, SenderPool};
use grammers_session::Session;
use grammers_session::types::PeerKind;
use regex::Regex;
use tokio::task::JoinSet;

use commands::{MessageAction, classify_message};
use config::Config;
use jobs::DownloadQueue;
use jobs::inbox::{Inbox, Recovery};
use media::request::{DownloadLimits, DownloadSettings};
use telegram::allowed_users::AllowedUsers;

const SESSION_FILE: &str = "userbot.session";
const MAX_HANDLER_TASKS: usize = 64;
const HANDLER_TIMEOUT: Duration = Duration::from_secs(15);

type AnyResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[tokio::main]
async fn main() -> AnyResult<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_ansi(false)
        .try_init()?;

    let cfg = Config::load()?;
    log::info!(
        "Telegram connections will use {}",
        if cfg.telegram_use_ipv6 {
            "IPv6"
        } else {
            "IPv4"
        }
    );

    let session = Arc::new(SqliteSession::open(SESSION_FILE).await?);

    let SenderPool {
        runner,
        handle,
        updates,
    } = SenderPool::with_configuration(
        Arc::clone(&session),
        cfg.api_id,
        ConnectionParams {
            use_ipv6: cfg.telegram_use_ipv6,
            retry_policy: Box::new(telegram::retry::ConnectionRetryPolicy::default()),
            ..Default::default()
        },
    );
    let client = Client::new(handle.clone());
    let _pool_task = tokio::spawn(runner.run());

    telegram::auth::ensure_authorized(&client, &cfg.api_hash).await?;

    log::info!("Userbot is running");

    let inbox = Arc::new(Inbox::open(Path::new("downloads.sqlite")).await?);

    let download_settings = DownloadSettings {
        limits: DownloadLimits {
            max_upload_bytes: (cfg.max_upload_size_mb as u64).saturating_mul(1024 * 1024),
            command_timeout: Duration::from_secs(cfg.command_timeout_secs as u64),
            upload_timeout: Duration::from_secs(cfg.upload_timeout_secs as u64),
            job_timeout: Duration::from_secs(cfg.job_timeout_secs as u64),
        },
        yt_dlp_domains: cfg.yt_dlp_domains,
    };
    let download_queue = DownloadQueue::new(
        client.clone(),
        cfg.download_concurrency,
        cfg.download_queue_capacity,
        download_settings,
        Arc::clone(&inbox),
    );
    let download_queue_handle = download_queue.handle();
    log::info!(
        "Download queue ready ({} active, {} waiting).",
        cfg.download_concurrency,
        cfg.download_queue_capacity
    );

    let super_users = Arc::new(cfg.super_users.clone());
    let allowed_users = Arc::new(AllowedUsers::new(
        cfg.allowed_users.iter().copied(),
        &cfg.config_path,
    ));
    let url_pattern: Regex = cfg.url_pattern;

    let recovery_cancellation = cancel::CancellationToken::new();
    let recovery = tokio::spawn(
        Recovery {
            inbox: Arc::clone(&inbox),
            client: client.clone(),
            session: Arc::clone(&session),
            queue: download_queue_handle.clone(),
            allowed_users: Arc::clone(&allowed_users),
            super_users: Arc::clone(&super_users),
            cancellation: recovery_cancellation.clone(),
        }
        .run(),
    );

    // Replay downtime updates into a durable inbox. The feeder respects the
    // existing worker and in-memory queue limits, even for a large backlog.
    let mut updates = client
        .stream_updates(updates, UpdatesConfiguration { catch_up: true })
        .await?;
    let mut checkpoint = telegram::checkpoint::Checkpoint::default();

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
            update = updates.next_raw() => update,
        };

        let (raw, state, peers) = match update {
            Ok(u) => u,
            Err(e) => {
                log::error!("Error receiving update: {e}");
                continue;
            }
        };
        if let Some(state) = checkpoint.observe(&state) {
            session.set_update_state(state).await?;
        }
        let update = Update::from_raw(&client, raw, state, peers);
        let Update::NewMessage(message) = update else {
            continue;
        };
        // Messages can contain credentials (for example, `/insta` cookie data)
        // or token-bearing URLs. Keep logs useful without persisting their content.
        log::info!("Received message (outgoing={})", message.outgoing());

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
        if !super_users.contains(&uid) && !allowed_users.contains(uid) {
            log::info!("Ignoring unauthorized user {uid}");
            continue;
        }

        log::info!("Dispatching message from user {uid}");
        let text = message.text().to_string();
        let action = classify_message(&text, &url_pattern);
        if let MessageAction::Downloads(requests) = &action {
            let added = inbox.admit(uid, message.id(), requests).await?;
            log::info!("Persisted {added} new download request(s) from user {uid}");
            continue;
        }
        if !matches!(action, MessageAction::Retry)
            && !inbox.first_message(uid, message.id()).await?
        {
            continue;
        }
        match action {
            MessageAction::AddUser(user_id) => {
                let supers = Arc::clone(&super_users);
                let users = Arc::clone(&allowed_users);
                spawn_handler(&mut handler_tasks, async move {
                    users::handle_add(message, uid, user_id, supers, users).await;
                });
            }
            MessageAction::RemoveUser(user_id) => {
                let supers = Arc::clone(&super_users);
                let users = Arc::clone(&allowed_users);
                spawn_handler(&mut handler_tasks, async move {
                    users::handle_remove(message, uid, user_id, supers, users).await;
                });
            }
            MessageAction::ListUsers => {
                let supers = Arc::clone(&super_users);
                let users = Arc::clone(&allowed_users);
                let client = client.clone();
                let session = Arc::clone(&session);
                spawn_handler(&mut handler_tasks, async move {
                    users::handle_list(message, uid, supers, users, client, session).await;
                });
            }
            MessageAction::InstagramCookies => {
                let supers = Arc::clone(&super_users);
                spawn_handler(&mut handler_tasks, async move {
                    cookies::handle_update(&message, &text, uid, &supers).await;
                });
            }
            MessageAction::HealthCheck => {
                spawn_handler(&mut handler_tasks, async move {
                    let _ = message.reply("alive").await;
                });
            }
            MessageAction::QueueStatus => {
                let status = download_queue_handle.status_report(uid).await;
                spawn_handler(&mut handler_tasks, async move {
                    let _ = message.reply(status).await;
                });
            }
            MessageAction::Downloads(_) => unreachable!("downloads were persisted above"),
            MessageAction::Cancel(Some(id)) => {
                let reply = download_queue_handle
                    .try_cancel(message.into_inner(), uid, id)
                    .await;
                spawn_handler(&mut handler_tasks, reply.send());
            }
            MessageAction::Cancel(None) => {
                if let Some(status_message_id) = message.reply_to_message_id() {
                    let reply = download_queue_handle
                        .try_cancel_replied(message.into_inner(), uid, status_message_id)
                        .await;
                    spawn_handler(&mut handler_tasks, reply.send());
                } else {
                    spawn_handler(&mut handler_tasks, async move {
                        let _ = message
                            .reply("Usage: /cancel <job-id>, or reply to a download status with /cancel.")
                            .await;
                    });
                }
            }
            MessageAction::Retry => {
                if let Some(status_message_id) = message.reply_to_message_id() {
                    let reply = download_queue_handle
                        .try_retry(message.into_inner(), uid, status_message_id)
                        .await;
                    if reply.is_accepted() {
                        spawn_definitive_handler(&mut handler_tasks, reply.send());
                    } else {
                        spawn_handler(&mut handler_tasks, reply.send());
                    }
                } else {
                    spawn_handler(&mut handler_tasks, async move {
                        let _ = message
                            .reply("Reply to a download status with /retry.")
                            .await;
                    });
                }
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
    recovery_cancellation.cancel();
    if let Err(error) = recovery.await {
        log::error!("Recovery task failed: {error}");
    }
    drop(download_queue_handle);
    while handler_tasks.join_next().await.is_some() {}
    log::info!("Draining download queue");
    download_queue.shutdown().await;

    log::info!("Saving session file");
    // The last processed group remains deliberately replayable. Do not save the
    // receiver's newer state: it can include messages still buffered in memory.

    log::info!("Gracefully closing connection");
    handle.quit();

    Ok(())
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
