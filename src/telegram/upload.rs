use std::future::Future;
use std::io;
use std::time::Duration;

use anyhow::Context;
use grammers_client::InvocationError;

use crate::cancel::CancellationToken;

const MAX_ATTEMPTS: u32 = 3;

// Uploading bytes does not send a message. A fresh upload can safely start
// again after a disconnect, unlike an arbitrary Telegram delivery operation.
pub async fn with_retries<T, F, Fut>(
    cancellation: &CancellationToken,
    timeout: Duration,
    operation: &str,
    mut upload: F,
) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = io::Result<T>>,
{
    let attempts = async {
        for attempt in 1..=MAX_ATTEMPTS {
            match upload().await {
                Ok(uploaded) => return Ok(uploaded),
                Err(error) if attempt < MAX_ATTEMPTS && is_retryable(&error) => {
                    let delay = Duration::from_secs(u64::from(attempt));
                    log::warn!(
                        "{operation} failed: {error}; restarting upload in {}s \
                         (attempt {}/{MAX_ATTEMPTS})",
                        delay.as_secs(),
                        attempt + 1,
                    );
                    tokio::time::sleep(delay).await;
                }
                Err(error) => {
                    let context = format!("{operation} failed after {attempt} attempt(s): {error}");
                    return Err(error).context(context);
                }
            }
        }
        unreachable!("the final attempt always returns")
    };

    tokio::select! {
        biased;
        _ = cancellation.cancelled() => anyhow::bail!("cancelled"),
        _ = tokio::time::sleep(timeout) => anyhow::bail!(
            "{operation} timed out after {} seconds",
            timeout.as_secs()
        ),
        result = attempts => result,
    }
}

fn is_retryable(error: &io::Error) -> bool {
    if let Some(error) = error
        .get_ref()
        .and_then(|source| source.downcast_ref::<InvocationError>())
    {
        return match error {
            InvocationError::Io(error) => is_connection_error(error.kind()),
            InvocationError::Rpc(error) => (500..600).contains(&error.code.unsigned_abs()),
            _ => false,
        };
    }
    is_connection_error(error.kind())
}

fn is_connection_error(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::NotConnected
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::TimedOut
            | io::ErrorKind::UnexpectedEof
    )
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use grammers_client::sender::RpcError;

    use super::*;

    #[test]
    fn recognizes_wrapped_telegram_disconnects_and_temporary_server_errors() {
        for kind in [io::ErrorKind::UnexpectedEof, io::ErrorKind::ConnectionReset] {
            let error = io::Error::other(InvocationError::Io(io::Error::from(kind)));
            assert!(is_retryable(&error));
        }
        for code in [-500, 503] {
            assert!(is_retryable(&io::Error::other(InvocationError::Rpc(
                RpcError {
                    code,
                    name: "INTERNAL".into(),
                    value: None,
                    caused_by: None,
                }
            ))));
        }
        for code in [400, 401, 403, 420] {
            assert!(!is_retryable(&io::Error::other(InvocationError::Rpc(
                RpcError {
                    code,
                    name: "PERMANENT_OR_FLOOD_WAIT".into(),
                    value: None,
                    caused_by: None,
                }
            ))));
        }
        assert!(!is_retryable(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
        assert!(!is_retryable(&io::Error::from(io::ErrorKind::NotFound)));
    }

    #[tokio::test]
    async fn recovers_from_disconnects_but_stops_after_three_attempts() {
        let calls = AtomicU32::new(0);
        let result = with_retries(
            &CancellationToken::new(),
            Duration::from_secs(10),
            "test upload",
            || {
                let attempt = calls.fetch_add(1, Ordering::Relaxed);
                async move {
                    if attempt < 2 {
                        Err(io::Error::from(io::ErrorKind::UnexpectedEof))
                    } else {
                        Ok(42)
                    }
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(result, 42);
        assert_eq!(calls.load(Ordering::Relaxed), 3);

        calls.store(0, Ordering::Relaxed);
        let result = with_retries(
            &CancellationToken::new(),
            Duration::from_secs(10),
            "test upload",
            || {
                calls.fetch_add(1, Ordering::Relaxed);
                async { Err::<(), _>(io::Error::from(io::ErrorKind::UnexpectedEof)) }
            },
        )
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn permanent_errors_are_not_retried() {
        let calls = AtomicU32::new(0);
        let result = with_retries(
            &CancellationToken::new(),
            Duration::from_secs(1),
            "test upload",
            || {
                calls.fetch_add(1, Ordering::Relaxed);
                async { Err::<(), _>(io::Error::from(io::ErrorKind::PermissionDenied)) }
            },
        )
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn cancellation_and_deadline_interrupt_retry_backoff() {
        let cancellation = CancellationToken::new();
        let result = with_retries(
            &cancellation,
            Duration::from_millis(20),
            "test upload",
            || async { Err::<(), _>(io::Error::from(io::ErrorKind::UnexpectedEof)) },
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("timed out"));

        let upload = with_retries(
            &cancellation,
            Duration::from_secs(10),
            "test upload",
            || async { Err::<(), _>(io::Error::from(io::ErrorKind::UnexpectedEof)) },
        );
        let cancel = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancellation.cancel();
        };
        let (result, ()) = tokio::join!(upload, cancel);
        assert_eq!(result.unwrap_err().to_string(), "cancelled");
    }
}
