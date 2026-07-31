use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use grammers_client::Client;
use grammers_client::update::Message as UpdateMessage;
use tokio::sync::{Notify, mpsc};
use tokio::task::{JoinHandle, JoinSet};

use crate::cancel::CancellationToken;
use crate::jobs::progress::{JobPhase, JobProgress};
use crate::media::pipeline;
use crate::media::request::{DownloadLimits, DownloadOutcome, DownloadRequest};
use crate::policy::EffectiveUserLimits;
use crate::store::{AppStore, DownloadStatOutcome};

const STATUS_HISTORY_LIMIT: usize = 4_096;

struct DownloadJob {
    id: u64,
    owner_id: i64,
    message: UpdateMessage,
    request: DownloadRequest,
    user_limits: EffectiveUserLimits,
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
    status_jobs: HashMap<(i64, i32), StatusJob>,
    status_order: VecDeque<(i64, i32)>,
}

#[derive(Clone)]
struct StatusJob {
    id: u64,
    request: DownloadRequest,
}

struct SharedQueue {
    state: Mutex<QueueState>,
    changed: Notify,
    concurrency: usize,
    capacity: usize,
    limits: DownloadLimits,
    next_id: AtomicU64,
    stats: mpsc::UnboundedSender<DownloadStatEvent>,
}

struct DownloadStatEvent {
    owner_id: i64,
    outcome: DownloadStatOutcome,
}

#[derive(Clone)]
pub struct DownloadQueueHandle {
    shared: Arc<SharedQueue>,
}

pub struct DownloadQueue {
    handle: DownloadQueueHandle,
    runner: JoinHandle<()>,
    stats_runner: JoinHandle<()>,
}

pub enum QueueReply {
    Accepted(AcceptedReply),
    Silent,
    Message {
        message: Box<UpdateMessage>,
        text: String,
    },
}

pub(crate) struct AcceptedReply {
    progress: JobProgress,
    owner_id: i64,
    request: DownloadRequest,
    shared: Arc<SharedQueue>,
}

#[derive(Clone)]
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
        limits: DownloadLimits,
        store: Arc<AppStore>,
    ) -> Self {
        assert!(concurrency > 0, "download concurrency must be positive");
        assert!(capacity > 0, "download queue capacity must be positive");

        let (stats, mut stats_rx) = mpsc::unbounded_channel::<DownloadStatEvent>();
        let stats_runner = tokio::spawn(async move {
            while let Some(event) = stats_rx.recv().await {
                if let Err(error) = store
                    .record_download_outcome(event.owner_id, event.outcome)
                    .await
                {
                    log::error!(
                        "Could not record download outcome for user {}: {error}",
                        event.owner_id
                    );
                }
            }
        });

        let shared = Arc::new(SharedQueue {
            state: Mutex::new(QueueState {
                accepting: true,
                pending: VecDeque::with_capacity(capacity),
                active: HashMap::with_capacity(concurrency),
                status_jobs: HashMap::new(),
                status_order: VecDeque::new(),
            }),
            changed: Notify::new(),
            concurrency,
            capacity,
            limits,
            next_id: AtomicU64::new(1),
            stats,
        });
        let runner_shared = Arc::clone(&shared);
        let runner = tokio::spawn(run_queue(runner_shared, client));

        Self {
            handle: DownloadQueueHandle { shared },
            runner,
            stats_runner,
        }
    }

    pub fn handle(&self) -> DownloadQueueHandle {
        self.handle.clone()
    }

    pub async fn shutdown(self) {
        let Self {
            handle,
            runner,
            stats_runner,
        } = self;
        {
            let mut state = lock(&handle.shared.state);
            state.accepting = false;
        }
        handle.shared.changed.notify_one();
        drop(handle);

        if let Err(error) = runner.await {
            log::warn!("Download queue ended unexpectedly: {error}");
        }
        if let Err(error) = stats_runner.await {
            log::warn!("Download statistics recorder ended unexpectedly: {error}");
        }
    }
}

impl DownloadQueueHandle {
    pub fn status_report(&self, owner_id: i64) -> String {
        let state = lock(&self.shared.state);
        let active: Vec<_> = state
            .active
            .iter()
            .filter(|(_, job)| job.owner_id == owner_id)
            .map(|(_, job)| job.progress.summary())
            .collect();
        let queued: Vec<_> = state
            .pending
            .iter()
            .filter(|job| job.owner_id == owner_id)
            .map(|job| format!("#{} queued", job.id))
            .collect();
        let mut lines = vec![format!(
            "Your downloads: {} active, {} queued. Capacity: {}/{} active, {}/{} waiting.",
            active.len(),
            queued.len(),
            state.active.len(),
            self.shared.concurrency,
            state.pending.len(),
            self.shared.capacity
        )];
        lines.extend(active);
        lines.extend(queued);
        lines.join("\n")
    }

    pub fn try_enqueue(
        &self,
        message: UpdateMessage,
        owner_id: i64,
        request: DownloadRequest,
        user_limits: EffectiveUserLimits,
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
        let owner_queued = state
            .pending
            .iter()
            .filter(|job| job.owner_id == owner_id)
            .count();
        if owner_queued >= user_limits.max_queued_jobs {
            return QueueReply::message(
                message,
                format!(
                    "Your download queue is full ({owner_queued}/{} waiting).",
                    user_limits.max_queued_jobs
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
            request: request.clone(),
            user_limits,
            cancellation,
            progress: progress.clone(),
        });
        drop(state);
        self.shared.changed.notify_one();

        QueueReply::Accepted(AcceptedReply {
            progress,
            owner_id,
            request,
            shared: Arc::clone(&self.shared),
        })
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
            record_download_stat(&self.shared, owner_id, DownloadStatOutcome::Cancelled);
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

    pub fn try_cancel_replied(
        &self,
        message: UpdateMessage,
        owner_id: i64,
        status_message_id: i32,
    ) -> QueueReply {
        let job_id = lock(&self.shared.state)
            .status_jobs
            .get(&(owner_id, status_message_id))
            .map(|job| job.id);
        match job_id {
            Some(job_id) => self.try_cancel(message, owner_id, job_id),
            None => QueueReply::message(
                message,
                "The replied message is not a known download status. Reply to a download status with /cancel.",
            ),
        }
    }

    pub fn retry_request(&self, owner_id: i64, status_message_id: i32) -> Option<DownloadRequest> {
        lock(&self.shared.state)
            .status_jobs
            .get(&(owner_id, status_message_id))
            .map(|job| job.request.clone())
    }
}

impl QueueReply {
    pub fn is_accepted(&self) -> bool {
        matches!(self, Self::Accepted(_))
    }

    pub(crate) fn message(message: UpdateMessage, text: impl Into<String>) -> Self {
        Self::Message {
            message: Box::new(message),
            text: text.into(),
        }
    }

    pub async fn send(self) {
        match self {
            Self::Accepted(AcceptedReply {
                progress,
                owner_id,
                request,
                shared,
            }) => {
                if let Some(status_message_id) = progress.attach().await {
                    register_status(
                        &shared,
                        owner_id,
                        status_message_id,
                        StatusJob {
                            id: progress.id(),
                            request,
                        },
                    );
                }
            }
            Self::Silent => {}
            Self::Message { message, text } => {
                let _ = message.reply(text).await;
            }
        }
    }
}

fn register_status(shared: &SharedQueue, owner_id: i64, status_message_id: i32, job: StatusJob) {
    let key = (owner_id, status_message_id);
    let mut state = lock(&shared.state);
    if state.status_jobs.insert(key, job).is_none() {
        state.status_order.push_back(key);
    }
    while state.status_order.len() > STATUS_HISTORY_LIMIT {
        if let Some(oldest) = state.status_order.pop_front() {
            state.status_jobs.remove(&oldest);
        }
    }
}

async fn run_queue(shared: Arc<SharedQueue>, client: Client) {
    let mut tasks = JoinSet::new();

    loop {
        while tasks.len() < shared.concurrency {
            let job = {
                let mut state = lock(&shared.state);
                let position = next_pending_position(
                    state
                        .pending
                        .iter()
                        .map(|job| (job.owner_id, job.user_limits.max_active_jobs)),
                    state.active.values().map(|job| job.owner_id),
                );
                let job = position.and_then(|position| state.pending.remove(position));
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
            let mut limits = shared.limits;
            limits.max_upload_bytes = job.user_limits.max_upload_bytes;
            tasks.spawn(async move {
                let _active_guard = ActiveJobGuard {
                    id: job.id,
                    shared: job_shared,
                };
                let result_progress = job.progress.clone();
                let cancellation = job.cancellation.clone();
                let watchdog_cancellation = job.cancellation.clone();
                let worker_progress = job.progress.clone();
                let owner_id = job.owner_id;
                let mut worker = tokio::spawn(async move {
                    worker_progress.downloading();
                    if cancellation.is_cancelled() {
                        DownloadOutcome::Cancelled
                    } else {
                        pipeline::download_and_upload(
                            client,
                            job.message,
                            job.request,
                            &cancellation,
                            &worker_progress,
                            limits,
                        )
                        .await
                    }
                });
                let outcome = match tokio::time::timeout(limits.job_timeout, &mut worker).await {
                    Ok(Ok(outcome)) => outcome,
                    Ok(Err(error)) => {
                        result_progress.fail(format!("job task crashed: {error}"));
                        DownloadOutcome::Failed
                    }
                    Err(_) => {
                        watchdog_cancellation.cancel();
                        if tokio::time::timeout(Duration::from_secs(5), &mut worker)
                            .await
                            .is_err()
                        {
                            worker.abort();
                            let _ = worker.await;
                        }
                        result_progress.fail(format!(
                            "job timed out after {} seconds",
                            limits.job_timeout.as_secs()
                        ));
                        DownloadOutcome::Failed
                    }
                };
                (owner_id, result_progress, outcome)
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
                    Some(Ok((owner_id, progress, outcome))) => {
                        let (phase, stat) = match outcome {
                            DownloadOutcome::Completed => (
                                JobPhase::Completed,
                                DownloadStatOutcome::Completed,
                            ),
                            DownloadOutcome::Failed => (
                                JobPhase::Failed,
                                DownloadStatOutcome::Failed,
                            ),
                            DownloadOutcome::Cancelled => (
                                JobPhase::Cancelled,
                                DownloadStatOutcome::Cancelled,
                            ),
                        };
                        progress.set_phase(phase);
                        record_download_stat(&shared, owner_id, stat);
                    }
                    Some(Err(error)) => log::error!(
                        "Download queue wrapper ended unexpectedly; this is a queue bug: {error}"
                    ),
                    None => {}
                }
            }
            _ = shared.changed.notified() => {}
        }
    }
}

fn record_download_stat(shared: &SharedQueue, owner_id: i64, outcome: DownloadStatOutcome) {
    if shared
        .stats
        .send(DownloadStatEvent { owner_id, outcome })
        .is_err()
    {
        log::error!("Download statistics recorder is unavailable");
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn next_pending_position(
    pending_jobs: impl IntoIterator<Item = (i64, usize)>,
    active_owners: impl IntoIterator<Item = i64>,
) -> Option<usize> {
    let mut active_counts = HashMap::new();
    for owner_id in active_owners {
        *active_counts.entry(owner_id).or_insert(0usize) += 1;
    }
    let pending_jobs: Vec<_> = pending_jobs.into_iter().collect();
    let mut below_limit = None;
    for (position, (owner_id, max_active_jobs)) in pending_jobs.into_iter().enumerate() {
        let active = active_counts.get(&owner_id).copied().unwrap_or_default();
        if active == 0 {
            return Some(position);
        }
        if active < max_active_jobs && below_limit.is_none() {
            below_limit = Some(position);
        }
    }
    below_limit
}

#[cfg(test)]
mod tests {
    use super::next_pending_position;

    #[test]
    fn prefers_a_user_without_an_active_job() {
        assert_eq!(
            next_pending_position([(11, 2), (11, 2), (22, 1)], [11]),
            Some(2)
        );
    }

    #[test]
    fn preserves_fifo_among_users_without_active_jobs() {
        assert_eq!(
            next_pending_position([(11, 2), (22, 1), (33, 1)], [11]),
            Some(1)
        );
    }

    #[test]
    fn uses_spare_per_user_capacity_after_fairness() {
        assert_eq!(
            next_pending_position([(11, 2), (22, 1), (11, 2)], [11, 22]),
            Some(0)
        );
        assert_eq!(next_pending_position([(11, 1), (11, 1)], [11]), None);
    }

    #[test]
    fn returns_none_for_an_empty_queue() {
        assert_eq!(next_pending_position([], [11]), None);
    }
}
