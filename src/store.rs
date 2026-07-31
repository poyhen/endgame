use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;
use turso::transaction::TransactionBehavior;
use turso::{Connection, params};

use crate::policy::{PackageName, UserLimitName, UserLimitOverrides, UserLimitValue};

const SCHEMA_VERSION: i64 = 2;
const BOOTSTRAP_IMPORT_KEY: &str = "bootstrap_users_imported_v1";

const SCHEMA_V1_SQL: &str = "
    CREATE TABLE app_meta (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    CREATE TABLE users (
        telegram_id INTEGER PRIMARY KEY CHECK (telegram_id > 0),
        enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
        tier TEXT NOT NULL DEFAULT 'free' CHECK (tier IN ('free', 'paid')),
        max_active_jobs INTEGER CHECK (
            max_active_jobs IS NULL OR max_active_jobs > 0
        ),
        max_queued_jobs INTEGER CHECK (
            max_queued_jobs IS NULL OR max_queued_jobs > 0
        ),
        daily_job_limit INTEGER CHECK (
            daily_job_limit IS NULL OR daily_job_limit > 0
        ),
        max_upload_size_mb INTEGER CHECK (
            max_upload_size_mb IS NULL OR max_upload_size_mb > 0
        ),
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE user_daily_usage (
        telegram_id INTEGER NOT NULL
            REFERENCES users(telegram_id) ON DELETE CASCADE,
        utc_day INTEGER NOT NULL,
        accepted_jobs INTEGER NOT NULL DEFAULT 0
            CHECK (accepted_jobs >= 0),
        PRIMARY KEY (telegram_id, utc_day)
    );";

const MIGRATE_V1_TO_V2_SQL: &str = "
    CREATE TABLE users_v2 (
        telegram_id INTEGER PRIMARY KEY CHECK (telegram_id > 0),
        enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
        package TEXT NOT NULL DEFAULT 'free'
            CHECK (length(package) BETWEEN 1 AND 32),
        max_active_jobs INTEGER CHECK (
            max_active_jobs IS NULL OR max_active_jobs > 0
        ),
        max_queued_jobs INTEGER CHECK (
            max_queued_jobs IS NULL OR max_queued_jobs > 0
        ),
        daily_job_limit INTEGER CHECK (
            daily_job_limit IS NULL OR daily_job_limit > 0
        ),
        max_upload_size_mb INTEGER CHECK (
            max_upload_size_mb IS NULL OR max_upload_size_mb > 0
        ),
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    INSERT INTO users_v2 (
        telegram_id, enabled, package, max_active_jobs, max_queued_jobs,
        daily_job_limit, max_upload_size_mb, created_at, updated_at
    )
    SELECT
        telegram_id, enabled, tier, max_active_jobs, max_queued_jobs,
        daily_job_limit, max_upload_size_mb, created_at, updated_at
    FROM users;

    CREATE TABLE user_daily_usage_v2 (
        telegram_id INTEGER NOT NULL
            REFERENCES users_v2(telegram_id) ON DELETE CASCADE,
        utc_day INTEGER NOT NULL,
        accepted_jobs INTEGER NOT NULL DEFAULT 0
            CHECK (accepted_jobs >= 0),
        PRIMARY KEY (telegram_id, utc_day)
    );
    INSERT INTO user_daily_usage_v2 (telegram_id, utc_day, accepted_jobs)
    SELECT telegram_id, utc_day, accepted_jobs
    FROM user_daily_usage;

    DROP TABLE user_daily_usage;
    DROP TABLE users;
    ALTER TABLE users_v2 RENAME TO users;
    ALTER TABLE user_daily_usage_v2 RENAME TO user_daily_usage;

    CREATE TABLE user_stats (
        telegram_id INTEGER PRIMARY KEY
            REFERENCES users(telegram_id) ON DELETE CASCADE,
        accepted_jobs INTEGER NOT NULL DEFAULT 0 CHECK (accepted_jobs >= 0),
        completed_jobs INTEGER NOT NULL DEFAULT 0 CHECK (completed_jobs >= 0),
        failed_jobs INTEGER NOT NULL DEFAULT 0 CHECK (failed_jobs >= 0),
        cancelled_jobs INTEGER NOT NULL DEFAULT 0 CHECK (cancelled_jobs >= 0),
        interrupted_jobs INTEGER NOT NULL DEFAULT 0 CHECK (interrupted_jobs >= 0),
        updated_at INTEGER NOT NULL
    );";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserRecord {
    pub telegram_id: i64,
    pub enabled: bool,
    pub package: PackageName,
    pub limits: UserLimitOverrides,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DownloadStats {
    pub accepted_jobs: u64,
    pub completed_jobs: u64,
    pub failed_jobs: u64,
    pub cancelled_jobs: u64,
    pub interrupted_jobs: u64,
    pub accepted_today: u64,
}

impl DownloadStats {
    pub fn unresolved_jobs(self) -> u64 {
        self.accepted_jobs.saturating_sub(
            self.completed_jobs
                .saturating_add(self.failed_jobs)
                .saturating_add(self.cancelled_jobs)
                .saturating_add(self.interrupted_jobs),
        )
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GlobalStats {
    pub users: u64,
    pub enabled_users: u64,
    pub downloads: DownloadStats,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownloadStatOutcome {
    Completed,
    Failed,
    Cancelled,
}

impl DownloadStatOutcome {
    const fn column(self) -> &'static str {
        match self {
            Self::Completed => "completed_jobs",
            Self::Failed => "failed_jobs",
            Self::Cancelled => "cancelled_jobs",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddUserOutcome {
    Added,
    Reenabled,
    AlreadyEnabled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoveUserOutcome {
    Disabled,
    AlreadyDisabled,
    NotFound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsageReservation {
    user_id: i64,
    utc_day: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReserveUsageOutcome {
    Reserved(UsageReservation),
    LimitReached,
}

pub struct AppStore {
    connection: Mutex<Connection>,
    path: PathBuf,
}

impl AppStore {
    pub async fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref().to_path_buf();
        prepare_database_files(&path).await?;
        let path_text = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("database path must be valid UTF-8"))?;
        let database = turso::Builder::new_local(path_text).build().await?;
        let connection = database.connect()?;
        connection.execute("PRAGMA foreign_keys = ON", ()).await?;
        connection.execute("PRAGMA busy_timeout = 5000", ()).await?;

        let store = Self {
            connection: Mutex::new(connection),
            path,
        };
        store.migrate().await?;
        secure_database_files(&store.path).await?;
        Ok(store)
    }

    async fn migrate(&self) -> anyhow::Result<()> {
        let mut connection = self.connection.lock().await;
        let mut version = query_optional_i64(&connection, "PRAGMA user_version", ())
            .await?
            .unwrap_or_default();
        if version > SCHEMA_VERSION {
            anyhow::bail!(
                "database schema version {version} is newer than supported version {SCHEMA_VERSION}"
            );
        }

        while version < SCHEMA_VERSION {
            match version {
                0 => {
                    let transaction = connection
                        .transaction_with_behavior(TransactionBehavior::Immediate)
                        .await?;
                    transaction.execute_batch(SCHEMA_V1_SQL).await?;
                    transaction.execute("PRAGMA user_version = 1", ()).await?;
                    transaction.commit().await?;
                    version = 1;
                }
                1 => {
                    let transaction = connection
                        .transaction_with_behavior(TransactionBehavior::Immediate)
                        .await?;
                    transaction.execute_batch(MIGRATE_V1_TO_V2_SQL).await?;
                    let now = unix_timestamp()?;
                    transaction
                        .execute(
                            "INSERT INTO user_stats (
                                telegram_id, accepted_jobs, interrupted_jobs, updated_at
                             )
                             SELECT
                                telegram_id, SUM(accepted_jobs), SUM(accepted_jobs), ?1
                             FROM user_daily_usage
                             GROUP BY telegram_id
                             HAVING SUM(accepted_jobs) > 0",
                            [now],
                        )
                        .await?;
                    transaction.execute("PRAGMA user_version = 2", ()).await?;
                    transaction.commit().await?;
                    version = 2;
                }
                _ => anyhow::bail!("no migration path from database schema version {version}"),
            }
        }
        Ok(())
    }

    pub async fn import_bootstrap_users(
        &self,
        allowed_users: &[i64],
        super_users: &[i64],
        default_package: &PackageName,
        superuser_package: &PackageName,
    ) -> anyhow::Result<usize> {
        let mut connection = self.connection.lock().await;
        if query_optional_string(
            &connection,
            "SELECT value FROM app_meta WHERE key = ?1",
            [BOOTSTRAP_IMPORT_KEY],
        )
        .await?
        .is_some()
        {
            return Ok(0);
        }

        let mut users = BTreeMap::new();
        for user_id in allowed_users.iter().copied() {
            users.insert(user_id, default_package);
        }
        for user_id in super_users.iter().copied() {
            users.insert(user_id, superuser_package);
        }

        let now = unix_timestamp()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await?;
        for (user_id, package) in &users {
            validate_user_id(*user_id)?;
            transaction
                .execute(
                    "INSERT OR IGNORE INTO users (
                        telegram_id, enabled, package, created_at, updated_at
                    ) VALUES (?1, 1, ?2, ?3, ?3)",
                    params![*user_id, package.as_str(), now],
                )
                .await?;
        }
        transaction
            .execute(
                "INSERT INTO app_meta (key, value) VALUES (?1, 'true')",
                [BOOTSTRAP_IMPORT_KEY],
            )
            .await?;
        transaction.commit().await?;
        Ok(users.len())
    }

    pub async fn ensure_superusers(
        &self,
        super_users: &[i64],
        package: &PackageName,
    ) -> anyhow::Result<usize> {
        let connection = self.connection.lock().await;
        let now = unix_timestamp()?;
        let mut inserted = 0usize;
        for user_id in super_users.iter().copied() {
            validate_user_id(user_id)?;
            inserted += connection
                .execute(
                    "INSERT OR IGNORE INTO users (
                        telegram_id, enabled, package, created_at, updated_at
                    ) VALUES (?1, 1, ?2, ?3, ?3)",
                    params![user_id, package.as_str(), now],
                )
                .await? as usize;
        }
        Ok(inserted)
    }

    pub async fn reconcile_interrupted_jobs(&self) -> anyhow::Result<u64> {
        let connection = self.connection.lock().await;
        let now = unix_timestamp()?;
        Ok(connection
            .execute(
                "UPDATE user_stats
                 SET interrupted_jobs = interrupted_jobs + (
                        accepted_jobs - completed_jobs - failed_jobs
                        - cancelled_jobs - interrupted_jobs
                     ),
                     updated_at = ?1
                 WHERE accepted_jobs > (
                        completed_jobs + failed_jobs
                        + cancelled_jobs + interrupted_jobs
                     )",
                [now],
            )
            .await?)
    }

    pub async fn is_user_enabled(&self, user_id: i64) -> anyhow::Result<bool> {
        Ok(self
            .get_user(user_id)
            .await?
            .is_some_and(|user| user.enabled))
    }

    pub async fn get_user(&self, user_id: i64) -> anyhow::Result<Option<UserRecord>> {
        let connection = self.connection.lock().await;
        query_user(&connection, user_id).await
    }

    pub async fn list_users(&self) -> anyhow::Result<Vec<UserRecord>> {
        let connection = self.connection.lock().await;
        let mut rows = connection
            .query(
                "SELECT telegram_id, enabled, package, max_active_jobs,
                        max_queued_jobs, daily_job_limit, max_upload_size_mb
                 FROM users
                 ORDER BY telegram_id",
                (),
            )
            .await?;
        let mut users = Vec::new();
        while let Some(row) = rows.next().await? {
            users.push(user_from_row(row)?);
        }
        Ok(users)
    }

    pub async fn add_user(
        &self,
        user_id: i64,
        default_package: &PackageName,
    ) -> anyhow::Result<AddUserOutcome> {
        validate_user_id(user_id)?;
        let connection = self.connection.lock().await;
        let existing = query_user(&connection, user_id).await?;
        let now = unix_timestamp()?;
        let outcome = match existing {
            None => {
                connection
                    .execute(
                        "INSERT INTO users (
                            telegram_id, enabled, package, created_at, updated_at
                        ) VALUES (?1, 1, ?2, ?3, ?3)",
                        params![user_id, default_package.as_str(), now],
                    )
                    .await?;
                AddUserOutcome::Added
            }
            Some(user) if !user.enabled => {
                connection
                    .execute(
                        "UPDATE users SET enabled = 1, updated_at = ?2
                         WHERE telegram_id = ?1",
                        params![user_id, now],
                    )
                    .await?;
                AddUserOutcome::Reenabled
            }
            Some(_) => AddUserOutcome::AlreadyEnabled,
        };
        Ok(outcome)
    }

    pub async fn remove_user(&self, user_id: i64) -> anyhow::Result<RemoveUserOutcome> {
        validate_user_id(user_id)?;
        let connection = self.connection.lock().await;
        let existing = query_user(&connection, user_id).await?;
        let outcome = match existing {
            None => RemoveUserOutcome::NotFound,
            Some(user) if !user.enabled => RemoveUserOutcome::AlreadyDisabled,
            Some(_) => {
                connection
                    .execute(
                        "UPDATE users SET enabled = 0, updated_at = ?2
                         WHERE telegram_id = ?1",
                        params![user_id, unix_timestamp()?],
                    )
                    .await?;
                RemoveUserOutcome::Disabled
            }
        };
        Ok(outcome)
    }

    pub async fn set_user_package(
        &self,
        user_id: i64,
        package: &PackageName,
    ) -> anyhow::Result<bool> {
        validate_user_id(user_id)?;
        let connection = self.connection.lock().await;
        if query_user(&connection, user_id).await?.is_none() {
            return Ok(false);
        }
        connection
            .execute(
                "UPDATE users SET package = ?2, updated_at = ?3
                 WHERE telegram_id = ?1",
                params![user_id, package.as_str(), unix_timestamp()?],
            )
            .await?;
        Ok(true)
    }

    pub async fn set_user_limit(
        &self,
        user_id: i64,
        name: UserLimitName,
        value: UserLimitValue,
    ) -> anyhow::Result<bool> {
        validate_user_id(user_id)?;
        let connection = self.connection.lock().await;
        if query_user(&connection, user_id).await?.is_none() {
            return Ok(false);
        }
        let column = name.as_str();
        let value = match value {
            UserLimitValue::Default => None,
            UserLimitValue::Value(value) => Some(
                i64::try_from(value).map_err(|_| anyhow::anyhow!("limit is too large to store"))?,
            ),
        };
        connection
            .execute(
                format!(
                    "UPDATE users SET {column} = ?2, updated_at = ?3
                     WHERE telegram_id = ?1"
                ),
                params![user_id, value, unix_timestamp()?],
            )
            .await?;
        Ok(true)
    }

    pub async fn reserve_daily_job(
        &self,
        user_id: i64,
        limit: Option<u64>,
    ) -> anyhow::Result<ReserveUsageOutcome> {
        let utc_day = current_utc_day()?;
        let now = unix_timestamp()?;
        let mut connection = self.connection.lock().await;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await?;
        let current = query_optional_i64(
            &transaction,
            "SELECT accepted_jobs FROM user_daily_usage
             WHERE telegram_id = ?1 AND utc_day = ?2",
            params![user_id, utc_day],
        )
        .await?
        .unwrap_or_default();
        if limit.is_some_and(|limit| current as u64 >= limit) {
            transaction.rollback().await?;
            return Ok(ReserveUsageOutcome::LimitReached);
        }
        transaction
            .execute(
                "INSERT INTO user_daily_usage (telegram_id, utc_day, accepted_jobs)
                 VALUES (?1, ?2, 1)
                 ON CONFLICT (telegram_id, utc_day)
                 DO UPDATE SET accepted_jobs = accepted_jobs + 1",
                params![user_id, utc_day],
            )
            .await?;
        transaction
            .execute(
                "INSERT INTO user_stats (telegram_id, accepted_jobs, updated_at)
                 VALUES (?1, 1, ?2)
                 ON CONFLICT (telegram_id)
                 DO UPDATE SET
                    accepted_jobs = accepted_jobs + 1,
                    updated_at = excluded.updated_at",
                params![user_id, now],
            )
            .await?;
        transaction.commit().await?;
        Ok(ReserveUsageOutcome::Reserved(UsageReservation {
            user_id,
            utc_day,
        }))
    }

    pub async fn release_daily_job(&self, reservation: UsageReservation) -> anyhow::Result<()> {
        let mut connection = self.connection.lock().await;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await?;
        transaction
            .execute(
                "UPDATE user_daily_usage
                 SET accepted_jobs = CASE
                     WHEN accepted_jobs > 0 THEN accepted_jobs - 1
                     ELSE 0
                 END
                 WHERE telegram_id = ?1 AND utc_day = ?2",
                params![reservation.user_id, reservation.utc_day],
            )
            .await?;
        transaction
            .execute(
                "UPDATE user_stats
                 SET accepted_jobs = CASE
                        WHEN accepted_jobs > 0 THEN accepted_jobs - 1
                        ELSE 0
                     END,
                     updated_at = ?2
                 WHERE telegram_id = ?1",
                params![reservation.user_id, unix_timestamp()?],
            )
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn record_download_outcome(
        &self,
        user_id: i64,
        outcome: DownloadStatOutcome,
    ) -> anyhow::Result<()> {
        validate_user_id(user_id)?;
        let connection = self.connection.lock().await;
        let column = outcome.column();
        connection
            .execute(
                format!(
                    "INSERT INTO user_stats (telegram_id, {column}, updated_at)
                     VALUES (?1, 1, ?2)
                     ON CONFLICT (telegram_id)
                     DO UPDATE SET
                        {column} = {column} + 1,
                        updated_at = excluded.updated_at"
                ),
                params![user_id, unix_timestamp()?],
            )
            .await?;
        Ok(())
    }

    pub async fn user_stats(&self, user_id: i64) -> anyhow::Result<DownloadStats> {
        validate_user_id(user_id)?;
        let connection = self.connection.lock().await;
        let mut rows = connection
            .query(
                "SELECT
                    COALESCE((SELECT accepted_jobs FROM user_stats
                              WHERE telegram_id = ?1), 0),
                    COALESCE((SELECT completed_jobs FROM user_stats
                              WHERE telegram_id = ?1), 0),
                    COALESCE((SELECT failed_jobs FROM user_stats
                              WHERE telegram_id = ?1), 0),
                    COALESCE((SELECT cancelled_jobs FROM user_stats
                              WHERE telegram_id = ?1), 0),
                    COALESCE((SELECT interrupted_jobs FROM user_stats
                              WHERE telegram_id = ?1), 0),
                    COALESCE((SELECT accepted_jobs FROM user_daily_usage
                              WHERE telegram_id = ?1 AND utc_day = ?2), 0)",
                params![user_id, current_utc_day()?],
            )
            .await?;
        let row = rows
            .next()
            .await?
            .ok_or_else(|| anyhow::anyhow!("statistics query returned no row"))?;
        stats_from_row(&row, 0)
    }

    pub async fn global_stats(&self) -> anyhow::Result<GlobalStats> {
        let connection = self.connection.lock().await;
        let mut rows = connection
            .query(
                "SELECT
                    (SELECT COUNT(*) FROM users),
                    (SELECT COUNT(*) FROM users WHERE enabled = 1),
                    COALESCE((SELECT SUM(accepted_jobs) FROM user_stats), 0),
                    COALESCE((SELECT SUM(completed_jobs) FROM user_stats), 0),
                    COALESCE((SELECT SUM(failed_jobs) FROM user_stats), 0),
                    COALESCE((SELECT SUM(cancelled_jobs) FROM user_stats), 0),
                    COALESCE((SELECT SUM(interrupted_jobs) FROM user_stats), 0),
                    COALESCE((SELECT SUM(accepted_jobs) FROM user_daily_usage
                              WHERE utc_day = ?1), 0)",
                [current_utc_day()?],
            )
            .await?;
        let row = rows
            .next()
            .await?
            .ok_or_else(|| anyhow::anyhow!("global statistics query returned no row"))?;
        Ok(GlobalStats {
            users: nonnegative_u64(row.get(0)?, "users")?,
            enabled_users: nonnegative_u64(row.get(1)?, "enabled_users")?,
            downloads: stats_from_row(&row, 2)?,
        })
    }

    #[cfg(test)]
    async fn daily_usage(&self, user_id: i64, utc_day: i64) -> anyhow::Result<u64> {
        let connection = self.connection.lock().await;
        let value = query_optional_i64(
            &connection,
            "SELECT accepted_jobs FROM user_daily_usage
             WHERE telegram_id = ?1 AND utc_day = ?2",
            params![user_id, utc_day],
        )
        .await?
        .unwrap_or_default();
        nonnegative_u64(value, "accepted_jobs")
    }
}

async fn query_user(connection: &Connection, user_id: i64) -> anyhow::Result<Option<UserRecord>> {
    let mut rows = connection
        .query(
            "SELECT telegram_id, enabled, package, max_active_jobs,
                    max_queued_jobs, daily_job_limit, max_upload_size_mb
             FROM users
             WHERE telegram_id = ?1",
            [user_id],
        )
        .await?;
    let user = match rows.next().await? {
        Some(row) => Some(user_from_row(row)?),
        None => None,
    };
    Ok(user)
}

fn user_from_row(row: turso::Row) -> anyhow::Result<UserRecord> {
    let enabled = row.get::<i64>(1)?;
    if !matches!(enabled, 0 | 1) {
        anyhow::bail!("database contains an invalid enabled value");
    }
    let package_text = row.get::<String>(2)?;
    let package = PackageName::require(&package_text)
        .map_err(|_| anyhow::anyhow!("database contains an invalid package name"))?;
    Ok(UserRecord {
        telegram_id: row.get(0)?,
        enabled: enabled == 1,
        package,
        limits: UserLimitOverrides {
            max_active_jobs: optional_usize(row.get(3)?, "max_active_jobs")?,
            max_queued_jobs: optional_usize(row.get(4)?, "max_queued_jobs")?,
            daily_job_limit: optional_u64(row.get(5)?, "daily_job_limit")?,
            max_upload_size_mb: optional_u64(row.get(6)?, "max_upload_size_mb")?,
        },
    })
}

fn stats_from_row(row: &turso::Row, offset: usize) -> anyhow::Result<DownloadStats> {
    Ok(DownloadStats {
        accepted_jobs: nonnegative_u64(row.get(offset)?, "accepted_jobs")?,
        completed_jobs: nonnegative_u64(row.get(offset + 1)?, "completed_jobs")?,
        failed_jobs: nonnegative_u64(row.get(offset + 2)?, "failed_jobs")?,
        cancelled_jobs: nonnegative_u64(row.get(offset + 3)?, "cancelled_jobs")?,
        interrupted_jobs: nonnegative_u64(row.get(offset + 4)?, "interrupted_jobs")?,
        accepted_today: nonnegative_u64(row.get(offset + 5)?, "accepted_today")?,
    })
}

fn nonnegative_u64(value: i64, name: &str) -> anyhow::Result<u64> {
    u64::try_from(value).map_err(|_| anyhow::anyhow!("database contains an invalid {name}"))
}

fn optional_usize(value: Option<i64>, name: &str) -> anyhow::Result<Option<usize>> {
    value
        .map(|value| {
            usize::try_from(value)
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| anyhow::anyhow!("database contains an invalid {name}"))
        })
        .transpose()
}

fn optional_u64(value: Option<i64>, name: &str) -> anyhow::Result<Option<u64>> {
    value
        .map(|value| {
            u64::try_from(value)
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| anyhow::anyhow!("database contains an invalid {name}"))
        })
        .transpose()
}

async fn query_optional_i64(
    connection: &Connection,
    sql: &str,
    params: impl turso::params::IntoParams,
) -> anyhow::Result<Option<i64>> {
    let mut rows = connection.query(sql, params).await?;
    Ok(match rows.next().await? {
        Some(row) => Some(row.get(0)?),
        None => None,
    })
}

async fn query_optional_string(
    connection: &Connection,
    sql: &str,
    params: impl turso::params::IntoParams,
) -> anyhow::Result<Option<String>> {
    let mut rows = connection.query(sql, params).await?;
    Ok(match rows.next().await? {
        Some(row) => Some(row.get(0)?),
        None => None,
    })
}

fn validate_user_id(user_id: i64) -> anyhow::Result<()> {
    if user_id <= 0 {
        anyhow::bail!("user ID must be positive");
    }
    Ok(())
}

fn unix_timestamp() -> anyhow::Result<i64> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| anyhow::anyhow!("system clock is before Unix epoch: {error}"))?
        .as_secs();
    i64::try_from(seconds).map_err(|_| anyhow::anyhow!("system clock is out of range"))
}

fn current_utc_day() -> anyhow::Result<i64> {
    Ok(unix_timestamp()? / 86_400)
}

async fn prepare_database_files(path: &Path) -> anyhow::Result<()> {
    if path.as_os_str() == OsStr::new(":memory:") {
        return Ok(());
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        tokio::fs::create_dir_all(parent).await?;
    }
    create_private_file(path).await?;
    #[cfg(unix)]
    create_private_file(&sidecar_path(path, "-wal")).await?;
    secure_database_files(path).await
}

async fn create_private_file(path: &Path) -> anyhow::Result<()> {
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    options.open(path).await?;
    secure_existing_file(path).await
}

async fn secure_database_files(path: &Path) -> anyhow::Result<()> {
    if path.as_os_str() == OsStr::new(":memory:") {
        return Ok(());
    }
    secure_existing_file(path).await?;
    for suffix in ["-journal", "-shm", "-tshm", "-wal"] {
        secure_existing_file(&sidecar_path(path, suffix)).await?;
    }
    Ok(())
}

async fn secure_existing_file(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        match tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(value: &str) -> PackageName {
        PackageName::require(value).unwrap()
    }

    fn temporary_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "endgame-store-test-{:016x}.db",
            rand::random::<u64>()
        ))
    }

    async fn remove_database(path: &Path) {
        for suffix in ["", "-journal", "-shm", "-tshm", "-wal"] {
            let candidate = sidecar_path(path, suffix);
            let _ = tokio::fs::remove_file(candidate).await;
        }
    }

    #[tokio::test]
    async fn migrates_v1_users_usage_and_tiers_to_packages() {
        let path = temporary_path();
        prepare_database_files(&path).await.unwrap();
        let database = turso::Builder::new_local(path.to_str().unwrap())
            .build()
            .await
            .unwrap();
        let connection = database.connect().unwrap();
        connection.execute_batch(SCHEMA_V1_SQL).await.unwrap();
        connection
            .execute_batch(
                "INSERT INTO users (
                    telegram_id, enabled, tier, created_at, updated_at
                 ) VALUES (42, 1, 'paid', 1, 1);
                 INSERT INTO user_daily_usage (
                    telegram_id, utc_day, accepted_jobs
                 ) VALUES (42, 1, 3);",
            )
            .await
            .unwrap();
        connection
            .execute("PRAGMA user_version = 1", ())
            .await
            .unwrap();
        drop(connection);
        drop(database);

        let store = AppStore::open(&path).await.unwrap();
        assert_eq!(
            store.get_user(42).await.unwrap().unwrap().package,
            package("paid")
        );
        let stats = store.user_stats(42).await.unwrap();
        assert_eq!(stats.accepted_jobs, 3);
        assert_eq!(stats.interrupted_jobs, 3);
        drop(store);
        remove_database(&path).await;
    }

    #[tokio::test]
    async fn imports_bootstrap_users_once_and_ensures_new_superusers() {
        let path = temporary_path();
        let store = AppStore::open(&path).await.unwrap();
        assert_eq!(
            store
                .import_bootstrap_users(&[11, 22], &[11, 33], &package("starter"), &package("pro"),)
                .await
                .unwrap(),
            3
        );
        assert_eq!(
            store
                .import_bootstrap_users(&[44], &[], &package("starter"), &package("pro"))
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            store.get_user(11).await.unwrap().unwrap().package,
            package("pro")
        );
        assert_eq!(
            store.get_user(22).await.unwrap().unwrap().package,
            package("starter")
        );
        assert_eq!(
            store
                .ensure_superusers(&[55], &package("enterprise"))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            store.get_user(55).await.unwrap().unwrap().package,
            package("enterprise")
        );
        drop(store);

        let store = AppStore::open(&path).await.unwrap();
        assert_eq!(
            store
                .import_bootstrap_users(&[44], &[], &package("starter"), &package("pro"))
                .await
                .unwrap(),
            0
        );
        assert!(store.get_user(44).await.unwrap().is_none());
        drop(store);
        remove_database(&path).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn database_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let path = temporary_path();
        let wal_path = sidecar_path(&path, "-wal");
        tokio::fs::write(&wal_path, []).await.unwrap();
        tokio::fs::set_permissions(&wal_path, std::fs::Permissions::from_mode(0o644))
            .await
            .unwrap();

        let store = AppStore::open(&path).await.unwrap();
        for path in [&path, &wal_path] {
            let mode = tokio::fs::metadata(path)
                .await
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "unexpected mode for {}", path.display());
        }
        drop(store);
        remove_database(&path).await;
    }

    #[tokio::test]
    async fn users_are_disabled_without_losing_package_or_overrides() {
        let path = temporary_path();
        let store = AppStore::open(&path).await.unwrap();
        assert_eq!(
            store.add_user(42, &package("starter")).await.unwrap(),
            AddUserOutcome::Added
        );
        assert!(store.set_user_package(42, &package("pro")).await.unwrap());
        assert!(
            store
                .set_user_limit(42, UserLimitName::MaxQueuedJobs, UserLimitValue::Value(7),)
                .await
                .unwrap()
        );
        assert_eq!(
            store.remove_user(42).await.unwrap(),
            RemoveUserOutcome::Disabled
        );
        assert!(!store.is_user_enabled(42).await.unwrap());
        assert_eq!(
            store.add_user(42, &package("starter")).await.unwrap(),
            AddUserOutcome::Reenabled
        );
        let user = store.get_user(42).await.unwrap().unwrap();
        assert_eq!(user.package, package("pro"));
        assert_eq!(user.limits.max_queued_jobs, Some(7));
        drop(store);
        remove_database(&path).await;
    }

    #[tokio::test]
    async fn admission_and_terminal_outcomes_update_statistics() {
        let path = temporary_path();
        let store = AppStore::open(&path).await.unwrap();
        store.add_user(42, &package("free")).await.unwrap();
        let day = current_utc_day().unwrap();

        let ReserveUsageOutcome::Reserved(_first) =
            store.reserve_daily_job(42, Some(2)).await.unwrap()
        else {
            panic!("first reservation should succeed");
        };
        let ReserveUsageOutcome::Reserved(second) =
            store.reserve_daily_job(42, Some(2)).await.unwrap()
        else {
            panic!("second reservation should succeed");
        };
        assert_eq!(
            store.reserve_daily_job(42, Some(2)).await.unwrap(),
            ReserveUsageOutcome::LimitReached
        );
        store.release_daily_job(second).await.unwrap();
        store
            .record_download_outcome(42, DownloadStatOutcome::Completed)
            .await
            .unwrap();

        let stats = store.user_stats(42).await.unwrap();
        assert_eq!(stats.accepted_jobs, 1);
        assert_eq!(stats.completed_jobs, 1);
        assert_eq!(stats.accepted_today, 1);
        assert_eq!(stats.unresolved_jobs(), 0);
        assert_eq!(store.daily_usage(42, day).await.unwrap(), 1);
        drop(store);
        remove_database(&path).await;
    }

    #[tokio::test]
    async fn startup_reconciles_unfinished_jobs_and_global_stats() {
        let path = temporary_path();
        let store = AppStore::open(&path).await.unwrap();
        store.add_user(42, &package("free")).await.unwrap();
        store.add_user(43, &package("pro")).await.unwrap();
        store.reserve_daily_job(42, None).await.unwrap();
        store.reserve_daily_job(42, None).await.unwrap();
        store
            .record_download_outcome(42, DownloadStatOutcome::Failed)
            .await
            .unwrap();
        assert_eq!(store.reconcile_interrupted_jobs().await.unwrap(), 1);

        let stats = store.user_stats(42).await.unwrap();
        assert_eq!(stats.failed_jobs, 1);
        assert_eq!(stats.interrupted_jobs, 1);
        assert_eq!(stats.unresolved_jobs(), 0);
        let global = store.global_stats().await.unwrap();
        assert_eq!(global.users, 2);
        assert_eq!(global.enabled_users, 2);
        assert_eq!(global.downloads.accepted_jobs, 2);
        drop(store);
        remove_database(&path).await;
    }
}
