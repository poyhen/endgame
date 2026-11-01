use regex::Regex;
use serde::Deserialize;
use std::path::PathBuf;

const DEFAULT_CONFIG_PATH: &str = "config.json";

pub struct Config {
    pub api_id: i32,
    pub api_hash: String,
    pub super_users: Vec<i64>,
    pub allowed_users: Vec<i64>,
    pub config_path: PathBuf,
    pub url_pattern: Regex,
    pub yt_dlp_domains: Vec<String>,
    pub telegram_use_ipv6: bool,
    pub download_concurrency: usize,
    pub download_queue_capacity: usize,
    pub max_upload_size_mb: usize,
    pub command_timeout_secs: usize,
    pub upload_timeout_secs: usize,
    pub job_timeout_secs: usize,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    superusers: Vec<i64>,
    allowed_users: Vec<i64>,
    yt_dlp_domains: Vec<String>,
    telegram_use_ipv6: bool,
    download_concurrency: usize,
    download_queue_capacity: usize,
    max_upload_size_mb: usize,
    command_timeout_secs: usize,
    upload_timeout_secs: usize,
    job_timeout_secs: usize,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            superusers: Vec::new(),
            allowed_users: Vec::new(),
            yt_dlp_domains: Vec::new(),
            telegram_use_ipv6: false,
            download_concurrency: 2,
            download_queue_capacity: 20,
            max_upload_size_mb: 1900,
            command_timeout_secs: 10800,
            upload_timeout_secs: 7200,
            job_timeout_secs: 21600,
        }
    }
}

impl FileConfig {
    fn validate(&self) -> anyhow::Result<()> {
        for (name, value) in [
            ("download_concurrency", self.download_concurrency),
            ("download_queue_capacity", self.download_queue_capacity),
            ("max_upload_size_mb", self.max_upload_size_mb),
            ("command_timeout_secs", self.command_timeout_secs),
            ("upload_timeout_secs", self.upload_timeout_secs),
            ("job_timeout_secs", self.job_timeout_secs),
        ] {
            if value == 0 {
                anyhow::bail!("{name} must be greater than zero");
            }
        }
        for user_id in self.superusers.iter().chain(self.allowed_users.iter()) {
            if *user_id <= 0 {
                anyhow::bail!("user IDs must be positive, found {user_id}");
            }
        }
        if self.superusers.is_empty() && self.allowed_users.is_empty() {
            anyhow::bail!("config must list at least one superuser or allowed user");
        }
        for (index, domain) in self.yt_dlp_domains.iter().enumerate() {
            let domain = domain.strip_suffix('.').unwrap_or(domain);
            if domain.len() > 253
                || domain.split('.').any(|label| {
                    label.is_empty()
                        || label.len() > 63
                        || label.starts_with('-')
                        || label.ends_with('-')
                        || !label
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                })
            {
                anyhow::bail!(
                    "yt_dlp_domains[{index}] must be a hostname without a scheme, path, port, or wildcard"
                );
            }
        }
        Ok(())
    }
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
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

        let config_path = std::env::var_os("CONFIG_FILE")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));
        let content = std::fs::read_to_string(&config_path).map_err(|error| {
            anyhow::anyhow!(
                "could not read {}: {error} (copy config.example.json and fill in your user IDs)",
                config_path.display()
            )
        })?;
        let file_config: FileConfig = serde_json::from_str(&content)
            .map_err(|error| anyhow::anyhow!("invalid {}: {error}", config_path.display()))?;
        file_config.validate()?;

        // NOTE: `$-_` is an ASCII range (0x24..=0x5F) that includes `/`, `:`, `?`, `=`, etc.
        // This mirrors the original Python regex `[$-_@.&+]` exactly (the `-` is NOT escaped).
        let url_pattern = Regex::new(r"https?://(?:[$-_@.&+!*(),a-zA-Z0-9]|(?:%[0-9a-fA-F]{2}))+")?;

        Ok(Self {
            api_id,
            api_hash,
            super_users: file_config.superusers,
            allowed_users: file_config.allowed_users,
            config_path,
            url_pattern,
            yt_dlp_domains: file_config.yt_dlp_domains,
            telegram_use_ipv6: file_config.telegram_use_ipv6,
            download_concurrency: file_config.download_concurrency,
            download_queue_capacity: file_config.download_queue_capacity,
            max_upload_size_mb: file_config.max_upload_size_mb,
            command_timeout_secs: file_config.command_timeout_secs,
            upload_timeout_secs: file_config.upload_timeout_secs,
            job_timeout_secs: file_config.job_timeout_secs,
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
    fn applies_defaults_for_missing_optional_keys() {
        let cfg: FileConfig = serde_json::from_str(r#"{"superusers": [1]}"#).unwrap();
        assert_eq!(cfg.superusers, vec![1]);
        assert!(cfg.allowed_users.is_empty());
        assert!(cfg.yt_dlp_domains.is_empty());
        assert!(!cfg.telegram_use_ipv6);
        assert_eq!(cfg.download_concurrency, 2);
        assert_eq!(cfg.download_queue_capacity, 20);
        assert_eq!(cfg.max_upload_size_mb, 1900);
        assert_eq!(cfg.command_timeout_secs, 10800);
        assert_eq!(cfg.upload_timeout_secs, 7200);
        assert_eq!(cfg.job_timeout_secs, 21600);
        cfg.validate().unwrap();
    }

    #[test]
    fn accepts_ipv6_for_hosts_with_a_better_telegram_route() {
        let cfg: FileConfig =
            serde_json::from_str(r#"{"superusers": [1], "telegram_use_ipv6": true}"#).unwrap();
        assert!(cfg.telegram_use_ipv6);
        cfg.validate().unwrap();
    }

    #[test]
    fn rejects_invalid_config_values() {
        let zero: FileConfig =
            serde_json::from_str(r#"{"superusers": [1], "download_queue_capacity": 0}"#).unwrap();
        assert!(zero.validate().is_err());

        let negative_id: FileConfig = serde_json::from_str(r#"{"allowed_users": [-1]}"#).unwrap();
        assert!(negative_id.validate().is_err());

        let nobody: FileConfig = serde_json::from_str("{}").unwrap();
        assert!(nobody.validate().is_err());
    }

    #[test]
    fn rejects_unknown_config_keys() {
        let error =
            serde_json::from_str::<FileConfig>(r#"{"superusers": [1], "download_concurency": 4}"#)
                .err()
                .expect("a misspelled config key must be rejected");

        assert!(
            error
                .to_string()
                .contains("unknown field `download_concurency`"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn validates_downloader_domain_allowlist() {
        let mut cfg = FileConfig {
            superusers: vec![1],
            yt_dlp_domains: vec!["media.example.com".into(), "VIDEO.EXAMPLE.NET.".into()],
            ..FileConfig::default()
        };
        cfg.validate().unwrap();

        for domain in [
            "",
            " ",
            "https://example.com",
            "example.com/video",
            "example.com:443",
            "*.example.com",
            "user@example.com",
            "example..com",
            "example.com..",
            "-example.com",
            "example-.com",
        ] {
            cfg.yt_dlp_domains = vec![domain.into()];
            let error = cfg.validate().unwrap_err();
            assert!(error.to_string().contains("yt_dlp_domains[0]"));
        }
    }

    #[test]
    fn committed_example_config_stays_valid() {
        let cfg: FileConfig = serde_json::from_str(include_str!("../config.example.json")).unwrap();
        cfg.validate().unwrap();
    }
}
