use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddOutcome {
    Added,
    AlreadyAllowed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveOutcome {
    Removed,
    NotAllowed,
}

/// The allowed-user set lives in the `allowed_users` array of the JSON config
/// file. `/add` and `/remove` update the in-memory set and rewrite that array
/// in place, so both commands are authoritative and survive restarts.
pub struct AllowedUsers {
    users: RwLock<HashSet<i64>>,
    config_path: PathBuf,
}

impl AllowedUsers {
    pub fn new(users: impl IntoIterator<Item = i64>, config_path: impl Into<PathBuf>) -> Self {
        Self {
            users: RwLock::new(users.into_iter().collect()),
            config_path: config_path.into(),
        }
    }

    pub fn contains(&self, user_id: i64) -> bool {
        self.users
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(&user_id)
    }

    pub fn list(&self) -> Vec<i64> {
        let mut users: Vec<_> = self
            .users
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .copied()
            .collect();
        users.sort_unstable();
        users
    }

    pub fn add(&self, user_id: i64) -> anyhow::Result<AddOutcome> {
        if user_id <= 0 {
            anyhow::bail!("user ID must be positive");
        }

        let mut users = self
            .users
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !users.insert(user_id) {
            return Ok(AddOutcome::AlreadyAllowed);
        }
        if let Err(error) = persist(&self.config_path, &users) {
            users.remove(&user_id);
            return Err(error);
        }
        Ok(AddOutcome::Added)
    }

    pub fn remove(&self, user_id: i64) -> anyhow::Result<RemoveOutcome> {
        if user_id <= 0 {
            anyhow::bail!("user ID must be positive");
        }

        let mut users = self
            .users
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !users.remove(&user_id) {
            return Ok(RemoveOutcome::NotAllowed);
        }
        if let Err(error) = persist(&self.config_path, &users) {
            users.insert(user_id);
            return Err(error);
        }
        Ok(RemoveOutcome::Removed)
    }
}

/// Rewrites only the `allowed_users` array of the JSON config, re-reading the
/// file first so unrelated keys (including hand edits) are left untouched.
fn persist(config_path: &Path, users: &HashSet<i64>) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(config_path)
        .map_err(|error| anyhow::anyhow!("could not read {}: {error}", config_path.display()))?;
    let mut config: serde_json::Value = serde_json::from_str(&content)
        .map_err(|error| anyhow::anyhow!("invalid {}: {error}", config_path.display()))?;
    if !config.is_object() {
        anyhow::bail!("{} must contain a JSON object", config_path.display());
    }

    let mut sorted: Vec<_> = users.iter().copied().collect();
    sorted.sort_unstable();
    config["allowed_users"] = serde_json::json!(sorted);

    let temporary = config_path.with_extension(format!("tmp-{:016x}", rand::random::<u64>()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let write_result = (|| {
        let mut file = options.open(&temporary)?;
        writeln!(file, "{}", serde_json::to_string_pretty(&config)?)?;
        file.sync_all()?;
        std::fs::rename(&temporary, config_path)
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

    fn temporary_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "endgame-config-test-{:016x}.json",
            rand::random::<u64>()
        ))
    }

    fn read_allowed_users(path: &Path) -> Vec<i64> {
        let content = std::fs::read_to_string(path).unwrap();
        serde_json::from_str::<serde_json::Value>(&content).unwrap()["allowed_users"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_i64().unwrap())
            .collect()
    }

    #[test]
    fn add_and_remove_persist_to_the_config_file() {
        let path = temporary_path();
        std::fs::write(&path, r#"{"superusers": [11], "allowed_users": [11]}"#).unwrap();
        let users = AllowedUsers::new([11], &path);

        assert_eq!(users.add(22).unwrap(), AddOutcome::Added);
        assert_eq!(users.add(22).unwrap(), AddOutcome::AlreadyAllowed);
        assert_eq!(read_allowed_users(&path), vec![11, 22]);

        assert_eq!(users.remove(22).unwrap(), RemoveOutcome::Removed);
        assert!(!users.contains(22));
        assert_eq!(users.remove(22).unwrap(), RemoveOutcome::NotAllowed);
        assert_eq!(read_allowed_users(&path), vec![11]);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn persistence_leaves_other_config_keys_untouched() {
        let path = temporary_path();
        std::fs::write(
            &path,
            r#"{"superusers": [11], "allowed_users": [], "download_concurrency": 4}"#,
        )
        .unwrap();
        let users = AllowedUsers::new([], &path);

        users.add(22).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let config: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(config["superusers"], serde_json::json!([11]));
        assert_eq!(config["allowed_users"], serde_json::json!([22]));
        assert_eq!(config["download_concurrency"], serde_json::json!(4));

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn failed_persistence_rolls_back_changes() {
        let directory = temporary_path();
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("missing").join("config.json");
        let users = AllowedUsers::new([11], &path);

        assert!(users.add(22).is_err());
        assert!(!users.contains(22));
        assert!(users.remove(11).is_err());
        assert!(users.contains(11));

        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn rejects_non_object_config_files() {
        let path = temporary_path();
        std::fs::write(&path, "[1, 2, 3]").unwrap();
        let users = AllowedUsers::new([11], &path);

        assert!(users.add(22).is_err());
        assert!(!users.contains(22));

        std::fs::remove_file(path).unwrap();
    }
}
