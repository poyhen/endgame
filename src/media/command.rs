use std::future::Future;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{ChildStdout, Command};

use crate::cancel::CancellationToken;

pub const OUTPUT_LIMIT: usize = 64 * 1024;

pub enum CommandOutcome {
    Finished {
        success: bool,
        stdout: String,
        stderr: String,
    },
    Cancelled,
    TimedOut,
}

pub async fn run<F, Fut>(
    mut command: Command,
    cancellation: &CancellationToken,
    timeout: Duration,
    operation: &str,
    stdout_reader: F,
) -> CommandOutcome
where
    F: FnOnce(ChildStdout) -> Fut,
    Fut: Future<Output = Vec<u8>> + Send + 'static,
{
    let program = command
        .as_std()
        .get_program()
        .to_string_lossy()
        .into_owned();
    command
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);

    let mut child = match command.spawn() {
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
    log::info!("{operation} started {program} (pid={pid:?})");

    let stdout_task = child
        .stdout
        .take()
        .map(|stdout| tokio::spawn(stdout_reader(stdout)));
    let stderr_task = child
        .stderr
        .take()
        .map(|mut stderr| tokio::spawn(async move { read_bounded(&mut stderr).await }));

    enum StopReason {
        Finished,
        Cancelled,
        TimedOut,
    }
    let (reason, status) = tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            terminate(&mut child).await;
            (StopReason::Cancelled, child.wait().await)
        },
        _ = tokio::time::sleep(timeout) => {
            terminate(&mut child).await;
            (StopReason::TimedOut, child.wait().await)
        },
        result = child.wait() => (StopReason::Finished, result),
    };
    let interrupted = !matches!(reason, StopReason::Finished);
    let stdout = collect(stdout_task, interrupted).await;
    let stderr = collect(stderr_task, interrupted).await;
    log::info!(
        "{operation} {program} ended after {:.1}s ({})",
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

pub async fn read_bounded(reader: &mut (impl AsyncRead + Unpin)) -> Vec<u8> {
    let mut output = Vec::with_capacity(OUTPUT_LIMIT);
    let mut buffer = [0u8; 8192];
    while let Ok(read) = reader.read(&mut buffer).await {
        if read == 0 {
            break;
        }
        output.extend_from_slice(&buffer[..read]);
        if output.len() > OUTPUT_LIMIT * 2 {
            let excess = output.len() - OUTPUT_LIMIT;
            output.drain(..excess);
        }
    }
    if output.len() > OUTPUT_LIMIT {
        let excess = output.len() - OUTPUT_LIMIT;
        output.drain(..excess);
    }
    output
}

async fn terminate(child: &mut tokio::process::Child) {
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

async fn collect(task: Option<tokio::task::JoinHandle<Vec<u8>>>, interrupted: bool) -> Vec<u8> {
    let Some(mut task) = task else {
        return Vec::new();
    };
    let timeout = if interrupted {
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn shell(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(script);
        command
    }

    async fn run_test(
        command: Command,
        cancellation: &CancellationToken,
        timeout: Duration,
    ) -> CommandOutcome {
        run(
            command,
            cancellation,
            timeout,
            "Command test",
            |mut stdout| async move { read_bounded(&mut stdout).await },
        )
        .await
    }

    #[tokio::test]
    async fn captures_successful_output() {
        let outcome = run_test(
            shell("printf success"),
            &CancellationToken::new(),
            Duration::from_secs(1),
        )
        .await;
        assert!(matches!(
            outcome,
            CommandOutcome::Finished {
                success: true,
                ref stdout,
                ..
            } if stdout == "success"
        ));
    }

    #[tokio::test]
    async fn enforces_timeout() {
        let outcome = run_test(
            shell("sleep 5"),
            &CancellationToken::new(),
            Duration::from_millis(20),
        )
        .await;
        assert!(matches!(outcome, CommandOutcome::TimedOut));
    }

    #[tokio::test]
    async fn cancellation_wins_before_completion() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let outcome = run_test(shell("sleep 5"), &cancellation, Duration::from_secs(1)).await;
        assert!(matches!(outcome, CommandOutcome::Cancelled));
    }
}
