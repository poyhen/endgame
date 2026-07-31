use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use grammers_client::peer::Peer;
use grammers_client::{Client, update::Message as UpdateMessage};
use grammers_session::Session;
use grammers_session::storages::SqliteSession;
use grammers_session::types::PeerId;
use tokio::task::JoinSet;

use crate::commands::StatsTarget;
use crate::policy::{
    EffectiveUserLimits, PackageName, UserLimitName, UserLimitValue, UserPolicies,
};
use crate::store::{
    AddUserOutcome, AppStore, DownloadStats, GlobalStats, RemoveUserOutcome, UserRecord,
};

const USER_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
const TELEGRAM_TEXT_LIMIT: usize = 4_000;
const SET_LIMIT_USAGE: &str =
    "Usage: /limit <user-id> <active|queued|daily|upload_mb> <value|default>";

pub struct LimitChange {
    pub user_id: Option<i64>,
    pub name: Option<UserLimitName>,
    pub value: Option<UserLimitValue>,
}

pub async fn handle_add(
    message: UpdateMessage,
    sender_id: i64,
    user_id: Option<i64>,
    super_users: Arc<Vec<i64>>,
    store: Arc<AppStore>,
    policies: Arc<UserPolicies>,
) {
    if !super_users.contains(&sender_id) {
        reply_unauthorized(&message).await;
        return;
    }
    let Some(user_id) = user_id else {
        let _ = message.reply("Usage: /add <user-id>").await;
        return;
    };

    let response = match store.add_user(user_id, policies.default_package()).await {
        Ok(AddUserOutcome::Added) => {
            log::info!("Superuser {sender_id} authorized user {user_id}");
            format!(
                "User {user_id} added with package `{}`.",
                policies.default_package()
            )
        }
        Ok(AddUserOutcome::Reenabled) => {
            log::info!("Superuser {sender_id} re-enabled user {user_id}");
            format!("User {user_id} re-enabled.")
        }
        Ok(AddUserOutcome::AlreadyEnabled) => format!("User {user_id} is already enabled."),
        Err(error) => {
            log::error!("Superuser {sender_id} could not authorize user {user_id}: {error}");
            format!("Failed to add user {user_id}: {error}")
        }
    };
    let _ = message.reply(response).await;
}

pub async fn handle_remove(
    message: UpdateMessage,
    sender_id: i64,
    user_id: Option<i64>,
    super_users: Arc<Vec<i64>>,
    store: Arc<AppStore>,
) {
    if !super_users.contains(&sender_id) {
        reply_unauthorized(&message).await;
        return;
    }
    let Some(user_id) = user_id else {
        let _ = message.reply("Usage: /remove <user-id>").await;
        return;
    };
    if super_users.contains(&user_id) {
        let _ = message
            .reply(format!(
                "User {user_id} is a superuser. Remove them from the config and restart before disabling their regular account."
            ))
            .await;
        return;
    }

    let response = match store.remove_user(user_id).await {
        Ok(RemoveUserOutcome::Disabled) => {
            log::info!("Superuser {sender_id} disabled user {user_id}");
            format!("User {user_id} disabled.")
        }
        Ok(RemoveUserOutcome::AlreadyDisabled) => format!("User {user_id} is already disabled."),
        Ok(RemoveUserOutcome::NotFound) => format!("User {user_id} does not exist."),
        Err(error) => {
            log::error!("Superuser {sender_id} could not disable user {user_id}: {error}");
            format!("Failed to disable user {user_id}: {error}")
        }
    };
    let _ = message.reply(response).await;
}

pub async fn handle_set_package(
    message: UpdateMessage,
    sender_id: i64,
    user_id: Option<i64>,
    package: Option<PackageName>,
    super_users: Arc<Vec<i64>>,
    store: Arc<AppStore>,
    policies: Arc<UserPolicies>,
) {
    if !super_users.contains(&sender_id) {
        reply_unauthorized(&message).await;
        return;
    }
    let (Some(user_id), Some(package)) = (user_id, package) else {
        let _ = message.reply("Usage: /package <user-id> <package>").await;
        return;
    };
    if super_users.contains(&user_id) {
        let _ = message
            .reply("Superusers use the unrestricted superuser policy.")
            .await;
        return;
    }
    if !policies.has_package(&package) {
        let available = policies
            .packages()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let _ = message
            .reply(format!(
                "Package `{package}` is not configured. Available: {available}"
            ))
            .await;
        return;
    }

    let response = match store.set_user_package(user_id, &package).await {
        Ok(true) => {
            log::info!("Superuser {sender_id} changed user {user_id} to package {package}");
            format!("User {user_id} now uses package `{package}`.")
        }
        Ok(false) => format!("User {user_id} does not exist. Add them first."),
        Err(error) => {
            log::error!("Could not change package for user {user_id}: {error}");
            format!("Failed to change user {user_id}: {error}")
        }
    };
    let _ = message.reply(response).await;
}

pub async fn handle_list_packages(message: UpdateMessage, policies: Arc<UserPolicies>) {
    let mut lines = vec![format!(
        "Configured packages (default: `{}`):",
        policies.default_package()
    )];
    for (package, _) in policies.packages() {
        match policies.package_defaults(package) {
            Ok(limits) => lines.push(format_package_line(
                package,
                limits,
                package == policies.default_package(),
            )),
            Err(error) => {
                log::error!("Could not resolve package {package}: {error}");
            }
        }
    }
    for page in paginate(&lines, TELEGRAM_TEXT_LIMIT) {
        if message.reply(page).await.is_err() {
            break;
        }
    }
}

pub async fn handle_show_limits(
    message: UpdateMessage,
    sender_id: i64,
    target_id: Option<i64>,
    super_users: Arc<Vec<i64>>,
    store: Arc<AppStore>,
    policies: Arc<UserPolicies>,
) {
    let target_id = target_id.unwrap_or(sender_id);
    if target_id != sender_id && !super_users.contains(&sender_id) {
        reply_unauthorized(&message).await;
        return;
    }

    let response = if super_users.contains(&target_id) {
        format_limits(target_id, true, None, policies.superuser_limits())
    } else {
        match store.get_user(target_id).await {
            Ok(Some(user)) => match policies.resolve(&user.package, user.limits) {
                Ok(effective) => format_limits(target_id, false, Some(&user), effective),
                Err(error) => format!("Could not resolve limits for user {target_id}: {error}"),
            },
            Ok(None) => format!("User {target_id} does not exist."),
            Err(error) => {
                log::error!("Could not read limits for user {target_id}: {error}");
                format!("Failed to read limits: {error}")
            }
        }
    };
    let _ = message.reply(response).await;
}

pub async fn handle_show_stats(
    message: UpdateMessage,
    sender_id: i64,
    target: StatsTarget,
    super_users: Arc<Vec<i64>>,
    store: Arc<AppStore>,
) {
    let response = match target {
        StatsTarget::All => {
            if !super_users.contains(&sender_id) {
                reply_unauthorized(&message).await;
                return;
            }
            match store.global_stats().await {
                Ok(stats) => format_global_stats(stats),
                Err(error) => {
                    log::error!("Could not read global download statistics: {error}");
                    "Failed to read download statistics.".to_string()
                }
            }
        }
        StatsTarget::Own | StatsTarget::User(_) => {
            let target_id = match target {
                StatsTarget::Own => sender_id,
                StatsTarget::User(user_id) => user_id,
                StatsTarget::All => unreachable!(),
            };
            if target_id != sender_id && !super_users.contains(&sender_id) {
                reply_unauthorized(&message).await;
                return;
            }
            let identity = if super_users.contains(&target_id) {
                Some(format!("User {target_id} — superuser"))
            } else {
                match store.get_user(target_id).await {
                    Ok(Some(user)) => Some(format!(
                        "User {target_id} — package `{}` — {}",
                        user.package,
                        if user.enabled { "enabled" } else { "disabled" }
                    )),
                    Ok(None) => None,
                    Err(error) => {
                        log::error!("Could not read user {target_id} for statistics: {error}");
                        return reply_text(&message, "Failed to read download statistics.").await;
                    }
                }
            };
            match identity {
                None => format!("User {target_id} does not exist."),
                Some(identity) => match store.user_stats(target_id).await {
                    Ok(stats) => format_user_stats(&identity, stats),
                    Err(error) => {
                        log::error!("Could not read download statistics for {target_id}: {error}");
                        "Failed to read download statistics.".to_string()
                    }
                },
            }
        }
    };
    let _ = message.reply(response).await;
}

pub async fn handle_set_limit(
    message: UpdateMessage,
    sender_id: i64,
    change: LimitChange,
    super_users: Arc<Vec<i64>>,
    store: Arc<AppStore>,
    policies: Arc<UserPolicies>,
) {
    if !super_users.contains(&sender_id) {
        reply_unauthorized(&message).await;
        return;
    }
    let (Some(user_id), Some(name), Some(value)) = (change.user_id, change.name, change.value)
    else {
        let _ = message.reply(SET_LIMIT_USAGE).await;
        return;
    };
    if super_users.contains(&user_id) {
        let _ = message
            .reply("Superusers use the unrestricted superuser policy.")
            .await;
        return;
    }
    if let Err(error) = policies.validate_override(name, value) {
        let _ = message.reply(error.to_string()).await;
        return;
    }

    let response = match store.set_user_limit(user_id, name, value).await {
        Ok(true) => {
            log::info!(
                "Superuser {sender_id} changed {} for user {user_id}",
                name.as_str()
            );
            match value {
                UserLimitValue::Default => {
                    format!(
                        "{} for user {user_id} now uses the package default.",
                        name.as_str()
                    )
                }
                UserLimitValue::Value(value) => {
                    format!("{} for user {user_id} set to {value}.", name.as_str())
                }
            }
        }
        Ok(false) => format!("User {user_id} does not exist. Add them first."),
        Err(error) => {
            log::error!(
                "Could not change {} for user {user_id}: {error}",
                name.as_str()
            );
            format!("Failed to change user limit: {error}")
        }
    };
    let _ = message.reply(response).await;
}

pub async fn handle_list(
    message: UpdateMessage,
    sender_id: i64,
    super_users: Arc<Vec<i64>>,
    store: Arc<AppStore>,
    client: Client,
    session: Arc<SqliteSession>,
) {
    if !super_users.contains(&sender_id) {
        reply_unauthorized(&message).await;
        return;
    }

    let stored_users = match store.list_users().await {
        Ok(users) => users,
        Err(error) => {
            log::error!("Could not list application users: {error}");
            let _ = message
                .reply(format!("Failed to list users: {error}"))
                .await;
            return;
        }
    };
    let mut users: BTreeMap<i64, Option<UserRecord>> = stored_users
        .into_iter()
        .map(|user| (user.telegram_id, Some(user)))
        .collect();
    for super_user in super_users.iter().copied() {
        users.entry(super_user).or_insert(None);
    }

    let mut lookups = JoinSet::new();
    for user_id in users.keys().copied() {
        let client = client.clone();
        let session = Arc::clone(&session);
        lookups.spawn(async move {
            let profile = tokio::time::timeout(
                USER_LOOKUP_TIMEOUT,
                resolve_user_profile(&client, session.as_ref(), user_id),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .flatten();
            (user_id, profile)
        });
    }

    let mut profiles = HashMap::with_capacity(users.len());
    while let Some(result) = lookups.join_next().await {
        match result {
            Ok((user_id, profile)) => {
                profiles.insert(user_id, profile);
            }
            Err(error) => log::warn!("User profile lookup task failed: {error}"),
        }
    }

    let mut lines = Vec::with_capacity(users.len() + 1);
    lines.push(format!("Configured users ({}):", users.len()));
    for (user_id, user) in users {
        let profile = profiles.remove(&user_id).flatten();
        lines.push(format_user_line(
            user_id,
            profile.as_ref(),
            super_users.contains(&user_id),
            user.as_ref(),
        ));
    }

    for page in paginate(&lines, TELEGRAM_TEXT_LIMIT) {
        if let Err(error) = message.reply(page).await {
            log::warn!("Could not send user list: {error}");
            break;
        }
    }
}

async fn reply_unauthorized(message: &UpdateMessage) {
    let _ = message
        .reply("You are not authorized to use this command.")
        .await;
}

async fn reply_text(message: &UpdateMessage, text: &str) {
    let _ = message.reply(text).await;
}

fn format_limits(
    user_id: i64,
    is_superuser: bool,
    user: Option<&UserRecord>,
    effective: EffectiveUserLimits,
) -> String {
    let identity = if is_superuser {
        format!("User {user_id} — superuser")
    } else {
        let user = user.expect("non-superuser limit formatting requires a user");
        let state = if user.enabled { "enabled" } else { "disabled" };
        format!("User {user_id} — package `{}` — {state}", user.package)
    };
    let daily = effective
        .daily_job_limit
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unlimited".to_string());
    let upload_mb = effective.max_upload_bytes / (1024 * 1024);
    format!(
        "{identity}\nEffective limits:\n- active jobs: {}\n- queued jobs: {}\n- daily jobs: {daily}\n- upload size: {upload_mb} MiB",
        effective.max_active_jobs, effective.max_queued_jobs
    )
}

fn format_package_line(
    package: &PackageName,
    limits: EffectiveUserLimits,
    is_default: bool,
) -> String {
    let daily = limits
        .daily_job_limit
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unlimited".to_string());
    let default = if is_default { " [default]" } else { "" };
    format!(
        "- `{package}`{default}: active {}, queued {}, daily {daily}, upload {} MiB",
        limits.max_active_jobs,
        limits.max_queued_jobs,
        limits.max_upload_bytes / (1024 * 1024)
    )
}

fn format_user_stats(identity: &str, stats: DownloadStats) -> String {
    format!("{identity}\n{}", format_download_stats(stats))
}

fn format_global_stats(stats: GlobalStats) -> String {
    format!(
        "Application download stats\nUsers: {} enabled / {} stored\n{}",
        stats.enabled_users,
        stats.users,
        format_download_stats(stats.downloads)
    )
}

fn format_download_stats(stats: DownloadStats) -> String {
    let decided = stats.completed_jobs.saturating_add(stats.failed_jobs);
    let success_rate = if decided == 0 {
        "n/a".to_string()
    } else {
        format!(
            "{:.1}%",
            stats.completed_jobs as f64 * 100.0 / decided as f64
        )
    };
    format!(
        "Accepted: {} ({} today)\nCompleted: {}\nFailed: {}\nCancelled: {}\nInterrupted/unknown: {}\nOpen/unresolved: {}\nSuccess rate (completed vs failed): {success_rate}",
        stats.accepted_jobs,
        stats.accepted_today,
        stats.completed_jobs,
        stats.failed_jobs,
        stats.cancelled_jobs,
        stats.interrupted_jobs,
        stats.unresolved_jobs(),
    )
}

#[derive(Debug, PartialEq, Eq)]
struct UserProfile {
    username: Option<String>,
    full_name: Option<String>,
}

async fn resolve_user_profile(
    client: &Client,
    session: &SqliteSession,
    user_id: i64,
) -> anyhow::Result<Option<UserProfile>> {
    let Some(peer_id) = PeerId::user(user_id) else {
        return Ok(None);
    };
    let peer_ref = session
        .peer_ref(peer_id)
        .await?
        .unwrap_or_else(|| peer_id.to_ambient_ref());
    let Peer::User(user) = client.resolve_peer(peer_ref).await? else {
        return Ok(None);
    };
    let username = user.username().map(str::to_owned).or_else(|| {
        user.usernames()
            .first()
            .map(|username| (*username).to_owned())
    });
    let full_name = match user.full_name() {
        name if name.trim().is_empty() => None,
        name => Some(name),
    };
    Ok(Some(UserProfile {
        username,
        full_name,
    }))
}

fn format_user_line(
    user_id: i64,
    profile: Option<&UserProfile>,
    is_superuser: bool,
    user: Option<&UserRecord>,
) -> String {
    let mut fields = vec![user_id.to_string()];
    if let Some(username) = profile.and_then(|profile| profile.username.as_deref()) {
        fields.push(format!("@{username}"));
    }
    if let Some(full_name) = profile.and_then(|profile| profile.full_name.as_deref()) {
        fields.push(full_name.to_string());
    }
    let mut badges = Vec::new();
    if is_superuser {
        badges.push("superuser");
    } else if let Some(user) = user {
        badges.push(user.package.as_str());
    }
    if !is_superuser && user.is_some_and(|user| !user.enabled) {
        badges.push("disabled");
    }
    let mut line = fields.join(" — ");
    for badge in badges {
        line.push_str(&format!(" [{badge}]"));
    }
    line
}

fn paginate(lines: &[String], limit: usize) -> Vec<String> {
    let mut pages = Vec::new();
    let mut page = String::new();
    for line in lines {
        if !page.is_empty() && page.len() + 1 + line.len() > limit {
            pages.push(std::mem::take(&mut page));
        }
        if !page.is_empty() {
            page.push('\n');
        }
        page.push_str(line);
    }
    if !page.is_empty() {
        pages.push(page);
    }
    pages
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::UserLimitOverrides;

    #[test]
    fn formats_identity_and_database_state() {
        let profile = UserProfile {
            username: Some("sample".to_string()),
            full_name: Some("Sample User".to_string()),
        };
        let user = UserRecord {
            telegram_id: 42,
            enabled: false,
            package: PackageName::require("pro").unwrap(),
            limits: UserLimitOverrides::default(),
        };
        assert_eq!(
            format_user_line(42, Some(&profile), false, Some(&user)),
            "42 — @sample — Sample User [pro] [disabled]"
        );
        assert_eq!(
            format_user_line(42, Some(&profile), true, Some(&user)),
            "42 — @sample — Sample User [superuser]"
        );
        assert_eq!(format_user_line(7, None, true, None), "7 [superuser]");
    }

    #[test]
    fn paginates_without_dropping_users() {
        let lines = vec![
            "header".to_string(),
            "user one".to_string(),
            "user two".to_string(),
        ];
        assert_eq!(paginate(&lines, 16), vec!["header\nuser one", "user two"]);
    }

    #[test]
    fn formats_download_statistics_and_ignores_cancelled_jobs_in_success_rate() {
        let stats = DownloadStats {
            accepted_jobs: 12,
            completed_jobs: 8,
            failed_jobs: 2,
            cancelled_jobs: 1,
            interrupted_jobs: 0,
            accepted_today: 3,
        };

        assert_eq!(
            format_download_stats(stats),
            "Accepted: 12 (3 today)\nCompleted: 8\nFailed: 2\nCancelled: 1\nInterrupted/unknown: 0\nOpen/unresolved: 1\nSuccess rate (completed vs failed): 80.0%"
        );
    }
}
