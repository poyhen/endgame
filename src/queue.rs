use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use grammers_client::Client;
use grammers_client::message::{InputMessage, Message as SentMessage};
use grammers_client::update::Message as UpdateMessage;
use tokio::sync::Notify;
use tokio::task::{JoinHandle, JoinSet};

use crate::cancel::CancellationToken;
use crate::download::{self, DownloadLimits, DownloadOutcome, DownloadRequest};

const STATUS_OPERATION_TIMEOUT: Duration = Duration::from_secs(10);

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

    pub fn uploading(&self, current: usize, total: usize) {
        self.set_phase(JobPhase::Uploading {
            current,
            total,
            bytes: None,
        });
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

    async fn attach(&self) {
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
            }
            Ok(Ok(status)) => {
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
                        return;
                    };
                    let deletion =
                        match tokio::time::timeout(STATUS_OPERATION_TIMEOUT, status.delete()).await
                        {
                            Ok(result) => result,
                            Err(_) => {
                                self.resume_text_status();
                                return;
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
            }
            Ok(Err(error)) => {
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
        match phase {
            JobPhase::Queued => format!("Job #{} queued.", self.inner.id),
            JobPhase::Downloading {
                percent: Some(percent),
            } => format!("Job #{} is downloading: {percent}%.", self.inner.id),
            JobPhase::Downloading { percent: None } => {
                format!("Job #{} is downloading.", self.inner.id)
            }
            JobPhase::Inspecting => format!("Job #{} is inspecting media.", self.inner.id),
            JobPhase::Transcoding {
                percent: Some(percent),
            } => {
                format!(
                    "Job #{} is transcoding compatible media: {percent}%.",
                    self.inner.id
                )
            }
            JobPhase::Transcoding { percent: None } => {
                format!("Job #{} is transcoding compatible media.", self.inner.id)
            }
            JobPhase::Thumbnailing => {
                format!("Job #{} is preparing a thumbnail.", self.inner.id)
            }
            JobPhase::Uploading {
                current,
                total,
                bytes,
            } => render_uploading(self.inner.id, current, total, bytes),
            JobPhase::Finalizing => format!("Job #{} is finalizing delivery.", self.inner.id),
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

    fn summary(&self) -> String {
        let phase = lock(&self.inner.phase).clone();
        let elapsed = lock(&self.inner.phase_since).elapsed();
        format!(
            "#{} {} for {}",
            self.inner.id,
            phase.label(),
            format_duration(elapsed)
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

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

fn render_uploading(id: u64, current: usize, total: usize, bytes: Option<(u64, u64)>) -> String {
    let Some((uploaded, size)) = bytes else {
        return format!("Job #{id} is uploading item {current}/{total}.");
    };
    let percentage = if size == 0 {
        100
    } else {
        ((uploaded as u128 * 100) / size as u128) as u64
    };
    let remaining = size.saturating_sub(uploaded);
    format!(
        "Job #{id} is uploading item {current}/{total}: {percentage}% • {} left.",
        format_bytes(remaining)
    )
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

    if bytes < 1024 {
        return format!("{bytes} B");
    }

    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let precision = if value >= 100.0 {
        0
    } else if value >= 10.0 {
        1
    } else {
        2
    };
    format!("{value:.precision$} {}", UNITS[unit])
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
                        download::download_and_upload(
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

fn truncate_failure(reason: String) -> String {
    if reason.chars().count() > 500 {
        format!("{}...", reason.chars().take(497).collect::<String>())
    } else {
        reason
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{JobPhase, format_bytes, format_duration, render_uploading, truncate_failure};

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

    #[test]
    fn upload_status_reports_percentage_and_remaining_bytes() {
        assert_eq!(
            render_uploading(15, 1, 1, Some((800, 2_000))),
            "Job #15 is uploading item 1/1: 40% • 1.17 KiB left."
        );
        assert_eq!(
            render_uploading(15, 1, 1, None),
            "Job #15 is uploading item 1/1."
        );
    }

    #[test]
    fn byte_count_formatting_uses_readable_binary_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1024), "1.00 KiB");
        assert_eq!(format_bytes(512 * 1024 * 1024), "512 MiB");
    }

    #[test]
    fn elapsed_phase_time_is_human_readable() {
        assert_eq!(format_duration(Duration::from_secs(8)), "8s");
        assert_eq!(format_duration(Duration::from_secs(125)), "2m 5s");
        assert_eq!(format_duration(Duration::from_secs(7_500)), "2h 5m");
    }
}
