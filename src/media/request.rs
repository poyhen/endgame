use std::time::Duration;

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
    Clip(ClipRange),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipRange {
    start: Duration,
    end: Duration,
}

impl ClipRange {
    pub fn new(start: Duration, end: Duration) -> Option<Self> {
        (start < end).then_some(Self { start, end })
    }

    pub fn start(&self) -> Duration {
        self.start
    }

    pub fn end(&self) -> Duration {
        self.end
    }
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

    pub fn clip(url: String, range: ClipRange) -> Self {
        Self {
            url,
            mode: DownloadMode::Clip(range),
        }
    }
}

pub enum DownloadOutcome {
    Completed,
    Failed,
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_ranges_must_move_forward() {
        let second = Duration::from_secs(1);
        let two_seconds = Duration::from_secs(2);

        assert_eq!(
            ClipRange::new(second, two_seconds),
            Some(ClipRange {
                start: second,
                end: two_seconds,
            })
        );
        assert!(ClipRange::new(second, second).is_none());
        assert!(ClipRange::new(two_seconds, second).is_none());
    }
}
