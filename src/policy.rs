use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;

const MEBIBYTE: u64 = 1024 * 1024;
const MAX_PACKAGE_NAME_LEN: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackageName(String);

impl PackageName {
    pub fn parse(value: &str) -> Option<Self> {
        let normalized = value.to_ascii_lowercase();
        let mut bytes = normalized.bytes();
        let first = bytes.next()?;
        if normalized.len() > MAX_PACKAGE_NAME_LEN
            || !first.is_ascii_alphanumeric()
            || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return None;
        }
        Some(Self(normalized))
    }

    pub fn require(value: &str) -> anyhow::Result<Self> {
        Self::parse(value).ok_or_else(|| {
            anyhow::anyhow!(
                "package names must be 1-{MAX_PACKAGE_NAME_LEN} ASCII letters, numbers, `_`, or `-`, and start with a letter or number"
            )
        })
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PackageName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserLimitName {
    MaxActiveJobs,
    MaxQueuedJobs,
    DailyJobLimit,
    MaxUploadSizeMb,
}

impl UserLimitName {
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "active" | "max_active_jobs" => Some(Self::MaxActiveJobs),
            "queued" | "queue" | "max_queued_jobs" => Some(Self::MaxQueuedJobs),
            "daily" | "daily_job_limit" => Some(Self::DailyJobLimit),
            "upload" | "upload_mb" | "max_upload_size_mb" => Some(Self::MaxUploadSizeMb),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MaxActiveJobs => "max_active_jobs",
            Self::MaxQueuedJobs => "max_queued_jobs",
            Self::DailyJobLimit => "daily_job_limit",
            Self::MaxUploadSizeMb => "max_upload_size_mb",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserLimitValue {
    Default,
    Value(u64),
}

impl UserLimitValue {
    pub fn parse(value: &str) -> Option<Self> {
        if value.eq_ignore_ascii_case("default") {
            return Some(Self::Default);
        }
        value
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .map(Self::Value)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PackageLimitDefaults {
    pub max_active_jobs: Option<usize>,
    pub max_queued_jobs: Option<usize>,
    pub daily_job_limit: Option<u64>,
    pub max_upload_size_mb: Option<u64>,
}

impl PackageLimitDefaults {
    pub fn validate(self, path: &str) -> anyhow::Result<()> {
        for (field, value) in [
            (
                "max_active_jobs",
                self.max_active_jobs.map(|value| value as u64),
            ),
            (
                "max_queued_jobs",
                self.max_queued_jobs.map(|value| value as u64),
            ),
            ("daily_job_limit", self.daily_job_limit),
            ("max_upload_size_mb", self.max_upload_size_mb),
        ] {
            if value == Some(0) {
                anyhow::bail!("{path}.{field} must be greater than zero or null");
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UserLimitOverrides {
    pub max_active_jobs: Option<usize>,
    pub max_queued_jobs: Option<usize>,
    pub daily_job_limit: Option<u64>,
    pub max_upload_size_mb: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffectiveUserLimits {
    pub max_active_jobs: usize,
    pub max_queued_jobs: usize,
    pub daily_job_limit: Option<u64>,
    pub max_upload_bytes: u64,
}

#[derive(Clone, Debug)]
pub struct UserPolicies {
    packages: BTreeMap<PackageName, PackageLimitDefaults>,
    default_package: PackageName,
    hard_max_active_jobs: usize,
    hard_max_queued_jobs: usize,
    hard_max_upload_size_mb: u64,
}

impl UserPolicies {
    pub fn new(
        packages: BTreeMap<PackageName, PackageLimitDefaults>,
        default_package: PackageName,
        hard_max_active_jobs: usize,
        hard_max_queued_jobs: usize,
        hard_max_upload_size_mb: u64,
    ) -> Self {
        Self {
            packages,
            default_package,
            hard_max_active_jobs,
            hard_max_queued_jobs,
            hard_max_upload_size_mb,
        }
    }

    pub fn default_package(&self) -> &PackageName {
        &self.default_package
    }

    pub fn has_package(&self, package: &PackageName) -> bool {
        self.packages.contains_key(package)
    }

    pub fn packages(&self) -> impl ExactSizeIterator<Item = (&PackageName, &PackageLimitDefaults)> {
        self.packages.iter()
    }

    pub fn resolve(
        &self,
        package: &PackageName,
        overrides: UserLimitOverrides,
    ) -> anyhow::Result<EffectiveUserLimits> {
        let defaults = self
            .packages
            .get(package)
            .ok_or_else(|| anyhow::anyhow!("package `{package}` is not configured"))?;
        Ok(self.resolve_with_defaults(*defaults, overrides))
    }

    pub fn package_defaults(&self, package: &PackageName) -> anyhow::Result<EffectiveUserLimits> {
        self.resolve(package, UserLimitOverrides::default())
    }

    pub fn superuser_limits(&self) -> EffectiveUserLimits {
        EffectiveUserLimits {
            max_active_jobs: self.hard_max_active_jobs,
            max_queued_jobs: self.hard_max_queued_jobs,
            daily_job_limit: None,
            max_upload_bytes: self.hard_max_upload_size_mb.saturating_mul(MEBIBYTE),
        }
    }

    pub fn validate_override(
        &self,
        name: UserLimitName,
        value: UserLimitValue,
    ) -> anyhow::Result<()> {
        let UserLimitValue::Value(value) = value else {
            return Ok(());
        };
        let hard_limit = match name {
            UserLimitName::MaxActiveJobs => Some(self.hard_max_active_jobs as u64),
            UserLimitName::MaxQueuedJobs => Some(self.hard_max_queued_jobs as u64),
            UserLimitName::DailyJobLimit => None,
            UserLimitName::MaxUploadSizeMb => Some(self.hard_max_upload_size_mb),
        };
        if let Some(hard_limit) = hard_limit
            && value > hard_limit
        {
            anyhow::bail!(
                "{} cannot exceed the global maximum of {hard_limit}",
                name.as_str()
            );
        }
        Ok(())
    }

    fn resolve_with_defaults(
        &self,
        defaults: PackageLimitDefaults,
        overrides: UserLimitOverrides,
    ) -> EffectiveUserLimits {
        let max_active_jobs = overrides
            .max_active_jobs
            .or(defaults.max_active_jobs)
            .unwrap_or(self.hard_max_active_jobs)
            .min(self.hard_max_active_jobs);
        let max_queued_jobs = overrides
            .max_queued_jobs
            .or(defaults.max_queued_jobs)
            .unwrap_or(self.hard_max_queued_jobs)
            .min(self.hard_max_queued_jobs);
        let max_upload_size_mb = overrides
            .max_upload_size_mb
            .or(defaults.max_upload_size_mb)
            .unwrap_or(self.hard_max_upload_size_mb)
            .min(self.hard_max_upload_size_mb);

        EffectiveUserLimits {
            max_active_jobs,
            max_queued_jobs,
            daily_job_limit: overrides.daily_job_limit.or(defaults.daily_job_limit),
            max_upload_bytes: max_upload_size_mb.saturating_mul(MEBIBYTE),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(value: &str) -> PackageName {
        PackageName::require(value).unwrap()
    }

    fn policies() -> UserPolicies {
        UserPolicies::new(
            BTreeMap::from([
                (
                    package("free"),
                    PackageLimitDefaults {
                        max_active_jobs: Some(1),
                        max_queued_jobs: Some(2),
                        daily_job_limit: Some(5),
                        max_upload_size_mb: Some(100),
                    },
                ),
                (
                    package("pro"),
                    PackageLimitDefaults {
                        max_active_jobs: Some(4),
                        max_queued_jobs: None,
                        daily_job_limit: None,
                        max_upload_size_mb: Some(2_000),
                    },
                ),
            ]),
            package("free"),
            3,
            10,
            1_900,
        )
    }

    #[test]
    fn package_names_are_normalized_and_validated() {
        assert_eq!(
            PackageName::parse("Pro_Annual").unwrap().as_str(),
            "pro_annual"
        );
        assert!(PackageName::parse("-invalid").is_none());
        assert!(PackageName::parse("has spaces").is_none());
        assert!(PackageName::parse("").is_none());
    }

    #[test]
    fn resolves_arbitrary_packages_overrides_and_hard_caps() {
        assert_eq!(
            policies()
                .resolve(&package("free"), UserLimitOverrides::default())
                .unwrap(),
            EffectiveUserLimits {
                max_active_jobs: 1,
                max_queued_jobs: 2,
                daily_job_limit: Some(5),
                max_upload_bytes: 100 * MEBIBYTE,
            }
        );

        assert_eq!(
            policies()
                .resolve(
                    &package("pro"),
                    UserLimitOverrides {
                        max_active_jobs: Some(2),
                        max_queued_jobs: Some(30),
                        daily_job_limit: Some(50),
                        max_upload_size_mb: Some(3_000),
                    },
                )
                .unwrap(),
            EffectiveUserLimits {
                max_active_jobs: 2,
                max_queued_jobs: 10,
                daily_job_limit: Some(50),
                max_upload_bytes: 1_900 * MEBIBYTE,
            }
        );
        assert!(
            policies()
                .resolve(&package("missing"), UserLimitOverrides::default())
                .is_err()
        );
    }

    #[test]
    fn superusers_only_keep_global_safety_caps() {
        assert_eq!(
            policies().superuser_limits(),
            EffectiveUserLimits {
                max_active_jobs: 3,
                max_queued_jobs: 10,
                daily_job_limit: None,
                max_upload_bytes: 1_900 * MEBIBYTE,
            }
        );
    }

    #[test]
    fn parses_command_friendly_limit_names() {
        assert_eq!(
            UserLimitName::parse("queue"),
            Some(UserLimitName::MaxQueuedJobs)
        );
        assert_eq!(
            UserLimitName::parse("upload_mb"),
            Some(UserLimitName::MaxUploadSizeMb)
        );
        assert_eq!(
            UserLimitValue::parse("default"),
            Some(UserLimitValue::Default)
        );
        assert_eq!(UserLimitValue::parse("12"), Some(UserLimitValue::Value(12)));
        assert_eq!(UserLimitValue::parse("0"), None);
    }
}
