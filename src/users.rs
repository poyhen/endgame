use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use grammers_client::peer::Peer;
use grammers_client::{Client, update::Message as UpdateMessage};
use grammers_session::Session;
use grammers_session::storages::SqliteSession;
use grammers_session::types::PeerId;
use tokio::task::JoinSet;

use crate::telegram::allowed_users::{AddOutcome, AllowedUsers, RemoveOutcome};

const USER_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
const TELEGRAM_TEXT_LIMIT: usize = 4_000;

pub async fn handle_add(
    message: UpdateMessage,
    sender_id: i64,
    user_id: Option<i64>,
    super_users: Arc<Vec<i64>>,
    allowed_users: Arc<AllowedUsers>,
) {
    if !super_users.contains(&sender_id) {
        let _ = message
            .reply("You are not authorized to use this command.")
            .await;
        return;
    }
    let Some(user_id) = user_id else {
        let _ = message.reply("Usage: /add <user-id>").await;
        return;
    };

    let result = tokio::task::spawn_blocking(move || allowed_users.add(user_id)).await;
    let response = match result {
        Ok(Ok(AddOutcome::Added)) => {
            log::info!("Superuser {sender_id} authorized user {user_id}");
            format!("User {user_id} added successfully.")
        }
        Ok(Ok(AddOutcome::AlreadyAllowed)) => format!("User {user_id} is already authorized."),
        Ok(Err(error)) => {
            log::error!("Superuser {sender_id} could not authorize user {user_id}: {error}");
            format!("Failed to add user {user_id}: {error}")
        }
        Err(error) => {
            log::error!("Allowed-user update task failed: {error}");
            format!("Failed to add user {user_id}: update task failed")
        }
    };
    let _ = message.reply(response).await;
}

pub async fn handle_remove(
    message: UpdateMessage,
    sender_id: i64,
    user_id: Option<i64>,
    super_users: Arc<Vec<i64>>,
    allowed_users: Arc<AllowedUsers>,
) {
    if !super_users.contains(&sender_id) {
        let _ = message
            .reply("You are not authorized to use this command.")
            .await;
        return;
    }
    let Some(user_id) = user_id else {
        let _ = message.reply("Usage: /remove <user-id>").await;
        return;
    };
    if super_users.contains(&user_id) {
        let _ = message
            .reply(format!(
                "User {user_id} is a superuser; edit the config file to revoke their access."
            ))
            .await;
        return;
    }

    let result = tokio::task::spawn_blocking(move || allowed_users.remove(user_id)).await;
    let response = match result {
        Ok(Ok(RemoveOutcome::Removed)) => {
            log::info!("Superuser {sender_id} removed user {user_id}");
            format!("User {user_id} removed successfully.")
        }
        Ok(Ok(RemoveOutcome::NotAllowed)) => format!("User {user_id} is not authorized."),
        Ok(Err(error)) => {
            log::error!("Superuser {sender_id} could not remove user {user_id}: {error}");
            format!("Failed to remove user {user_id}: {error}")
        }
        Err(error) => {
            log::error!("Allowed-user update task failed: {error}");
            format!("Failed to remove user {user_id}: update task failed")
        }
    };
    let _ = message.reply(response).await;
}

pub async fn handle_list(
    message: UpdateMessage,
    sender_id: i64,
    super_users: Arc<Vec<i64>>,
    allowed_users: Arc<AllowedUsers>,
    client: Client,
    session: Arc<SqliteSession>,
) {
    if !super_users.contains(&sender_id) {
        let _ = message
            .reply("You are not authorized to use this command.")
            .await;
        return;
    }

    let user_ids: BTreeSet<_> = allowed_users
        .list()
        .into_iter()
        .chain(super_users.iter().copied())
        .collect();
    let mut lookups = JoinSet::new();
    for user_id in user_ids.iter().copied() {
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

    let mut profiles = HashMap::with_capacity(user_ids.len());
    while let Some(result) = lookups.join_next().await {
        match result {
            Ok((user_id, profile)) => {
                profiles.insert(user_id, profile);
            }
            Err(error) => log::warn!("Allowed-user profile lookup task failed: {error}"),
        }
    }

    let mut lines = Vec::with_capacity(user_ids.len() + 1);
    lines.push(format!("Users allowed to download ({}):", user_ids.len()));
    for user_id in user_ids {
        let profile = profiles.remove(&user_id).flatten();
        lines.push(format_user_line(
            user_id,
            profile.as_ref(),
            super_users.contains(&user_id),
        ));
    }

    for page in paginate(&lines, TELEGRAM_TEXT_LIMIT) {
        if let Err(error) = message.reply(page).await {
            log::warn!("Could not send allowed-user list: {error}");
            break;
        }
    }
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

fn format_user_line(user_id: i64, profile: Option<&UserProfile>, is_superuser: bool) -> String {
    let mut fields = vec![user_id.to_string()];
    if let Some(username) = profile.and_then(|profile| profile.username.as_deref()) {
        fields.push(format!("@{username}"));
    }
    if let Some(full_name) = profile.and_then(|profile| profile.full_name.as_deref()) {
        fields.push(full_name.to_string());
    }
    let mut line = fields.join(" — ");
    if is_superuser {
        line.push_str(" [superuser]");
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

    #[test]
    fn formats_every_available_identity_field() {
        let profile = UserProfile {
            username: Some("sample".to_string()),
            full_name: Some("Sample User".to_string()),
        };
        assert_eq!(
            format_user_line(42, Some(&profile), true),
            "42 — @sample — Sample User [superuser]"
        );
        assert_eq!(format_user_line(7, None, false), "7");
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
}
