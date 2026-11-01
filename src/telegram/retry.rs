use std::ops::ControlFlow;
use std::time::Duration;

use grammers_client::InvocationError;
use grammers_client::sender::{AutoSleep, RetryContext, RetryPolicy};
use grammers_client::tl::{self, Identifiable};

const MAX_ATTEMPTS: u32 = 8;
const CONNECTION_INIT_ID: u32 =
    tl::functions::InvokeWithLayer::<tl::functions::help::GetConfig>::CONSTRUCTOR_ID;

#[derive(Default)]
pub struct ConnectionRetryPolicy {
    fallback: AutoSleep,
}

impl RetryPolicy for ConnectionRetryPolicy {
    fn should_retry(&self, ctx: &RetryContext) -> ControlFlow<(), Duration> {
        // The pool initializes connections with invokeWithLayer(initConnection(getConfig)).
        // If that fails, the caller's request has not been sent, so retrying is safe
        // even when the caller was about to send a message or upload media.
        if let InvocationError::Rpc(error) = &ctx.error
            && (500..600).contains(&error.code.unsigned_abs())
            && error.caused_by == Some(CONNECTION_INIT_ID)
        {
            let attempt = ctx.fail_count.get();
            if attempt >= MAX_ATTEMPTS {
                log::error!(
                    "Telegram connection is still unavailable after {attempt} attempts; \
                     try starting again later: {error}"
                );
                return ControlFlow::Break(());
            }

            let delay = Duration::from_secs((1_u64 << attempt).min(30));
            log::warn!(
                "Telegram connection initialization failed: {error}; \
                 retrying in {}s (attempt {}/{MAX_ATTEMPTS})",
                delay.as_secs(),
                attempt + 1,
            );
            return ControlFlow::Continue(delay);
        }

        self.fallback.should_retry(ctx)
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use grammers_client::sender::RpcError;

    use super::*;

    fn context(code: i32, caused_by: Option<u32>, attempt: u32) -> RetryContext {
        RetryContext {
            fail_count: NonZeroU32::new(attempt).unwrap(),
            slept_so_far: Duration::ZERO,
            error: InvocationError::Rpc(RpcError {
                code,
                name: "No workers running".into(),
                value: None,
                caused_by,
            }),
        }
    }

    #[test]
    fn retries_positive_and_negative_server_errors_during_connection_init() {
        let policy = ConnectionRetryPolicy::default();
        for code in [-500, 500, 503] {
            assert_eq!(
                policy.should_retry(&context(code, Some(CONNECTION_INIT_ID), 1)),
                ControlFlow::Continue(Duration::from_secs(2)),
            );
        }
    }

    #[test]
    fn backs_off_and_stops_after_two_minutes_of_delays() {
        let policy = ConnectionRetryPolicy::default();
        let mut total = Duration::ZERO;
        for (index, seconds) in [2, 4, 8, 16, 30, 30, 30].into_iter().enumerate() {
            let mut ctx = context(-500, Some(CONNECTION_INIT_ID), index as u32 + 1);
            ctx.slept_so_far = total;
            let delay = Duration::from_secs(seconds);
            assert_eq!(policy.should_retry(&ctx), ControlFlow::Continue(delay));
            total += delay;
        }
        assert_eq!(total, Duration::from_secs(120));
        assert_eq!(
            policy.should_retry(&context(-500, Some(CONNECTION_INIT_ID), 8)),
            ControlFlow::Break(()),
        );
    }

    #[test]
    fn does_not_retry_permanent_init_errors_or_failed_application_requests() {
        let policy = ConnectionRetryPolicy::default();
        for (code, cause) in [
            (400, Some(CONNECTION_INIT_ID)),
            (401, Some(CONNECTION_INIT_ID)),
            (403, Some(CONNECTION_INIT_ID)),
            (
                500,
                Some(tl::functions::messages::SendMessage::CONSTRUCTOR_ID),
            ),
            (-500, None),
        ] {
            assert_eq!(
                policy.should_retry(&context(code, cause, 1)),
                ControlFlow::Break(()),
            );
        }
    }

    #[test]
    fn preserves_default_flood_wait_and_io_retries() {
        let policy = ConnectionRetryPolicy::default();
        let mut ctx = context(420, None, 1);
        if let InvocationError::Rpc(error) = &mut ctx.error {
            error.name = "FLOOD_WAIT".into();
            error.value = Some(12);
        }
        assert_eq!(
            policy.should_retry(&ctx),
            ControlFlow::Continue(Duration::from_secs(12)),
        );
        ctx.fail_count = NonZeroU32::new(2).unwrap();
        assert_eq!(policy.should_retry(&ctx), ControlFlow::Break(()));

        ctx.fail_count = NonZeroU32::new(1).unwrap();
        ctx.error = InvocationError::Io(std::io::Error::from(std::io::ErrorKind::ConnectionReset));
        assert_eq!(
            policy.should_retry(&ctx),
            ControlFlow::Continue(Duration::from_secs(1)),
        );
        ctx.fail_count = NonZeroU32::new(2).unwrap();
        assert_eq!(policy.should_retry(&ctx), ControlFlow::Break(()));
    }
}
