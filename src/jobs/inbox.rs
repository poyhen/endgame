use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use grammers_client::Client;
use grammers_session::Session;
use grammers_session::storages::SqliteSession;
use grammers_session::types::PeerId;
use libsql::{Connection, params};
use tokio::sync::{Mutex, Notify};

use crate::cancel::CancellationToken;
use crate::jobs::queue::DownloadQueueHandle;
use crate::media::request::{DownloadOutcome, DownloadRequest};
use crate::telegram::allowed_users::AllowedUsers;

const PENDING: i64 = 0;
const ACTIVE: i64 = 1;
const COMPLETED: i64 = 2;
const FAILED: i64 = 3;
const CANCELLED: i64 = 4;
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

pub struct Inbox {
    connection: Mutex<Connection>,
    changed: Notify,
}

pub struct PendingDownload {
    pub id: u64,
    pub owner_id: i64,
    pub message_id: i32,
    pub request: DownloadRequest,
    pub status_message_id: Option<i32>,
}

#[derive(Clone)]
pub struct Receipt {
    pub id: u64,
    inbox: Arc<Inbox>,
}

impl Inbox {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        if path != Path::new(":memory:") {
            let mut options = std::fs::OpenOptions::new();
            options.create(true).append(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options.open(path)?;
        }
        let database = libsql::Builder::new_local(path).build().await?;
        let connection = database.connect()?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             CREATE TABLE IF NOT EXISTS downloads (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 owner_id INTEGER NOT NULL,
                 message_id INTEGER NOT NULL,
                 request_index INTEGER NOT NULL,
                 request_json TEXT NOT NULL,
                 status_message_id INTEGER,
                 state INTEGER NOT NULL DEFAULT 0 CHECK (state BETWEEN 0 AND 4),
                 UNIQUE (owner_id, message_id, request_index)
             );
             CREATE INDEX IF NOT EXISTS downloads_pending ON downloads(state, id);
             CREATE TABLE IF NOT EXISTS handled_messages (
                 owner_id INTEGER NOT NULL,
                 message_id INTEGER NOT NULL,
                 PRIMARY KEY (owner_id, message_id)
             );",
            )
            .await?;
        // A process can die with jobs in memory. Their durable records remain
        // recoverable, including the Telegram status that may already hold media.
        connection
            .execute(
                "UPDATE downloads SET state = ? WHERE state = ?",
                params![PENDING, ACTIVE],
            )
            .await?;
        Ok(Self {
            connection: Mutex::new(connection),
            changed: Notify::new(),
        })
    }

    pub async fn admit(
        &self,
        owner_id: i64,
        message_id: i32,
        requests: &[DownloadRequest],
    ) -> anyhow::Result<usize> {
        let connection = self.connection.lock().await;
        let transaction = connection.transaction().await?;
        let mut added = 0;
        for (index, request) in requests.iter().enumerate() {
            added += transaction.execute(
                "INSERT OR IGNORE INTO downloads (owner_id, message_id, request_index, request_json) VALUES (?, ?, ?, ?)",
                params![owner_id, message_id, i64::try_from(index)?, serde_json::to_string(request)?],
            ).await? as usize;
        }
        transaction.commit().await?;
        drop(connection);
        if added > 0 {
            self.changed.notify_one();
        }
        Ok(added)
    }

    pub async fn first_message(&self, owner_id: i64, message_id: i32) -> anyhow::Result<bool> {
        Ok(self
            .connection
            .lock()
            .await
            .execute(
                "INSERT OR IGNORE INTO handled_messages VALUES (?, ?)",
                params![owner_id, message_id],
            )
            .await?
            > 0)
    }

    pub async fn pending(&self, limit: usize) -> anyhow::Result<Vec<PendingDownload>> {
        let connection = self.connection.lock().await;
        let mut rows = connection.query(
            "SELECT id, owner_id, message_id, request_json, status_message_id FROM downloads WHERE state = ? ORDER BY id LIMIT ?",
            params![PENDING, i64::try_from(limit)?],
        ).await?;
        let mut downloads = Vec::new();
        while let Some(row) = rows.next().await? {
            downloads.push(PendingDownload {
                id: u64::try_from(row.get::<i64>(0)?)?,
                owner_id: row.get(1)?,
                message_id: row.get(2)?,
                request: serde_json::from_str(&row.get::<String>(3)?)?,
                status_message_id: row.get(4)?,
            });
        }
        Ok(downloads)
    }

    pub async fn unfinished(&self, owner_id: i64) -> anyhow::Result<usize> {
        let connection = self.connection.lock().await;
        let mut rows = connection
            .query(
                "SELECT COUNT(*) FROM downloads WHERE owner_id = ? AND state IN (0, 1)",
                params![owner_id],
            )
            .await?;
        Ok(usize::try_from(rows.next().await?.unwrap().get::<i64>(0)?)?)
    }

    async fn claim(&self, id: u64) -> anyhow::Result<bool> {
        Ok(self
            .connection
            .lock()
            .await
            .execute(
                "UPDATE downloads SET state = ? WHERE id = ? AND state = ?",
                params![ACTIVE, i64::try_from(id)?, PENDING],
            )
            .await?
            > 0)
    }

    pub fn receipt(self: &Arc<Self>, id: u64) -> Receipt {
        Receipt {
            id,
            inbox: Arc::clone(self),
        }
    }

    pub async fn cancel_pending(&self, owner_id: i64, id: u64) -> anyhow::Result<bool> {
        Ok(self
            .connection
            .lock()
            .await
            .execute(
                "UPDATE downloads SET state = ? WHERE owner_id = ? AND id = ? AND state = ?",
                params![CANCELLED, owner_id, i64::try_from(id)?, PENDING],
            )
            .await?
            > 0)
    }

    pub async fn request_for_status(
        &self,
        owner_id: i64,
        status_id: i32,
    ) -> anyhow::Result<Option<DownloadRequest>> {
        let connection = self.connection.lock().await;
        let mut rows = connection.query(
            "SELECT request_json FROM downloads WHERE owner_id = ? AND status_message_id = ? LIMIT 1",
            params![owner_id, status_id],
        ).await?;
        rows.next()
            .await?
            .map(|row| Ok(serde_json::from_str(&row.get::<String>(0)?)?))
            .transpose()
    }

    pub async fn job_for_status(
        &self,
        owner_id: i64,
        status_id: i32,
    ) -> anyhow::Result<Option<u64>> {
        let connection = self.connection.lock().await;
        let mut rows = connection
            .query(
                "SELECT id FROM downloads WHERE owner_id = ? AND status_message_id = ? LIMIT 1",
                params![owner_id, status_id],
            )
            .await?;
        rows.next()
            .await?
            .map(|row| Ok(u64::try_from(row.get::<i64>(0)?)?))
            .transpose()
    }
}

impl Receipt {
    pub async fn status(&self, status_id: i32) -> anyhow::Result<()> {
        self.inbox
            .connection
            .lock()
            .await
            .execute(
                "UPDATE downloads SET status_message_id = ? WHERE id = ?",
                params![status_id, i64::try_from(self.id)?],
            )
            .await?;
        Ok(())
    }

    pub async fn finish(&self, outcome: DownloadOutcome) -> anyhow::Result<()> {
        let state = match outcome {
            DownloadOutcome::Completed => COMPLETED,
            DownloadOutcome::Failed => FAILED,
            DownloadOutcome::Cancelled => CANCELLED,
        };
        self.inbox
            .connection
            .lock()
            .await
            .execute(
                "UPDATE downloads SET state = ? WHERE id = ?",
                params![state, i64::try_from(self.id)?],
            )
            .await?;
        self.inbox.changed.notify_one();
        Ok(())
    }

    async fn release(&self) -> anyhow::Result<()> {
        self.inbox
            .connection
            .lock()
            .await
            .execute(
                "UPDATE downloads SET state = ? WHERE id = ? AND state = ?",
                params![PENDING, i64::try_from(self.id)?, ACTIVE],
            )
            .await?;
        Ok(())
    }
}

pub struct Recovery {
    pub inbox: Arc<Inbox>,
    pub client: Client,
    pub session: Arc<SqliteSession>,
    pub queue: DownloadQueueHandle,
    pub allowed_users: Arc<AllowedUsers>,
    pub super_users: Arc<Vec<i64>>,
    pub cancellation: CancellationToken,
}

impl Recovery {
    pub async fn run(self) {
        loop {
            if self.cancellation.is_cancelled() {
                break;
            }
            let work = async {
                let pending = self.inbox.pending(20).await?;
                for download in pending {
                    if self.cancellation.is_cancelled() {
                        break;
                    }
                    if !self.queue.has_capacity() {
                        break;
                    }
                    if !self.allowed_users.contains(download.owner_id)
                        && !self.super_users.contains(&download.owner_id)
                    {
                        self.inbox
                            .receipt(download.id)
                            .finish(DownloadOutcome::Cancelled)
                            .await?;
                        continue;
                    }
                    if let Err(error) = self.enqueue(&download).await {
                        log::warn!("Could not recover download #{}: {error:#}", download.id);
                    }
                }
                Ok::<_, anyhow::Error>(())
            };
            if let Err(error) = work.await {
                log::error!("Download recovery failed: {error:#}");
            }
            tokio::select! {
                biased;
                _ = self.cancellation.cancelled() => break,
                _ = self.inbox.changed.notified() => {},
                _ = tokio::time::sleep(Duration::from_secs(1)) => {},
            }
        }
    }

    async fn enqueue(&self, download: &PendingDownload) -> anyhow::Result<()> {
        let peer_id =
            PeerId::user(download.owner_id).ok_or_else(|| anyhow::anyhow!("invalid owner ID"))?;
        let peer = self
            .session
            .peer_ref(peer_id)
            .await?
            .unwrap_or_else(|| peer_id.to_ambient_ref());
        let receipt = self.inbox.receipt(download.id);
        let status = if let Some(status_id) = download.status_message_id {
            let messages = tokio::time::timeout(
                FETCH_TIMEOUT,
                self.client.get_messages_by_id(peer, &[status_id]),
            )
            .await??;
            messages.into_iter().next().flatten()
        } else {
            None
        };
        if let Some(status) = status.as_ref().filter(|message| message.media().is_some()) {
            // The final media edit may have reached Telegram just before a crash.
            tokio::time::timeout(
                FETCH_TIMEOUT,
                crate::telegram::caption::clear(&self.client, status),
            )
            .await??;
            receipt.finish(DownloadOutcome::Completed).await?;
            log::info!("Recovered already delivered download #{}", download.id);
            return Ok(());
        }
        let messages = tokio::time::timeout(
            FETCH_TIMEOUT,
            self.client.get_messages_by_id(peer, &[download.message_id]),
        )
        .await??;
        let Some(source) = messages.into_iter().next().flatten() else {
            receipt.finish(DownloadOutcome::Failed).await?;
            log::warn!("Download #{} source message was deleted", download.id);
            return Ok(());
        };
        if !self.inbox.claim(download.id).await? {
            return Ok(());
        }
        let reply = self.queue.try_enqueue(
            source,
            download.owner_id,
            download.request.clone(),
            receipt.clone(),
            status,
        );
        if reply.is_accepted() {
            reply.send().await;
        } else {
            receipt.release().await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::media::request::ClipRange;

    use super::*;

    fn request() -> DownloadRequest {
        DownloadRequest::video("https://example.com/video".into(), None)
    }

    async fn memory() -> Arc<Inbox> {
        Arc::new(Inbox::open(Path::new(":memory:")).await.unwrap())
    }

    #[tokio::test]
    async fn replay_is_deduplicated_without_merging_separate_messages_or_url_positions() {
        let inbox = memory().await;
        let requests = vec![request(), request()];
        assert_eq!(inbox.admit(11, 100, &requests).await.unwrap(), 2);
        assert_eq!(inbox.admit(11, 100, &requests).await.unwrap(), 0);
        assert_eq!(inbox.admit(11, 101, &[request()]).await.unwrap(), 1);
        assert_eq!(inbox.admit(22, 100, &[request()]).await.unwrap(), 1);
        assert_eq!(inbox.pending(20).await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn backlog_larger_than_memory_capacity_is_saved_and_fetched_in_batches() {
        let inbox = memory().await;
        assert_eq!(
            inbox.admit(11, 100, &vec![request(); 125]).await.unwrap(),
            125
        );
        let batch = inbox.pending(20).await.unwrap();
        assert_eq!(batch.len(), 20);
        for download in batch {
            assert!(inbox.claim(download.id).await.unwrap());
            assert!(!inbox.claim(download.id).await.unwrap());
            inbox
                .receipt(download.id)
                .finish(DownloadOutcome::Completed)
                .await
                .unwrap();
        }
        assert_eq!(inbox.unfinished(11).await.unwrap(), 105);
        assert_eq!(inbox.pending(200).await.unwrap().len(), 105);
        assert_eq!(
            inbox.admit(11, 100, &vec![request(); 125]).await.unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn restart_recovers_interrupted_jobs_and_keeps_terminal_jobs_terminal() {
        let folder =
            std::env::temp_dir().join(format!("endgame-inbox-{:016x}", rand::random::<u64>()));
        std::fs::create_dir(&folder).unwrap();
        let path = folder.join("downloads.sqlite");
        let id;
        {
            let inbox = Arc::new(Inbox::open(&path).await.unwrap());
            inbox
                .admit(11, 100, &[request(), request(), request()])
                .await
                .unwrap();
            let rows = inbox.pending(20).await.unwrap();
            id = rows[0].id;
            inbox.claim(id).await.unwrap();
            inbox.receipt(id).status(700).await.unwrap();
            inbox
                .receipt(rows[1].id)
                .finish(DownloadOutcome::Completed)
                .await
                .unwrap();
            assert!(inbox.cancel_pending(11, rows[2].id).await.unwrap());
        }
        {
            let inbox = Arc::new(Inbox::open(&path).await.unwrap());
            let recovered = inbox.pending(20).await.unwrap();
            assert_eq!(recovered.len(), 1);
            assert_eq!(recovered[0].id, id);
            assert_eq!(recovered[0].status_message_id, Some(700));
            assert_eq!(recovered[0].request, request());
            assert_eq!(inbox.unfinished(11).await.unwrap(), 1);
            assert_eq!(
                inbox
                    .admit(11, 100, &[request(), request(), request()])
                    .await
                    .unwrap(),
                0
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[tokio::test]
    async fn persisted_retries_preserve_clip_options_and_owner_scope() {
        let inbox = memory().await;
        let clip = DownloadRequest::clip(
            "https://example.com/video".into(),
            ClipRange::new(Duration::from_millis(1500), Duration::from_secs(6)).unwrap(),
        );
        inbox
            .admit(11, 100, std::slice::from_ref(&clip))
            .await
            .unwrap();
        let row = inbox.pending(1).await.unwrap().remove(0);
        inbox.receipt(row.id).status(700).await.unwrap();
        assert_eq!(inbox.request_for_status(11, 700).await.unwrap(), Some(clip));
        assert!(inbox.request_for_status(22, 700).await.unwrap().is_none());
        assert_eq!(inbox.job_for_status(11, 700).await.unwrap(), Some(row.id));
        assert!(inbox.job_for_status(22, 700).await.unwrap().is_none());
        assert!(!inbox.cancel_pending(22, row.id).await.unwrap());
        assert!(inbox.cancel_pending(11, row.id).await.unwrap());
    }

    #[tokio::test]
    async fn failed_and_cancelled_jobs_are_not_automatically_retried() {
        let inbox = memory().await;
        inbox.admit(11, 100, &[request(), request()]).await.unwrap();
        let rows = inbox.pending(20).await.unwrap();
        inbox
            .receipt(rows[0].id)
            .finish(DownloadOutcome::Failed)
            .await
            .unwrap();
        inbox
            .receipt(rows[1].id)
            .finish(DownloadOutcome::Cancelled)
            .await
            .unwrap();
        assert!(inbox.pending(20).await.unwrap().is_empty());
        assert_eq!(
            inbox.admit(11, 100, &[request(), request()]).await.unwrap(),
            0
        );
        assert_eq!(inbox.admit(11, 101, &[request()]).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn other_commands_are_not_replied_to_again_during_replay() {
        let inbox = memory().await;
        assert!(inbox.first_message(11, 100).await.unwrap());
        assert!(!inbox.first_message(11, 100).await.unwrap());
        assert!(inbox.first_message(22, 100).await.unwrap());
    }
}
