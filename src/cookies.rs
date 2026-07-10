use std::io::Write;
use std::path::Path;

use grammers_client::update::Message as UpdateMessage;

pub async fn handle_update(
    message: &UpdateMessage,
    text: &str,
    sender_id: i64,
    super_users: &[i64],
) {
    if !super_users.contains(&sender_id) {
        let _ = message
            .reply("You are not authorized to use this command.")
            .await;
        return;
    }
    let content = text
        .split_once(char::is_whitespace)
        .map(|(_, content)| content)
        .unwrap_or("")
        .trim();
    if content.is_empty() {
        let _ = message
            .reply("Please provide the cookie content after the command.")
            .await;
        return;
    }
    if let Err(error) = normalize_netscape(content) {
        let _ = message
            .reply(format!("Invalid cookie content: {error}"))
            .await;
        return;
    }
    if let Err(error) = message.delete().await {
        log::warn!("Could not delete the Instagram cookie message: {error}");
    }

    let content = content.to_string();
    let installation =
        tokio::task::spawn_blocking(move || install(Path::new("instacookies.txt"), &content)).await;
    let response = match installation {
        Ok(Ok(())) => "Instagram cookies updated successfully.".to_string(),
        Ok(Err(error)) => format!("Failed to update cookies: {error}"),
        Err(error) => format!("Cookie installation task failed: {error}"),
    };
    let _ = message.respond(response).await;
}

pub fn normalize_netscape(content: &str) -> anyhow::Result<String> {
    let mut normalized = String::new();
    let mut cookie_count = 0usize;

    for (line_number, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            normalized.push_str(trimmed);
            normalized.push('\n');
            continue;
        }

        let fields: Vec<_> = trimmed.split_whitespace().collect();
        if fields.len() < 7 {
            anyhow::bail!(
                "cookie line {} has {} fields; expected at least 7",
                line_number + 1,
                fields.len()
            );
        }
        normalized.push_str(&fields.join("\t"));
        normalized.push('\n');
        cookie_count += 1;
    }

    if cookie_count == 0 {
        anyhow::bail!("cookie content contains no Netscape cookie records");
    }
    Ok(normalized)
}

pub fn install(path: &Path, content: &str) -> anyhow::Result<()> {
    let normalized = normalize_netscape(content)?;
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
        file.write_all(normalized.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_cookie_fields_to_tabs() {
        let normalized = normalize_netscape(
            "# Netscape HTTP Cookie File\n.instagram.com TRUE / TRUE 0 sessionid secret\n",
        )
        .unwrap();
        assert_eq!(
            normalized,
            "# Netscape HTTP Cookie File\n.instagram.com\tTRUE\t/\tTRUE\t0\tsessionid\tsecret\n"
        );
    }

    #[test]
    fn rejects_empty_and_malformed_cookie_files() {
        assert!(normalize_netscape("# header only").is_err());
        assert!(normalize_netscape("sessionid=secret").is_err());
    }

    #[test]
    fn rejected_update_does_not_replace_existing_secret() {
        let path = std::env::temp_dir().join(format!(
            "endgame-cookie-test-{:016x}.txt",
            rand::random::<u64>()
        ));
        std::fs::write(&path, "original").unwrap();
        assert!(install(&path, "invalid").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
        std::fs::remove_file(path).unwrap();
    }
}
