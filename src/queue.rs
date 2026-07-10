use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use grammers_client::Client;
use grammers_client::message::{InputMessage, Message as SentMessage};
use grammers_client::update::Message as UpdateMessage;
use tokio::sync::Notify;
use tokio::task::{JoinHandle, JoinSet};

use crate::cancel::CancellationToken;
use crate::download::{self, DownloadOutcome, DownloadRequest};

struct DownloadJob {
    id: u64,
    owner_id: i64,
    message: UpdateMessage,
    request: DownloadRequest,
    cancellation: CancellationToken,
    progress: JobProgress,
}

#[derive(Clone)]
struct ActiveJob {
    owner_id: i64,
    cancellation: CancellationToken,
    progress: JobProgress,
}

struct QueueState {
    accepting: bool,
    pending: VecDeque<DownloadJob>,
    active: HashMap<u64, ActiveJob>,
}

struct SharedQueue {
    state: Mutex<QueueState>,
    changed: Notify,
    concurrency: usize,
    capacity: usize,
    max_upload_bytes: u64,
    next_id: AtomicU64,
}

#[derive(Clone)]
pub struct DownloadQueueHandle {
    shared: Arc<SharedQueue>,
}

pub struct DownloadQueue {
    handle: DownloadQueueHandle,
    runner: JoinHandle<()>,
}

pub enum QueueReply {
    Accepted(JobProgress),
    Silent,
    Message {
        message: Box<UpdateMessage>,
        text: String,
    },
}

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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobPhase {
    Queued,
    Downloading,
    Processing,
    Uploading { current: usize, total: usize },
    Cancelling,
    Cancelled,
    Completed,
    Failed,
}

struct ActiveJobGuard {
    id: u64,
    shared: Arc<SharedQueue>,
}

impl Drop for ActiveJobGuard {
    fn drop(&mut self) {
        lock(&self.shared.state).active.remove(&self.id);
        self.shared.changed.notify_one();
    }
}

impl DownloadQueue {
    pub fn new(
        client: Client,
        concurrency: usize,
        capacity: usize,
        max_upload_size_mb: usize,
    ) -> Self {
        assert!(concurrency > 0, "download concurrency must be positive");
        assert!(capacity > 0, "download queue capacity must be positive");

        let shared = Arc::new(SharedQueue {
            state: Mutex::new(QueueState {
                accepting: true,
                pending: VecDeque::with_capacity(capacity),
                active: HashMap::with_capacity(concurrency),
            }),
            changed: Notify::new(),
            concurrency,
            capacity,
            max_upload_bytes: (max_upload_size_mb as u64).saturating_mul(1024 * 1024),
            next_id: AtomicU64::new(1),
        });
        let runner_shared = Arc::clone(&shared);
        let runner = tokio::spawn(run_queue(runner_shared, client));

        Self {
            handle: DownloadQueueHandle { shared },
            runner,
        }
    }

    pub fn handle(&self) -> DownloadQueueHandle {
        self.handle.clone()
    }

    pub async fn shutdown(self) {
        let Self { handle, runner } = self;
        {
            let mut state = lock(&handle.shared.state);
            state.accepting = false;
        }
        handle.shared.changed.notify_one();
        drop(handle);

        if let Err(error) = runner.await {
            log::warn!("Download queue ended unexpectedly: {error}");
        }
    }
}

impl DownloadQueueHandle {
    pub fn status_report(&self) -> String {
        let state = lock(&self.shared.state);
        format!(
            "Downloads: {}/{} active, {}/{} queued.",
            state.active.len(),
            self.shared.concurrency,
            state.pending.len(),
            self.shared.capacity
        )
    }

    pub fn try_enqueue(
        &self,
        message: UpdateMessage,
        owner_id: i64,
        request: DownloadRequest,
    ) -> QueueReply {
        let mut state = lock(&self.shared.state);
        if !state.accepting {
            return QueueReply::message(
                message,
                "The downloader is shutting down. Please try again after it restarts.",
            );
        }
        if state.pending.len() >= self.shared.capacity {
            return QueueReply::message(
                message,
                format!(
                    "The download queue is full ({} waiting, {}/{} active). Please try again later.",
                    state.pending.len(),
                    state.active.len(),
                    self.shared.concurrency
                ),
            );
        }

        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let cancellation = CancellationToken::new();
        let progress = JobProgress::new(id, message.clone());
        state.pending.push_back(DownloadJob {
            id,
            owner_id,
            message,
            request,
            cancellation,
            progress: progress.clone(),
        });
        drop(state);
        self.shared.changed.notify_one();

        QueueReply::Accepted(progress)
    }

    pub fn try_cancel(&self, message: UpdateMessage, owner_id: i64, id: u64) -> QueueReply {
        let mut state = lock(&self.shared.state);

        if let Some(position) = state
            .pending
            .iter()
            .position(|job| job.id == id && job.owner_id == owner_id)
        {
            let job = state.pending.remove(position).expect("position must exist");
            job.cancellation.cancel();
            job.progress.set_phase(JobPhase::Cancelled);
            drop(state);
            self.shared.changed.notify_one();
            return QueueReply::Silent;
        }

        if let Some(job) = state
            .active
            .get(&id)
            .filter(|job| job.owner_id == owner_id)
            .cloned()
        {
            job.cancellation.cancel();
            job.progress.set_phase(JobPhase::Cancelling);
            drop(state);
            return QueueReply::Silent;
        }

        QueueReply::message(
            message,
            format!("No cancellable job #{id} was found for your account."),
        )
    }
}

impl QueueReply {
    pub fn is_accepted(&self) -> bool {
        matches!(self, Self::Accepted(_))
    }

    fn message(message: UpdateMessage, text: impl Into<String>) -> Self {
        Self::Message {
            message: Box::new(message),
            text: text.into(),
        }
    }

    pub async fn send(self) {
        match self {
            Self::Accepted(progress) => progress.attach().await,
            Self::Silent => {}
            Self::Message { message, text } => {
                let _ = message.reply(text).await;
            }
        }
    }
}

impl JobProgress {
    fn new(id: u64, source: UpdateMessage) -> Self {
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
            }),
        }
    }

    pub fn downloading(&self) {
        self.set_phase(JobPhase::Downloading);
    }

    pub fn uploading(&self, current: usize, total: usize) {
        self.set_phase(JobPhase::Uploading { current, total });
    }

    pub fn processing(&self) {
        self.set_phase(JobPhase::Processing);
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

        let _edit_guard = self.inner.edit_lock.lock().await;
        status.edit(message).await?;
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

        let edit_guard = self.inner.edit_lock.lock().await;
        let deletion = status.delete().await;
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

    async fn attach(&self) {
        let initial = self.render();
        match self.inner.source.reply(initial.clone()).await {
            Ok(status) => {
                *lock(&self.inner.status) = Some(status.clone());
                *lock(&self.inner.last_render) = Some(initial);
                self.inner.attachment_done.store(true, Ordering::Release);
                self.inner.status_ready.notify_waiters();
                if self.inner.delete_when_attached.load(Ordering::Acquire) {
                    let edit_guard = self.inner.edit_lock.lock().await;
                    let deletion = status.delete().await;
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
            }
            Err(error) => {
                self.inner.attachment_done.store(true, Ordering::Release);
                self.inner.status_ready.notify_waiters();
                log::warn!("Failed to create job status message: {error}");
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

    fn set_phase(&self, new_phase: JobPhase) {
        let changed = {
            let mut phase = lock(&self.inner.phase);
            if !phase.can_transition_to(&new_phase) {
                false
            } else {
                *phase = new_phase;
                true
            }
        };
        if changed {
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
            let _edit_guard = self.inner.edit_lock.lock().await;
            if self.inner.final_media.load(Ordering::Acquire) {
                self.inner.flushing.store(false, Ordering::Release);
                break;
            }
            let desired = self.render();
            let status = lock(&self.inner.status).clone();
            let last_render = lock(&self.inner.last_render).clone();
            if last_render.as_deref() != Some(&desired)
                && let Some(status) = status
            {
                let edit = status.edit(desired.clone()).await;
                if edit.is_ok() {
                    *lock(&self.inner.last_render) = Some(desired.clone());
                }
            }

            self.inner.flushing.store(false, Ordering::Release);
            if self.render() == desired || self.inner.flushing.swap(true, Ordering::AcqRel) {
                break;
            }
        }
    }

    fn render(&self) -> String {
        let phase = lock(&self.inner.phase).clone();
        match phase {
            JobPhase::Queued => format!("Job #{} queued.", self.inner.id),
            JobPhase::Downloading => format!("Job #{} is downloading.", self.inner.id),
            JobPhase::Processing => {
                format!("Job #{} is preparing compatible media.", self.inner.id)
            }
            JobPhase::Uploading { current, total } => format!(
                "Job #{} is uploading item {current}/{total}.",
                self.inner.id
            ),
            JobPhase::Cancelling => format!("Job #{} is cancelling…", self.inner.id),
            JobPhase::Cancelled => format!("Job #{} was cancelled.", self.inner.id),
            JobPhase::Completed => match lock(&self.inner.failure).as_deref() {
                Some(reason) => format!("Job #{} completed with warnings: {reason}", self.inner.id),
                None => format!("Job #{} completed.", self.inner.id),
            },
            JobPhase::Failed => match lock(&self.inner.failure).as_deref() {
                Some(reason) => format!("Job #{} failed: {reason}", self.inner.id),
                None => format!("Job #{} failed.", self.inner.id),
            },
        }
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
                Self::Queued | Self::Downloading | Self::Uploading { .. } | Self::Processing
            )
        )
    }
}

async fn run_queue(shared: Arc<SharedQueue>, client: Client) {
    let mut tasks = JoinSet::new();

    loop {
        while tasks.len() < shared.concurrency {
            let job = {
                let mut state = lock(&shared.state);
                let job = state.pending.pop_front();
                if let Some(job) = &job {
                    state.active.insert(
                        job.id,
                        ActiveJob {
                            owner_id: job.owner_id,
                            cancellation: job.cancellation.clone(),
                            progress: job.progress.clone(),
                        },
                    );
                }
                job
            };

            let Some(job) = job else {
                break;
            };
            let client = client.clone();
            let job_shared = Arc::clone(&shared);
            let max_upload_bytes = shared.max_upload_bytes;
            tasks.spawn(async move {
                let _active_guard = ActiveJobGuard {
                    id: job.id,
                    shared: job_shared,
                };
                job.progress.downloading();
                let outcome = if job.cancellation.is_cancelled() {
                    DownloadOutcome::Cancelled
                } else {
                    download::download_and_upload(
                        client,
                        job.message,
                        job.request,
                        &job.cancellation,
                        &job.progress,
                        max_upload_bytes,
                    )
                    .await
                };
                (job.progress, outcome)
            });
        }

        let should_exit = {
            let state = lock(&shared.state);
            !state.accepting && state.pending.is_empty() && state.active.is_empty()
        };
        if should_exit && tasks.is_empty() {
            break;
        }

        tokio::select! {
            result = tasks.join_next(), if !tasks.is_empty() => {
                match result {
                    Some(Ok((progress, outcome))) => {
                        let phase = match outcome {
                            DownloadOutcome::Completed => JobPhase::Completed,
                            DownloadOutcome::Failed => JobPhase::Failed,
                            DownloadOutcome::Cancelled => JobPhase::Cancelled,
                        };
                        progress.set_phase(phase);
                    }
                    Some(Err(error)) => log::warn!("Download task ended unexpectedly: {error}"),
                    None => {}
                }
            }
            _ = shared.changed.notified() => {}
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn truncate_failure(reason: String) -> String {
    if reason.chars().count() > 500 {
        format!("{}...", reason.chars().take(497).collect::<String>())
    } else {
        reason
    }
}

#[cfg(test)]
mod tests {
    use super::{JobPhase, truncate_failure};

    #[test]
    fn only_final_phases_are_terminal() {
        assert!(!JobPhase::Queued.is_terminal());
        assert!(!JobPhase::Downloading.is_terminal());
        assert!(!JobPhase::Cancelling.is_terminal());
        assert!(JobPhase::Cancelled.is_terminal());
        assert!(JobPhase::Completed.is_terminal());
        assert!(JobPhase::Failed.is_terminal());
    }

    #[test]
    fn cancelling_cannot_regress_to_active_progress() {
        assert!(!JobPhase::Cancelling.can_transition_to(&JobPhase::Downloading));
        assert!(!JobPhase::Cancelling.can_transition_to(&JobPhase::Processing));
        assert!(
            !JobPhase::Cancelling.can_transition_to(&JobPhase::Uploading {
                current: 1,
                total: 2,
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
