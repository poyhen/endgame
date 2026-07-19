use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddOutcome {
    Added,
    AlreadyAllowed,
}

pub struct AllowedUsers {
    state: RwLock<AllowedUserState>,
    path: PathBuf,
}

struct AllowedUserState {
    users: HashSet<i64>,
    persisted: HashSet<i64>,
}

impl AllowedUsers {
    pub fn load(
        seed: impl IntoIterator<Item = i64>,
        path: impl Into<PathBuf>,
    ) -> anyhow::Result<Self> {
        let path = path.into();
        let mut users: HashSet<_> = seed.into_iter().collect();
        let mut persisted = HashSet::new();

        match std::fs::read_to_string(&path) {
            Ok(content) => {
                for (index, line) in content.lines().enumerate() {
                    let value = line.trim();
                    if value.is_empty() || value.starts_with('#') {
                        continue;
                    }
                    let user_id = value.parse::<i64>().map_err(|error| {
                        anyhow::anyhow!(
                            "invalid user ID on line {} of {}: {error}",
                            index + 1,
                            path.display()
                        )
                    })?;
                    if user_id <= 0 {
                        anyhow::bail!(
                            "invalid user ID on line {} of {}: IDs must be positive",
                            index + 1,
                            path.display()
                        );
                    }
                    users.insert(user_id);
                    persisted.insert(user_id);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                anyhow::bail!("could not read {}: {error}", path.display());
            }
        }

        Ok(Self {
            state: RwLock::new(AllowedUserState { users, persisted }),
            path,
        })
    }

    pub fn contains(&self, user_id: i64) -> bool {
        self.state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .users
            .contains(&user_id)
    }

    pub fn list(&self) -> Vec<i64> {
        let mut users: Vec<_> = self
            .state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .users
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

        let mut state = self
            .state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.users.contains(&user_id) {
            return Ok(AddOutcome::AlreadyAllowed);
        }

        state.users.insert(user_id);
        state.persisted.insert(user_id);
        if let Err(error) = persist(&self.path, &state.persisted) {
            state.users.remove(&user_id);
            state.persisted.remove(&user_id);
            return Err(error);
        }
        Ok(AddOutcome::Added)
    }
}

fn persist(path: &Path, users: &HashSet<i64>) -> anyhow::Result<()> {
    let temporary = path.with_extension(format!("tmp-{:016x}", rand::random::<u64>()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let write_result = (|| {
        let mut sorted: Vec<_> = users.iter().copied().collect();
        sorted.sort_unstable();

        let mut file = options.open(&temporary)?;
        for user_id in sorted {
            writeln!(file, "{user_id}")?;
        }
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

    fn temporary_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "endgame-allowed-users-test-{:016x}.txt",
            rand::random::<u64>()
        ))
    }

    #[test]
    fn combines_seed_and_persisted_users() {
        let path = temporary_path();
        std::fs::write(&path, "22\n# comment\n33\n").unwrap();

        let users = AllowedUsers::load([11, 22], &path).unwrap();
        assert!(users.contains(11));
        assert!(users.contains(22));
        assert!(users.contains(33));
        assert_eq!(users.list(), vec![11, 22, 33]);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn additions_are_idempotent_and_survive_reload() {
        let path = temporary_path();
        let users = AllowedUsers::load([11], &path).unwrap();

        assert_eq!(users.add(22).unwrap(), AddOutcome::Added);
        assert_eq!(users.add(22).unwrap(), AddOutcome::AlreadyAllowed);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "22\n");

        let reloaded = AllowedUsers::load([11], &path).unwrap();
        assert!(reloaded.contains(22));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_malformed_persisted_users() {
        let path = temporary_path();
        std::fs::write(&path, "not-an-id\n").unwrap();

        assert!(AllowedUsers::load([], &path).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn failed_persistence_rolls_back_addition() {
        let directory = temporary_path();
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("missing").join("allowed-users.txt");
        let users = AllowedUsers::load([11], &path).unwrap();

        assert!(users.add(22).is_err());
        assert!(!users.contains(22));
        std::fs::remove_dir(directory).unwrap();
    }
}
