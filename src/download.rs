use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use grammers_client::Client;
use grammers_client::media::{Attribute, InputMedia, Uploaded};
use grammers_client::message::InputMessage;
use grammers_client::update::Message as UpdateMessage;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader, ReadBuf};
use tokio::process::Command;

use crate::cancel::CancellationToken;
use crate::queue::JobProgress;
use crate::utils::{clean_cookie_file, extract_thumbnail, generate_random_filename, probe_video};

const GALLERY_DL_DOWNLOAD_PATH: &str = "gallery_dl_downloads";

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:131.0) Gecko/20100101 Firefox/131.0";

const DEFAULT_YT_DLP_FORMAT_SELECTOR: &str =
    "bestvideo[vcodec!*=av01][vcodec!*=vp9][ext=mp4]+bestaudio[ext=m4a]/best[ext=mp4]/best";

const UPLOAD_PROGRESS_INTERVAL: Duration = Duration::from_secs(5);
const TELEGRAM_OPERATION_TIMEOUT: Duration = Duration::from_secs(120);
const UTILITY_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const COMMAND_OUTPUT_LIMIT: usize = 64 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct DownloadLimits {
    pub max_upload_bytes: u64,
    pub command_timeout: Duration,
    pub upload_timeout: Duration,
    pub job_timeout: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadRequest {
    pub url: String,
    pub mode: DownloadMode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DownloadMode {
    Video { max_height: Option<u32> },
    Audio,
}

impl DownloadRequest {
    pub fn video(url: String, max_height: Option<u32>) -> Self {
        Self {
            url,
            mode: DownloadMode::Video { max_height },
        }
    }

    pub fn audio(url: String) -> Self {
        Self {
            url,
            mode: DownloadMode::Audio,
        }
    }
}

pub enum DownloadOutcome {
    Completed,
    Failed,
    Cancelled,
}

enum CommandOutcome {
    Finished {
        success: bool,
        stdout: String,
        stderr: String,
    },
    Cancelled,
    TimedOut,
}

struct TempFileGuard(PathBuf);

enum CommandProgress {
    Download(JobProgress),
    Transcode {
        progress: JobProgress,
        duration_micros: u64,
    },
}

struct ProgressReader<R> {
    inner: R,
    progress: JobProgress,
    position: (usize, usize),
    bytes_read: u64,
    total_bytes: u64,
    last_reported_bytes: u64,
    last_report: Instant,
}

impl<R> ProgressReader<R> {
    fn new(inner: R, progress: JobProgress, position: (usize, usize), total_bytes: u64) -> Self {
        progress.upload_progress(position.0, position.1, 0, total_bytes);
        Self {
            inner,
            progress,
            position,
            bytes_read: 0,
            total_bytes,
            last_reported_bytes: 0,
            last_report: Instant::now(),
        }
    }

    fn record_read(&mut self, bytes: usize) {
        self.bytes_read = self
            .bytes_read
            .saturating_add(bytes as u64)
            .min(self.total_bytes);
        if self.bytes_read > self.last_reported_bytes
            && self.last_report.elapsed() >= UPLOAD_PROGRESS_INTERVAL
        {
            let reported_bytes = in_flight_upload_bytes(self.bytes_read, self.total_bytes);
            self.progress.upload_progress(
                self.position.0,
                self.position.1,
                reported_bytes,
                self.total_bytes,
            );
            self.last_reported_bytes = self.bytes_read;
            self.last_report = Instant::now();
        }
    }
}

fn in_flight_upload_bytes(bytes_read: u64, total_bytes: u64) -> u64 {
    let bytes_read = bytes_read.min(total_bytes);
    // Grammers can read a few chunks ahead of Telegram's acknowledgements, so
    // reserve 100% for the successful return from `upload_stream`.
    if bytes_read == total_bytes && total_bytes > 0 {
        total_bytes - 1
    } else {
        bytes_read
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for ProgressReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let filled_before = buffer.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(context, buffer);
        if let Poll::Ready(Ok(())) = &result {
            self.record_read(buffer.filled().len().saturating_sub(filled_before));
        }
        result
    }
}

impl TempFileGuard {
    fn new(path: String) -> Self {
        Self(PathBuf::from(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

struct PreparedVideo {
    path: PathBuf,
    _cleanup: Option<TempFileGuard>,
}

#[derive(Clone, Copy)]
struct DeliveryPlan {
    position: (usize, usize),
    replace_status: bool,
}

impl PreparedVideo {
    fn original(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            _cleanup: None,
        }
    }

    fn temporary(path: String) -> Self {
        let cleanup = TempFileGuard::new(path);
        Self {
            path: cleanup.path().to_path_buf(),
            _cleanup: Some(cleanup),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn user_info(message: &UpdateMessage) -> String {
    let peer = message.sender().or_else(|| message.peer());
    let Some(peer) = peer else {
        return "User".to_string();
    };
    let Some(id) = peer.id().bare_id() else {
        return "User".to_string();
    };
    match peer.username() {
        Some(u) => format!("User @{u} ({id})"),
        None => format!("User {id}"),
    }
}

fn should_use_yt_dlp(url: &str) -> bool {
    [
        "youtube.com/",
        "youtu.be/",
        "instagram.com/share/",
        "instagram.com/reels/",
        "instagram.com/tv/",
        "x.com/i/broadcasts/",
        "tiktok.com/",
    ]
    .iter()
    .any(|d| url.contains(d))
}

async fn run_command(
    mut cmd: Command,
    cancellation: &CancellationToken,
    timeout: Duration,
    job_id: u64,
    command_progress: Option<CommandProgress>,
) -> CommandOutcome {
    let program = cmd.as_std().get_program().to_string_lossy().into_owned();
    cmd.kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            return CommandOutcome::Finished {
                success: false,
                stdout: String::new(),
                stderr: error.to_string(),
            };
        }
    };
    let pid = child.id();
    let started = Instant::now();
    log::info!("Job #{job_id} started {program} (pid={pid:?})");
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_task = tokio::spawn(async move {
        if let Some(mut stdout) = stdout {
            match command_progress {
                Some(CommandProgress::Download(progress)) => {
                    read_ytdlp_progress(stdout, progress).await
                }
                Some(CommandProgress::Transcode {
                    progress,
                    duration_micros,
                }) => read_ffmpeg_progress(stdout, progress, duration_micros).await,
                None => read_bounded_output(&mut stdout).await,
            }
        } else {
            Vec::new()
        }
    });
    let stderr_task = tokio::spawn(async move {
        if let Some(mut stderr) = stderr {
            read_bounded_output(&mut stderr).await
        } else {
            Vec::new()
        }
    });

    enum StopReason {
        Finished,
        Cancelled,
        TimedOut,
    }

    let (reason, status) = tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            terminate_child(&mut child).await;
            (StopReason::Cancelled, child.wait().await)
        },
        _ = tokio::time::sleep(timeout) => {
            terminate_child(&mut child).await;
            (StopReason::TimedOut, child.wait().await)
        },
        result = child.wait() => (StopReason::Finished, result),
    };
    let interrupted = !matches!(reason, StopReason::Finished);
    let stdout = collect_reader(stdout_task, interrupted).await;
    let stderr = collect_reader(stderr_task, interrupted).await;
    log::info!(
        "Job #{job_id} {program} ended after {:.1}s ({})",
        started.elapsed().as_secs_f64(),
        match reason {
            StopReason::Finished => "finished",
            StopReason::Cancelled => "cancelled",
            StopReason::TimedOut => "timed out",
        }
    );
    match reason {
        StopReason::Cancelled => return CommandOutcome::Cancelled,
        StopReason::TimedOut => return CommandOutcome::TimedOut,
        StopReason::Finished => {}
    }

    match status {
        Ok(status) => CommandOutcome::Finished {
            success: status.success(),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        },
        Err(error) => CommandOutcome::Finished {
            success: false,
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: error.to_string(),
        },
    }
}

async fn terminate_child(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let _ = Command::new("/bin/kill")
            .arg("-KILL")
            .arg("--")
            .arg(format!("-{pid}"))
            .status()
            .await;
    }
    let _ = child.kill().await;
}

async fn read_bounded_output(reader: &mut (impl AsyncRead + Unpin)) -> Vec<u8> {
    let mut output = Vec::with_capacity(COMMAND_OUTPUT_LIMIT);
    let mut buffer = [0u8; 8192];
    while let Ok(read) = reader.read(&mut buffer).await {
        if read == 0 {
            break;
        }
        output.extend_from_slice(&buffer[..read]);
        if output.len() > COMMAND_OUTPUT_LIMIT * 2 {
            let excess = output.len() - COMMAND_OUTPUT_LIMIT;
            output.drain(..excess);
        }
    }
    if output.len() > COMMAND_OUTPUT_LIMIT {
        let excess = output.len() - COMMAND_OUTPUT_LIMIT;
        output.drain(..excess);
    }
    output
}

async fn read_ffmpeg_progress(
    reader: impl AsyncRead + Unpin,
    progress: JobProgress,
    duration_micros: u64,
) -> Vec<u8> {
    let mut reader = BufReader::new(reader);
    let mut output = Vec::with_capacity(COMMAND_OUTPUT_LIMIT);
    let mut line = Vec::new();
    let mut last_bucket = 0u8;
    loop {
        line.clear();
        let Ok(read) = reader.read_until(b'\n', &mut line).await else {
            break;
        };
        if read == 0 {
            break;
        }
        output.extend_from_slice(&line);
        if output.len() > COMMAND_OUTPUT_LIMIT * 2 {
            let excess = output.len() - COMMAND_OUTPUT_LIMIT;
            output.drain(..excess);
        }

        if let Some(percent) = transcode_percent(&line, duration_micros) {
            let bucket = percent / 5 * 5;
            if bucket >= last_bucket.saturating_add(5) {
                last_bucket = bucket;
                progress.transcoding(Some(bucket));
            }
        }
    }
    if output.len() > COMMAND_OUTPUT_LIMIT {
        let excess = output.len() - COMMAND_OUTPUT_LIMIT;
        output.drain(..excess);
    }
    output
}

async fn read_ytdlp_progress(reader: impl AsyncRead + Unpin, progress: JobProgress) -> Vec<u8> {
    let mut reader = BufReader::new(reader);
    let mut output = Vec::with_capacity(COMMAND_OUTPUT_LIMIT);
    let mut line = Vec::new();
    let mut last_bucket = 0u8;
    loop {
        line.clear();
        let Ok(read) = reader.read_until(b'\n', &mut line).await else {
            break;
        };
        if read == 0 {
            break;
        }
        output.extend_from_slice(&line);
        if output.len() > COMMAND_OUTPUT_LIMIT * 2 {
            let excess = output.len() - COMMAND_OUTPUT_LIMIT;
            output.drain(..excess);
        }
        if let Some(percent) = download_percent(&line) {
            let bucket = percent / 5 * 5;
            if bucket >= last_bucket.saturating_add(5) {
                last_bucket = bucket;
                progress.download_progress(bucket);
            }
        }
    }
    if output.len() > COMMAND_OUTPUT_LIMIT {
        let excess = output.len() - COMMAND_OUTPUT_LIMIT;
        output.drain(..excess);
    }
    output
}

fn download_percent(line: &[u8]) -> Option<u8> {
    let text = String::from_utf8_lossy(line);
    let value = text
        .trim()
        .strip_prefix("download:")?
        .trim()
        .trim_end_matches('%')
        .trim()
        .parse::<f64>()
        .ok()?;
    Some(value.clamp(0.0, 100.0) as u8)
}

fn transcode_percent(line: &[u8], duration_micros: u64) -> Option<u8> {
    if duration_micros == 0 {
        return None;
    }
    let text = String::from_utf8_lossy(line);
    let elapsed = text
        .trim()
        .strip_prefix("out_time_us=")
        .or_else(|| text.trim().strip_prefix("out_time_ms="))?
        .parse::<u64>()
        .ok()?;
    Some(((elapsed as u128 * 100) / duration_micros as u128).min(99) as u8)
}

async fn collect_reader(mut task: tokio::task::JoinHandle<Vec<u8>>, cancelled: bool) -> Vec<u8> {
    let timeout = if cancelled {
        Duration::from_secs(2)
    } else {
        Duration::from_secs(5)
    };
    match tokio::time::timeout(timeout, &mut task).await {
        Ok(result) => result.unwrap_or_default(),
        Err(_) => {
            task.abort();
            Vec::new()
        }
    }
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn is_video_ext(p: &str) -> bool {
    let p = p.to_lowercase();
    p.ends_with(".mp4") || p.ends_with(".mkv") || p.ends_with(".webm") || p.ends_with(".mov")
}

fn is_image_ext(p: &str) -> bool {
    let p = p.to_lowercase();
    p.ends_with(".jpg")
        || p.ends_with(".jpeg")
        || p.ends_with(".png")
        || p.ends_with(".gif")
        || p.ends_with(".webp")
}

fn media_kind(path: &Path) -> (bool, bool, bool, Option<String>) {
    match mime_guess::from_path(path).first() {
        Some(m) => (
            m.type_() == mime::VIDEO,
            m.type_() == mime::IMAGE,
            m.type_() == mime::AUDIO,
            Some(m.essence_str().to_string()),
        ),
        None => (false, false, false, None),
    }
}

async fn upload_file_with_progress(
    client: &Client,
    path: &Path,
    cancellation: &CancellationToken,
    progress: &JobProgress,
    position: (usize, usize),
    upload_timeout: Duration,
) -> anyhow::Result<Uploaded> {
    let file = tokio::fs::File::open(path).await?;
    let total_bytes = file.metadata().await?.len();
    let size = usize::try_from(total_bytes)
        .map_err(|_| anyhow::anyhow!("file is too large for this platform"))?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow::anyhow!("upload path has no file name"))?;
    let mut reader = ProgressReader::new(file, progress.clone(), position, total_bytes);

    let uploaded = tokio::select! {
        biased;
        _ = cancellation.cancelled() => anyhow::bail!("cancelled"),
        _ = tokio::time::sleep(upload_timeout) => anyhow::bail!(
            "Telegram upload timed out after {} seconds",
            upload_timeout.as_secs()
        ),
        result = client.upload_stream(&mut reader, size, name) => result?,
    };
    progress.upload_progress(position.0, position.1, total_bytes, total_bytes);
    Ok(uploaded)
}

pub async fn download_and_upload(
    client: Client,
    message: UpdateMessage,
    request: DownloadRequest,
    cancellation: &CancellationToken,
    progress: &JobProgress,
    limits: DownloadLimits,
) -> DownloadOutcome {
    if cancellation.is_cancelled() {
        return DownloadOutcome::Cancelled;
    }

    let info = user_info(&message);
    let DownloadRequest { url, mode } = request;
    let use_yt_dlp = matches!(&mode, DownloadMode::Audio) || should_use_yt_dlp(&url);

    // Cookies selection.
    let mut cookies_file = "cookies.txt".to_string();
    if url.contains("instagram.com/") && Path::new("instacookies.txt").exists() {
        cookies_file = "instacookies.txt".to_string();
        clean_cookie_file(&cookies_file);
    }

    let mut cleanup_path: Option<PathBuf> = None;
    let mut yt_dlp_base: Option<String> = None;

    let downloader_name = if use_yt_dlp { "yt-dlp" } else { "gallery-dl" };

    let mut cmd = Command::new(downloader_name);

    if use_yt_dlp {
        let base = generate_random_filename("");
        yt_dlp_base = Some(base.clone());
        let output_template = format!("{base}.%(ext)s");
        cmd.arg("-o")
            .arg(&output_template)
            .arg("--cookies")
            .arg(&cookies_file)
            .arg("--user-agent")
            .arg(USER_AGENT)
            .arg("--newline")
            .arg("--progress-template")
            .arg("download:download:%(progress._percent_str)s");

        if matches!(&mode, DownloadMode::Audio) {
            cmd.arg("--extract-audio")
                .arg("--audio-format")
                .arg("mp3")
                .arg("--audio-quality")
                .arg("0");
        } else if url.contains("youtube.com/") || url.contains("youtu.be/") {
            let max_height = match &mode {
                DownloadMode::Video { max_height } => *max_height,
                DownloadMode::Audio => None,
            };
            cmd.arg("-f")
                .arg(youtube_format_selector(limits.max_upload_bytes, max_height));
        } else if let DownloadMode::Video {
            max_height: Some(height),
        } = &mode
        {
            cmd.arg("-f").arg(video_format_selector(*height));
        } else {
            let mut chosen = DEFAULT_YT_DLP_FORMAT_SELECTOR.to_string();
            for (domain, fmt) in [(
                "tiktok.com",
                "bestvideo[ext=mp4][vcodec=h264]+bestaudio[ext=m4a]/best[ext=mp4][vcodec=h264]/best",
            )] {
                if url.contains(domain) {
                    chosen = fmt.to_string();
                    break;
                }
            }
            cmd.arg("-f").arg(&chosen);
        }
        cmd.arg("--no-playlist").arg(&url);
        println!("{info} | Using yt-dlp for URL: {url}");
    } else {
        let instance_path = Path::new(GALLERY_DL_DOWNLOAD_PATH).join(generate_random_filename(""));
        if let Err(e) = std::fs::create_dir_all(&instance_path) {
            progress.fail(format!("could not create the download directory: {e}"));
            return DownloadOutcome::Failed;
        }
        cleanup_path = Some(instance_path.clone());
        cmd.arg("--cookies")
            .arg(&cookies_file)
            .arg("--user-agent")
            .arg(USER_AGENT)
            .arg("-D")
            .arg(&instance_path)
            .arg(&url);
        println!("{info} | Using gallery-dl for URL: {url}");
    }

    let (success, stdout, stderr) = match run_command(
        cmd,
        cancellation,
        limits.command_timeout,
        progress.id(),
        use_yt_dlp.then(|| CommandProgress::Download(progress.clone())),
    )
    .await
    {
        CommandOutcome::Finished {
            success,
            stdout,
            stderr,
        } => (success, stdout, stderr),
        CommandOutcome::Cancelled => {
            perform_cleanup(&cleanup_path, &yt_dlp_base, use_yt_dlp, &info);
            return DownloadOutcome::Cancelled;
        }
        CommandOutcome::TimedOut => {
            progress.fail(format!(
                "{downloader_name} timed out after {} seconds",
                limits.command_timeout.as_secs()
            ));
            perform_cleanup(&cleanup_path, &yt_dlp_base, use_yt_dlp, &info);
            return DownloadOutcome::Failed;
        }
    };

    if !success {
        let mut error_message = stderr;
        if error_message.is_empty() {
            error_message = stdout;
        }
        if error_message.is_empty() {
            error_message = "Unknown error".to_string();
        }
        progress.fail(format!("{downloader_name} download error: {error_message}"));
        perform_cleanup(&cleanup_path, &yt_dlp_base, use_yt_dlp, &info);
        return DownloadOutcome::Failed;
    }

    // Gather downloaded files.
    let mut downloaded: Vec<PathBuf> = Vec::new();
    if use_yt_dlp {
        let base = yt_dlp_base.clone().unwrap_or_default();
        if let Ok(entries) = std::fs::read_dir(".") {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(&base) {
                    downloaded.push(entry.path());
                }
            }
        }
        if let Some(first) = downloaded.first() {
            cleanup_path = Some(first.clone());
        } else {
            progress.fail(format!(
                "{downloader_name} finished, but its output file was not found"
            ));
            perform_cleanup(&cleanup_path, &yt_dlp_base, use_yt_dlp, &info);
            return DownloadOutcome::Failed;
        }
    } else {
        let dir = cleanup_path.clone().unwrap_or_default();
        collect_files(&dir, &mut downloaded);
        if downloaded.is_empty() {
            progress.fail(format!(
                "{downloader_name} finished without producing a media file"
            ));
            perform_cleanup(&cleanup_path, &yt_dlp_base, use_yt_dlp, &info);
            return DownloadOutcome::Failed;
        }
        // Sort to prefer videos, then images.
        downloaded.sort_by_key(|p| {
            let s = p.to_string_lossy().into_owned();
            (!is_video_ext(&s), !is_image_ext(&s), s)
        });
    }

    let mut any_success = false;
    let mut sent_count = 0usize;
    let mut last_error: Option<String> = None;

    let gallery_media_count = downloaded
        .iter()
        .filter(|item| {
            let (is_video, is_image, _, _) = media_kind(item);
            is_video || is_image
        })
        .count();
    let recognized_media_count = downloaded
        .iter()
        .filter(|item| {
            let (is_video, is_image, is_audio, _) = media_kind(item);
            is_video || is_image || is_audio
        })
        .count();
    let replace_status_with_media = recognized_media_count == 1;
    if !use_yt_dlp && gallery_media_count > 1 {
        let outcome = send_gallery_albums(
            &client,
            &message,
            &downloaded,
            cancellation,
            progress,
            &info,
            limits,
        )
        .await;
        perform_cleanup(&cleanup_path, &yt_dlp_base, use_yt_dlp, &info);
        return outcome;
    }

    for (index, item) in downloaded.iter().enumerate() {
        if cancellation.is_cancelled() {
            perform_cleanup(&cleanup_path, &yt_dlp_base, use_yt_dlp, &info);
            return DownloadOutcome::Cancelled;
        }
        progress.uploading(index + 1, downloaded.len());

        if !item.exists() {
            if !use_yt_dlp {
                last_error = Some("a downloaded gallery item disappeared before upload".into());
                continue;
            } else {
                progress.fail("the downloaded file disappeared before upload");
                perform_cleanup(&cleanup_path, &yt_dlp_base, use_yt_dlp, &info);
                return DownloadOutcome::Failed;
            }
        }

        let (is_video, is_image, is_audio, mime_type) = media_kind(item);
        let mut processed = false;

        if is_video {
            let send_result = send_video(
                &client,
                &message,
                item,
                cancellation,
                progress,
                limits,
                DeliveryPlan {
                    position: (index + 1, downloaded.len()),
                    replace_status: replace_status_with_media,
                },
            )
            .await;
            if cancellation.is_cancelled() {
                perform_cleanup(&cleanup_path, &yt_dlp_base, use_yt_dlp, &info);
                return DownloadOutcome::Cancelled;
            }
            match send_result {
                Ok(()) => {
                    any_success = true;
                    processed = true;
                    sent_count += 1;
                }
                Err(e) => {
                    let name = item
                        .file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    println!("{info} | Error processing video {name}: {e}");
                    last_error = Some(format!("could not process video {name}: {e}"));
                }
            }
        } else if is_image {
            let send_result = send_image(
                &client,
                &message,
                item,
                cancellation,
                progress,
                limits,
                DeliveryPlan {
                    position: (index + 1, downloaded.len()),
                    replace_status: replace_status_with_media,
                },
            )
            .await;
            if cancellation.is_cancelled() {
                perform_cleanup(&cleanup_path, &yt_dlp_base, use_yt_dlp, &info);
                return DownloadOutcome::Cancelled;
            }
            match send_result {
                Ok(()) => {
                    any_success = true;
                    processed = true;
                    sent_count += 1;
                }
                Err(e) => {
                    let name = item
                        .file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    println!("{info} | Error sending image {name}: {e}");
                    last_error = Some(format!("could not upload image {name}: {e}"));
                }
            }
        } else if is_audio {
            let send_result = send_audio(
                &client,
                &message,
                item,
                cancellation,
                progress,
                limits,
                DeliveryPlan {
                    position: (index + 1, downloaded.len()),
                    replace_status: replace_status_with_media,
                },
            )
            .await;
            if cancellation.is_cancelled() {
                perform_cleanup(&cleanup_path, &yt_dlp_base, use_yt_dlp, &info);
                return DownloadOutcome::Cancelled;
            }
            match send_result {
                Ok(()) => {
                    any_success = true;
                    processed = true;
                    sent_count += 1;
                }
                Err(error) => {
                    let name = item
                        .file_name()
                        .map(|file| file.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    log::warn!("{info} | Error sending audio {name}: {error}");
                    last_error = Some(format!("could not upload audio {name}: {error}"));
                }
            }
        }

        if !processed && !is_video && !is_image && !is_audio {
            let name = item
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            log::warn!(
                "{info} | Skipping non-media file from {downloader_name}: {name} (MIME: {mime_type:?})"
            );
            last_error = Some(format!(
                "{downloader_name} produced an unsupported file: {name}"
            ));
        }
    }

    if !any_success && !downloaded.is_empty() {
        println!(
            "{info} | {downloader_name} downloaded content, but could not process or send any recognized media file."
        );
        progress.fail(
            last_error.clone().unwrap_or_else(|| {
                format!("{downloader_name} produced no supported media to upload")
            }),
        );
    } else if !use_yt_dlp && sent_count > 0 {
        let total = downloaded.len();
        if sent_count == total {
            println!("{info} | Finished processing gallery. Sent all {sent_count} item(s).");
        } else {
            println!(
                "{info} | Finished processing gallery. Sent {sent_count} item(s) from {total} downloaded file(s)."
            );
        }
    }

    if any_success && !replace_status_with_media {
        if let Some(error) = last_error {
            progress.complete_with_warning(error);
        } else {
            progress.delete_status().await;
        }
    }

    perform_cleanup(&cleanup_path, &yt_dlp_base, use_yt_dlp, &info);
    if any_success {
        DownloadOutcome::Completed
    } else {
        DownloadOutcome::Failed
    }
}

fn video_format_selector(max_height: u32) -> String {
    format!(
        "bestvideo[height<={max_height}][vcodec!*=av01][vcodec!*=vp9][ext=mp4]+bestaudio[ext=m4a]/best[height<={max_height}][ext=mp4]/best[height<={max_height}]"
    )
}

fn youtube_format_selector(max_upload_bytes: u64, max_height: Option<u32>) -> String {
    // Reserve ten percent for audio and container overhead. Prefer formats with
    // a known exact size, then estimated size, before falling back to the old
    // compatible selector when YouTube does not expose either value.
    let video_budget = max_upload_bytes.saturating_mul(90) / 100;
    let height = max_height
        .map(|height| format!("[height<={height}]"))
        .unwrap_or_default();
    let fallback = max_height.map_or_else(
        || DEFAULT_YT_DLP_FORMAT_SELECTOR.to_string(),
        video_format_selector,
    );
    format!(
        "bestvideo[ext=mp4][vcodec^=avc1]{height}[filesize<{video_budget}]+bestaudio[ext=m4a]/\
         bestvideo[ext=mp4][vcodec^=avc1]{height}[filesize_approx<{video_budget}]+bestaudio[ext=m4a]/\
         best[ext=mp4][vcodec^=avc1]{height}[filesize<{max_upload_bytes}]/\
         best[ext=mp4][vcodec^=avc1]{height}[filesize_approx<{max_upload_bytes}]/{fallback}"
    )
}

async fn prepare_video(
    path: &Path,
    cancellation: &CancellationToken,
    progress: &JobProgress,
    limits: DownloadLimits,
) -> anyhow::Result<PreparedVideo> {
    progress.inspecting();
    let metadata = probe_video(path, cancellation, UTILITY_COMMAND_TIMEOUT).await?;
    let file_size = std::fs::metadata(path)?.len();
    let container_is_mp4 = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("mp4"));
    if !requires_video_transcode(
        container_is_mp4,
        &metadata.codec,
        file_size,
        limits.max_upload_bytes,
    ) {
        return Ok(PreparedVideo::original(path));
    }
    if cancellation.is_cancelled() {
        anyhow::bail!("cancelled");
    }

    progress.transcoding(None);
    let output = generate_random_filename(".mp4");
    let prepared = PreparedVideo::temporary(output);
    let mut command = Command::new("ffmpeg");
    command
        .arg("-y")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-i")
        .arg(path)
        .arg("-map")
        .arg("0:v:0")
        .arg("-map")
        .arg("0:a:0?")
        .arg("-sn")
        .arg("-dn")
        .arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("medium")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-vf")
        .arg("scale=min(1920\\,iw):min(1080\\,ih):force_original_aspect_ratio=decrease:force_divisible_by=2,setsar=1")
        .arg("-c:a")
        .arg("aac")
        .arg("-b:a")
        .arg("128k")
        .arg("-ac")
        .arg("2");

    if file_size > limits.max_upload_bytes {
        let bitrate = target_video_bitrate(limits.max_upload_bytes, metadata.duration_secs)?;
        command
            .arg("-b:v")
            .arg(bitrate.to_string())
            .arg("-maxrate")
            .arg(bitrate.to_string())
            .arg("-bufsize")
            .arg(bitrate.saturating_mul(2).to_string());
    } else {
        command.arg("-crf").arg("23");
    }
    command
        .arg("-progress")
        .arg("pipe:1")
        .arg("-nostats")
        .arg("-movflags")
        .arg("+faststart")
        .arg("-tag:v")
        .arg("avc1")
        .arg(prepared.path());

    match run_command(
        command,
        cancellation,
        limits.command_timeout,
        progress.id(),
        Some(CommandProgress::Transcode {
            progress: progress.clone(),
            duration_micros: metadata.duration_secs.max(0) as u64 * 1_000_000,
        }),
    )
    .await
    {
        CommandOutcome::Cancelled => anyhow::bail!("cancelled"),
        CommandOutcome::TimedOut => anyhow::bail!(
            "ffmpeg timed out after {} seconds",
            limits.command_timeout.as_secs()
        ),
        CommandOutcome::Finished { success: true, .. } => progress.transcoding(Some(100)),
        CommandOutcome::Finished { stderr, .. } => {
            anyhow::bail!("ffmpeg could not prepare the video: {stderr}")
        }
    }

    let output_size = std::fs::metadata(prepared.path())?.len();
    if output_size > limits.max_upload_bytes {
        anyhow::bail!(
            "prepared video is still too large ({} MiB; limit {} MiB)",
            output_size / (1024 * 1024),
            limits.max_upload_bytes / (1024 * 1024)
        );
    }
    Ok(prepared)
}

fn requires_video_transcode(
    container_is_mp4: bool,
    codec: &str,
    file_size: u64,
    max_upload_bytes: u64,
) -> bool {
    !container_is_mp4 || codec != "h264" || file_size > max_upload_bytes
}

fn target_video_bitrate(max_upload_bytes: u64, duration_secs: i64) -> anyhow::Result<u64> {
    if duration_secs <= 0 {
        anyhow::bail!("cannot resize a video with unknown duration");
    }
    let target_bits_per_second = (max_upload_bytes as u128)
        .saturating_mul(8)
        .saturating_mul(85)
        / 100
        / duration_secs as u128;
    let video_bits_per_second = target_bits_per_second.saturating_sub(128_000);
    if video_bits_per_second < 200_000 {
        anyhow::bail!("the configured upload limit is too small for this video's duration");
    }
    Ok(video_bits_per_second.min(u64::MAX as u128) as u64)
}

async fn send_gallery_albums(
    client: &Client,
    message: &UpdateMessage,
    downloaded: &[PathBuf],
    cancellation: &CancellationToken,
    progress: &JobProgress,
    info: &str,
    limits: DownloadLimits,
) -> DownloadOutcome {
    let media_paths: Vec<&Path> = downloaded
        .iter()
        .filter_map(|item| {
            let (is_video, is_image, _, _) = media_kind(item);
            (is_video || is_image).then_some(item.as_path())
        })
        .collect();
    let total = media_paths.len();
    let mut sent = 0usize;
    let mut cursor = 0usize;
    let mut last_error: Option<String> = None;

    while cursor < total {
        let remaining = total - cursor;
        // Telegram albums accept at most ten items. Avoid a one-item final
        // batch when possible (e.g. split 11 as 9 + 2).
        let batch_size = if remaining == 11 {
            9
        } else {
            remaining.min(10)
        };
        let mut batch = Vec::with_capacity(batch_size);

        for (offset, path) in media_paths[cursor..cursor + batch_size].iter().enumerate() {
            if cancellation.is_cancelled() {
                return DownloadOutcome::Cancelled;
            }
            progress.uploading(cursor + offset + 1, total);

            let prepared = prepare_album_media(
                client,
                path,
                cancellation,
                progress,
                limits,
                (cursor + offset + 1, total),
            )
            .await;
            if cancellation.is_cancelled() {
                return DownloadOutcome::Cancelled;
            }
            match prepared {
                Ok(media) => {
                    progress.uploading(cursor + offset + 1, total);
                    batch.push(media);
                }
                Err(error) => {
                    log::warn!(
                        "{info} | Failed to prepare {} for an album: {error}",
                        path.display()
                    );
                    last_error = Some(format!(
                        "could not prepare {}: {error}",
                        path.file_name()
                            .map(|name| name.to_string_lossy())
                            .unwrap_or_default()
                    ));
                }
            }
        }
        cursor += batch_size;

        if batch.is_empty() {
            continue;
        }
        let batch_count = batch.len();
        let send_result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return DownloadOutcome::Cancelled,
            _ = tokio::time::sleep(TELEGRAM_OPERATION_TIMEOUT) => {
                Err(anyhow::anyhow!(
                    "Telegram album send timed out after {} seconds",
                    TELEGRAM_OPERATION_TIMEOUT.as_secs()
                ))
            },
            result = message.reply_album(batch) => result.map_err(anyhow::Error::from),
        };
        match send_result {
            Ok(_) => sent += batch_count,
            Err(error) => {
                log::warn!("{info} | Failed to send Telegram album: {error}");
                last_error = Some(format!("could not upload a media album: {error}"));
            }
        }
    }

    if sent > 0 {
        println!("{info} | Finished processing gallery. Sent {sent}/{total} item(s) in albums.");
        if let Some(error) = last_error {
            progress.complete_with_warning(error);
        } else {
            progress.delete_status().await;
        }
        DownloadOutcome::Completed
    } else {
        progress.fail(last_error.unwrap_or_else(|| "gallery contained no uploadable media".into()));
        DownloadOutcome::Failed
    }
}

async fn prepare_album_media(
    client: &Client,
    path: &Path,
    cancellation: &CancellationToken,
    progress: &JobProgress,
    limits: DownloadLimits,
    position: (usize, usize),
) -> anyhow::Result<InputMedia> {
    let (is_video, is_image, _, _) = media_kind(path);
    if is_image {
        let photo = upload_file_with_progress(
            client,
            path,
            cancellation,
            progress,
            position,
            limits.upload_timeout,
        )
        .await?;
        return Ok(InputMedia::new().photo(photo));
    }
    if !is_video {
        anyhow::bail!("file is not recognized as an image or video");
    }

    let prepared = prepare_video(path, cancellation, progress, limits).await?;
    progress.inspecting();
    let metadata = probe_video(prepared.path(), cancellation, UTILITY_COMMAND_TIMEOUT).await?;
    progress.thumbnailing();
    let thumbnail = extract_thumbnail(prepared.path(), cancellation, UTILITY_COMMAND_TIMEOUT)
        .await
        .ok()
        .map(TempFileGuard::new);
    let result: anyhow::Result<InputMedia> = async {
        let video = upload_file_with_progress(
            client,
            prepared.path(),
            cancellation,
            progress,
            position,
            limits.upload_timeout,
        )
        .await?;
        let mut media = InputMedia::new()
            .document(video)
            .attribute(Attribute::Video {
                round_message: false,
                supports_streaming: true,
                duration: Duration::from_secs(metadata.duration_secs.max(0) as u64),
                w: metadata.width,
                h: metadata.height,
            });
        if let Some(thumbnail) = &thumbnail
            && let Ok(Ok(thumb)) = tokio::time::timeout(
                TELEGRAM_OPERATION_TIMEOUT,
                client.upload_file(thumbnail.path()),
            )
            .await
        {
            media = media.thumbnail(thumb);
        }
        Ok(media)
    }
    .await;

    result
}

async fn send_video(
    client: &Client,
    message: &UpdateMessage,
    path: &Path,
    cancellation: &CancellationToken,
    progress: &JobProgress,
    limits: DownloadLimits,
    delivery: DeliveryPlan,
) -> anyhow::Result<()> {
    // Metadata and thumbnail are best-effort: a valid video must still be sent
    // even if probing or thumbnail extraction hiccups. `probe_video` only fails
    // when the file is not a real/decodable video, which we treat as a hard
    // error so we never upload garbage masquerading as media.
    let prepared = prepare_video(path, cancellation, progress, limits).await?;
    progress.inspecting();
    let metadata = probe_video(prepared.path(), cancellation, UTILITY_COMMAND_TIMEOUT).await?;
    progress.uploading(delivery.position.0, delivery.position.1);
    progress.thumbnailing();
    let thumbnail = extract_thumbnail(prepared.path(), cancellation, UTILITY_COMMAND_TIMEOUT)
        .await
        .ok()
        .map(TempFileGuard::new);

    let result: anyhow::Result<()> = async {
        let video = upload_file_with_progress(
            client,
            prepared.path(),
            cancellation,
            progress,
            delivery.position,
            limits.upload_timeout,
        )
        .await?;
        let mut input = InputMessage::new()
            .document(video)
            .attribute(Attribute::Video {
                round_message: false,
                supports_streaming: true,
                duration: Duration::from_secs(metadata.duration_secs.max(0) as u64),
                w: metadata.width,
                h: metadata.height,
            });
        if let Some(thumbnail) = &thumbnail
            && let Ok(Ok(thumb)) = tokio::time::timeout(
                TELEGRAM_OPERATION_TIMEOUT,
                client.upload_file(thumbnail.path()),
            )
            .await
        {
            input = input.thumbnail(thumb);
        }
        deliver_media(message, progress, input, delivery.replace_status).await?;
        Ok(())
    }
    .await;

    result
}

async fn send_image(
    client: &Client,
    message: &UpdateMessage,
    path: &Path,
    cancellation: &CancellationToken,
    progress: &JobProgress,
    limits: DownloadLimits,
    delivery: DeliveryPlan,
) -> anyhow::Result<()> {
    let photo = upload_file_with_progress(
        client,
        path,
        cancellation,
        progress,
        delivery.position,
        limits.upload_timeout,
    )
    .await?;
    deliver_media(
        message,
        progress,
        InputMessage::new().photo(photo),
        delivery.replace_status,
    )
    .await
}

async fn send_audio(
    client: &Client,
    message: &UpdateMessage,
    path: &Path,
    cancellation: &CancellationToken,
    progress: &JobProgress,
    limits: DownloadLimits,
    delivery: DeliveryPlan,
) -> anyhow::Result<()> {
    let audio = upload_file_with_progress(
        client,
        path,
        cancellation,
        progress,
        delivery.position,
        limits.upload_timeout,
    )
    .await?;
    deliver_media(
        message,
        progress,
        InputMessage::new().document(audio),
        delivery.replace_status,
    )
    .await
}

async fn deliver_media(
    source: &UpdateMessage,
    progress: &JobProgress,
    media: InputMessage,
    replace_status: bool,
) -> anyhow::Result<()> {
    progress.finalizing();
    if !replace_status {
        tokio::time::timeout(TELEGRAM_OPERATION_TIMEOUT, source.respond(media))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "Telegram media send timed out after {} seconds",
                    TELEGRAM_OPERATION_TIMEOUT.as_secs()
                )
            })??;
        return Ok(());
    }
    match progress.replace_with_media(media.clone()).await {
        Ok(true) => Ok(()),
        Ok(false) => match timed_fallback_send(source, progress, media).await {
            Ok(_) => {
                progress.delete_status().await;
                Ok(())
            }
            Err(send_error) => Err(anyhow::anyhow!(
                "status was unavailable and fallback media failed: {send_error}"
            )),
        },
        Err(edit_error) => match timed_fallback_send(source, progress, media).await {
            Ok(_) => {
                log::warn!("Could not edit status into media; used fallback send: {edit_error}");
                progress.delete_status().await;
                Ok(())
            }
            Err(send_error) => Err(anyhow::anyhow!(
                "could not edit status ({edit_error}) or send fallback media ({send_error})"
            )),
        },
    }
}

async fn timed_fallback_send(
    source: &UpdateMessage,
    progress: &JobProgress,
    media: InputMessage,
) -> anyhow::Result<()> {
    match tokio::time::timeout(TELEGRAM_OPERATION_TIMEOUT, source.reply(media)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => {
            progress.resume_text_status();
            Err(error.into())
        }
        Err(_) => {
            progress.resume_text_status();
            anyhow::bail!("Telegram fallback media send timed out")
        }
    }
}

fn perform_cleanup(
    cleanup_path: &Option<PathBuf>,
    yt_dlp_base: &Option<String>,
    is_yt_dlp: bool,
    info: &str,
) {
    if let Some(path) = cleanup_path
        && path.exists()
    {
        if is_yt_dlp && path.is_file() {
            match std::fs::remove_file(path) {
                Ok(()) => println!("{info} | Cleaned up yt-dlp file: {}", path.display()),
                Err(e) => println!("{info} | Error during cleanup of {}: {e}", path.display()),
            }
        }
        if !is_yt_dlp && path.is_dir() {
            match std::fs::remove_dir_all(path) {
                Ok(()) => println!(
                    "{info} | Cleaned up gallery-dl directory: {}",
                    path.display()
                ),
                Err(e) => println!("{info} | Error during cleanup of {}: {e}", path.display()),
            }
            return;
        }
    }

    // Orphaned yt-dlp files (prefix-based) cleanup.
    if is_yt_dlp
        && let Some(base) = yt_dlp_base
        && let Ok(entries) = std::fs::read_dir(".")
    {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(base) {
                match std::fs::remove_file(entry.path()) {
                    Ok(()) => println!("{info} | Cleaned up orphaned yt-dlp file: {name}"),
                    Err(e) => {
                        println!("{info} | Error cleaning up orphaned yt-dlp file {name}: {e}")
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcodes_incompatible_or_oversized_video() {
        assert!(!requires_video_transcode(true, "h264", 99, 100));
        assert!(requires_video_transcode(false, "h264", 99, 100));
        assert!(requires_video_transcode(true, "vp9", 99, 100));
        assert!(requires_video_transcode(true, "h264", 101, 100));
    }

    #[test]
    fn bitrate_math_keeps_headroom_and_rejects_impossible_limits() {
        let bitrate = target_video_bitrate(100_000_000, 100).unwrap();
        assert_eq!(bitrate, 6_672_000);
        assert!(target_video_bitrate(100_000, 3_600).is_err());
        assert!(target_video_bitrate(100_000_000, 0).is_err());
    }

    #[test]
    fn quality_selector_caps_every_fallback() {
        let selector = video_format_selector(720);
        assert_eq!(selector.matches("height<=720").count(), 3);
    }

    #[test]
    fn youtube_selector_prefers_compatible_media_within_upload_budget() {
        let selector = youtube_format_selector(1_000_000, Some(720));
        assert!(selector.contains("[vcodec^=avc1][height<=720][filesize<900000]"));
        assert!(selector.contains("[filesize_approx<900000]"));
        assert!(selector.contains("[filesize<1000000]"));
        assert!(selector.ends_with(&video_format_selector(720)));
    }

    #[test]
    fn ffmpeg_progress_is_bounded_and_tolerates_unknown_lines() {
        assert_eq!(
            transcode_percent(b"out_time_us=25000000\n", 100_000_000),
            Some(25)
        );
        assert_eq!(
            transcode_percent(b"out_time_ms=200000000\n", 100_000_000),
            Some(99)
        );
        assert_eq!(transcode_percent(b"progress=continue\n", 100_000_000), None);
        assert_eq!(transcode_percent(b"out_time_us=1\n", 0), None);
    }

    #[test]
    fn yt_dlp_progress_parser_handles_padding_and_bounds() {
        assert_eq!(download_percent(b"download:  42.7%\n"), Some(42));
        assert_eq!(download_percent(b"download:101.0%\n"), Some(100));
        assert_eq!(download_percent(b"[download] destination\n"), None);
    }

    #[test]
    fn recognizes_audio_downloads() {
        let (_, _, is_audio, mime) = media_kind(Path::new("track.mp3"));
        assert!(is_audio);
        assert_eq!(mime.as_deref(), Some("audio/mpeg"));
    }

    #[test]
    fn in_flight_progress_never_claims_completion() {
        assert_eq!(in_flight_upload_bytes(500, 1_000), 500);
        assert_eq!(in_flight_upload_bytes(1_000, 1_000), 999);
        assert_eq!(in_flight_upload_bytes(2_000, 1_000), 999);
    }
}
