use std::path::{Path, PathBuf};

pub fn random_file_in(directory: &Path, extension: &str) -> PathBuf {
    directory.join(format!("media-{:016x}{extension}", rand::random::<u64>()))
}

pub struct JobWorkspace {
    path: PathBuf,
    cleaned: bool,
}

impl JobWorkspace {
    pub async fn create(job_id: u64) -> anyhow::Result<Self> {
        let path = std::env::temp_dir()
            .join("endgame")
            .join(format!("job-{job_id}-{:016x}", rand::random::<u64>()));
        tokio::fs::create_dir_all(&path).await?;
        Ok(Self {
            path,
            cleaned: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn cleanup(mut self) {
        match tokio::fs::remove_dir_all(&self.path).await {
            Ok(()) => self.cleaned = true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => self.cleaned = true,
            Err(error) => log::warn!(
                "Could not remove job workspace {}: {error}",
                self.path.display()
            ),
        }
    }
}

impl Drop for JobWorkspace {
    fn drop(&mut self) {
        if !self.cleaned {
            log::warn!(
                "Job workspace was not explicitly cleaned: {}",
                self.path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn workspaces_are_private_and_explicitly_cleaned() {
        let first = JobWorkspace::create(7).await.unwrap();
        let second = JobWorkspace::create(7).await.unwrap();
        assert_ne!(first.path(), second.path());
        assert!(first.path().is_dir());
        let first_path = first.path().to_path_buf();
        first.cleanup().await;
        second.cleanup().await;
        assert!(!first_path.exists());
    }
}
