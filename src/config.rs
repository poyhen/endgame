use regex::Regex;
use std::path::PathBuf;

pub struct Config {
    pub api_id: i32,
    pub api_hash: String,
    pub allowed_user_ids: Vec<i64>,
    pub allowed_users_file: PathBuf,
    pub super_users: Vec<i64>,
    pub url_pattern: Regex,
    pub download_concurrency: usize,
    pub download_queue_capacity: usize,
    pub max_upload_size_mb: usize,
    pub command_timeout_secs: usize,
    pub upload_timeout_secs: usize,
    pub job_timeout_secs: usize,
}

fn parse_id_list(value: &str) -> Result<Vec<i64>, String> {
    value
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<i64>()
                .map_err(|e| format!("invalid user id {s:?}: {e}"))
        })
        .collect()
}

fn parse_positive_usize(name: &str, value: &str) -> anyhow::Result<usize> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("{name} must be a positive integer"))?;
    if parsed == 0 {
        anyhow::bail!("{name} must be greater than zero");
    }
    Ok(parsed)
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let api_id = std::env::var("API_ID")
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("API ID must be a non-empty integer or string"))?
            .parse::<i32>()
            .map_err(|_| anyhow::anyhow!("API_ID must be an integer"))?;

        let api_hash = std::env::var("API_HASH")
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("API Hash must be a non-empty string"))?;

        let allowed_user_ids =
            parse_id_list(&std::env::var("ALLOWED_USER_IDS").unwrap_or_default())
                .map_err(|e| anyhow::anyhow!("ALLOWED_USER_IDS: {e}"))?;
        if allowed_user_ids.is_empty() {
            anyhow::bail!(
                "ALLOWED_USER_IDS environment variable must be set with at least one user ID"
            );
        }

        let super_users = parse_id_list(&std::env::var("SUPERUSERS").unwrap_or_default())
            .map_err(|e| anyhow::anyhow!("SUPERUSERS: {e}"))?;
        let allowed_users_file = std::env::var_os("ALLOWED_USERS_FILE")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("allowed-users.txt"));

        // NOTE: `$-_` is an ASCII range (0x24..=0x5F) that includes `/`, `:`, `?`, `=`, etc.
        // This mirrors the original Python regex `[$-_@.&+]` exactly (the `-` is NOT escaped).
        let url_pattern = Regex::new(r"https?://(?:[$-_@.&+!*(),a-zA-Z0-9]|(?:%[0-9a-fA-F]{2}))+")?;

        let download_concurrency = parse_positive_usize(
            "DOWNLOAD_CONCURRENCY",
            &std::env::var("DOWNLOAD_CONCURRENCY").unwrap_or_else(|_| "2".to_string()),
        )?;
        let download_queue_capacity = parse_positive_usize(
            "DOWNLOAD_QUEUE_CAPACITY",
            &std::env::var("DOWNLOAD_QUEUE_CAPACITY").unwrap_or_else(|_| "20".to_string()),
        )?;
        let max_upload_size_mb = parse_positive_usize(
            "MAX_UPLOAD_SIZE_MB",
            &std::env::var("MAX_UPLOAD_SIZE_MB").unwrap_or_else(|_| "1900".to_string()),
        )?;
        let command_timeout_secs = parse_positive_usize(
            "COMMAND_TIMEOUT_SECS",
            &std::env::var("COMMAND_TIMEOUT_SECS").unwrap_or_else(|_| "10800".to_string()),
        )?;
        let upload_timeout_secs = parse_positive_usize(
            "UPLOAD_TIMEOUT_SECS",
            &std::env::var("UPLOAD_TIMEOUT_SECS").unwrap_or_else(|_| "7200".to_string()),
        )?;
        let job_timeout_secs = parse_positive_usize(
            "JOB_TIMEOUT_SECS",
            &std::env::var("JOB_TIMEOUT_SECS").unwrap_or_else(|_| "21600".to_string()),
        )?;

        Ok(Self {
            api_id,
            api_hash,
            allowed_user_ids,
            allowed_users_file,
            super_users,
            url_pattern,
            download_concurrency,
            download_queue_capacity,
            max_upload_size_mb,
            command_timeout_secs,
            upload_timeout_secs,
            job_timeout_secs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern() -> Regex {
        Regex::new(r"https?://(?:[$-_@.&+!*(),a-zA-Z0-9]|(?:%[0-9a-fA-F]{2}))+").unwrap()
    }

    #[test]
    fn captures_full_url_with_path() {
        let re = pattern();
        let msg = "https://x.com/musa1907_/status/2067571186532270261";
        let m = re.find(msg).expect("should match");
        assert_eq!(m.as_str(), msg, "must include the path after the host");
    }

    #[test]
    fn captures_query_chars() {
        let re = pattern();
        let url = "https://www.youtube.com/watch?v=abc-123_456&t=10s";
        let m = re.find(url).expect("should match");
        assert_eq!(m.as_str(), url);
    }

    #[test]
    fn accepts_positive_queue_settings() {
        assert_eq!(parse_positive_usize("TEST", "1").unwrap(), 1);
        assert_eq!(parse_positive_usize("TEST", "32").unwrap(), 32);
    }

    #[test]
    fn rejects_invalid_queue_settings() {
        assert!(parse_positive_usize("TEST", "0").is_err());
        assert!(parse_positive_usize("TEST", "-1").is_err());
        assert!(parse_positive_usize("TEST", "many").is_err());
    }
}
