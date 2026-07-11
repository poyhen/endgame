use std::sync::Arc;

use grammers_client::update::Message as UpdateMessage;

use crate::telegram::allowed_users::{AddOutcome, AllowedUsers};

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
