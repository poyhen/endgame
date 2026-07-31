use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::Deserialize;

use crate::policy::{PackageLimitDefaults, PackageName, UserPolicies};

const DEFAULT_CONFIG_PATH: &str = "config.json";

pub struct Config {
    pub api_id: i32,
    pub api_hash: String,
    pub super_users: Vec<i64>,
    pub allowed_users: Vec<i64>,
    pub database_path: PathBuf,
    pub user_policies: UserPolicies,
    pub default_package: PackageName,
    pub superuser_package: PackageName,
    pub url_pattern: Regex,
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
    database: DatabaseConfig,
    default_package: String,
    superuser_package: Option<String>,
    packages: BTreeMap<String, PackageLimitDefaults>,
    user_tiers: Option<LegacyUserTiersConfig>,
    download_concurrency: usize,
    download_queue_capacity: usize,
    max_upload_size_mb: usize,
    command_timeout_secs: usize,
    upload_timeout_secs: usize,
    job_timeout_secs: usize,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct DatabaseConfig {
    path: PathBuf,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from("endgame.db"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
struct LegacyUserTiersConfig {
    free: PackageLimitDefaults,
    paid: PackageLimitDefaults,
}

struct ResolvedPackages {
    packages: BTreeMap<PackageName, PackageLimitDefaults>,
    default_package: PackageName,
    superuser_package: PackageName,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            superusers: Vec::new(),
            allowed_users: Vec::new(),
            database: DatabaseConfig::default(),
            default_package: "free".to_string(),
            superuser_package: None,
            packages: BTreeMap::new(),
            user_tiers: None,
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
    fn validate(&self) -> anyhow::Result<ResolvedPackages> {
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
        if self.database.path.as_os_str().is_empty() {
            anyhow::bail!("database.path must not be empty");
        }
        self.resolve_packages()
    }

    fn resolve_packages(&self) -> anyhow::Result<ResolvedPackages> {
        if !self.packages.is_empty() && self.user_tiers.is_some() {
            anyhow::bail!("configure `packages` or legacy `user_tiers`, not both");
        }

        let raw_packages = if self.packages.is_empty() {
            let legacy = self.user_tiers.unwrap_or_default();
            BTreeMap::from([
                ("free".to_string(), legacy.free),
                ("paid".to_string(), legacy.paid),
            ])
        } else {
            self.packages.clone()
        };

        let mut packages = BTreeMap::new();
        for (raw_name, limits) in raw_packages {
            let name = PackageName::require(&raw_name)
                .map_err(|error| anyhow::anyhow!("invalid package `{raw_name}`: {error}"))?;
            let path = format!("packages.{name}");
            limits.validate(&path)?;
            self.validate_package_caps(&path, limits)?;
            if packages.insert(name.clone(), limits).is_some() {
                anyhow::bail!("package `{name}` is configured more than once after normalization");
            }
        }

        let default_package = PackageName::require(&self.default_package)
            .map_err(|error| anyhow::anyhow!("invalid default_package: {error}"))?;
        if !packages.contains_key(&default_package) {
            anyhow::bail!("default_package `{default_package}` is not defined in packages");
        }

        let superuser_package = match self.superuser_package.as_deref() {
            Some(value) => PackageName::require(value)
                .map_err(|error| anyhow::anyhow!("invalid superuser_package: {error}"))?,
            None => PackageName::parse("paid")
                .filter(|package| packages.contains_key(package))
                .unwrap_or_else(|| default_package.clone()),
        };
        if !packages.contains_key(&superuser_package) {
            anyhow::bail!("superuser_package `{superuser_package}` is not defined in packages");
        }

        Ok(ResolvedPackages {
            packages,
            default_package,
            superuser_package,
        })
    }

    fn validate_package_caps(
        &self,
        path: &str,
        limits: PackageLimitDefaults,
    ) -> anyhow::Result<()> {
        for (field, value, hard_limit) in [
            (
                "max_active_jobs",
                limits.max_active_jobs.map(|value| value as u64),
                self.download_concurrency as u64,
            ),
            (
                "max_queued_jobs",
                limits.max_queued_jobs.map(|value| value as u64),
                self.download_queue_capacity as u64,
            ),
            (
                "max_upload_size_mb",
                limits.max_upload_size_mb,
                self.max_upload_size_mb as u64,
            ),
        ] {
            if value.is_some_and(|value| value > hard_limit) {
                anyhow::bail!("{path}.{field} cannot exceed the global maximum of {hard_limit}");
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
        let resolved_packages = file_config.validate()?;
        let database_path = resolve_from_config(&config_path, &file_config.database.path);
        let user_policies = UserPolicies::new(
            resolved_packages.packages,
            resolved_packages.default_package.clone(),
            file_config.download_concurrency,
            file_config.download_queue_capacity,
            file_config.max_upload_size_mb as u64,
        );

        // NOTE: `$-_` is an ASCII range (0x24..=0x5F) that includes `/`, `:`, `?`, `=`, etc.
        // This mirrors the original Python regex `[$-_@.&+]` exactly (the `-` is NOT escaped).
        let url_pattern = Regex::new(r"https?://(?:[$-_@.&+!*(),a-zA-Z0-9]|(?:%[0-9a-fA-F]{2}))+")?;

        Ok(Self {
            api_id,
            api_hash,
            super_users: file_config.superusers,
            allowed_users: file_config.allowed_users,
            database_path,
            user_policies,
            default_package: resolved_packages.default_package,
            superuser_package: resolved_packages.superuser_package,
            url_pattern,
            download_concurrency: file_config.download_concurrency,
            download_queue_capacity: file_config.download_queue_capacity,
            max_upload_size_mb: file_config.max_upload_size_mb,
            command_timeout_secs: file_config.command_timeout_secs,
            upload_timeout_secs: file_config.upload_timeout_secs,
            job_timeout_secs: file_config.job_timeout_secs,
        })
    }
}

fn resolve_from_config(config_path: &Path, value: &Path) -> PathBuf {
    if value.is_absolute() {
        return value.to_path_buf();
    }
    config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(value)
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
    fn applies_compatible_defaults_for_missing_package_keys() {
        let cfg: FileConfig = serde_json::from_str(r#"{"superusers": [1]}"#).unwrap();
        assert_eq!(cfg.superusers, vec![1]);
        assert!(cfg.allowed_users.is_empty());
        assert_eq!(cfg.database.path, PathBuf::from("endgame.db"));
        let packages = cfg.validate().unwrap();
        assert_eq!(packages.default_package.as_str(), "free");
        assert_eq!(packages.superuser_package.as_str(), "paid");
        assert_eq!(packages.packages.len(), 2);
        assert_eq!(cfg.download_concurrency, 2);
        assert_eq!(cfg.download_queue_capacity, 20);
        assert_eq!(cfg.max_upload_size_mb, 1900);
        assert_eq!(cfg.command_timeout_secs, 10800);
        assert_eq!(cfg.upload_timeout_secs, 7200);
        assert_eq!(cfg.job_timeout_secs, 21600);
    }

    #[test]
    fn supports_legacy_user_tiers() {
        let cfg: FileConfig = serde_json::from_str(
            r#"{
                "superusers": [1],
                "user_tiers": {"free": {"daily_job_limit": 5}}
            }"#,
        )
        .unwrap();
        let packages = cfg.validate().unwrap();
        assert_eq!(
            packages
                .packages
                .get(&PackageName::require("free").unwrap())
                .unwrap()
                .daily_job_limit,
            Some(5)
        );
    }

    #[test]
    fn accepts_arbitrary_named_packages() {
        let cfg: FileConfig = serde_json::from_str(
            r#"{
                "superusers": [1],
                "default_package": "starter",
                "superuser_package": "enterprise",
                "packages": {
                    "starter": {"daily_job_limit": 5},
                    "pro-monthly": {"daily_job_limit": 50},
                    "enterprise": {"daily_job_limit": null}
                }
            }"#,
        )
        .unwrap();
        let packages = cfg.validate().unwrap();
        assert_eq!(packages.packages.len(), 3);
        assert_eq!(packages.default_package.as_str(), "starter");
        assert_eq!(packages.superuser_package.as_str(), "enterprise");
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

        let missing_default: FileConfig = serde_json::from_str(
            r#"{
                "superusers": [1],
                "packages": {"starter": {}}
            }"#,
        )
        .unwrap();
        assert!(missing_default.validate().is_err());
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
    fn validates_package_limits_against_global_caps() {
        let too_many_active: FileConfig = serde_json::from_str(
            r#"{
                "superusers": [1],
                "packages": {
                    "free": {},
                    "paid": {"max_active_jobs": 3}
                },
                "download_concurrency": 2
            }"#,
        )
        .unwrap();
        assert!(too_many_active.validate().is_err());

        let zero_daily: FileConfig = serde_json::from_str(
            r#"{
                "superusers": [1],
                "packages": {
                    "free": {"daily_job_limit": 0},
                    "paid": {}
                }
            }"#,
        )
        .unwrap();
        assert!(zero_daily.validate().is_err());
    }

    #[test]
    fn resolves_database_paths_beside_the_config_file() {
        assert_eq!(
            resolve_from_config(
                Path::new("/srv/endgame/config.json"),
                Path::new("data/endgame.db")
            ),
            PathBuf::from("/srv/endgame/data/endgame.db")
        );
        assert_eq!(
            resolve_from_config(
                Path::new("/srv/endgame/config.json"),
                Path::new("/var/lib/endgame.db")
            ),
            PathBuf::from("/var/lib/endgame.db")
        );
    }

    #[test]
    fn committed_example_config_stays_valid() {
        let cfg: FileConfig = serde_json::from_str(include_str!("../config.example.json")).unwrap();
        cfg.validate().unwrap();
    }
}
