use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use grammers_client::message::{InputMessage, Message as SentMessage};
use grammers_client::update::Message as UpdateMessage;
use tokio::sync::Notify;

use crate::jobs::status;

const STATUS_OPERATION_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct JobProgress {
    inner: Arc<JobProgressInner>,
}

struct JobProgressInner {
    id: u64,
    source: UpdateMessage,
    status: Mutex<Option<SentMessage>>,
    phase: Mutex<JobPhase>,
    last_render: Mutex<Option<String>>,
    flushing: AtomicBool,
    edit_lock: tokio::sync::Mutex<()>,
    status_ready: Notify,
    attachment_done: AtomicBool,
    final_media: AtomicBool,
    delete_when_attached: AtomicBool,
    failure: Mutex<Option<String>>,
    phase_since: Mutex<Instant>,
    edit_failures: AtomicU8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobPhase {
    Queued,
    Downloading {
        percent: Option<u8>,
    },
    Inspecting,
    Transcoding {
        percent: Option<u8>,
    },
    Thumbnailing,
    Uploading {
        current: usize,
        total: usize,
        bytes: Option<(u64, u64)>,
    },
    Finalizing,
    Cancelling,
    Cancelled,
    Completed,
    Failed,
}

impl JobProgress {
    pub(crate) fn new(id: u64, source: UpdateMessage) -> Self {
        Self {
            inner: Arc::new(JobProgressInner {
                id,
                source,
                status: Mutex::new(None),
                phase: Mutex::new(JobPhase::Queued),
                last_render: Mutex::new(None),
                flushing: AtomicBool::new(false),
                edit_lock: tokio::sync::Mutex::new(()),
                status_ready: Notify::new(),
                attachment_done: AtomicBool::new(false),
                final_media: AtomicBool::new(false),
                delete_when_attached: AtomicBool::new(false),
                failure: Mutex::new(None),
                phase_since: Mutex::new(Instant::now()),
                edit_failures: AtomicU8::new(0),
            }),
        }
    }

    pub fn downloading(&self) {
        self.set_phase(JobPhase::Downloading { percent: None });
    }

    pub fn download_progress(&self, percent: u8) {
        self.set_phase(JobPhase::Downloading {
            percent: Some(percent.min(100)),
        });
    }

    pub fn id(&self) -> u64 {
        self.inner.id
    }

    pub fn inspecting(&self) {
        self.set_phase(JobPhase::Inspecting);
    }

    pub fn transcoding(&self, percent: Option<u8>) {
        self.set_phase(JobPhase::Transcoding {
            percent: percent.map(|value| value.min(100)),
        });
    }

    pub fn thumbnailing(&self) {
        self.set_phase(JobPhase::Thumbnailing);
    }

    pub fn finalizing(&self) {
        self.set_phase(JobPhase::Finalizing);
    }

    pub fn upload_progress(
        &self,
        current: usize,
        total: usize,
        uploaded_bytes: u64,
        total_bytes: u64,
    ) {
        self.set_phase(JobPhase::Uploading {
            current,
            total,
            bytes: Some((uploaded_bytes.min(total_bytes), total_bytes)),
        });
    }

    pub fn fail(&self, reason: impl Into<String>) {
        let reason = truncate_failure(reason.into());
        *lock(&self.inner.failure) = Some(reason);
        self.set_phase(JobPhase::Failed);
    }

    pub fn complete_with_warning(&self, reason: impl Into<String>) {
        let reason = truncate_failure(reason.into());
        *lock(&self.inner.failure) = Some(reason);
        self.set_phase(JobPhase::Completed);
    }

    pub async fn replace_with_media(&self, message: InputMessage) -> anyhow::Result<bool> {
        self.inner.final_media.store(true, Ordering::Release);
        let Some(status) = self.wait_for_status().await else {
            return Ok(false);
        };

        let _edit_guard =
            tokio::time::timeout(STATUS_OPERATION_TIMEOUT, self.inner.edit_lock.lock())
                .await
                .map_err(|_| anyhow::anyhow!("timed out waiting for the status edit lock"))?;
        tokio::time::timeout(STATUS_OPERATION_TIMEOUT, status.edit(message))
            .await
            .map_err(|_| anyhow::anyhow!("final status edit timed out"))??;
        Ok(true)
    }

    pub async fn delete_status(&self) {
        self.inner.final_media.store(true, Ordering::Release);
        self.inner
            .delete_when_attached
            .store(true, Ordering::Release);
        let Some(status) = self.wait_for_status().await else {
            return;
        };

        let Ok(edit_guard) =
            tokio::time::timeout(STATUS_OPERATION_TIMEOUT, self.inner.edit_lock.lock()).await
        else {
            log::warn!(
                "Job #{} timed out waiting to delete its status",
                self.inner.id
            );
            self.resume_text_status();
            return;
        };
        let deletion = match tokio::time::timeout(STATUS_OPERATION_TIMEOUT, status.delete()).await {
            Ok(result) => result,
            Err(_) => {
                log::warn!("Job #{} status deletion timed out", self.inner.id);
                self.inner.final_media.store(false, Ordering::Release);
                self.inner
                    .delete_when_attached
                    .store(false, Ordering::Release);
                drop(edit_guard);
                self.set_phase(JobPhase::Completed);
                self.schedule_flush();
                return;
            }
        };
        if deletion.is_ok() {
            *lock(&self.inner.status) = None;
            return;
        }

        log::warn!("Failed to delete completed job status: {deletion:?}");
        self.inner.final_media.store(false, Ordering::Release);
        self.inner
            .delete_when_attached
            .store(false, Ordering::Release);
        drop(edit_guard);
        self.set_phase(JobPhase::Completed);
        self.schedule_flush();
    }

    pub fn resume_text_status(&self) {
        self.inner.final_media.store(false, Ordering::Release);
        self.inner
            .delete_when_attached
            .store(false, Ordering::Release);
        self.schedule_flush();
    }

    pub(crate) async fn attach(&self) -> Option<i32> {
        let initial = self.render();
        match tokio::time::timeout(
            STATUS_OPERATION_TIMEOUT,
            self.inner.source.reply(initial.clone()),
        )
        .await
        {
            Err(_) => {
                self.inner.attachment_done.store(true, Ordering::Release);
                self.inner.status_ready.notify_waiters();
                log::warn!("Job #{} status creation timed out", self.inner.id);
                None
            }
            Ok(Ok(status)) => {
                let status_message_id = status.id();
                *lock(&self.inner.status) = Some(status.clone());
                *lock(&self.inner.last_render) = Some(initial);
                self.inner.attachment_done.store(true, Ordering::Release);
                self.inner.status_ready.notify_waiters();
                if self.inner.delete_when_attached.load(Ordering::Acquire) {
                    let Ok(edit_guard) =
                        tokio::time::timeout(STATUS_OPERATION_TIMEOUT, self.inner.edit_lock.lock())
                            .await
                    else {
                        self.resume_text_status();
                        return Some(status_message_id);
                    };
                    let deletion =
                        match tokio::time::timeout(STATUS_OPERATION_TIMEOUT, status.delete()).await
                        {
                            Ok(result) => result,
                            Err(_) => {
                                self.resume_text_status();
                                return Some(status_message_id);
                            }
                        };
                    if deletion.is_ok() {
                        *lock(&self.inner.status) = None;
                    } else {
                        self.inner.final_media.store(false, Ordering::Release);
                        self.inner
                            .delete_when_attached
                            .store(false, Ordering::Release);
                        drop(edit_guard);
                        self.set_phase(JobPhase::Completed);
                        self.schedule_flush();
                    }
                } else {
                    self.schedule_flush();
                }
                Some(status_message_id)
            }
            Ok(Err(error)) => {
                self.inner.attachment_done.store(true, Ordering::Release);
                self.inner.status_ready.notify_waiters();
                log::warn!("Failed to create job status message: {error}");
                None
            }
        }
    }

    async fn wait_for_status(&self) -> Option<SentMessage> {
        let wait = async {
            loop {
                if let Some(status) = lock(&self.inner.status).clone() {
                    return Some(status);
                }
                if self.inner.attachment_done.load(Ordering::Acquire) {
                    return None;
                }

                let notified = self.inner.status_ready.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if lock(&self.inner.status).is_some()
                    || self.inner.attachment_done.load(Ordering::Acquire)
                {
                    continue;
                }
                notified.await;
            }
        };

        tokio::time::timeout(std::time::Duration::from_secs(10), wait)
            .await
            .ok()
            .flatten()
    }

    pub(crate) fn set_phase(&self, new_phase: JobPhase) {
        let old_phase = {
            let mut phase = lock(&self.inner.phase);
            if !phase.can_transition_to(&new_phase) {
                None
            } else {
                let old = phase.clone();
                *phase = new_phase;
                Some(old)
            }
        };
        if let Some(old_phase) = old_phase {
            let new_label = lock(&self.inner.phase).label();
            if old_phase.label() != new_label {
                *lock(&self.inner.phase_since) = Instant::now();
                self.inner.edit_failures.store(0, Ordering::Release);
                log::info!(
                    "Job #{} phase {} -> {}",
                    self.inner.id,
                    old_phase.label(),
                    new_label
                );
            }
            self.schedule_flush();
        }
    }

    fn schedule_flush(&self) {
        if self.inner.final_media.load(Ordering::Acquire)
            || lock(&self.inner.status).is_none()
            || self.inner.flushing.swap(true, Ordering::AcqRel)
        {
            return;
        }

        let progress = self.clone();
        tokio::spawn(async move { progress.flush().await });
    }

    async fn flush(self) {
        loop {
            let Ok(_edit_guard) =
                tokio::time::timeout(STATUS_OPERATION_TIMEOUT, self.inner.edit_lock.lock()).await
            else {
                log::warn!("Job #{} status edit lock timed out", self.inner.id);
                let retry = self.inner.edit_failures.fetch_add(1, Ordering::AcqRel) < 2;
                self.inner.flushing.store(false, Ordering::Release);
                if retry {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    self.schedule_flush();
                }
                break;
            };
            if self.inner.final_media.load(Ordering::Acquire) {
                self.inner.flushing.store(false, Ordering::Release);
                break;
            }
            let desired = self.render();
            let status = lock(&self.inner.status).clone();
            let last_render = lock(&self.inner.last_render).clone();
            let mut retry = false;
            if last_render.as_deref() != Some(&desired)
                && let Some(status) = status
            {
                match tokio::time::timeout(STATUS_OPERATION_TIMEOUT, status.edit(desired.clone()))
                    .await
                {
                    Ok(Ok(())) => {
                        *lock(&self.inner.last_render) = Some(desired.clone());
                        self.inner.edit_failures.store(0, Ordering::Release);
                    }
                    Ok(Err(error)) => {
                        log::warn!("Job #{} status edit failed: {error}", self.inner.id);
                        retry = self.inner.edit_failures.fetch_add(1, Ordering::AcqRel) < 2;
                    }
                    Err(_) => {
                        log::warn!("Job #{} status edit timed out", self.inner.id);
                        retry = self.inner.edit_failures.fetch_add(1, Ordering::AcqRel) < 2;
                    }
                }
            }

            self.inner.flushing.store(false, Ordering::Release);
            if retry {
                drop(_edit_guard);
                tokio::time::sleep(Duration::from_secs(2)).await;
                self.schedule_flush();
                break;
            }
            if self.render() == desired || self.inner.flushing.swap(true, Ordering::AcqRel) {
                break;
            }
        }
    }

    fn render(&self) -> String {
        let phase = lock(&self.inner.phase).clone();
        let failure = lock(&self.inner.failure).clone();
        status::render(self.inner.id, &phase, failure.as_deref())
    }

    pub(crate) fn summary(&self) -> String {
        let phase = lock(&self.inner.phase).clone();
        let elapsed = lock(&self.inner.phase_since).elapsed();
        format!(
            "#{} {} for {}",
            self.inner.id,
            phase.label(),
            status::format_duration(elapsed)
        )
    }
}

impl JobPhase {
    fn is_terminal(&self) -> bool {
        matches!(self, Self::Cancelled | Self::Completed | Self::Failed)
    }

    fn can_transition_to(&self, next: &Self) -> bool {
        if self.is_terminal() {
            return false;
        }
        if self == next {
            return false;
        }
        !matches!(
            (self, next),
            (
                Self::Cancelling,
                Self::Queued
                    | Self::Downloading { .. }
                    | Self::Inspecting
                    | Self::Transcoding { .. }
                    | Self::Thumbnailing
                    | Self::Uploading { .. }
                    | Self::Finalizing
            )
        )
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Downloading { .. } => "downloading",
            Self::Inspecting => "inspecting media",
            Self::Transcoding { .. } => "transcoding",
            Self::Thumbnailing => "preparing thumbnail",
            Self::Uploading { .. } => "uploading",
            Self::Finalizing => "finalizing",
            Self::Cancelling => "cancelling",
            Self::Cancelled => "cancelled",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

fn truncate_failure(reason: String) -> String {
    if reason.chars().count() > 500 {
        format!("{}...", reason.chars().take(497).collect::<String>())
    } else {
        reason
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::{JobPhase, truncate_failure};

    #[test]
    fn only_final_phases_are_terminal() {
        assert!(!JobPhase::Queued.is_terminal());
        assert!(!(JobPhase::Downloading { percent: None }).is_terminal());
        assert!(!JobPhase::Cancelling.is_terminal());
        assert!(JobPhase::Cancelled.is_terminal());
        assert!(JobPhase::Completed.is_terminal());
        assert!(JobPhase::Failed.is_terminal());
    }

    #[test]
    fn cancelling_cannot_regress_to_active_progress() {
        assert!(
            !JobPhase::Cancelling.can_transition_to(&JobPhase::Downloading { percent: Some(50) })
        );
        assert!(!JobPhase::Cancelling.can_transition_to(&JobPhase::Inspecting));
        assert!(
            !JobPhase::Cancelling.can_transition_to(&JobPhase::Transcoding { percent: Some(50) })
        );
        assert!(
            !JobPhase::Cancelling.can_transition_to(&JobPhase::Uploading {
                current: 1,
                total: 2,
                bytes: None,
            })
        );
        assert!(JobPhase::Cancelling.can_transition_to(&JobPhase::Cancelled));
        assert!(JobPhase::Cancelling.can_transition_to(&JobPhase::Completed));
    }

    #[test]
    fn failure_text_truncation_is_unicode_safe() {
        let reason = "🙂".repeat(600);
        let truncated = truncate_failure(reason);
        assert_eq!(truncated.chars().count(), 500);
        assert!(truncated.ends_with("..."));
    }
}
