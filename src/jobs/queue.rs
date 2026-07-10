use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use grammers_client::Client;
use grammers_client::update::Message as UpdateMessage;
use tokio::sync::Notify;
use tokio::task::{JoinHandle, JoinSet};

use crate::cancel::CancellationToken;
use crate::jobs::progress::{JobPhase, JobProgress};
use crate::media::pipeline;
use crate::media::request::{DownloadLimits, DownloadOutcome, DownloadRequest};

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
    limits: DownloadLimits,
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
            limits,
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
            let limits = shared.limits;
            tasks.spawn(async move {
                let _active_guard = ActiveJobGuard {
                    id: job.id,
                    shared: job_shared,
                };
                let result_progress = job.progress.clone();
                let cancellation = job.cancellation.clone();
                let watchdog_cancellation = job.cancellation.clone();
                let worker_progress = job.progress.clone();
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
                (result_progress, outcome)
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

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
