mod cancel;
mod commands;
mod config;
mod cookies;
mod jobs;
mod media;
mod policy;
mod store;
mod telegram;
mod users;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use grammers_client::client::UpdatesConfiguration;
use grammers_client::session::storages::SqliteSession;
use grammers_client::update::{Message as UpdateMessage, Update};
use grammers_client::{Client, SenderPool};
use grammers_session::types::PeerKind;
use regex::Regex;
use simple_logger::SimpleLogger;
use tokio::task::JoinSet;

use commands::{MessageAction, classify_message};
use config::Config;
use jobs::DownloadQueue;
use jobs::queue::{DownloadQueueHandle, QueueReply};
use media::request::{DownloadLimits, DownloadRequest};
use policy::UserPolicies;
use store::{AppStore, ReserveUsageOutcome};

const SESSION_FILE: &str = "userbot.session";
const MAX_HANDLER_TASKS: usize = 64;
const HANDLER_TIMEOUT: Duration = Duration::from_secs(15);

type AnyResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[tokio::main]
async fn main() -> AnyResult<()> {
    let _ = SimpleLogger::new()
        .with_level(log::LevelFilter::Info)
        .init();

    let cfg = Config::load()?;
    let app_store = Arc::new(AppStore::open(&cfg.database_path).await?);
    let imported = app_store
        .import_bootstrap_users(
            &cfg.allowed_users,
            &cfg.super_users,
            &cfg.default_package,
            &cfg.superuser_package,
        )
        .await?;
    if imported > 0 {
        log::info!("Imported {imported} bootstrap users into the application database");
    }
    let ensured_superusers = app_store
        .ensure_superusers(&cfg.super_users, &cfg.superuser_package)
        .await?;
    if ensured_superusers > 0 {
        log::info!("Added {ensured_superusers} new configured superusers to the user database");
    }
    let interrupted = app_store.reconcile_interrupted_jobs().await?;
    if interrupted > 0 {
        log::warn!("Reconciled unfinished download statistics for {interrupted} users");
    }
    let user_policies = Arc::new(cfg.user_policies.clone());

    let session = Arc::new(SqliteSession::open(SESSION_FILE).await?);

    let SenderPool {
        runner,
        handle,
        updates,
    } = SenderPool::new(Arc::clone(&session), cfg.api_id);
    let client = Client::new(handle.clone());
    let _pool_task = tokio::spawn(runner.run());

    telegram::auth::ensure_authorized(&client, &cfg.api_hash).await?;

    log::info!("Userbot is running");

    let download_limits = DownloadLimits {
        max_upload_bytes: (cfg.max_upload_size_mb as u64).saturating_mul(1024 * 1024),
        command_timeout: Duration::from_secs(cfg.command_timeout_secs as u64),
        upload_timeout: Duration::from_secs(cfg.upload_timeout_secs as u64),
        job_timeout: Duration::from_secs(cfg.job_timeout_secs as u64),
    };
    let download_queue = DownloadQueue::new(
        client.clone(),
        cfg.download_concurrency,
        cfg.download_queue_capacity,
        download_limits,
        Arc::clone(&app_store),
    );
    let download_queue_handle = download_queue.handle();
    log::info!(
        "Download queue ready ({} active, {} waiting).",
        cfg.download_concurrency,
        cfg.download_queue_capacity
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
        let is_superuser = super_users.contains(&uid);
        if !is_superuser {
            match app_store.is_user_enabled(uid).await {
                Ok(true) => {}
                Ok(false) => {
                    log::info!("Ignoring unauthorized user {uid}");
                    continue;
                }
                Err(error) => {
                    log::error!("Could not authorize user {uid}: {error}");
                    continue;
                }
            }
        }

        log::info!("Dispatching message from user {uid}");
        let text = message.text().to_string();
        match classify_message(&text, &url_pattern) {
            MessageAction::AddUser(user_id) => {
                let supers = Arc::clone(&super_users);
                let store = Arc::clone(&app_store);
                let policies = Arc::clone(&user_policies);
                spawn_handler(&mut handler_tasks, async move {
                    users::handle_add(message, uid, user_id, supers, store, policies).await;
                });
            }
            MessageAction::RemoveUser(user_id) => {
                let supers = Arc::clone(&super_users);
                let store = Arc::clone(&app_store);
                spawn_handler(&mut handler_tasks, async move {
                    users::handle_remove(message, uid, user_id, supers, store).await;
                });
            }
            MessageAction::ListUsers => {
                let supers = Arc::clone(&super_users);
                let store = Arc::clone(&app_store);
                let client = client.clone();
                let session = Arc::clone(&session);
                spawn_handler(&mut handler_tasks, async move {
                    users::handle_list(message, uid, supers, store, client, session).await;
                });
            }
            MessageAction::SetUserPackage { user_id, package } => {
                let supers = Arc::clone(&super_users);
                let store = Arc::clone(&app_store);
                let policies = Arc::clone(&user_policies);
                spawn_handler(&mut handler_tasks, async move {
                    users::handle_set_package(
                        message, uid, user_id, package, supers, store, policies,
                    )
                    .await;
                });
            }
            MessageAction::ListPackages => {
                let policies = Arc::clone(&user_policies);
                spawn_handler(&mut handler_tasks, async move {
                    users::handle_list_packages(message, policies).await;
                });
            }
            MessageAction::ShowUserLimits(target_id) => {
                let supers = Arc::clone(&super_users);
                let store = Arc::clone(&app_store);
                let policies = Arc::clone(&user_policies);
                spawn_handler(&mut handler_tasks, async move {
                    users::handle_show_limits(message, uid, target_id, supers, store, policies)
                        .await;
                });
            }
            MessageAction::SetUserLimit {
                user_id,
                name,
                value,
            } => {
                let supers = Arc::clone(&super_users);
                let store = Arc::clone(&app_store);
                let policies = Arc::clone(&user_policies);
                spawn_handler(&mut handler_tasks, async move {
                    users::handle_set_limit(
                        message,
                        uid,
                        users::LimitChange {
                            user_id,
                            name,
                            value,
                        },
                        supers,
                        store,
                        policies,
                    )
                    .await;
                });
            }
            MessageAction::ShowStats(target) => {
                let supers = Arc::clone(&super_users);
                let store = Arc::clone(&app_store);
                spawn_handler(&mut handler_tasks, async move {
                    users::handle_show_stats(message, uid, target, supers, store).await;
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
                let status = download_queue_handle.status_report(uid);
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
                    let reply = admit_download(
                        message.clone(),
                        uid,
                        url,
                        is_superuser,
                        app_store.as_ref(),
                        user_policies.as_ref(),
                        &download_queue_handle,
                    )
                    .await;
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
                if let Some(status_message_id) = message.reply_to_message_id() {
                    let reply =
                        download_queue_handle.try_cancel_replied(message, uid, status_message_id);
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
                if handler_tasks.len() >= MAX_HANDLER_TASKS {
                    log::warn!("Skipping a retry because no acknowledgement slot is available");
                    continue;
                }
                if let Some(status_message_id) = message.reply_to_message_id() {
                    let reply = match download_queue_handle.retry_request(uid, status_message_id) {
                        Some(request) => {
                            admit_download(
                                message,
                                uid,
                                request,
                                is_superuser,
                                app_store.as_ref(),
                                user_policies.as_ref(),
                                &download_queue_handle,
                            )
                            .await
                        }
                        None => QueueReply::message(
                            message,
                            "The replied message is not a known download status. Reply to a download status with /retry.",
                        ),
                    };
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
    drop(download_queue_handle);
    while handler_tasks.join_next().await.is_some() {}
    log::info!("Draining download queue");
    download_queue.shutdown().await;

    log::info!("Saving session file");
    let _ = updates.sync_update_state().await;

    log::info!("Gracefully closing connection");
    handle.quit();

    Ok(())
}

async fn admit_download(
    message: UpdateMessage,
    owner_id: i64,
    request: DownloadRequest,
    is_superuser: bool,
    store: &AppStore,
    policies: &UserPolicies,
    queue: &DownloadQueueHandle,
) -> QueueReply {
    let user_limits = if is_superuser {
        policies.superuser_limits()
    } else {
        let user = match store.get_user(owner_id).await {
            Ok(Some(user)) if user.enabled => user,
            Ok(_) => {
                return QueueReply::message(
                    message,
                    "Your account is no longer authorized to download.",
                );
            }
            Err(error) => {
                log::error!("Could not load policy for user {owner_id}: {error}");
                return QueueReply::message(
                    message,
                    "The user database is unavailable. Please try again.",
                );
            }
        };
        match policies.resolve(&user.package, user.limits) {
            Ok(limits) => limits,
            Err(error) => {
                log::error!("Could not resolve policy for user {owner_id}: {error}");
                return QueueReply::message(
                    message,
                    "Your assigned package is not configured. Please contact an administrator.",
                );
            }
        }
    };

    let reservation = match store
        .reserve_daily_job(owner_id, user_limits.daily_job_limit)
        .await
    {
        Ok(ReserveUsageOutcome::Reserved(reservation)) => reservation,
        Ok(ReserveUsageOutcome::LimitReached) => {
            let limit = user_limits
                .daily_job_limit
                .expect("a reached daily limit must be finite");
            return QueueReply::message(
                message,
                format!(
                    "You have reached your daily download limit of {limit}. Try again tomorrow (UTC)."
                ),
            );
        }
        Err(error) => {
            log::error!("Could not reserve daily usage for user {owner_id}: {error}");
            return QueueReply::message(
                message,
                "The user database is unavailable. Please try again.",
            );
        }
    };

    let reply = queue.try_enqueue(message, owner_id, request, user_limits);
    if !reply.is_accepted()
        && let Err(error) = store.release_daily_job(reservation).await
    {
        log::error!("Could not release daily usage for user {owner_id}: {error}");
    }
    reply
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
